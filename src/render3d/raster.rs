//! Column-major flat-shaded triangle rasterizer.
//!
//! The framebuffer stores pixels column-major (panel scan order, see
//! [`crate::gfx`]), so this rasterizer walks triangles column-by-column: sort
//! the vertices by x, interpolate the two edge y-boundaries per column, and
//! fill the contiguous y-run. That makes the hot inner loop a sequential byte
//! fill, the same shape as `FrameBuffer::fill_solid`.
//!
//! Numeric scheme: f32 for per-triangle setup (at most three FPU divides),
//! then 16.16 fixed-point y-increments per column — a tiny integer loop with
//! no accumulating float error. A half-pixel bias keeps rounding identical for
//! triangles sharing an edge, and because setup is seeded from *absolute* x,
//! the two cores make bit-identical decisions: no seam at the split.
//!
//! This exact code runs on both cores; keep it free of embassy/time/log calls.

use super::{ScreenTri, TriList};
use crate::bsp::display::HEIGHT;

/// Rounding bias: half a pixel in 16.16 fixed point.
const HALF_PX: i32 = 0x8000;

/// Clears a framebuffer half to a vertical gradient (`clear_top` at y=0 to
/// `clear_bottom` at the bottom) and draws `list` back-to-front.
///
/// The gradient costs almost nothing in column-major storage: every column is
/// identical, so one 240-pixel column is computed and block-copied per column.
///
/// `x0..x1` is the half's absolute column range; `half` must be exactly
/// `(x1 - x0) * HEIGHT * 2` bytes.
pub fn draw_list(list: &TriList, half: &mut [u8], x0: i32, x1: i32, clear_top: u16, clear_bottom: u16) {
    debug_assert_eq!(half.len(), ((x1 - x0) as usize) * HEIGHT * 2);

    // Ordered (Bayer 4x4) dithering hides the banding a 5/6-bit gradient
    // would otherwise show over 240 rows. All columns share the gradient but
    // dithering must vary with x, so four column variants are built (one per
    // Bayer x-phase) and cycled by *absolute* column so the pattern is
    // seamless across the two cores' halves.
    const BAYER: [[u8; 4]; 4] = [
        [0, 128, 32, 160],
        [192, 64, 224, 96],
        [48, 176, 16, 144],
        [240, 112, 208, 80],
    ];
    let (tr, tg, tb) = unpack565(clear_top);
    let (br, bg, bb) = unpack565(clear_bottom);
    let mut columns = [[0u8; HEIGHT * 2]; 4];
    for (phase, column) in columns.iter_mut().enumerate() {
        for (y, px) in column.chunks_exact_mut(2).enumerate() {
            // 8.8 fixed-point lerp per channel; the fractional part decides,
            // against the Bayer threshold, whether to round this pixel up.
            let t = (y * 256 / (HEIGHT - 1)) as i32;
            let threshold = i32::from(BAYER[y & 3][phase]);
            let mix = |top: i32, bottom: i32, max: i32| {
                let fp = (top << 8) + (bottom - top) * t;
                ((fp >> 8) + i32::from(fp & 0xff > threshold)).clamp(0, max)
            };
            let r = mix(tr, br, 31);
            let g = mix(tg, bg, 63);
            let b = mix(tb, bb, 31);
            let c = ((r as u16) << 11) | ((g as u16) << 5) | b as u16;
            px[0] = (c >> 8) as u8;
            px[1] = c as u8;
        }
    }
    for (i, col) in half.chunks_exact_mut(HEIGHT * 2).enumerate() {
        let x = x0 as usize + i;
        col.copy_from_slice(&columns[x & 3]);
    }

    for tri in &list.tris[..list.len] {
        draw_tri(tri, half, x0, x1);
    }
}

fn unpack565(c: u16) -> (i32, i32, i32) {
    (i32::from(c >> 11) & 31, i32::from(c >> 5) & 63, i32::from(c) & 31)
}

/// Rasterizes one screen-space triangle into a framebuffer half.
fn draw_tri(t: &ScreenTri, half: &mut [u8], x0: i32, x1: i32) {
    // Sort the three vertices by x (a.x <= b.x <= c.x), as f32.
    let mut v = [
        (f32::from(t.x[0]), f32::from(t.y[0])),
        (f32::from(t.x[1]), f32::from(t.y[1])),
        (f32::from(t.x[2]), f32::from(t.y[2])),
    ];
    if v[0].0 > v[1].0 {
        v.swap(0, 1);
    }
    if v[1].0 > v[2].0 {
        v.swap(1, 2);
    }
    if v[0].0 > v[1].0 {
        v.swap(0, 1);
    }
    let (ax, ay) = v[0];
    let (bx, by) = v[1];
    let (cx, cy) = v[2];

    let width = cx - ax;
    if width < 0.5 {
        return; // degenerate vertical sliver
    }
    // Reject triangles entirely outside this half's columns.
    if cx < x0 as f32 || ax >= x1 as f32 {
        return;
    }

    // Long edge a->c spans every column; the other boundary is a->b then b->c.
    let slope_ac = (cy - ay) / width;

    // Column range (half-open on the right).
    let xs = ceil_i32(ax).max(x0);
    let xe = (ceil_i32(cx) - 1).min(x1 - 1);
    if xs > xe {
        return;
    }

    // Long-edge y at the first column, and per-column increment, in 16.16.
    let mut y_long = fx16(ay + (xs as f32 - ax) * slope_ac) + HALF_PX;
    let d_long = fx16(slope_ac);

    // Short edge state: start on a->b (unless it is vertical/empty).
    let mut on_second = bx - ax < 0.5 || xs as f32 >= bx;
    let (mut y_short, mut d_short) = short_edge(on_second, xs, ax, ay, bx, by, cx, cy);

    let hi = (t.color >> 8) as u8;
    let lo = t.color as u8;
    for x in xs..=xe {
        // Switch to edge b->c the first time a column passes b.
        if !on_second && x as f32 >= bx {
            on_second = true;
            let (ys, ds) = short_edge(true, x, ax, ay, bx, by, cx, cy);
            y_short = ys;
            d_short = ds;
        }

        let ya = y_long >> 16;
        let yb = y_short >> 16;
        let (mut ymin, mut ymax) = if ya <= yb { (ya, yb) } else { (yb, ya) };
        ymin = ymin.max(0);
        ymax = ymax.min(HEIGHT as i32 - 1);
        if ymin <= ymax {
            let base = (((x - x0) * HEIGHT as i32 + ymin) * 2) as usize;
            let run = &mut half[base..base + ((ymax - ymin + 1) * 2) as usize];
            for px in run.chunks_exact_mut(2) {
                px[0] = hi;
                px[1] = lo;
            }
        }

        y_long += d_long;
        y_short += d_short;
    }
}

/// Seeds the short-edge interpolator at absolute column `x`.
///
/// Seeding from absolute x (not the clipped range) is what makes both cores'
/// rounding identical at the split boundary.
#[expect(clippy::too_many_arguments, reason = "plain coordinate bundle, internal helper")]
fn short_edge(second: bool, x: i32, ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32) -> (i32, i32) {
    let (ex, ey, fx_, fy) = if second { (bx, by, cx, cy) } else { (ax, ay, bx, by) };
    let dx = fx_ - ex;
    if dx < 0.5 {
        // Vertical short edge: hold at its far endpoint, no increment.
        return (fx16(fy) + HALF_PX, 0);
    }
    let slope = (fy - ey) / dx;
    (fx16(ey + (x as f32 - ex) * slope) + HALF_PX, fx16(slope))
}

#[inline]
fn fx16(v: f32) -> i32 {
    (v * 65536.0) as i32
}

#[inline]
fn ceil_i32(v: f32) -> i32 {
    let i = v as i32;
    if v > i as f32 { i + 1 } else { i }
}

// Rust guideline compliant 2026-08-21
