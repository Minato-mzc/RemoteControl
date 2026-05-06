//! Frame capture sources.
//!
//! Today: DXGI Desktop Duplication (Windows). The trait surface is intentionally
//! not abstracted yet — adding macOS / X11 capture would justify a `FrameSource`
//! trait, but until that need is real we keep the concrete type to avoid
//! premature abstraction.

#[cfg(windows)]
pub mod dxgi;

#[cfg(windows)]
pub use dxgi::DxgiCapture;
