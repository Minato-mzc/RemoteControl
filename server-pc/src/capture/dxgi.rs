//! DXGI Desktop Duplication capture.
//!
//! Captures the primary monitor as BGRA8 frames into a CPU buffer (we copy via
//! a STAGING texture). The encoder consumes the borrow of `frame_buf` returned
//! inside `CapturedFrame<'_>`, so callers must finish each frame before the
//! next `next_frame()` call invalidates the borrow.
//!
//! Windows specifics worth knowing:
//!  * `D3D11CreateDevice` with an explicit adapter REQUIRES `D3D_DRIVER_TYPE_UNKNOWN`.
//!  * `AcquireNextFrame` returns the SAME texture for unchanged regions plus
//!    dirty rects; we don't try to be clever — every frame is a full copy.
//!  * After every successful `AcquireNextFrame` we MUST `ReleaseFrame` exactly
//!    once before the next acquire, or the duplication breaks.

use anyhow::{Context, Result};
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::video::{CapturedFrame, PixelFormat};

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    started_at: Instant,
    staging: Option<ID3D11Texture2D>,
    frame_buf: Vec<u8>,
    holding_frame: bool,
}

// SAFETY: D3D11 device/context and DXGI duplication created on this thread are
// safe to move once we're in the multi-threaded apartment. We never share them
// across threads — we only move ownership into the capture worker thread.
unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter = factory.EnumAdapters1(0).context("EnumAdapters1(0)")?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("D3D11Device returned null")?;
            let context = context.context("D3D11DeviceContext returned null")?;

            let output = adapter.EnumOutputs(0).context("EnumOutputs(0) — no monitor?")?;
            let output1: IDXGIOutput1 = output.cast().context("cast to IDXGIOutput1")?;
            let duplication = output1.DuplicateOutput(&device).context("DuplicateOutput")?;

            let desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            let width = desc.ModeDesc.Width;
            let height = desc.ModeDesc.Height;

            Ok(Self {
                device,
                context,
                duplication,
                width,
                height,
                started_at: Instant::now(),
                staging: None,
                frame_buf: Vec::with_capacity((width as usize) * (height as usize) * 4),
                holding_frame: false,
            })
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Block up to `timeout_ms` for the next frame. Returns `Ok(None)` on
    /// timeout (the screen content didn't change), `Ok(Some(frame))` on
    /// success, `Err` on hard failure.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame<'_>>> {
        unsafe {
            // Always release the previously-acquired frame first.
            if self.holding_frame {
                let _ = self.duplication.ReleaseFrame();
                self.holding_frame = false;
            }

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let acquire = self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource);
            match acquire {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(anyhow::anyhow!("AcquireNextFrame: {e}")),
            }
            self.holding_frame = true;
            let resource = resource.context("AcquireNextFrame returned null IDXGIResource")?;
            let tex: ID3D11Texture2D = resource.cast().context("cast to ID3D11Texture2D")?;

            let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut tex_desc);

            self.ensure_staging(&tex_desc)?;
            let staging = self.staging.as_ref().expect("ensure_staging set it");
            self.context.CopyResource(staging, &tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map staging texture")?;

            let stride = mapped.RowPitch;
            let total = (stride as usize) * (self.height as usize);
            self.frame_buf.clear();
            self.frame_buf
                .extend_from_slice(std::slice::from_raw_parts(mapped.pData as *const u8, total));

            self.context.Unmap(staging, 0);

            Ok(Some(CapturedFrame {
                width: self.width,
                height: self.height,
                stride,
                format: PixelFormat::Bgra8,
                pts: self.started_at.elapsed(),
                pixels: &self.frame_buf,
            }))
        }
    }

    fn ensure_staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<()> {
        if self.staging.is_some() {
            return Ok(());
        }
        unsafe {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut s: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut s))
                .context("CreateTexture2D(staging)")?;
            self.staging = s;
        }
        Ok(())
    }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        if self.holding_frame {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}
