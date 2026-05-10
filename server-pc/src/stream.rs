//! Per-connection screen + audio stream pipeline.
//!
//! Two worker threads share a single mpsc channel of wire-ready binary frames
//! (already wrapped with the 12-byte header from `crate::protocol`):
//!   * **Video worker** owns DXGI capture + the H.264 encoder.
//!   * **Audio worker** owns WASAPI loopback + the Opus encoder. It's optional
//!     — if WASAPI init fails (no audio device, exclusive-mode collision, etc.)
//!     the stream still runs video-only.
//!
//! Both workers timestamp their frames against the same `Instant` so the
//! Android side can drive video render time off the audio clock for A/V sync.
//!
//! Lifecycle: dropping the `StreamHandle.packets` receiver closes the channel;
//! the next `blocking_send` from either worker fails and the worker exits.
//! `stop` is a backup for cases where the worker is parked in capture I/O.

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::capture::DxgiCapture;
use crate::encoder::{MediaFoundationH264Encoder, NvencMftEncoder, NvencSdkEncoder};
use crate::protocol::{build_audio_frame, build_video_frame, AudioMetadata};
use crate::video::{EncodedPacket, EncoderConfig, H264Profile, VideoEncoder};

/// Backend slot for the video worker. The SDK path drives both capture
/// and encode through one zero-copy GPU pipeline (so it owns DxgiCapture
/// itself); the trait-object path keeps capture and encode separate.
enum EncoderSlot {
    /// CPU-buffer path — `Box<dyn VideoEncoder>` consumes BGRA bytes from
    /// `DxgiCapture::next_frame()`. Used by NVENC-via-MFT and MS software.
    Cpu(Box<dyn VideoEncoder>, &'static str),
    /// GPU zero-copy path — NVENC SDK direct integration (M4-B). Reads
    /// the duplication frame straight into a registered D3D11 texture,
    /// no CPU memcpy on the hot path.
    GpuSdk(NvencSdkEncoder),
}

impl EncoderSlot {
    fn name(&self) -> &str {
        match self {
            Self::Cpu(e, _) => e.name(),
            Self::GpuSdk(e) => e.name(),
        }
    }

    fn codec_wire(&self) -> &'static str {
        match self {
            Self::Cpu(_, w) => w,
            Self::GpuSdk(_) => "h264",
        }
    }
}

#[cfg(windows)]
use crate::audio::{
    build_opus_id_header, AudioEncoder, OpusEncoder, WasapiLoopback, OPUS_PRE_SKIP_SAMPLES,
    OPUS_SEEK_PREROLL_NS,
};

#[derive(Debug, Clone, Copy)]
pub struct StreamRequestParams {
    pub codec: RequestedCodec,
    pub max_bitrate_kbps: Option<u32>,
    pub max_fps: Option<u32>,
    pub keyframe_interval_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedCodec {
    H264,
    Hevc,
}

pub struct StreamHandle {
    pub stream_id: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
    pub profile: H264Profile,
    pub codec_wire_name: &'static str,
    pub started_at_unix_ms: u64,
    pub audio_metadata: Option<AudioMetadata>,
    /// Wire-ready binary frames (header + payload). The WS task forwards these
    /// straight into `Message::Binary`.
    pub packets: mpsc::Receiver<Vec<u8>>,
    force_keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl StreamHandle {
    pub fn force_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start_stream(req: StreamRequestParams) -> Result<StreamHandle> {
    // Retry DXGI duplication setup a couple times. When a previous
    // stream's video loop has just exited (peer disconnect → handle drop
    // → loop notices stop flag → returns), the underlying COM
    // `IDXGIOutputDuplication` object can take ~100-300 ms to fully
    // release inside Windows; if the user mashes "重试" right after a
    // disconnect, the new `DxgiCapture::new()` lands inside that window
    // and surfaces as `DuplicateOutput` (Windows reports
    // `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` because only one duplication
    // per output is allowed). A short retry loop hides this from the
    // user without needing global serialization or a sleep on the disconnect
    // path.
    let cap = {
        let mut last_err: Option<anyhow::Error> = None;
        let mut out: Option<DxgiCapture> = None;
        for attempt in 0..5 {
            match DxgiCapture::new() {
                Ok(c) => {
                    out = Some(c);
                    break;
                }
                Err(e) => {
                    if attempt < 4 {
                        warn!(
                            "DxgiCapture::new failed on attempt {}: {e:#} — retrying in 200ms",
                            attempt + 1
                        );
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    last_err = Some(e);
                }
            }
        }
        out.ok_or_else(|| {
            last_err.unwrap_or_else(|| anyhow::anyhow!("DxgiCapture::new failed (no error)"))
        })?
    };
    let (cap_w, cap_h) = cap.dimensions();

    // Plan A: encode at native capture resolution (1080p). The 720p
    // downscale (Plan B) didn't deliver enough quality bump to offset
    // the perceived sharpness loss on a phone screen — and at 30fps the
    // 1660Ti has plenty of NVENC budget to encode 1080p with the
    // bitrate we have, so 720p just gives up resolution for nothing.
    let (width, height) = (cap_w, cap_h);

    let fps = req.max_fps.unwrap_or(60).clamp(15, 60);
    let bitrate_kbps = req.max_bitrate_kbps.unwrap_or(16_000).clamp(1_000, 50_000);
    // 1s I-frame interval (was 3s). Empirically NVENC on this rig produces
    // visible blocking/pixelation when motion suddenly appears: the encoder
    // takes ~1s to ramp bitrate from idle (0.8Mbps) to motion (25-30Mbps),
    // and any P-frame artifacts during ramp propagate until the next I-frame.
    // GOP=fps gives the decoder a clean refresh every second — pixelation
    // self-heals quickly instead of lingering for 3s. Bandwidth cost is
    // small (I-frame ~3x P-frame, so ~1.7x average bitrate at 30Mbps cap).
    let kf_interval_ms = req.keyframe_interval_ms.unwrap_or(1_000).clamp(200, 10_000);
    let keyframe_interval_frames = ((kf_interval_ms * fps) / 1000).max(1);

    info!(
        "stream: capture {}x{} → encode {}x{} (M4 720p downscale: {})",
        cap_w,
        cap_h,
        width,
        height,
        width != cap_w || height != cap_h
    );

    let cfg = EncoderConfig {
        width,
        height,
        fps,
        bitrate_kbps,
        keyframe_interval_frames,
        profile: H264Profile::High,
    };
    // Encoder fallback chain:
    //   1. H.264 NVENC SDK (zero-copy GPU, ULTRA_LOW_LATENCY tuning) — only if
    //      H.264 was requested. HEVC SDK path isn't wired yet.
    //   2. NVENC via MFT (HEVC or H.264 depending on request) — uses the
    //      same hardware engine but with MS's restrictive rate control.
    //   3. MS software H.264 — universal fallback.
    let encoder: EncoderSlot = match req.codec {
        RequestedCodec::H264 => match NvencSdkEncoder::new(cfg, &cap) {
            Ok(e) => {
                info!("video encoder: {} (NVENC SDK direct, GPU zero-copy)", e.name());
                EncoderSlot::GpuSdk(e)
            }
            Err(sdk_err) => {
                warn!("NVENC SDK direct unavailable ({sdk_err:#}); falling back to MFT");
                match NvencMftEncoder::new(cfg) {
                    Ok(e) => {
                        info!("video encoder: {}", e.name());
                        EncoderSlot::Cpu(Box::new(e), "h264")
                    }
                    Err(e) => {
                        warn!("NVENC H.264 MFT unavailable ({e:#}); using MS software");
                        EncoderSlot::Cpu(
                            Box::new(MediaFoundationH264Encoder::new(cfg)?),
                            "h264",
                        )
                    }
                }
            }
        },
        RequestedCodec::Hevc => match NvencMftEncoder::new_hevc(cfg) {
            Ok(e) => {
                info!("video encoder: {}", e.name());
                EncoderSlot::Cpu(Box::new(e), "hevc")
            }
            Err(e) => {
                warn!("NVENC HEVC unavailable ({e:#}); trying H.264 NVENC SDK");
                match NvencSdkEncoder::new(cfg, &cap) {
                    Ok(e2) => {
                        info!("video encoder: {} (HEVC requested, fell back to H.264 SDK)", e2.name());
                        EncoderSlot::GpuSdk(e2)
                    }
                    Err(_) => match NvencMftEncoder::new(cfg) {
                        Ok(e2) => {
                            info!("video encoder: {}", e2.name());
                            EncoderSlot::Cpu(Box::new(e2), "h264")
                        }
                        Err(e2) => {
                            warn!("NVENC H.264 unavailable ({e2:#}); using MS software");
                            EncoderSlot::Cpu(
                                Box::new(MediaFoundationH264Encoder::new(cfg)?),
                                "h264",
                            )
                        }
                    },
                }
            }
        },
    };
    let codec_wire = encoder.codec_wire();

    // Try to bring up audio. Failure here is non-fatal — fall back to video-only.
    #[cfg(windows)]
    let audio_pair = match WasapiLoopback::new() {
        Ok(w) => match OpusEncoder::new() {
            Ok(e) => {
                info!(
                    "audio capture ready: sr={} ch={} (encoder will resample if needed)",
                    w.sample_rate(),
                    w.channels()
                );
                Some((w, e))
            }
            Err(e) => {
                warn!("opus encoder init failed: {e:#} — stream will be video-only");
                None
            }
        },
        Err(e) => {
            warn!("WASAPI loopback init failed: {e:#} — stream will be video-only");
            None
        }
    };
    #[cfg(not(windows))]
    let audio_pair: Option<((), ())> = None;

    let audio_metadata = audio_pair.as_ref().map(|_| AudioMetadata {
        codec: "opus".to_string(),
        sample_rate: 48000,
        channels: 2,
        frame_size_ms: 20,
        bitrate_kbps: 64,
        csd_0_b64: STANDARD.encode(build_opus_id_header(2, OPUS_PRE_SKIP_SAMPLES as u16, 48000)),
        csd_1_b64: STANDARD.encode(
            (OPUS_PRE_SKIP_SAMPLES as i64 * 1_000_000_000 / 48000)
                .to_le_bytes(),
        ),
        csd_2_b64: STANDARD.encode(OPUS_SEEK_PREROLL_NS.to_le_bytes()),
    });

    let (tx, rx) = mpsc::channel::<Vec<u8>>(32);
    let force_kf = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let stream_id = uuid::Uuid::new_v4().to_string();
    let started_at_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // Shared monotonic clock origin for both video and audio PTS.
    let started_at = Instant::now();

    // Video worker.
    {
        let tx_v = tx.clone();
        let force_kf_v = force_kf.clone();
        let stop_v = stop.clone();
        let stream_id_v = stream_id.clone();
        let target_fps = fps;
        std::thread::Builder::new()
            .name(format!("rc-video-{}", &stream_id[..8]))
            .spawn(move || {
                run_video_loop(
                    cap,
                    encoder,
                    tx_v,
                    force_kf_v,
                    stop_v,
                    started_at,
                    &stream_id_v,
                    target_fps,
                );
            })?;
    }

    // Audio worker (optional).
    #[cfg(windows)]
    if let Some((wasapi, opus_enc)) = audio_pair {
        let tx_a = tx.clone();
        let stop_a = stop.clone();
        let stream_id_a = stream_id.clone();
        std::thread::Builder::new()
            .name(format!("rc-audio-{}", &stream_id[..8]))
            .spawn(move || {
                run_audio_loop(wasapi, opus_enc, tx_a, stop_a, started_at, &stream_id_a);
            })?;
    }

    drop(tx); // workers hold their own clones; this lets rx end when they all exit

    info!(
        "stream {stream_id} started: {width}x{height}@{fps}fps {bitrate_kbps}kbps audio={}",
        audio_metadata.is_some()
    );

    Ok(StreamHandle {
        stream_id,
        width,
        height,
        fps,
        bitrate_kbps,
        keyframe_interval_frames,
        profile: H264Profile::High,
        codec_wire_name: codec_wire,
        started_at_unix_ms,
        audio_metadata,
        packets: rx,
        force_keyframe: force_kf,
        stop,
    })
}

fn run_video_loop(
    mut cap: DxgiCapture,
    mut encoder: EncoderSlot,
    tx: mpsc::Sender<Vec<u8>>,
    force_kf: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    started_at: Instant,
    stream_id: &str,
    target_fps: u32,
) {
    let mut packets: Vec<EncodedPacket> = Vec::with_capacity(2);
    let mut idle_strikes = 0u32;

    // Frame pacing: encode at the configured fps. DXGI gives us frames as the
    // screen changes (often >60fps in dynamic content); feeding them all into
    // an encoder configured for e.g. 30fps wrecks its rate control because
    // its per-second bit budget is divided across way more frames than
    // expected. Skip frames whose arrival is faster than the target interval.
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    // Allow a small jitter margin so we don't drop a frame that's a hair early.
    let early_margin = Duration::from_micros(2_000);
    let mut next_due = Instant::now();

    // Diagnostic: average bitrate every ~3 seconds so we can confirm the
    // encoder is actually spending the bits we asked for.
    let mut frames_in_window = 0u32;
    let mut bytes_in_window = 0u64;
    let mut window_start = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Frame-pacing gate. Run-it-as-fast-as-possible doesn't help and
        // wrecks rate control; we either wait or drop.
        let now = Instant::now();
        let _stale_pace = if now + early_margin < next_due {
            // Sleep until next_due so we don't burn CPU polling capture.
            let wait = (next_due - now).as_millis() as u64;
            std::thread::sleep(Duration::from_millis(wait.min(20)));
            continue;
        } else {
            ()
        };

        if force_kf.swap(false, Ordering::Relaxed) {
            match &mut encoder {
                EncoderSlot::Cpu(e, _) => e.force_keyframe(),
                EncoderSlot::GpuSdk(e) => e.force_keyframe(),
            }
        }

        packets.clear();

        let captured = match &mut encoder {
            EncoderSlot::Cpu(e, _) => match cap.next_frame(100) {
                Ok(None) => {
                    idle_strikes = idle_strikes.saturating_add(1);
                    if idle_strikes >= 30 {
                        e.force_keyframe();
                        idle_strikes = 0;
                    }
                    false
                }
                Ok(Some(frame)) => {
                    idle_strikes = 0;
                    if let Err(err) = e.encode(&frame, &mut packets) {
                        warn!("stream {stream_id} CPU encode error: {err:#} — stopping");
                        break;
                    }
                    true
                }
                Err(err) => {
                    warn!("stream {stream_id} capture error: {err:#} — stopping");
                    break;
                }
            },
            EncoderSlot::GpuSdk(e) => match e.capture_and_encode(&mut cap, 100, &mut packets) {
                Ok(0) => {
                    idle_strikes = idle_strikes.saturating_add(1);
                    if idle_strikes >= 30 {
                        e.force_keyframe();
                        idle_strikes = 0;
                    }
                    false
                }
                Ok(_n) => {
                    idle_strikes = 0;
                    true
                }
                Err(err) => {
                    warn!("stream {stream_id} SDK encode error: {err:#} — stopping");
                    break;
                }
            },
        };

        if captured {
            // Successfully consumed one capture; advance pace clock.
            next_due = (next_due + frame_interval).max(Instant::now());
        } else {
            continue;
        }

        for pkt in packets.drain(..) {
            let pts_us = (started_at.elapsed().as_micros()).min(u64::MAX as u128) as u64;
            bytes_in_window += pkt.data.len() as u64;
            frames_in_window += 1;
            let bin = build_video_frame(&pkt.data, pts_us, pkt.is_keyframe, pkt.has_config);
            if tx.blocking_send(bin).is_err() {
                info!("stream {stream_id} consumer gone; video exits");
                return;
            }
        }

        if window_start.elapsed() >= Duration::from_secs(3) {
            let secs = window_start.elapsed().as_secs_f64();
            let mbps = (bytes_in_window * 8) as f64 / 1_000_000.0 / secs;
            info!(
                "video stats: {} frames / {:.1}s = {:.1} fps, {:.1} Mbps avg",
                frames_in_window,
                secs,
                frames_in_window as f64 / secs,
                mbps,
            );
            frames_in_window = 0;
            bytes_in_window = 0;
            window_start = Instant::now();
        }
    }
    info!("stream {stream_id} video loop exited");
}


#[cfg(windows)]
fn run_audio_loop(
    capture: WasapiLoopback,
    mut encoder: OpusEncoder,
    tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    started_at: Instant,
    stream_id: &str,
) {
    let frame_samples_per_ch = encoder.frame_samples_per_channel(); // 960
    let cap_channels = capture.channels();
    let cap_sample_rate = capture.sample_rate();

    if cap_channels != 2 || cap_sample_rate != 48000 {
        warn!(
            "stream {stream_id} audio: device format {}ch @ {}Hz; only 2ch/48kHz supported in M5 — audio disabled",
            cap_channels, cap_sample_rate
        );
        return;
    }

    let frame_total = frame_samples_per_ch * cap_channels as usize;
    let mut pending = Vec::<f32>::with_capacity(frame_total * 4);

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = capture.read_into(&mut pending) {
            warn!("stream {stream_id} audio capture error: {e:#}");
            break;
        }

        while pending.len() >= frame_total {
            let frame: Vec<f32> = pending.drain(..frame_total).collect();
            let opus_pkt = match encoder.encode(&frame) {
                Ok(p) => p,
                Err(e) => {
                    warn!("stream {stream_id} opus encode: {e:#}");
                    return;
                }
            };
            let pts_us = (started_at.elapsed().as_micros())
                .min(u64::MAX as u128) as u64;
            let bin = build_audio_frame(&opus_pkt, pts_us);
            if tx.blocking_send(bin).is_err() {
                info!("stream {stream_id} consumer gone; audio exits");
                return;
            }
        }

        // WASAPI shared-mode period is ~10ms; sleep a little less to stay responsive
        std::thread::sleep(Duration::from_millis(5));
    }
    info!("stream {stream_id} audio loop exited");
}
