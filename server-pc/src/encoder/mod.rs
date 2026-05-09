//! H.264 video encoder backends.
//!
//! Pluggable through [`crate::video::VideoEncoder`]. Tried in this order:
//!  * `nvenc_sdk` — NVENC via the official Video Codec SDK (M4-B). Direct
//!    nvEncodeAPI64.dll calls with D3D11 input; gives us tunings the MFT
//!    layer hides (ULTRA_LOW_LATENCY, CBR_HQ, fine HRD buffer control) so
//!    60fps 1080p stays sharp where the MFT path collapsed. Preferred when
//!    available.
//!  * `nvenc_mft` — NVIDIA hardware H.264 via Microsoft's MFT wrapper.
//!    Same underlying engine as `nvenc_sdk` but with MS's restrictive rate
//!    control. Kept as a fallback in case the SDK DLL is missing or the
//!    driver is too old.
//!  * `mf_h264` — Microsoft H.264 software encoder MFT. Universal fallback.

#[cfg(windows)]
pub mod common;
#[cfg(windows)]
pub mod mf_h264;
#[cfg(windows)]
pub mod nvenc_mft;
#[cfg(windows)]
pub mod nvenc_sdk;
#[cfg(windows)]
pub mod nvenc_sys;
#[cfg(windows)]
pub mod resize;

#[cfg(windows)]
pub use mf_h264::MediaFoundationH264Encoder;
#[cfg(windows)]
pub use nvenc_mft::NvencMftEncoder;
#[cfg(windows)]
pub use nvenc_sdk::NvencSdkEncoder;
