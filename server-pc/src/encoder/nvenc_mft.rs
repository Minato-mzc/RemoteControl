//! NVIDIA hardware H.264 encoder via Media Foundation async MFT.
//!
//! NVIDIA's driver registers a hardware H.264 encoder MFT (CLSID varies, but
//! it's discoverable through `MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE`). The
//! MFT internally drives NVENC, so this gives the same quality as linking the
//! NVIDIA Video Codec SDK directly, while reusing the Media Foundation
//! framework we already use for the software encoder.
//!
//! The hardware MFT is **asynchronous**: input and output don't block, but
//! they're gated by events delivered via `IMFMediaEventGenerator`:
//!
//!  * `METransformNeedInput`  — encoder wants a frame; we may now `ProcessInput`
//!  * `METransformHaveOutput` — encoded packet ready; we must `ProcessOutput`
//!
//! Each `encode(frame)` call:
//!  1. Queues the frame as an `IMFSample` in `pending_inputs`.
//!  2. Drains all currently-available events (non-blocking poll), pulling
//!     queued frames into the encoder when it asks and pushing finished
//!     packets out to the caller.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::time::Duration;
use windows::core::{Interface, VARIANT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::encoder::common::frame_to_nv12;
use crate::encoder::resize::BgraResizer;
use crate::video::{
    CapturedFrame, EncodedPacket, EncoderConfig, H264Profile, PixelFormat, VideoEncoder,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencCodec {
    H264,
    Hevc,
}

impl NvencCodec {
    fn subtype(self) -> windows::core::GUID {
        match self {
            Self::H264 => MFVideoFormat_H264,
            Self::Hevc => MFVideoFormat_HEVC,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::H264 => "nvenc-mft-h264",
            Self::Hevc => "nvenc-mft-hevc",
        }
    }
    fn wire_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }
}

pub struct NvencMftEncoder {
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    config: EncoderConfig,
    codec: NvencCodec,
    nv12: Vec<u8>,
    /// Lazily initialized when the captured frame size doesn't match `config`.
    resizer: Option<BgraResizer>,
    resized_bgra: Vec<u8>,
    pending_inputs: VecDeque<IMFSample>,
    pending_keyframe: bool,
    output_provides_sample: bool,
    sample_alloc_size: u32,
}

// SAFETY: COM objects in the multi-threaded apartment are free-threaded. The
// hardware H.264 MFT is documented as MTA-safe.
unsafe impl Send for NvencMftEncoder {}

impl NvencMftEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::new_with_codec(config, NvencCodec::H264)
    }

    pub fn new_hevc(config: EncoderConfig) -> Result<Self> {
        Self::new_with_codec(config, NvencCodec::Hevc)
    }

    fn new_with_codec(config: EncoderConfig, codec: NvencCodec) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_SDK_VERSION << 16 | MF_API_VERSION, MFSTARTUP_FULL)
                .context("MFStartup")?;

            let transform = enumerate_hardware_encoder(codec)?;

            // Mark this transform as ours so the framework allows direct use
            // of an async MFT outside an `IMFMediaSession` pipeline.
            let attrs = transform.GetAttributes().context("transform.GetAttributes")?;
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .context("SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK)")?;

            // Tune through ICodecAPI BEFORE setting media types.
            //
            // The previous run with PeakConstrainedVBR @ 30Mbps target still
            // looked blocky on motion — the encoder's own RC was deciding it
            // had "enough" bits and not spending them. Switch to **CBR** to
            // force the full bitrate budget out every second, then crank the
            // budget up further (50Mbps target). On a LAN with H.264 hardware
            // decode, 50Mbps is trivially carried.
            //
            //   * RateControlMode = CBR (constant 50Mbps)
            //   * QualityVsSpeed = 100 (slowest/best preset)
            //   * No B frames (desktop = strict IBPP)
            if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(false));
                // P4-ish preset. Slower presets crash fps; faster gains nothing
                // visible at this quality target.
                let _ = codec_api
                    .SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(70u32));
                // CBR with 30fps + 30Mbps was the only configuration that
                // empirically held a steady high bitrate. VBR let NVENC self-
                // select 5-10Mbps regardless of target; 60fps caused frame
                // pipeline collapse (≤5fps) regardless of rate-control mode.
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncCommonMeanBitRate,
                    &VARIANT::from(config.bitrate_kbps.saturating_mul(1000)),
                );
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncMPVGOPSize,
                    &VARIANT::from(config.keyframe_interval_frames),
                );
                // 2 B frames. At 30fps the pipeline is well within NVENC's
                // throughput budget, so B-frames give us their normal ~25%
                // compression bonus → sharper picture at the same 30Mbps cap.
                // (The 60fps tests where B>0 destabilized the pipeline are
                // a separate problem that direct-NVENC-SDK will address.)
                let _ = codec_api
                    .SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(2u32));
            }

            // Output FIRST, then input (NV12) — encoder requirement.
            let out_type = make_output_type(&config, codec)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType")?;

            let in_type = make_input_type(&config)?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType")?;

            let event_gen = transform
                .cast::<IMFMediaEventGenerator>()
                .context("cast IMFMediaEventGenerator")?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;

            let out_info = transform
                .GetOutputStreamInfo(0)
                .context("GetOutputStreamInfo")?;
            let output_provides_sample =
                (out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let sample_alloc_size = out_info.cbSize.max(1);

            let nv12_size = (config.width as usize) * (config.height as usize) * 3 / 2;

            tracing::info!(
                "NVENC hardware MFT [{}] initialized: {}x{}@{}fps target={}kbps gop={}",
                codec.wire_name(),
                config.width,
                config.height,
                config.fps,
                config.bitrate_kbps,
                config.keyframe_interval_frames,
            );

            Ok(Self {
                transform,
                event_gen,
                config,
                codec,
                nv12: vec![0u8; nv12_size],
                resizer: None,
                resized_bgra: Vec::new(),
                pending_inputs: VecDeque::with_capacity(4),
                pending_keyframe: true,
                output_provides_sample,
                sample_alloc_size,
            })
        }
    }

    pub fn codec(&self) -> NvencCodec {
        self.codec
    }

    /// Drain all currently-available transform events, processing them in
    /// order. Returns when `GetEvent` reports no more pending events.
    fn drain_events(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        unsafe {
            loop {
                let event_result = self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT);
                let event = match event_result {
                    Ok(e) => e,
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(e) => return Err(anyhow!("GetEvent: {e}")),
                };
                let kind = event.GetType().unwrap_or(MEUnknown.0 as u32);
                if kind == METransformNeedInput.0 as u32 {
                    if let Some(sample) = self.pending_inputs.pop_front() {
                        self.transform
                            .ProcessInput(0, &sample, 0)
                            .context("ProcessInput")?;
                    }
                } else if kind == METransformHaveOutput.0 as u32 {
                    self.process_one_output(out)?;
                }
                // Other event kinds (METransformDrainComplete, etc.) — ignore.
            }
        }
        Ok(())
    }

    fn process_one_output(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        unsafe {
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
                    let _ = std::mem::ManuallyDrop::take(&mut buf.pEvents);
                    out.push(pkt);
                    Ok(())
                }
                Err(e) => {
                    let _ = std::mem::ManuallyDrop::take(&mut buf.pSample);
                    let _ = std::mem::ManuallyDrop::take(&mut buf.pEvents);
                    Err(anyhow!("ProcessOutput: {e}"))
                }
            }
        }
    }
}

impl VideoEncoder for NvencMftEncoder {
    fn encode(&mut self, frame: &CapturedFrame, out: &mut Vec<EncodedPacket>) -> Result<()> {
        if frame.format != PixelFormat::Bgra8 {
            bail!("nvenc MFT expects BGRA8 input (got {:?})", frame.format);
        }

        frame_to_nv12(
            &mut self.resizer,
            &mut self.resized_bgra,
            frame,
            self.config.width,
            self.config.height,
            &mut self.nv12,
        )?;

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

            self.pending_inputs.push_back(sample);
        }

        self.drain_events(out)
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
        // Drain remaining events for up to ~200ms.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            self.drain_events(out)?;
            if self.pending_inputs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        self.codec.name()
    }
}

fn enumerate_hardware_encoder(codec: NvencCodec) -> Result<IMFTransform> {
    unsafe {
        let info_in = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let info_out = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: codec.subtype(),
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&info_in),
            Some(&info_out),
            &mut activates,
            &mut count,
        )
        .with_context(|| format!("MFTEnumEx(hardware {} encoder)", codec.wire_name()))?;

        if count == 0 {
            bail!("no hardware {} encoder MFT found", codec.wire_name());
        }

        let slice = std::slice::from_raw_parts(activates, count as usize);
        let first = slice[0]
            .clone()
            .context("first hardware MFT activate is null")?;
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

        let transform: IMFTransform = first.ActivateObject().context("ActivateObject")?;
        Ok(transform)
    }
}

fn make_output_type(c: &EncoderConfig, codec: NvencCodec) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType(output)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &codec.subtype())?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, c.bitrate_kbps.saturating_mul(1000))?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        set_packed_uint64(&t, &MF_MT_FRAME_SIZE, c.width, c.height)?;
        set_packed_uint64(&t, &MF_MT_FRAME_RATE, c.fps, 1)?;
        set_packed_uint64(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        let profile_val = match codec {
            NvencCodec::H264 => match c.profile {
                H264Profile::Baseline => eAVEncH264VProfile_Base.0,
                H264Profile::Main => eAVEncH264VProfile_Main.0,
                H264Profile::High => eAVEncH264VProfile_High.0,
            },
            // HEVC Main 8-bit profile.
            NvencCodec::Hevc => eAVEncH265VProfile_Main_420_8.0,
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

fn set_packed_uint64(
    t: &IMFMediaType,
    attr: &windows::core::GUID,
    hi: u32,
    lo: u32,
) -> Result<()> {
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
        let pts = std::time::Duration::from_nanos((pts_100ns.max(0) as u64).saturating_mul(100));

        let is_keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;

        Ok(EncodedPacket {
            data: bytes,
            pts,
            is_keyframe,
            has_config: is_keyframe,
        })
    }
}
