//! NVENC encoder using NVIDIA's Video Codec SDK directly (M4-B).
//!
//! ## Why this exists
//! Microsoft's NVENC MFT wrapper (`nvenc_mft.rs`) hides the rate-control
//! knobs that matter for a stable 60fps remote-desktop stream — chiefly
//! `NV_ENC_PARAMS_RC_CBR`'s HRD buffer size, `NV_ENC_TUNING_INFO_LOW_LATENCY`,
//! `NV_ENC_MULTI_PASS_QUARTER_RESOLUTION`, and the ability to declare async
//! output completion via Win32 event objects. Going to the SDK directly
//! lets us configure all of those, and to feed NVENC the DXGI-captured
//! BGRA texture without ever touching CPU memory.
//!
//! ## Pipeline
//! ```text
//! DXGI duplication → ID3D11Texture2D (BGRA, GPU)
//!     │
//!     ▼ CopyResource (GPU)
//! input_texture (registered with NVENC via NvEncRegisterResource)
//!     │
//!     ▼ NvEncMapInputResource → NvEncEncodePicture
//! NVENC engine
//!     │
//!     ▼ async event handle signalled
//! output_buffer (NvEncLockBitstream → memcpy out → NvEncUnlockBitstream)
//! ```
//!
//! ## Resource lifecycle
//! All NVENC handles are tied to `NvencSdkEncoder`'s lifetime. `Drop` walks
//! the unwind path in reverse construction order: unregister event,
//! destroy bitstream buffer, unregister input resource, destroy session,
//! release D3D11 textures, drop the DLL.

use anyhow::{anyhow, Context, Result};
use libloading::Library;
use std::ffi::c_void;
use std::ptr;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::capture::DxgiCapture;
use crate::encoder::nvenc_sys::*;
use crate::video::{EncodedPacket, EncoderConfig};

const NVENCAPI_DLL_NAME: &str = "nvEncodeAPI64.dll";
const NVENC_SUCCESS: NVENCSTATUS = _NVENCSTATUS::NV_ENC_SUCCESS;

/// Output queue depth. NVENC tolerates submitting many frames in flight,
/// but we only ever have one in-flight (sync-after-submit) so a single
/// bitstream buffer + event is enough for a remote-desktop stream.
const OUTPUT_BUFFERS: usize = 1;

/// NVENC SDK direct encoder. Owns:
///  * one runtime-loaded `nvEncodeAPI64.dll`
///  * one encoder session
///  * one D3D11 input texture (registered with NVENC, BGRA)
///  * one bitstream output buffer
///  * one Win32 event handle for async output signalling
pub struct NvencSdkEncoder {
    api: NvencApi,
    encoder: *mut c_void,

    config: EncoderConfig,

    /// Capture-source dimensions (e.g. 1920x1080). The input texture and
    /// registered NVENC resource live at this size; NVENC's hardware
    /// downscaler maps it to `config.width`×`config.height` (e.g. 720p)
    /// during encoding, so we don't pay any CPU for the resize.
    input_dims: (u32, u32),

    // D3D11 resources kept alive for as long as we use them.
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    input_texture: ID3D11Texture2D,

    registered_input: NV_ENC_REGISTERED_PTR,
    output_buffer: NV_ENC_OUTPUT_PTR,

    /// Win32 manual-reset event the encoder signals when output is ready.
    /// `NULL` if async mode wasn't supported on this rig.
    event_handle: HANDLE,
    async_mode: bool,

    /// Pending IDR request, serviced on the next encode call.
    pending_idr: bool,

    /// Frame index used as PTS source. NVENC's PTS field is 64-bit; we
    /// feed it microseconds since stream start to match the audio worker.
    started_at: std::time::Instant,
    frames_submitted: u64,

    /// Single-shot debug flag: dump the very first IDR packet bytes to
    /// the log so we can verify the wire format (Annex-B start codes,
    /// SPS+PPS prepended, etc.) when first bringing up the SDK path.
    first_kf_dumped: u8,
}

unsafe impl Send for NvencSdkEncoder {}

impl NvencSdkEncoder {
    /// Try to construct an SDK encoder. Returns `Err` (handled by
    /// stream.rs as a fallback signal) if:
    ///  * `nvEncodeAPI64.dll` isn't on the system path (no NVIDIA driver),
    ///  * the GPU's NVENC engine doesn't support H.264,
    ///  * NVENC initialization fails for any other reason.
    pub fn new(config: EncoderConfig, capture: &DxgiCapture) -> Result<Self> {
        let api = NvencApi::load().context("load NVENC SDK")?;

        let device = capture.device().clone();
        let context = capture.context().clone();
        // NVENC's hardware downscaler kicks in when input texture dims
        // exceed encode dims. We register the input at capture-native size
        // (so DxgiCapture's CopyResource is a 1:1 GPU blit, no scaling on
        // our side) and let NVENC do the resize as part of its existing
        // encode pass.
        let input_dims = capture.dimensions();

        // 1. Open the encode session against our D3D11 device.
        let mut session_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: _NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut encoder: *mut c_void = ptr::null_mut();
        api.check(
            unsafe { (api.fns.nvEncOpenEncodeSessionEx.unwrap())(&mut session_params, &mut encoder) },
            encoder,
            "OpenEncodeSessionEx",
        )?;
        debug_assert!(!encoder.is_null(), "OpenEncodeSessionEx returned null");

        // 2. Build the encode config from P4 + LOW_LATENCY. We tried P5
        //    (slower, higher quality) but the bitstream subtly stopped
        //    decoding on the phone — likely a preset-default config field
        //    we don't override interacts badly with our P-only / async
        //    setup. P4 + LOW_LATENCY is the proven-working combo; we get
        //    the quality boost we need from spatial AQ instead of the
        //    preset itself.
        let mut preset_cfg = NV_ENC_PRESET_CONFIG {
            version: NV_ENC_PRESET_CONFIG_VER,
            presetCfg: NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        api.check(
            unsafe {
                (api.fns.nvEncGetEncodePresetConfigEx.unwrap())(
                    encoder,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P4_GUID,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY,
                    &mut preset_cfg,
                )
            },
            encoder,
            "GetEncodePresetConfigEx",
        )?;
        let mut enc_cfg = preset_cfg.presetCfg;
        enc_cfg.version = NV_ENC_CONFIG_VER;
        enc_cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        enc_cfg.gopLength = config.keyframe_interval_frames;
        // P-only (no B frames). Async event signalling assumes one output
        // per submit; B-frames make NvEncEncodePicture return
        // NV_ENC_ERR_NEED_MORE_INPUT for the first few frames of every GOP
        // while it buffers the IBBPB... reorder window, and that path
        // doesn't signal the event. Strict IBPP is what every low-latency
        // streaming path (Parsec, Moonlight, etc.) uses for the same reason.
        enc_cfg.frameIntervalP = 1;
        enc_cfg.rcParams.version = NV_ENC_RC_PARAMS_VER;
        // CBR with the same parameters that produced a watchable (if
        // pixelated) stream the very first time SDK direct came up.
        // Earlier "improvements" (VBR / 1s VBV / TWO_PASS / AQ in various
        // combinations) drove playback into either heavier mosaic or full
        // black — most likely a phone-decoder interaction with one of
        // those preset-default fields we don't override. Re-introduce
        // tweaks one at a time from this baseline.
        enc_cfg.rcParams.rateControlMode = _NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        let avg_bps = config.bitrate_kbps.saturating_mul(1000);
        enc_cfg.rcParams.averageBitRate = avg_bps;
        // 0.5s VBV buffer matches the original working build.
        enc_cfg.rcParams.vbvBufferSize = avg_bps / 2;
        enc_cfg.rcParams.vbvInitialDelay = enc_cfg.rcParams.vbvBufferSize;
        // Two-pass quarter-resolution + spatial AQ. At 30fps the encoder
        // has 33ms per frame instead of 16ms, so the extra 1/4-res
        // analysis pass fits comfortably and lifts visual quality on
        // high-motion regions. AQ on top of that re-distributes the bit
        // budget toward perceptually-noticeable areas (text edges,
        // high-contrast borders), which keeps small text crisp through
        // scrolling — the place AQ pays off most for desktop streams.
        enc_cfg.rcParams.multiPass =
            _NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
        enc_cfg.rcParams.set_enableAQ(1);
        enc_cfg.rcParams.set_aqStrength(8);

        // Repeat SPS/PPS on every IDR. We previously set OUTPUT_SPSPPS
        // only on the first frame, which meant the implicit recovery IDRs
        // (every gopLength frames) had only the IDR slice — no parameter
        // sets. MediaCodec on the phone caches SPS/PPS from the first
        // IDR but loses that cache on certain transient errors (network
        // hiccup, decoder reset after long idle), and once the cache is
        // gone every subsequent IDR fails to decode and the screen stays
        // black. With repeatSPSPPS=1 NVENC prepends them to every IDR
        // automatically, so the decoder can recover from any IDR boundary.
        unsafe {
            enc_cfg
                .encodeCodecConfig
                .h264Config
                .set_repeatSPSPPS(1);
        }

        // 3. Initialize the encoder.
        let mut init = NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: NV_ENC_CODEC_H264_GUID,
            presetGUID: NV_ENC_PRESET_P4_GUID,
            encodeWidth: config.width,
            encodeHeight: config.height,
            darWidth: config.width,
            darHeight: config.height,
            frameRateNum: config.fps,
            frameRateDen: 1,
            enablePTD: 1,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY,
            bufferFormat: _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_UNDEFINED,
            ..Default::default()
        };
        init.encodeConfig = &mut enc_cfg;

        // 4. Async output via event signalling (vs. spinning on
        //    NvEncLockBitstream returning ENCODER_BUSY). NV_ENC_BUSY would
        //    be a CPU-burning poll; an event is a 0% CPU wait.
        let event_handle = unsafe { CreateEventW(None, true, false, None) }
            .context("CreateEventW for NVENC output")?;
        init.enableEncodeAsync = 1;

        api.check(
            unsafe { (api.fns.nvEncInitializeEncoder.unwrap())(encoder, &mut init) },
            encoder,
            "InitializeEncoder",
        )?;

        // Register the event so NVENC knows where to signal.
        let mut event_params = NV_ENC_EVENT_PARAMS {
            version: NV_ENC_EVENT_PARAMS_VER,
            completionEvent: event_handle.0 as *mut c_void,
            ..Default::default()
        };
        api.check(
            unsafe { (api.fns.nvEncRegisterAsyncEvent.unwrap())(encoder, &mut event_params) },
            encoder,
            "RegisterAsyncEvent",
        )?;

        // 5. Allocate the D3D11 input texture at CAPTURE-native size, not
        //    encode size. The capture path needs a 1:1-sized destination
        //    for `ID3D11DeviceContext::CopyResource`; NVENC handles the
        //    1080p→720p downscale itself on the same GPU pass.
        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: input_dims.0,
            Height: input_dims.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut input_texture: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&tex_desc, None, Some(&mut input_texture))
                .context("CreateTexture2D(NVENC input)")?;
        }
        let input_texture = input_texture.context("CreateTexture2D returned null")?;

        // 6. Register the texture with NVENC. width/height MUST be the
        //    actual texture dims (capture size). NVENC's downscaler reads
        //    these to know how much it has to shrink for the encode.
        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: _NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: input_dims.0,
            height: input_dims.1,
            pitch: 0,
            subResourceIndex: 0,
            resourceToRegister: input_texture.as_raw(),
            registeredResource: ptr::null_mut(),
            bufferFormat: _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: _NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        api.check(
            unsafe { (api.fns.nvEncRegisterResource.unwrap())(encoder, &mut reg) },
            encoder,
            "RegisterResource",
        )?;
        let registered_input = reg.registeredResource;

        // 7. Allocate a single bitstream output buffer.
        let mut bs = NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        api.check(
            unsafe { (api.fns.nvEncCreateBitstreamBuffer.unwrap())(encoder, &mut bs) },
            encoder,
            "CreateBitstreamBuffer",
        )?;

        Ok(Self {
            api,
            encoder,
            config,
            _device: device,
            context,
            input_texture,
            registered_input,
            output_buffer: bs.bitstreamBuffer,
            event_handle,
            async_mode: true,
            input_dims,
            pending_idr: true, // first frame must be IDR
            started_at: std::time::Instant::now(),
            frames_submitted: 0,
            first_kf_dumped: 0,
        })
    }

    pub fn name(&self) -> &'static str {
        "nvenc-sdk-h264"
    }

    pub fn force_keyframe(&mut self) {
        self.pending_idr = true;
    }

    /// Drive one capture-encode round-trip.
    /// Returns the number of [`EncodedPacket`]s pushed into `out`.
    /// `Ok(0)` on capture-side timeout (no screen change).
    pub fn capture_and_encode(
        &mut self,
        capture: &mut DxgiCapture,
        timeout_ms: u32,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<usize> {
        // 1. Pull the latest desktop frame straight into our registered
        //    input texture. Returns early on no-change timeout.
        let got = capture.next_frame_into(&self.input_texture, timeout_ms)?;
        if !got {
            return Ok(0);
        }

        // 2. Map the input — NVENC needs to flip ownership from app→engine.
        let mut map = NV_ENC_MAP_INPUT_RESOURCE {
            version: NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.registered_input,
            mappedResource: ptr::null_mut(),
            mappedBufferFmt: _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_UNDEFINED,
            ..Default::default()
        };
        self.api.check(
            unsafe { (self.api.fns.nvEncMapInputResource.unwrap())(self.encoder, &mut map) },
            self.encoder,
            "MapInputResource",
        )?;
        let mapped_input: NV_ENC_INPUT_PTR = map.mappedResource;

        // 3. Submit. picType=AUTOSELECT lets NVENC's PTD pick I/P/B; we
        //    only override on a forced IDR.
        let pts_us = self.started_at.elapsed().as_micros() as u64;
        let mut pic = NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            // inputWidth/inputHeight = source texture dims (NOT encode
            // dims). NVENC downscales internally if encode dims are
            // smaller than these — see the input_dims field doc.
            inputWidth: self.input_dims.0,
            inputHeight: self.input_dims.1,
            inputPitch: 0,
            encodePicFlags: if self.pending_idr {
                _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            },
            frameIdx: 0,
            inputTimeStamp: pts_us,
            inputDuration: 0,
            inputBuffer: mapped_input,
            outputBitstream: self.output_buffer,
            completionEvent: if self.async_mode {
                self.event_handle.0 as *mut c_void
            } else {
                ptr::null_mut()
            },
            bufferFmt: _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            pictureStruct: _NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            pictureType: _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_UNKNOWN,
            ..Default::default()
        };
        self.pending_idr = false;
        self.frames_submitted += 1;

        let submit = unsafe { (self.api.fns.nvEncEncodePicture.unwrap())(self.encoder, &mut pic) };

        // NEED_MORE_INPUT means the encoder swallowed our input but isn't
        // ready to emit a packet yet — common in the first frame or two of
        // an IBBP GOP, or after a config change. Per the SDK docs the event
        // is NOT signalled in this case, so we must NOT wait on it: just
        // unmap and tell the caller "no output this round". Should be rare
        // now that frameIntervalP=1 (P-only) is set; we keep the branch in
        // case NVENC ever decides to defer for its own reasons.
        if submit == _NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
            unsafe {
                let _ = (self.api.fns.nvEncUnmapInputResource.unwrap())(self.encoder, mapped_input);
            }
            return Ok(0);
        }
        if submit != NVENC_SUCCESS {
            // Real failure — release the map and bail.
            unsafe {
                let _ = (self.api.fns.nvEncUnmapInputResource.unwrap())(self.encoder, mapped_input);
            }
            return Err(self.api.last_error(self.encoder, "EncodePicture", submit));
        }

        // 4. If async mode is on, wait for the output-ready event before
        //    locking. Reset it before each wait — manual-reset events
        //    stay signaled until cleared.
        if self.async_mode {
            // 1s upper bound is generous; encoder normally signals in
            // sub-millisecond on Turing. If we ever block this long we'd
            // rather log + drop than starve the WS sender.
            const NVENC_ASYNC_TIMEOUT_MS: u32 = 1000;
            let wait =
                unsafe { WaitForSingleObject(self.event_handle, NVENC_ASYNC_TIMEOUT_MS) };
            if wait != WAIT_OBJECT_0 {
                unsafe {
                    let _ = (self.api.fns.nvEncUnmapInputResource.unwrap())(self.encoder, mapped_input);
                }
                if wait == WAIT_FAILED {
                    return Err(anyhow!("WaitForSingleObject failed"));
                }
                return Err(anyhow!("NVENC async output wait timed out (1s)"));
            }
            // ResetEvent so the next frame waits for a fresh signal.
            // CreateEventW(manualReset=true) so we have to clear it ourselves.
            unsafe {
                let _ = windows::Win32::System::Threading::ResetEvent(self.event_handle);
            }
        }

        // 5. Lock + drain the bitstream.
        let mut lock = NV_ENC_LOCK_BITSTREAM {
            version: NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.output_buffer,
            ..Default::default()
        };
        let lock_status =
            unsafe { (self.api.fns.nvEncLockBitstream.unwrap())(self.encoder, &mut lock) };

        let pushed = if lock_status == NVENC_SUCCESS {
            let len = lock.bitstreamSizeInBytes as usize;
            let data = unsafe {
                std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, len).to_vec()
            };
            let pic_type = lock.pictureType;
            let is_keyframe = pic_type == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
                || pic_type == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I;

            // Debug: dump the first 5 frames (IDR + first 4 P frames) to
            // verify each is valid Annex-B with start codes. If only the
            // IDR is intact and P frames are malformed, that explains why
            // the phone shows the IDR for a moment then black: subsequent
            // P frames don't decode and the renderer has nothing to draw.
            // Also dump every IDR boundary (~once a second at GOP=fps) so
            // we can check that recovery IDRs are well-formed too.
            if self.frames_submitted <= 5 || (is_keyframe && self.first_kf_dumped < 4) {
                let preview_len = len.min(48);
                let mut hex = String::with_capacity(preview_len * 3);
                for &b in &data[..preview_len] {
                    use std::fmt::Write as _;
                    let _ = write!(&mut hex, "{:02x} ", b);
                }
                tracing::info!(
                    "NVENC out [{} #{} pt={}]: size={} bytes={}",
                    if is_keyframe { "IDR" } else { "P  " },
                    self.frames_submitted,
                    pic_type,
                    len,
                    hex.trim_end()
                );
                if is_keyframe {
                    self.first_kf_dumped = self.first_kf_dumped.saturating_add(1);
                }
            }

            unsafe {
                let _ = (self.api.fns.nvEncUnlockBitstream.unwrap())(
                    self.encoder,
                    self.output_buffer,
                );
            }
            out.push(EncodedPacket {
                data,
                pts: Duration::from_micros(pts_us),
                is_keyframe,
                has_config: is_keyframe, // we OR'd FLAG_OUTPUT_SPSPPS on IDR
            });
            1
        } else {
            unsafe {
                let _ = (self.api.fns.nvEncUnmapInputResource.unwrap())(self.encoder, mapped_input);
            }
            return Err(self.api.last_error(self.encoder, "LockBitstream", lock_status));
        };

        // 6. Return the input map so NVENC can reuse the texture next round.
        unsafe {
            let _ = (self.api.fns.nvEncUnmapInputResource.unwrap())(self.encoder, mapped_input);
        }

        Ok(pushed)
    }

    pub fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        // Send an EOS so any buffered B-frames drain. NVENC documentation
        // recommends EOS pic with completionEvent set even in sync mode.
        if self.encoder.is_null() {
            return Ok(());
        }
        let mut pic = NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            encodePicFlags: _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS as u32,
            completionEvent: if self.async_mode {
                self.event_handle.0 as *mut c_void
            } else {
                ptr::null_mut()
            },
            ..Default::default()
        };
        let _ = unsafe { (self.api.fns.nvEncEncodePicture.unwrap())(self.encoder, &mut pic) };
        // Drain any buffered output one shot. Ignored if nothing remains.
        if self.async_mode {
            unsafe {
                let _ = WaitForSingleObject(self.event_handle, 200);
                let _ = windows::Win32::System::Threading::ResetEvent(self.event_handle);
            }
        }
        let mut lock = NV_ENC_LOCK_BITSTREAM {
            version: NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.output_buffer,
            ..Default::default()
        };
        let s = unsafe { (self.api.fns.nvEncLockBitstream.unwrap())(self.encoder, &mut lock) };
        if s == NVENC_SUCCESS && lock.bitstreamSizeInBytes > 0 {
            let len = lock.bitstreamSizeInBytes as usize;
            let data = unsafe {
                std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, len).to_vec()
            };
            unsafe {
                let _ =
                    (self.api.fns.nvEncUnlockBitstream.unwrap())(self.encoder, self.output_buffer);
            }
            out.push(EncodedPacket {
                data,
                pts: Duration::from_micros(self.started_at.elapsed().as_micros() as u64),
                is_keyframe: lock.pictureType == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
                has_config: false,
            });
        }
        Ok(())
    }
}

impl Drop for NvencSdkEncoder {
    fn drop(&mut self) {
        // Destroy in reverse construction order.
        unsafe {
            if !self.output_buffer.is_null() {
                let _ = (self.api.fns.nvEncDestroyBitstreamBuffer.unwrap())(
                    self.encoder,
                    self.output_buffer,
                );
            }
            if !self.registered_input.is_null() {
                let _ = (self.api.fns.nvEncUnregisterResource.unwrap())(
                    self.encoder,
                    self.registered_input,
                );
            }
            if self.async_mode && !self.event_handle.is_invalid() {
                let mut event_params = NV_ENC_EVENT_PARAMS {
                    version: NV_ENC_EVENT_PARAMS_VER,
                    completionEvent: self.event_handle.0 as *mut c_void,
                    ..Default::default()
                };
                let _ = (self.api.fns.nvEncUnregisterAsyncEvent.unwrap())(
                    self.encoder,
                    &mut event_params,
                );
            }
            if !self.encoder.is_null() {
                let _ = (self.api.fns.nvEncDestroyEncoder.unwrap())(self.encoder);
                self.encoder = ptr::null_mut();
            }
            if !self.event_handle.is_invalid() {
                let _ = CloseHandle(self.event_handle);
            }
        }
        // _device, context, input_texture drop their COM refs automatically.
        // self.context isn't used after init in this minimal pipeline (we
        // do CopyResource via DxgiCapture's context, which shares the same
        // device); keep the field to silence unused warnings without
        // changing the Rust drop order.
        let _ = &self.context;
    }
}

/// Wraps `nvEncodeAPI64.dll` + the function pointer table NVENC fills in.
struct NvencApi {
    _lib: Library,
    fns: NV_ENCODE_API_FUNCTION_LIST,
}

impl NvencApi {
    fn load() -> Result<Self> {
        unsafe {
            // Load the driver-installed DLL. Fails if there's no NVIDIA
            // driver — we let `NvencSdkEncoder::new()` translate that into
            // a graceful fallback to the MFT path.
            let lib = Library::new(NVENCAPI_DLL_NAME)
                .with_context(|| format!("LoadLibrary {NVENCAPI_DLL_NAME}"))?;

            // Resolve and call NvEncodeAPICreateInstance to get all the
            // other function pointers in one shot. Bindgen renders the
            // entry as `unsafe extern "C" fn`, so we cast through a
            // matching prototype.
            type CreateInstanceFn =
                unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
            let sym: libloading::Symbol<CreateInstanceFn> = lib
                .get(b"NvEncodeAPICreateInstance\0")
                .context("get NvEncodeAPICreateInstance symbol")?;

            let mut fns = NV_ENCODE_API_FUNCTION_LIST::default();
            fns.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            let status = (*sym)(&mut fns);
            if status != NVENC_SUCCESS {
                anyhow::bail!(
                    "NvEncodeAPICreateInstance failed: NVENCSTATUS={status} (NVENC API \
                     version mismatch — driver too old?)"
                );
            }
            // The function pointers in `fns` are owned by the DLL we just
            // loaded; they remain valid as long as `_lib` lives.
            drop(sym);
            Ok(Self { _lib: lib, fns })
        }
    }

    /// Fold an NVENCSTATUS into an `anyhow::Result` carrying the SDK's
    /// human-readable last-error string (which beats raw status codes).
    fn check(&self, status: NVENCSTATUS, encoder: *mut c_void, op: &'static str) -> Result<()> {
        if status == NVENC_SUCCESS {
            Ok(())
        } else {
            Err(self.last_error(encoder, op, status))
        }
    }

    fn last_error(
        &self,
        encoder: *mut c_void,
        op: &'static str,
        status: NVENCSTATUS,
    ) -> anyhow::Error {
        let mut detail = String::new();
        if !encoder.is_null() {
            if let Some(get_last) = self.fns.nvEncGetLastErrorString {
                let cstr = unsafe { get_last(encoder) };
                if !cstr.is_null() {
                    let s = unsafe { std::ffi::CStr::from_ptr(cstr) }
                        .to_string_lossy()
                        .into_owned();
                    if !s.is_empty() {
                        detail = format!(": {s}");
                    }
                }
            }
        }
        anyhow!("NVENC {op} returned {status}{detail}")
    }
}
