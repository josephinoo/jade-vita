// Adapted from green-vita (MPL-2.0, https://github.com/Day-OS/green-vita)
// src/streaming/video/mod.rs - the decoder/render-thread synchronization for the direct
// video-texture path, plus the hardware decoder pieces in the submodules.
// See THIRD_PARTY_NOTICES.md.

mod decoder;
#[cfg(target_os = "vita")]
mod memory;
mod worker;

#[cfg(target_os = "vita")]
pub use memory::reserve_decoder_cdram;
pub use worker::VideoDecodeWorker;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Pixel format negotiated between the render thread (which knows what SDL texture formats
/// the platform supports) and the decoder (which asks sceAvcdec for matching output).
/// Vita3K's sceAvcdec only implements YUV420 output (RGBA silently produces black frames),
/// while SDL's GXM renderer on real hardware is happiest with RGB565 - so the surface tries
/// IYUV first and falls back, recording its choice here for the decoder to follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoPixelFormat {
    Bgr565,
    Iyuv,
}

// Let a short render hitch absorb at most two 30 fps intervals. Together with the
// frame already pending for presentation, this caps the microbuffer at three frames.
const MAX_PENDING_TEXTURE_WAIT: Duration = Duration::from_millis(67);

/// One SDL streaming texture's writable memory, registered by the shell (`shell::surface`).
/// The pointer is stored as an integer so the platform-specific unsafe boundary stays in the
/// code that registers and consumes the textures.
#[derive(Clone, Copy)]
pub struct VideoTextureTarget {
    pub ptr: usize,
    pub pitch: u32,
    pub capacity: u32,
}

struct DirectVideoOutputState {
    targets: Option<[VideoTextureTarget; 2]>,
    displayed: Option<usize>,
    pending: Option<(usize, u64)>,
    next_generation: u64,
}

/// Synchronizes the frame-producer thread with the two SDL/GXM textures owned by the render
/// thread. The producer writes pixels straight into the texture memory - nothing video-sized
/// is ever allocated per frame, which is what keeps the Vita inside its VRAM budget.
pub struct DirectVideoOutput {
    state: Mutex<DirectVideoOutputState>,
    frame_displayed: Condvar,
    pub decoder_ready: AtomicBool,
    /// 0 = not yet registered, 1 = Bgr565, 2 = Iyuv. Set by the render thread together with
    /// `set_targets`; read by the decode thread on every frame.
    pixel_format: AtomicU8,
    pub width: u32,
    pub height: u32,
}

impl DirectVideoOutput {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            state: Mutex::new(DirectVideoOutputState {
                targets: None,
                displayed: None,
                pending: None,
                next_generation: 0,
            }),
            frame_displayed: Condvar::new(),
            decoder_ready: AtomicBool::new(false),
            pixel_format: AtomicU8::new(0),
            width,
            height,
        }
    }

    pub fn set_pixel_format(&self, format: VideoPixelFormat) {
        let value = match format {
            VideoPixelFormat::Bgr565 => 1,
            VideoPixelFormat::Iyuv => 2,
        };
        self.pixel_format.store(value, Ordering::Release);
    }

    pub fn pixel_format(&self) -> Option<VideoPixelFormat> {
        match self.pixel_format.load(Ordering::Acquire) {
            1 => Some(VideoPixelFormat::Bgr565),
            2 => Some(VideoPixelFormat::Iyuv),
            _ => None,
        }
    }

    pub fn set_targets(&self, targets: [VideoTextureTarget; 2]) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = Some(targets);
            state.displayed = None;
            state.pending = None;
        }
    }

    pub fn clear_targets(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = None;
            state.displayed = None;
            state.pending = None;
        }
        self.frame_displayed.notify_all();
    }

    pub fn mark_displayed(&self, index: usize, generation: u64) {
        let mut cleared_pending = false;
        if let Ok(mut state) = self.state.lock() {
            state.displayed = Some(index);
            if state.pending == Some((index, generation)) {
                state.pending = None;
                cleared_pending = true;
            }
        }
        if cleared_pending {
            self.frame_displayed.notify_one();
        }
    }

    /// Blocks (bounded by `MAX_PENDING_TEXTURE_WAIT`) until a texture is free to write into.
    /// Must be called from a dedicated OS thread, never from the tokio/UI thread.
    pub fn lock_decode_target(&self) -> Option<DirectVideoTargetGuard<'_>> {
        let mut state = self.state.lock().ok()?;
        if state.pending.is_some() {
            let (waited_state, _) = self
                .frame_displayed
                .wait_timeout_while(state, MAX_PENDING_TEXTURE_WAIT, |state| {
                    state.targets.is_some() && state.pending.is_some()
                })
                .ok()?;
            state = waited_state;
        }
        let targets = state.targets?;
        let index = state
            .pending
            .map(|(index, _)| index)
            .unwrap_or_else(|| state.displayed.map_or(0, |displayed| 1 - displayed));
        Some(DirectVideoTargetGuard {
            state,
            target: targets[index],
            index,
        })
    }
}

pub struct DirectVideoTargetGuard<'a> {
    state: MutexGuard<'a, DirectVideoOutputState>,
    target: VideoTextureTarget,
    index: usize,
}

impl DirectVideoTargetGuard<'_> {
    pub fn target(&self) -> VideoTextureTarget {
        self.target
    }

    pub fn publish(mut self) -> (usize, u64) {
        self.state.next_generation = self.state.next_generation.wrapping_add(1);
        let generation = self.state.next_generation;
        self.state.pending = Some((self.index, generation));
        (self.index, generation)
    }
}

#[derive(Clone, Copy)]
pub struct DecodedFrame {
    pub texture_index: usize,
    pub generation: u64,
}

#[derive(Clone, Copy)]
pub struct DecoderConfig {
    pub decode_width: u32,
    pub decode_height: u32,
    pub output_width: u32,
    pub output_height: u32,
}
