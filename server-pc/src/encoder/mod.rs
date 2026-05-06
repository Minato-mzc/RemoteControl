//! H.264 video encoder backends.
//!
//! Pluggable through [`crate::video::VideoEncoder`]:
//!  * `nvenc_mft` — NVIDIA hardware H.264 encoder via the Media Foundation
//!    async hardware MFT. NVIDIA's driver registers a hardware MFT that wraps
//!    the same NVENC engine you'd hit by linking the Video Codec SDK directly,
//!    so quality matches "NVENC direct" without the SDK headers / FFI work.
//!  * `mf_h264` — Microsoft H.264 software encoder MFT. Universal fallback.

#[cfg(windows)]
pub mod common;
#[cfg(windows)]
pub mod mf_h264;
#[cfg(windows)]
pub mod nvenc_mft;

#[cfg(windows)]
pub use mf_h264::MediaFoundationH264Encoder;
#[cfg(windows)]
pub use nvenc_mft::NvencMftEncoder;
