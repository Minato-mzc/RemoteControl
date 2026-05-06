//! Video pipeline traits and shared data types.
//!
//! M2 keeps the encoder swappable (NVENC today, AMF/QSV/x264 tomorrow) and the
//! capture source decoupled from it. The first concrete implementation lives
//! in `capture::dxgi` (Windows DXGI Desktop Duplication) + `encoder::nvenc`
//! (NVIDIA NVENC via the Video Codec SDK).
//!
//! We deliberately keep the trait surface small. M2's first version takes the
//! straightforward CPU-buffer path (DXGI → BGRA on CPU → NVENC). M4 will
//! revisit GPU zero-copy if the CPU copy bandwidth becomes the bottleneck.

use anyhow::Result;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit BGRA (Windows DXGI Desktop Duplication default).
    Bgra8,
    /// Planar YUV 4:2:0, 8-bit (NVENC native input).
    Nv12,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra8 => 4,
            Self::Nv12 => 1, // average; NV12 is 1.5 bytes/pixel total across planes
        }
    }
}

/// One frame coming out of the capture source.
///
/// `pixels` is a borrow into the capture device's buffer; encoders MUST finish
/// reading before the borrow ends. Stride may exceed `width * bpp` due to
/// hardware alignment requirements (DXGI commonly aligns to 64/128 bytes).
pub struct CapturedFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    /// Monotonic timestamp since capture session start.
    pub pts: Duration,
    pub pixels: &'a [u8],
}

/// One H.264 packet emitted by the encoder, ready to be wrapped into a binary
/// WebSocket frame via [`crate::protocol::build_video_frame`].
#[derive(Debug)]
pub struct EncodedPacket {
    /// H.264 NAL units in Annex-B format. IDR packets begin with SPS, PPS,
    /// then the IDR slice; P frames contain only the slice.
    pub data: Vec<u8>,
    pub pts: Duration,
    pub is_keyframe: bool,
    /// True when SPS/PPS are inlined ahead of the slice (always set on IDR).
    pub has_config: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
    pub profile: H264Profile,
}

#[derive(Debug, Clone, Copy)]
pub enum H264Profile {
    Baseline,
    Main,
    High,
}

impl H264Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Main => "main",
            Self::High => "high",
        }
    }
}

/// Pluggable H.264 encoder backend.
///
/// Implementations are constructed with an [`EncoderConfig`]; the constructor
/// signature is left to each backend so they can take their own context
/// (e.g. NVENC needs an `IDirect3DDevice` handle).
pub trait VideoEncoder: Send {
    /// Push one raw frame; `out` is appended to with any packets the encoder
    /// produced. May produce zero packets (initial buffering) or multiple
    /// (rare; e.g. when flushed implicitly).
    fn encode(&mut self, frame: &CapturedFrame, out: &mut Vec<EncodedPacket>) -> Result<()>;

    /// Hint that the next encoded frame should be an IDR. Called on stream
    /// start and on `keyframe_request` from the client.
    fn force_keyframe(&mut self);

    /// Best-effort flush at end-of-stream.
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()>;

    /// Identifier for logs ("nvenc-h264", "x264", …).
    fn name(&self) -> &'static str;
}
