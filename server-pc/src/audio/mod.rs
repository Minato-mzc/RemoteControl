//! Audio capture + encode pipeline (M5).

#[cfg(windows)]
pub mod capture;
pub mod encoder;

#[cfg(windows)]
pub use capture::WasapiLoopback;
pub use encoder::{
    build_opus_id_header, AudioEncoder, OpusEncoder, OPUS_PRE_SKIP_SAMPLES, OPUS_SEEK_PREROLL_NS,
};
