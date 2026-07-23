// Adapted from green-vita (MPL-2.0, https://github.com/Day-OS/green-vita)
// src/streaming/video/worker.rs - dedicated decode thread pulling H.264 access units from a
// bounded queue and publishing decoded frames through DirectVideoOutput. Metrics and the
// adaptive queue sizing were dropped in this port; results publish straight into the
// `(frame id, DecodedFrame)` slot the shell polls. See THIRD_PARTY_NOTICES.md.

use super::decoder::HwVideoDecoder;
use super::{DecodedFrame, DecoderConfig, DirectVideoOutput};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased, unbounded};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// Two compressed frames of queue (~66ms at 30 fps) - green-vita's minimum. Anything deeper
// converts network jitter into steady-state latency; when the queue is full we drop instead.
const MAX_PENDING_ACCESS_UNITS: usize = 2;

struct QueuedAccessUnit {
    data: Vec<u8>,
    generation: u64,
}

enum DecoderCommand {
    Reset,
    Stop,
}

pub struct VideoDecodeWorker {
    access_units: Sender<QueuedAccessUnit>,
    commands: Sender<DecoderCommand>,
    generation: Arc<AtomicU64>,
}

impl VideoDecodeWorker {
    /// Spawns the decode thread. Decoded frames land in `latest_frame` (with a monotonically
    /// increasing id) for `shell::surface::sync_video_frame` to pick up.
    pub fn spawn(
        config: DecoderConfig,
        direct_output: Arc<DirectVideoOutput>,
        latest_frame: Arc<Mutex<Option<(u64, DecodedFrame)>>>,
    ) -> Result<Self> {
        let decoder =
            HwVideoDecoder::new(config).context("failed to create hardware H264 decoder")?;
        direct_output.decoder_ready.store(true, Ordering::Release);
        let (access_units, worker_access_units) = bounded(MAX_PENDING_ACCESS_UNITS);
        let (commands, worker_commands) = unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);

        std::thread::Builder::new()
            .name("jade-vita-video-decode".to_owned())
            .spawn(move || {
                #[cfg(target_os = "vita")]
                pin_decoder_thread();
                run_decode_loop(
                    worker_access_units,
                    worker_commands,
                    worker_generation,
                    latest_frame,
                    decoder,
                    config,
                    direct_output,
                )
            })
            .context("failed to spawn video decode worker")?;

        Ok(Self {
            access_units,
            commands,
            generation,
        })
    }

    /// Queues one Annex-B access unit; drops it (returning `false`) when the decoder is
    /// falling behind, which is preferable to buffering latency.
    pub fn submit_access_unit(&self, data: Vec<u8>) -> bool {
        let access_unit = QueuedAccessUnit {
            data,
            generation: self.generation.load(Ordering::Acquire),
        };
        match self.access_units.try_send(access_unit) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Drops queued frames and recreates the decoder - used after stream discontinuities.
    pub fn reset_decoder(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.commands.send(DecoderCommand::Reset);
    }
}

impl Drop for VideoDecodeWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(DecoderCommand::Stop);
    }
}

#[cfg(target_os = "vita")]
fn pin_decoder_thread() {
    let thread_id = unsafe { vitasdk_sys::sceKernelGetThreadId() };
    let result = unsafe {
        vitasdk_sys::sceKernelChangeThreadCpuAffinityMask(
            thread_id,
            vitasdk_sys::SCE_KERNEL_CPU_MASK_USER_2 as i32,
        )
    };
    if result < 0 {
        eprintln!("Failed to pin video decoder thread to user CPU 2: {result:#x}");
    }
}

fn run_decode_loop(
    access_units: Receiver<QueuedAccessUnit>,
    commands: Receiver<DecoderCommand>,
    generation: Arc<AtomicU64>,
    latest_frame: Arc<Mutex<Option<(u64, DecodedFrame)>>>,
    initial_decoder: HwVideoDecoder,
    config: DecoderConfig,
    direct_output: Arc<DirectVideoOutput>,
) {
    let mut decoder = Some(initial_decoder);
    let mut frame_id: u64 = 0;

    loop {
        select_biased! {
            recv(commands) -> command => match command {
                Ok(DecoderCommand::Reset) => {
                    decoder = None;
                    continue;
                }
                Ok(DecoderCommand::Stop) | Err(_) => break,
            },
            recv(access_units) -> access_unit => {
                let Ok(access_unit) = access_unit else { break };
                decode_queued_access_unit(
                    &mut decoder,
                    config,
                    &generation,
                    &latest_frame,
                    &mut frame_id,
                    access_unit,
                    &direct_output,
                );
            }
        }
    }
}

fn decode_queued_access_unit(
    decoder: &mut Option<HwVideoDecoder>,
    config: DecoderConfig,
    generation: &AtomicU64,
    latest_frame: &Mutex<Option<(u64, DecodedFrame)>>,
    frame_id: &mut u64,
    access_unit: QueuedAccessUnit,
    direct_output: &DirectVideoOutput,
) {
    if access_unit.generation != generation.load(Ordering::Acquire) {
        return;
    }

    if decoder.is_none() {
        match HwVideoDecoder::new(config) {
            Ok(new_decoder) => *decoder = Some(new_decoder),
            Err(error) => {
                eprintln!("failed to recreate H264 decoder: {error:#}");
                return;
            }
        }
    }

    // The renderer registers the texture pixel format together with the targets; without
    // either there is nowhere (and no way) to decode.
    let Some(pixel_format) = direct_output.pixel_format() else {
        return;
    };
    let Some(direct_target) = direct_output.lock_decode_target() else {
        return;
    };
    // Contain an unexpected decoder panic inside its worker thread.
    let decode_result = catch_unwind(AssertUnwindSafe(|| {
        decoder
            .as_mut()
            .expect("decoder recreated above")
            .decode(&access_unit.data, direct_target.target(), pixel_format)
    }));
    if access_unit.generation != generation.load(Ordering::Acquire) {
        return;
    }

    match decode_result {
        Ok(Ok(true)) => {
            let (texture_index, generation) = direct_target.publish();
            *frame_id += 1;
            if let Ok(mut slot) = latest_frame.lock() {
                *slot = Some((
                    *frame_id,
                    DecodedFrame {
                        texture_index,
                        generation,
                    },
                ));
            }
        }
        Ok(Ok(false)) => {}
        Ok(Err(error)) => {
            eprintln!("H264 decode error, recreating decoder: {error:#}");
            *decoder = None;
        }
        Err(_) => {
            eprintln!("H264 decoder panicked; recreating decoder on next frame");
            *decoder = None;
        }
    }
}
