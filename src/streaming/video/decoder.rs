// Adapted from green-vita (MPL-2.0, https://github.com/Day-OS/green-vita)
// src/streaming/video/decoder.rs - PS Vita hardware H.264 decoder (`sceVideodec`/`sceAvcdec`)
// writing RGB565 straight into the SDL texture registered by the shell.
// See THIRD_PARTY_NOTICES.md.

use super::{DecoderConfig, VideoPixelFormat, VideoTextureTarget};
#[cfg(not(target_os = "vita"))]
use anyhow::Result;

#[cfg(target_os = "vita")]
mod vita {
    use super::super::memory::{CdramBlock, release_reserved_decoder_cdram};
    use super::{DecoderConfig, VideoPixelFormat, VideoTextureTarget};
    use anyhow::{Result, bail};
    use std::os::raw::c_void;
    use vitasdk_sys::*;

    // The idea of reducing the reference frames came from MattKC's Vanilla project.
    const AVCDEC_NUM_REF_FRAMES: u32 = 1;

    struct AvcdecLibrary {
        module_loaded: bool,
    }

    impl AvcdecLibrary {
        fn initialize(width: u32, height: u32) -> Result<Self> {
            let module_loaded = unsafe {
                let loaded_before = sceSysmoduleIsLoaded(SCE_SYSMODULE_AVCDEC);
                let ret = sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC);
                if ret >= 0 {
                    true
                } else if ret as u32 == SCE_SYSMODULE_ERROR_INVALID_VALUE {
                    eprintln!(
                        "sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC) returned {ret:#x}; continuing with SceVideodec imports; is_loaded_before={loaded_before:#x}",
                    );
                    false
                } else {
                    bail!(
                        "sceSysmoduleLoadModule(SCE_SYSMODULE_AVCDEC) failed: {ret:#x}; is_loaded_before={loaded_before:#x}",
                    );
                }
            };

            let init_info = SceVideodecQueryInitInfoHwAvcdec {
                size: size_of::<SceVideodecQueryInitInfoHwAvcdec>() as u32,
                horizontal: width,
                vertical: height,
                numOfRefFrames: AVCDEC_NUM_REF_FRAMES,
                numOfStreams: 1,
            };
            let ret = unsafe { sceVideodecInitLibrary(SCE_VIDEODEC_TYPE_HW_AVCDEC, &init_info) };
            if ret < 0 {
                if module_loaded {
                    unsafe {
                        sceSysmoduleUnloadModule(SCE_SYSMODULE_AVCDEC);
                    }
                }
                bail!("sceVideodecInitLibrary failed: {ret:#x}");
            }

            Ok(Self { module_loaded })
        }
    }

    impl Drop for AvcdecLibrary {
        fn drop(&mut self) {
            unsafe {
                sceVideodecTermLibrary(SCE_VIDEODEC_TYPE_HW_AVCDEC);
                if self.module_loaded {
                    sceSysmoduleUnloadModule(SCE_SYSMODULE_AVCDEC);
                }
            }
        }
    }

    struct AvcdecDecoder(SceAvcdecCtrl);

    impl Drop for AvcdecDecoder {
        fn drop(&mut self) {
            unsafe {
                sceAvcdecDeleteDecoder(&mut self.0);
            }
        }
    }

    pub struct HwVideoDecoder {
        decoder: AvcdecDecoder,
        _frame_memory: CdramBlock,
        _library: AvcdecLibrary,
        width: u32,
        height: u32,
    }

    impl HwVideoDecoder {
        pub fn new(config: DecoderConfig) -> Result<Self> {
            unsafe {
                let library =
                    AvcdecLibrary::initialize(config.decode_width, config.decode_height)?;

                let query = SceAvcdecQueryDecoderInfo {
                    horizontal: config.decode_width,
                    vertical: config.decode_height,
                    numOfRefFrames: AVCDEC_NUM_REF_FRAMES,
                };
                let mut decoder_info = SceAvcdecDecoderInfo { frameMemSize: 0 };
                let ret = sceAvcdecQueryDecoderMemSize(
                    SCE_VIDEODEC_TYPE_HW_AVCDEC,
                    &query,
                    &mut decoder_info,
                );
                if ret < 0 {
                    bail!("sceAvcdecQueryDecoderMemSize failed: {ret:#x}");
                }
                release_reserved_decoder_cdram();
                let frame_memory =
                    CdramBlock::allocate("jade_hw_video_frame", decoder_info.frameMemSize)?;
                let mut decoder_control = SceAvcdecCtrl {
                    handle: 0,
                    frameBuf: SceAvcdecBuf {
                        pBuf: frame_memory.ptr.cast(),
                        size: decoder_info.frameMemSize,
                    },
                };
                let ret = sceAvcdecCreateDecoder(
                    SCE_VIDEODEC_TYPE_HW_AVCDEC,
                    &mut decoder_control,
                    &query,
                );
                if ret < 0 {
                    bail!("sceAvcdecCreateDecoder failed: {ret:#x}");
                }
                let decoder = AvcdecDecoder(decoder_control);

                Ok(Self {
                    decoder,
                    _frame_memory: frame_memory,
                    _library: library,
                    width: config.output_width,
                    height: config.output_height,
                })
            }
        }

        /// Decodes one Access Unit into `direct_target` using the pixel format the render
        /// thread registered. Returns `false` if the hardware buffered it without producing a
        /// picture yet.
        pub fn decode(
            &mut self,
            access_unit: &[u8],
            direct_target: VideoTextureTarget,
            format: VideoPixelFormat,
        ) -> Result<bool> {
            unsafe {
                let au = SceAvcdecAu {
                    pts: SceVideodecTimeStamp {
                        upper: 0xFFFFFFFF,
                        lower: 0xFFFFFFFF,
                    },
                    dts: SceVideodecTimeStamp {
                        upper: 0xFFFFFFFF,
                        lower: 0xFFFFFFFF,
                    },
                    es: SceAvcdecBuf {
                        pBuf: access_unit.as_ptr() as *mut c_void,
                        size: access_unit.len() as u32,
                    },
                };

                let output_ptr = direct_target.ptr as *mut u8;
                let output_capacity = direct_target.capacity;
                // framePitch is in pixels: 2 bytes each for RGB565, 1 byte per luma sample
                // for planar YUV (whose three tightly-packed planes need w*h*3/2 bytes).
                let (pixel_type, output_pitch, required_capacity) = match format {
                    VideoPixelFormat::Bgr565 => (
                        SCE_AVCDEC_PIXELFORMAT_RGBA565 as u32,
                        direct_target.pitch / 2,
                        (direct_target.pitch / 2).saturating_mul(self.height) * 2,
                    ),
                    VideoPixelFormat::Iyuv => (
                        SCE_AVCDEC_PIXELFORMAT_YUV420_RASTER as u32,
                        direct_target.pitch,
                        self.width.saturating_mul(self.height) * 3 / 2,
                    ),
                };
                if output_pitch < self.width {
                    bail!(
                        "direct video texture pitch {output_pitch} is smaller than {}",
                        self.width
                    );
                }
                if required_capacity > output_capacity {
                    bail!(
                        "video output needs {required_capacity} bytes but texture has {output_capacity}"
                    );
                }

                let mut picture = SceAvcdecPicture {
                    size: size_of::<SceAvcdecPicture>() as u32,
                    frame: SceAvcdecFrame {
                        pixelType: pixel_type,
                        framePitch: output_pitch,
                        frameWidth: self.width,
                        frameHeight: self.height,
                        horizontalSize: self.width,
                        verticalSize: self.height,
                        frameCropLeftOffset: 0,
                        frameCropRightOffset: 0,
                        frameCropTopOffset: 0,
                        frameCropBottomOffset: 0,
                        opt: SceAvcdecFrameOption {
                            rgba: SceAvcdecFrameOptionRGBA {
                                alpha: 0xff,
                                cscCoefficient: 0,
                                reserved: [0; 14],
                            },
                        },
                        // Second plane pointer: Vita3K writes all planes contiguously through
                        // pPicture[0], but real hardware expects the chroma base here for
                        // YUV output. Points just past the luma plane either way.
                        pPicture: match format {
                            VideoPixelFormat::Bgr565 => {
                                [output_ptr.cast(), std::ptr::null_mut()]
                            }
                            VideoPixelFormat::Iyuv => [
                                output_ptr.cast(),
                                output_ptr
                                    .add((self.width * self.height) as usize)
                                    .cast(),
                            ],
                        },
                    },
                    info: std::mem::zeroed(),
                };
                let mut picture_ptr: *mut SceAvcdecPicture = &mut picture;
                let mut array_picture = SceAvcdecArrayPicture {
                    numOfOutput: 0,
                    numOfElm: 1,
                    pPicture: &mut picture_ptr,
                };

                let ret = sceAvcdecDecode(&self.decoder.0, &au, &mut array_picture);
                if ret < 0 {
                    bail!("sceAvcdecDecode failed: {ret:#x}");
                }
                if array_picture.numOfOutput == 0 {
                    return Ok(false);
                }

                Ok(true)
            }
        }
    }

    // SAFETY: the CDRAM blocks and decoder handle have no thread affinity in the underlying
    // SCE API - this is only ever moved once (into `VideoDecodeWorker`'s thread), never
    // accessed concurrently.
    unsafe impl Send for HwVideoDecoder {}
}

#[cfg(target_os = "vita")]
pub use vita::HwVideoDecoder;

/// Host-build stand-in so `cargo check` for non-Vita targets still type-checks; the real
/// binary only ever ships for the Vita.
#[cfg(not(target_os = "vita"))]
pub struct HwVideoDecoder;

#[cfg(not(target_os = "vita"))]
impl HwVideoDecoder {
    pub fn new(_config: DecoderConfig) -> Result<Self> {
        anyhow::bail!("hardware H.264 decoder is only available on the PS Vita target")
    }

    pub fn decode(
        &mut self,
        _access_unit: &[u8],
        _direct_target: VideoTextureTarget,
        _format: VideoPixelFormat,
    ) -> Result<bool> {
        anyhow::bail!("hardware H.264 decoder is only available on the PS Vita target")
    }
}
