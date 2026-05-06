//! End-to-end capture+encode smoke test.
//!
//! Captures the primary monitor for ~3 seconds via DXGI, encodes with the
//! Media Foundation H.264 software encoder, writes the raw Annex-B stream to
//! `test_capture.h264`, and prints a summary. Sanity-checks that every packet
//! starts with a valid Annex-B start code so encoder issues fail loudly here
//! instead of further down the pipeline.
//!
//! Run with:
//!   cargo run --release --example capture_encode_test

use anyhow::{bail, Result};
use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use remotecontrol_server::capture::DxgiCapture;
use remotecontrol_server::encoder::MediaFoundationH264Encoder;
use remotecontrol_server::video::{EncoderConfig, H264Profile, VideoEncoder};

fn main() -> Result<()> {
    let mut cap = DxgiCapture::new()?;
    let (w, h) = cap.dimensions();
    println!("[capture]  desktop = {w}x{h}");

    let cfg = EncoderConfig {
        width: w,
        height: h,
        fps: 30,
        bitrate_kbps: 6000,
        keyframe_interval_frames: 30,
        profile: H264Profile::High,
    };
    let mut enc = MediaFoundationH264Encoder::new(cfg)?;
    println!("[encoder]  {} @ {}x{}@{}fps {}kbps", enc.name(), w, h, cfg.fps, cfg.bitrate_kbps);

    let out_path = "test_capture.h264";
    let mut file = File::create(out_path)?;

    let test_dur = Duration::from_secs(3);
    let started = Instant::now();
    let mut packets: Vec<remotecontrol_server::video::EncodedPacket> = Vec::new();
    let mut frames_seen = 0u64;
    let mut idr_count = 0u64;
    let mut p_count = 0u64;
    let mut total_bytes = 0u64;

    while started.elapsed() < test_dur {
        match cap.next_frame(50)? {
            None => continue,
            Some(frame) => {
                frames_seen += 1;
                packets.clear();
                enc.encode(&frame, &mut packets)?;
                for pkt in &packets {
                    sanity_check_annex_b(&pkt.data)?;
                    if pkt.is_keyframe {
                        idr_count += 1;
                    } else {
                        p_count += 1;
                    }
                    total_bytes += pkt.data.len() as u64;
                    file.write_all(&pkt.data)?;
                }
            }
        }
    }

    // Drain anything still in the encoder.
    packets.clear();
    enc.flush(&mut packets)?;
    for pkt in &packets {
        sanity_check_annex_b(&pkt.data)?;
        if pkt.is_keyframe {
            idr_count += 1;
        } else {
            p_count += 1;
        }
        total_bytes += pkt.data.len() as u64;
        file.write_all(&pkt.data)?;
    }

    let elapsed = started.elapsed();
    println!("[done]     elapsed = {:.2}s", elapsed.as_secs_f32());
    println!("           frames captured = {frames_seen}");
    println!("           packets emitted = IDR {idr_count} + P {p_count}");
    println!(
        "           total bytes     = {total_bytes} ({:.1} KB/s avg)",
        total_bytes as f64 / elapsed.as_secs_f64() / 1024.0
    );
    println!("           output          = {out_path}  (raw Annex-B H.264)");
    println!();
    println!("Inspect with VLC, or:  ffplay -loglevel warning -f h264 {out_path}");

    if idr_count == 0 {
        bail!("no IDR frames emitted — encoder didn't produce a keyframe");
    }
    Ok(())
}

fn sanity_check_annex_b(data: &[u8]) -> Result<()> {
    if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) {
        return Ok(());
    }
    bail!(
        "packet does not start with H.264 Annex-B start code (first 6 bytes: {:02x?})",
        &data[..data.len().min(6)]
    );
}
