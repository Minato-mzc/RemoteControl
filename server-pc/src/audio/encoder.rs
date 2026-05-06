//! Opus audio encoder.
//!
//! 48kHz stereo, 20ms frames, ~64kbps. Wraps the `opus` crate, which uses
//! a vendored libopus built via cmake (Windows MSVC works fine — the cmake
//! that ships with Visual Studio Build Tools is enough).

use anyhow::Result;

pub trait AudioEncoder: Send {
    /// Encode one frame of `frame_samples_per_channel * channels` interleaved
    /// f32 samples. Returns the Opus packet bytes.
    fn encode(&mut self, pcm_interleaved: &[f32]) -> Result<Vec<u8>>;
    fn name(&self) -> &'static str;
    fn frame_samples_per_channel(&self) -> usize;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u32;
}

pub struct OpusEncoder {
    inner: opus::Encoder,
    out_buf: Vec<u8>,
    frame_samples_per_ch: usize,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let mut enc = opus::Encoder::new(
            48000,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        enc.set_bitrate(opus::Bitrate::Bits(64_000))?;
        Ok(Self {
            inner: enc,
            // 4000 bytes is enough for any sane Opus packet (max ~1275 per
            // frame at 510kbps, plus headroom for repacketization).
            out_buf: vec![0u8; 4000],
            frame_samples_per_ch: 960, // 20ms @ 48kHz
        })
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm_interleaved: &[f32]) -> Result<Vec<u8>> {
        let n = self.inner.encode_float(pcm_interleaved, &mut self.out_buf)?;
        Ok(self.out_buf[..n].to_vec())
    }
    fn name(&self) -> &'static str { "opus" }
    fn frame_samples_per_channel(&self) -> usize { self.frame_samples_per_ch }
    fn sample_rate(&self) -> u32 { 48000 }
    fn channels(&self) -> u32 { 2 }
}

/// Default Opus encoder pre-skip in samples at 48kHz.
pub const OPUS_PRE_SKIP_SAMPLES: u32 = 312;
/// Standard Opus seek pre-roll in nanoseconds.
pub const OPUS_SEEK_PREROLL_NS: i64 = 80_000_000;

/// Build the 19-byte Opus ID Header (csd-0 for Android `MediaCodec`).
///
/// Layout: `"OpusHead"` (8) | version=1 | channels | pre_skip LE u16 |
/// input_sample_rate LE u32 | output_gain LE i16 (=0) | channel_mapping_family (=0).
pub fn build_opus_id_header(channels: u8, pre_skip: u16, sample_rate: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(19);
    buf.extend_from_slice(b"OpusHead");
    buf.push(1);
    buf.push(channels);
    buf.extend_from_slice(&pre_skip.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&0i16.to_le_bytes());
    buf.push(0);
    buf
}
