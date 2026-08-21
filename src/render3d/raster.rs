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

/// Clears a framebuffer half to `clear` and draws `list` back-to-front.
///
/// `x0..x1` is the half's absolute column range; `half` must be exactly
/// `(x1 - x0) * HEIGHT * 2` bytes.
pub fn draw_list(list: &TriList, half: &mut [u8], x0: i32, x1: i32, clear: u16) {
    debug_assert_eq!(half.len(), ((x1 - x0) as usize) * HEIGHT * 2);
    let hi = (clear >> 8) as u8;
    let lo = clear as u8;
    for px in half.chunks_exact_mut(2) {
        px[0] = hi;
        px[1] = lo;
    }
    for tri in &list.tris[..list.len] {
        draw_tri(tri, half, x0, x1);
    }
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
