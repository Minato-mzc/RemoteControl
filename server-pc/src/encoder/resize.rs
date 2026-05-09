//! BGRA bilinear downscale, SIMD-accelerated via the `fast_image_resize` crate.
//!
//! Used by the M4 path that captures the desktop at native 1080p but encodes
//! at 720p to give NVENC more bits per pixel inside the same Mbps budget. The
//! resize step measured ~0.5–1.5 ms per 1080p→720p frame on the test machine.
//!
//! `fast_image_resize` requires its inputs to be packed (no row padding); DXGI
//! sometimes returns a stride larger than `width * 4`, so we pre-pack to a
//! reusable buffer before handing off to the resizer.

use anyhow::{Context, Result};
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

pub struct BgraResizer {
    resizer: Resizer,
    packed_src: Vec<u8>,
    src_w: u32,
    src_h: u32,
}

impl BgraResizer {
    pub fn new(src_w: u32, src_h: u32) -> Self {
        Self {
            resizer: Resizer::new(),
            packed_src: vec![0u8; (src_w as usize) * (src_h as usize) * 4],
            src_w,
            src_h,
        }
    }

    /// Resize `src` (BGRA, possibly padded — `src_stride` is bytes per row)
    /// down to `dst_w` × `dst_h` BGRA, packed, written into `dst`.
    pub fn resize_into(
        &mut self,
        src: &[u8],
        src_stride: usize,
        dst_w: u32,
        dst_h: u32,
        dst: &mut Vec<u8>,
    ) -> Result<()> {
        // Pack the source — fast_image_resize expects width*4 stride.
        let row_bytes = (self.src_w as usize) * 4;
        for y in 0..self.src_h as usize {
            let src_off = y * src_stride;
            let dst_off = y * row_bytes;
            self.packed_src[dst_off..dst_off + row_bytes]
                .copy_from_slice(&src[src_off..src_off + row_bytes]);
        }

        let src_image =
            Image::from_slice_u8(self.src_w, self.src_h, &mut self.packed_src, PixelType::U8x4)
                .context("Image::from_slice_u8(src)")?;

        let needed = (dst_w as usize) * (dst_h as usize) * 4;
        if dst.len() != needed {
            dst.resize(needed, 0);
        }
        let mut dst_image =
            Image::from_slice_u8(dst_w, dst_h, dst, PixelType::U8x4)
                .context("Image::from_slice_u8(dst)")?;

        let opts = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
        self.resizer
            .resize(&src_image, &mut dst_image, &opts)
            .context("resizer.resize")?;
        Ok(())
    }
}
