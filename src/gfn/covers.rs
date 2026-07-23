//! Cover-art cache + background download.
//!
//! Mirrors `green-vita`'s pattern (`src/app/image.rs` + `src/api/catalog/worker.rs`):
//! downloaded bytes -> decoded RGBA -> cached `TitleImage` with a lazily-initialized
//! `egui::TextureHandle` (decoupled from any one egui context so it can be reused across
//! `build_ui` calls).
//!
//! Simplified vs. green-vita: instead of a dedicated OS thread running its own tokio runtime
//! and shipping `Crossbeam` jobs (which it needs because green-vita runs its UI on a separate
//! thread from the async runtime), jade-vita's UI loop IS the single-threaded tokio runtime,
//! so spawned cover-fetch tasks naturally advance on the same runtime as the rest of the
//! app - no separate worker, no channel plumbing.
//!
//! Lazy loading: covers are only requested when a tile becomes visible in the grid (see
//! `app::ui::catalog_screen`). A bounded semaphore caps concurrent downloads so the image CDN
//! isn't hammered if the user flicks through the catalog quickly - matches green-vita's
//! `MAX_PENDING_*_JOBS` intent.

use anyhow::{Context, Result};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

/// Maximum simultaneous cover downloads. Conservative: the Vita has limited memory and a
/// single Wi-Fi radio - one in-flight HTTP download per visible tile (typically 12-20 in a
/// grid view) would still be fine cap-wise, but parallelizing at ~8 is plenty of
/// throughput for an image CDN and keeps TLS/HTTP state machine memory bounded.
const MAX_CONCURRENT_COVER_DOWNLOADS: usize = 8;

/// Decoded RGBA cover image, with a lazily-initialized egui texture. Identical in shape to
/// `green-vita::app::image::TitleImage` - reusing that design rather than reinventing.
pub struct TitleImage {
    rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    texture: OnceLock<egui::TextureHandle>,
}

impl TitleImage {
    fn new(rgba: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            rgba,
            width,
            height,
            texture: OnceLock::new(),
        }
    }

    /// Lazily uploads the RGBA to the egui context. Safe to call every frame - the first
    /// caller wins, subsequent calls return the cached handle. `key` is a per-app stable id so
    /// egui dedupes uploads across visits to the same catalog item.
    pub fn texture(&self, ctx: &egui::Context, key: &str) -> &egui::TextureHandle {
        self.texture.get_or_init(|| {
            ctx.load_texture(
                key.to_owned(),
                egui::ColorImage::from_rgba_unmultiplied(
                    [self.width as usize, self.height as usize],
                    &self.rgba,
                ),
                egui::TextureOptions::LINEAR,
            )
        })
    }
}

#[derive(Clone)]
enum CoverState {
    /// A `request` has fired but no terminal state yet.
    Loading,
    /// Decode succeeded (or recovered from cache).
    Ready(Arc<TitleImage>),
    /// Download/decode failed. Re-requesting can retry.
    Failed,
}

/// Shared, lazily-populated cache of cover art. Lives inside `AppState::Catalog`, cloned as an
/// `Arc` into the async download tasks that fill it.
///
/// `Arc<Mutex<HashMap>>` chosen over `DashMap`/`RwLock<HashMap>` for two reasons:
/// - Reads happen every frame (one cell per visible tile), writes are rare (one per finished
///   download). A plain Mutex is still cheap under contention this low and keeps the code small.
/// - The cloneable `Arc` lets a download task drop a value into the map after the originating
///   state has been replaced (e.g. user pressed Back - we'd discard the maps but inflight tasks
///   just write into an orphaned cache that gets GC'd when their last Arc drops). No
///   channel/plumbing through `App` needed.
#[derive(Clone)]
pub struct CoverStore {
    inner: Arc<Mutex<HashMap<String, CoverState>>>,
    download_permits: Arc<Semaphore>,
}

impl Default for CoverStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CoverStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            download_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_COVER_DOWNLOADS)),
        }
    }

    /// Idempotent: no-op if already loading / ready / failed-by-default. On a fresh entry we
    /// record `Loading` and spawn the download task. The joined task lives independent of the
    /// caller - its outcome is written into the shared map on completion.
    pub fn request(&self, http_client: &Client, ctx: &egui::Context, app_id: String, url: String) {
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match inner.get(&app_id) {
            Some(CoverState::Loading | CoverState::Ready(_)) => return,
            Some(CoverState::Failed) => {
                // Allow retry - fall through to spawn a new download.
            }
            None => {}
        }
        inner.insert(app_id.clone(), CoverState::Loading);

        let store = self.clone();
        let http_client = http_client.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            // Bound concurrent downloads so a flurried scroll over many new tiles doesn't fan
            // out ~50 parallel HTTPS requests against NVIDIA's image CDN. Released on drop at
            // the end of the task.
            let _permit = match store.download_permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(error) => {
                    eprintln!("Cover semaphore closed for {app_id}: {error}");
                    let mut inner = match store.inner.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    inner.insert(app_id, CoverState::Failed);
                    return;
                }
            };

            let outcome = fetch_and_decode(&http_client, &url).await;
            let texture_key = format!("gfn_cover_{app_id}");
            let mut inner = match store.inner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match outcome {
                Ok(image) => {
                    let texture = Arc::new(image);
                    // Pre-create the egui texture from the async context so the UI thread can
                    // just blit it next frame without paying decode/upload cost. egui's context
                    // is Send+Sync and `load_texture` uses its internal lock.
                    let _ = texture.texture(&ctx, &texture_key);
                    inner.insert(app_id, CoverState::Ready(texture));
                }
                Err(error) => {
                    eprintln!("Cover fetch for {app_id} failed: {error:#}");
                    inner.insert(app_id, CoverState::Failed);
                }
            }
        });
    }

    /// Returns a snapshot of the current state for `app_id` if present. The returned `Arc`
    /// can be held cheaply across the duration of one frame by the UI without locking the
    /// map again.
    pub fn get(&self, app_id: &str) -> Option<CoverSnapshot> {
        let inner = self.inner.lock().ok()?;
        match inner.get(app_id)? {
            CoverState::Loading => Some(CoverSnapshot::Loading),
            CoverState::Ready(image) => Some(CoverSnapshot::Ready(image.clone())),
            CoverState::Failed => Some(CoverSnapshot::Failed),
        }
    }
}

pub enum CoverSnapshot {
    Loading,
    Ready(Arc<TitleImage>),
    Failed,
}

async fn fetch_and_decode(client: &Client, url: &str) -> Result<TitleImage> {
    let bytes = client
        .get(url)
        .send()
        .await
        .context("cover request failed")?
        .error_for_status()
        .context("cover request returned an error status")?
        .bytes()
        .await
        .context("failed to read cover response body")?;
    decode_rgba(&bytes)
}

/// Decodes JPEG/PNG bytes to RGBA. Resizes if larger than `MAX_COVER_DIM` along its largest
/// axis, mirroring `green-vita`'s `decode_image_rgba` (we skip the "pad to bounds" branch:
/// we render covers at their natural aspect ratio inside a fixed-height tile, so uniform
/// padding is unnecessary).
fn decode_rgba(bytes: &[u8]) -> Result<TitleImage> {
    const MAX_COVER_DIM: u32 = 256;
    let image = image::load_from_memory(bytes).context("failed to decode cover image")?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let scale = (MAX_COVER_DIM as f32 / width.max(height) as f32).min(1.0);
    let target_w = ((width as f32 * scale).round() as u32).max(1);
    let target_h = ((height as f32 * scale).round() as u32).max(1);
    if (target_w, target_h) == (width, height) {
        return Ok(TitleImage::new(rgba.into_raw(), width, height));
    }
    let resized = image::imageops::resize(
        &rgba,
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    Ok(TitleImage::new(resized.into_raw(), target_w, target_h))
}
