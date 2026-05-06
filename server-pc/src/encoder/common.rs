//! Helpers shared across H.264 encoder backends.

/// CPU BGRA → NV12 (BT.601 limited range). Not SIMD; ~5–8% of one core at
/// 1080p60 on a modern desktop. `out` MUST be sized `w * h * 3 / 2`.
pub fn bgra_to_nv12(bgra: &[u8], stride: usize, w: usize, h: usize, out: &mut [u8]) {
    debug_assert!(out.len() >= w * h * 3 / 2);

    let (y_plane, uv_plane) = out.split_at_mut(w * h);

    for y in 0..h {
        let row = &bgra[y * stride..y * stride + w * 4];
        let dst = &mut y_plane[y * w..y * w + w];
        for x in 0..w {
            let b = row[x * 4] as i32;
            let g = row[x * 4 + 1] as i32;
            let r = row[x * 4 + 2] as i32;
            let yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            dst[x] = yv.clamp(0, 255) as u8;
        }
    }

    for y in (0..h).step_by(2) {
        let r0 = &bgra[y * stride..y * stride + w * 4];
        let r1 = &bgra[(y + 1) * stride..(y + 1) * stride + w * 4];
        let uv_row = &mut uv_plane[(y / 2) * w..(y / 2) * w + w];
        for x in (0..w).step_by(2) {
            let mut sum_b = 0i32;
            let mut sum_g = 0i32;
            let mut sum_r = 0i32;
            for (px, py) in [(x, 0), (x + 1, 0), (x, 1), (x + 1, 1)] {
                let row = if py == 0 { r0 } else { r1 };
                sum_b += row[px * 4] as i32;
                sum_g += row[px * 4 + 1] as i32;
                sum_r += row[px * 4 + 2] as i32;
            }
            let b = sum_b / 4;
            let g = sum_g / 4;
            let r = sum_r / 4;
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            uv_row[x] = u.clamp(0, 255) as u8;
            uv_row[x + 1] = v.clamp(0, 255) as u8;
        }
    }
}
