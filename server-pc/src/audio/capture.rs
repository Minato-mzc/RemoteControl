//! WASAPI Loopback capture of the system default render endpoint.
//!
//! Captures whatever the user is hearing. Returns interleaved f32 samples
//! at the device's mix-format sample rate (48kHz on most modern hardware).
//!
//! Lifetime / threading:
//!  * COM is initialized on the calling thread (multithreaded apartment).
//!  * The capture loop is polling-based — `read_into` drains all packets
//!    that have arrived since last call.
//!  * `Drop` calls `Stop()` to release the audio endpoint promptly.

use anyhow::{bail, Context, Result};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

pub struct WasapiLoopback {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    sample_rate: u32,
    channels: u32,
}

// SAFETY: COM objects in the multi-threaded apartment are free-threaded.
unsafe impl Send for WasapiLoopback {}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("CoCreateInstance(MMDeviceEnumerator)")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("GetDefaultAudioEndpoint(eRender)")?;

            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("device.Activate(IAudioClient)")?;

            let mix_format_ptr = audio_client.GetMixFormat().context("GetMixFormat")?;
            if mix_format_ptr.is_null() {
                bail!("GetMixFormat returned null");
            }
            let sample_rate = (*mix_format_ptr).nSamplesPerSec;
            let channels = (*mix_format_ptr).nChannels as u32;

            // Buffer duration: 100ms in 100-ns units.
            let buffer_duration: i64 = 1_000_000;
            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    buffer_duration,
                    0,
                    mix_format_ptr,
                    None,
                )
                .context("audio_client.Initialize(loopback)")?;

            CoTaskMemFree(Some(mix_format_ptr as *const _));

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .context("audio_client.GetService(IAudioCaptureClient)")?;

            audio_client.Start().context("audio_client.Start")?;

            Ok(Self {
                audio_client,
                capture_client,
                sample_rate,
                channels,
            })
        }
    }

    pub fn sample_rate(&self) -> u32 { self.sample_rate }
    pub fn channels(&self) -> u32 { self.channels }

    /// Drain all available frames into `out` (interleaved f32). Returns
    /// number of per-channel sample frames appended.
    pub fn read_into(&self, out: &mut Vec<f32>) -> Result<usize> {
        unsafe {
            let mut total = 0;
            loop {
                let packet_size = self
                    .capture_client
                    .GetNextPacketSize()
                    .context("GetNextPacketSize")?;
                if packet_size == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture_client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let n = (frames as usize) * (self.channels as usize);
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    out.extend(std::iter::repeat(0.0f32).take(n));
                } else {
                    let slice = std::slice::from_raw_parts(data as *const f32, n);
                    out.extend_from_slice(slice);
                }
                self.capture_client
                    .ReleaseBuffer(frames)
                    .context("ReleaseBuffer")?;
                total += frames as usize;
            }
            Ok(total)
        }
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
        }
    }
}
