use vita_newlib_shims as _;

mod i18n;
mod locale;
mod app;
mod gfn;
mod input;
mod jobs;
mod safe_memory;
mod shell;
mod streaming;

use app::App;

#[used]
#[unsafe(export_name = "sceUserMainThreadStackSize")]
pub static SCE_USER_MAIN_THREAD_STACK_SIZE: u32 = 4 * 1024 * 1024;

#[used]
#[unsafe(export_name = "sceLibcHeapSize")]
pub static SCE_LIBC_HEAP_SIZE: u32 = 40 * 1024 * 1024;

#[used]
#[unsafe(export_name = "_newlib_heap_size_user")]
pub static NEWLIB_HEAP_SIZE_USER: u32 = 192 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    // Kept alive for the app's whole lifetime: the GFN token encryption key lives in Safe
    // Memory (see gfn::auth), which requires AppUtil to be initialized first.
    let _app_util = safe_memory::AppUtil::initialize()?;
    // Grab CDRAM for the hardware H.264 decoder before anything can fragment it (green-vita's
    // pattern); the decoder swaps it for an exact-size block when a stream starts.
    #[cfg(target_os = "vita")]
    streaming::video::reserve_decoder_cdram();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        let app = App::new()?;
        shell::run(app).await
    })
}
