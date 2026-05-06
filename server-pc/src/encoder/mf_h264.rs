//! Media Foundation H.264 software encoder.
//!
//! Uses CLSID_MSH264EncoderMFT (the Microsoft H.264 Encoder MFT) in synchronous
//! mode. Input is NV12 — we convert from BGRA on the CPU before each frame, so
//! the GPU stays clean. M2 first version; M4 may swap to NVENC or to a GPU
//! BGRA→NV12 path if profiling shows the CPU conversion as the bottleneck.
//!
//! Important Windows quirks worth a comment:
//!  * H.264 encoder MFTs require `SetOutputType` BEFORE `SetInputType`.
//!  * Frame size & frame rate live in packed UINT64 attributes (hi32 = first
//!    component, lo32 = second).
//!  * The encoder inlines SPS/PPS at the start of every IDR frame; we surface
//!    that via `EncodedPacket::has_config = is_keyframe`.

use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::encoder::common::bgra_to_nv12;
use crate::video::{
    CapturedFrame, EncodedPacket, EncoderConfig, H264Profile, PixelFormat, VideoEncoder,
};

/// Microsoft H.264 Encoder MFT (synchronous, software).
/// CLSID `{6CA50344-051A-4DED-9779-A43305165E35}`.
const CLSID_MSH264_ENCODER_MFT: GUID = GUID::from_u128(0x6CA5_0344_051A_4DED_9779_A433_0516_5E35);

pub struct MediaFoundationH264Encoder {
    transform: IMFTransform,
    config: EncoderConfig,
    nv12: Vec<u8>,
    pending_keyframe: bool,
    output_provides_sample: bool,
    sample_alloc_size: u32,
}

// SAFETY: We initialize COM in the multi-threaded apartment (MTA) via
// `CoInitializeEx(COINIT_MULTITHREADED)`, in which case COM objects are
// free-threaded and may be moved between threads. The Media Foundation
// H.264 software MFT documents itself as MTA-safe.
unsafe impl Send for MediaFoundationH264Encoder {}

impl MediaFoundationH264Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        unsafe {
            // CoInit + MFStartup are idempotent for our purposes — both may
            // already be done by another module, in which case the second
            // call is a no-op (we ignore the "already initialized" return).
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_SDK_VERSION << 16 | MF_API_VERSION, MFSTARTUP_FULL)
                .context("MFStartup")?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264_ENCODER_MFT, None, CLSCTX_INPROC_SERVER)
                    .context("CoCreateInstance(CLSID_MSH264EncoderMFT)")?;

            // Tune encoder via ICodecAPI BEFORE setting media types. The
            // defaults (low-latency CBR with no lookahead) are tuned for
            // video calls and look terrible on motion-heavy desktop content.
            // Best-effort — failures are non-fatal so older Windows still works.
            if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(false));
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(80u32));
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                );
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncCommonMaxBitRate,
                    &VARIANT::from(config.bitrate_kbps.saturating_mul(1500)),
                );
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncMPVGOPSize,
                    &VARIANT::from(config.keyframe_interval_frames),
                );
            }

            // Output FIRST (H264 encoder requirement).
            let out_type = make_output_type(&config)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType")?;

            let in_type = make_input_type(&config)?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType")?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;

            let out_info = transform
                .GetOutputStreamInfo(0)
                .context("GetOutputStreamInfo")?;
            let output_provides_sample =
                (out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let sample_alloc_size = out_info.cbSize.max(1);

            let nv12_size = (config.width as usize) * (config.height as usize) * 3 / 2;

            Ok(Self {
                transform,
                config,
                nv12: vec![0u8; nv12_size],
                pending_keyframe: true, // first frame must be IDR
                output_provides_sample,
                sample_alloc_size,
            })
        }
    }

    fn drain_output(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        unsafe {
            loop {
                let mut buf = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(if self.output_provides_sample {
                        None
                    } else {
                        Some(create_sample_with_buffer(self.sample_alloc_size)?)
                    }),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                let mut status: u32 = 0;
                let r = self
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
                match r {
                    Ok(()) => {
                        let sample = std::mem::ManuallyDrop::take(&mut buf.pSample)
                            .context("ProcessOutput returned no sample")?;
                        let pkt = sample_to_packet(&sample)?;
                        // Drop pEvents if any.
                        let _ = std::mem::ManuallyDrop::take(&mut buf.pEvents);
                        out.push(pkt);
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        // Drop allocated-but-unused sample/events.
                        let _ = std::mem::ManuallyDrop::take(&mut buf.pSample);
                        let _ = std::mem::ManuallyDrop::take(&mut buf.pEvents);
                        break;
                    }
                    Err(e) => {
                        let _ = std::mem::ManuallyDrop::take(&mut buf.pSample);
                        let _ = std::mem::ManuallyDrop::take(&mut buf.pEvents);
                        return Err(anyhow!("ProcessOutput: {e}"));
                    }
                }
            }
        }
        Ok(())
    }
}

impl VideoEncoder for MediaFoundationH264Encoder {
    fn encode(&mut self, frame: &CapturedFrame, out: &mut Vec<EncodedPacket>) -> Result<()> {
        if frame.format != PixelFormat::Bgra8 {
            bail!("MF H264 encoder expects BGRA8 input (got {:?})", frame.format);
        }
        if frame.width != self.config.width || frame.height != self.config.height {
            bail!(
                "encoder dimensions {}x{} but frame is {}x{} — dynamic resize not implemented",
                self.config.width, self.config.height, frame.width, frame.height
            );
        }

        bgra_to_nv12(
            frame.pixels,
            frame.stride as usize,
            frame.width as usize,
            frame.height as usize,
            &mut self.nv12,
        );

        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)
                .context("MFCreateMemoryBuffer(input)")?;
            {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None).context("buffer.Lock")?;
                std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
                buffer.Unlock().context("buffer.Unlock")?;
                buffer
                    .SetCurrentLength(self.nv12.len() as u32)
                    .context("SetCurrentLength")?;
            }

            let sample = MFCreateSample().context("MFCreateSample(input)")?;
            sample.AddBuffer(&buffer).context("AddBuffer(input)")?;

            // Sample time/duration in 100-ns units (MF native unit).
            let pts_100ns = (frame.pts.as_nanos() / 100) as i64;
            sample.SetSampleTime(pts_100ns).context("SetSampleTime")?;
            let dur_100ns = 10_000_000i64 / (self.config.fps as i64);
            sample
                .SetSampleDuration(dur_100ns)
                .context("SetSampleDuration")?;

            if self.pending_keyframe {
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .context("SetUINT32(CleanPoint)")?;
                self.pending_keyframe = false;
            }

            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }

        self.drain_output(out)?;
        Ok(())
    }

    fn force_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .context("MFT_MESSAGE_COMMAND_DRAIN")?;
        }
        self.drain_output(out)
    }

    fn name(&self) -> &'static str {
        "media-foundation-h264"
    }
}

fn make_output_type(c: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType(output)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, c.bitrate_kbps.saturating_mul(1000))?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        set_packed_uint64(&t, &MF_MT_FRAME_SIZE, c.width, c.height)?;
        set_packed_uint64(&t, &MF_MT_FRAME_RATE, c.fps, 1)?;
        set_packed_uint64(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        let profile_val = match c.profile {
            H264Profile::Baseline => eAVEncH264VProfile_Base.0,
            H264Profile::Main => eAVEncH264VProfile_Main.0,
            H264Profile::High => eAVEncH264VProfile_High.0,
        };
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile_val as u32)?;
        Ok(t)
    }
}

fn make_input_type(c: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType(input)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        set_packed_uint64(&t, &MF_MT_FRAME_SIZE, c.width, c.height)?;
        set_packed_uint64(&t, &MF_MT_FRAME_RATE, c.fps, 1)?;
        set_packed_uint64(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        Ok(t)
    }
}

fn set_packed_uint64(t: &IMFMediaType, attr: &GUID, hi: u32, lo: u32) -> Result<()> {
    let packed = ((hi as u64) << 32) | (lo as u64);
    unsafe { t.SetUINT64(attr, packed)? };
    Ok(())
}

fn create_sample_with_buffer(size: u32) -> Result<IMFSample> {
    unsafe {
        let s = MFCreateSample().context("MFCreateSample(output)")?;
        let b = MFCreateMemoryBuffer(size).context("MFCreateMemoryBuffer(output)")?;
        s.AddBuffer(&b).context("AddBuffer(output)")?;
        Ok(s)
    }
}

fn sample_to_packet(sample: &IMFSample) -> Result<EncodedPacket> {
    unsafe {
        let buf = sample
            .ConvertToContiguousBuffer()
            .context("ConvertToContiguousBuffer")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        buf.Lock(&mut ptr, None, Some(&mut cur_len))
            .context("buffer.Lock(out)")?;
        let bytes = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        buf.Unlock().context("buffer.Unlock(out)")?;

        let pts_100ns = sample.GetSampleTime().unwrap_or(0);
        let pts = Duration::from_nanos((pts_100ns.max(0) as u64).saturating_mul(100));

        let is_keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;

        Ok(EncodedPacket {
            data: bytes,
            pts,
            is_keyframe,
            // MS H.264 encoder inlines SPS/PPS only on IDR frames; that matches
            // our `has_config` semantic.
            has_config: is_keyframe,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::encoder::common::bgra_to_nv12;

    #[test]
    fn nv12_size_matches_4_2_0() {
        let w = 4;
        let h = 4;
        let bgra = vec![128u8; w * h * 4];
        let mut out = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w * 4, w, h, &mut out);
        let y0 = out[0];
        let u0 = out[w * h];
        let v0 = out[w * h + 1];
        assert!((124..=126).contains(&y0), "Y ≈ 124, got {y0}");
        assert!((127..=129).contains(&u0), "U ≈ 128, got {u0}");
        assert!((127..=129).contains(&v0), "V ≈ 128, got {v0}");
    }
}
