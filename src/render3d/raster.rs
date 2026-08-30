//! Column-major rasterizer: anti-aliased flat triangles and
//! interpolator-driven textured sprites.
//!
//! The framebuffer stores pixels column-major (panel scan order, see
//! [`crate::gfx`]), so this rasterizer walks primitives column-by-column:
//! interpolate the upper and lower boundary y per column and fill the
//! contiguous y-run. That makes the hot inner loop a sequential fill, the
//! same shape as `FrameBuffer::fill_solid`.
//!
//! Numeric scheme: f32 for per-primitive setup (a handful of FPU divides),
//! then 16.16 fixed-point y-increments per column — a tiny integer loop with
//! no accumulating float error. Because setup is seeded from *absolute* x,
//! the two cores make bit-identical decisions: no seam at the split.
//!
//! # Edge anti-aliasing
//!
//! The fractional bits of each run's end positions are exact vertical
//! coverage of the run's first and last pixel, so instead of rounding they
//! become a 5-bit alpha and those two pixels are blended over whatever is
//! already in the framebuffer. Back-to-front painter's order makes that
//! composite correctly for free, and a sliver thinner than a pixel becomes a
//! faint pixel instead of flickering in and out. Coverage is only resolved
//! along y (runs), which smooths the shallow edges that shimmer most under
//! wind sway; steep edges keep their stairs.
//!
//! Only silhouette edges (per [`ScreenTri::aa`]) are blended. An edge shared
//! by two triangles must instead use one exact ownership rule — a pixel
//! belongs to the triangle containing its center — or both would partially
//! cover the seam pixel and the background would bleed through as a hairline.
//!
//! # Textured sprites
//!
//! A sprite is a parallelogram (a rotated rectangle) walked as one convex
//! quad: one setup, one run per column. Affine mapping is exact for
//! screen-parallel billboards, so u and v are linear in x and y and one
//! 16.16 step per pixel walks the map. That per-pixel "add, shift, mask,
//! combine into an address" is exactly what the RP2350's SIO interpolator
//! does in hardware: lane 0 accumulates u, lane 1 accumulates v, and a single
//! `POP_FULL` read yields the texel address (`base2 + masked u | masked v`)
//! while auto-advancing both lanes by the per-pixel gradients. Each core has
//! its own interpolators, so both halves run this with no contention. The
//! masks also wrap out-of-range coordinates back into the map, and every
//! map has a transparent border, so rounding slop at a quad edge is harmless.
//!
//! The edge (`EDGE` texel) blend is a fixed 50% average rather than the
//! general lerp used for silhouette AA: it is a large share of a small
//! sprite's covered texels, and the average is a handful of ALU ops.
//!
//! # Ground
//!
//! Terrain triangles are perspective-correct: `u/z`, `v/z` and `1/z` are
//! affine in screen space, so per column the run is walked in chunks of
//! eight pixels with one reciprocal per chunk, and the interpolator steps
//! u/v linearly inside the chunk (the Quake scheme). Texture coordinates are
//! world x/z, so the grass is nailed to the ground and does not swim as the
//! camera orbits; the mip is chosen per triangle from its depth so distant
//! ground is smooth rather than sparkly.
//!
//! # Placement
//!
//! Everything here is linked into RAM (`.data`): both cores execute it
//! concurrently and the XIP flash cache is shared, so flash-resident hot
//! loops stall on each other's misses. This exact code runs on both cores;
//! keep it free of embassy/time/log calls.

use embassy_rp::pac;

use super::texture::{content_rect, GroundMap, TexMap, Textures, EDGE, GROUND_LEVELS, TRANSPARENT};
use super::{GroundAttr, ScreenTri, TriList, FOCAL, GROUND_TEX};
use crate::bsp::screen::HEIGHT;

/// Framebuffer height in 16.16 fixed point.
const HEIGHT_FX: i32 = (HEIGHT as i32) << 16;

/// Bytes per framebuffer column.
const COL_BYTES: usize = HEIGHT * 2;

/// Where a raster pass spent its time (microseconds, from the free-running
/// `TIMER0`, which both cores can read without the time driver).
#[derive(Debug, Clone, Copy, Default)]
pub struct RasterStats {
    pub clear_us: u32,
    pub flat_us: u32,
    pub sprite_us: u32,
    pub ground_us: u32,
}

#[inline(always)]
fn now_us() -> u32 {
    pac::TIMER0.timerawl().read()
}

/// The four dithered sky-gradient columns (one per Bayer x-phase) for the
/// last `(top, bottom)` pair, kept per core: the quantized sky colours change
/// only every few frames, and rebuilding the columns costs ~0.7 ms.
pub struct ClearCache {
    key: u32,
    columns: [[u32; COL_BYTES / 4]; 4],
}

impl Default for ClearCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ClearCache {
    pub const fn new() -> Self {
        Self { key: u32::MAX, columns: [[0; COL_BYTES / 4]; 4] }
    }

    /// Returns the columns for the gradient, rebuilding them on a miss.
    fn columns(&mut self, clear_top: u16, clear_bottom: u16) -> &[[u32; COL_BYTES / 4]; 4] {
        let key = u32::from(clear_top) << 16 | u32::from(clear_bottom);
        if self.key != key {
            self.key = key;
            build_gradient(&mut self.columns, clear_top, clear_bottom);
        }
        &self.columns
    }
}

/// Builds the dithered gradient columns. Ordered (Bayer 4x4) dithering hides
/// the banding a 5/6-bit gradient would otherwise show over 240 rows. All
/// columns share the gradient but dithering must vary with x, so four column
/// variants are built (one per Bayer x-phase) and cycled by *absolute*
/// column so the pattern is seamless across the two cores' halves.
fn build_gradient(columns: &mut [[u32; COL_BYTES / 4]; 4], clear_top: u16, clear_bottom: u16) {
    const BAYER: [[u8; 4]; 4] = [
        [0, 128, 32, 160],
        [192, 64, 224, 96],
        [48, 176, 16, 144],
        [240, 112, 208, 80],
    ];
    let (tr, tg, tb) = unpack565(clear_top);
    let (br, bg, bb) = unpack565(clear_bottom);
    for (phase, column) in columns.iter_mut().enumerate() {
        for (pair, word) in column.iter_mut().enumerate() {
            let mut w = 0u32;
            for k in 0..2 {
                let y = pair * 2 + k;
                // 8.8 fixed-point lerp per channel; the fractional part
                // decides, against the Bayer threshold, whether to round
                // this pixel up.
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
                // Big-endian pixel bytes, little-endian word: swap.
                w |= u32::from(c.swap_bytes()) << (16 * k);
            }
            *word = w;
        }
    }
}

/// Clears a framebuffer half to a vertical gradient (`clear_top` at y=0 to
/// `clear_bottom` at the bottom) and draws two independently sorted lists
/// back-to-front by merge-walking them (each core builds and sorts its own
/// list during the parallel geometry phase; merging here costs one compare
/// per primitive instead of a copy + re-sort).
///
/// The gradient costs almost nothing in column-major storage: every column is
/// identical, so one 240-pixel column is computed (and cached in `clear`)
/// and block-copied per column.
///
/// `x0..x1` is the half's absolute column range; `half` must be exactly
/// `(x1 - x0) * HEIGHT * 2` bytes.
#[expect(clippy::too_many_arguments, reason = "one frame's raster inputs; the two cores call it identically")]
#[link_section = ".data.raster"]
#[inline(never)]
pub fn draw_lists(
    a: &TriList,
    b: &TriList,
    half: &mut [u8],
    x0: i32,
    x1: i32,
    clear_top: u16,
    clear_bottom: u16,
    textures: &Textures,
    clear: &mut ClearCache,
) -> RasterStats {
    debug_assert_eq!(half.len(), ((x1 - x0) as usize) * COL_BYTES);
    let mut stats = RasterStats::default();
    let t = now_us();

    let columns = clear.columns(clear_top, clear_bottom);
    let base = half.as_mut_ptr();
    for i in 0..(x1 - x0) as usize {
        let x = x0 as usize + i;
        let src = &columns[x & 3];
        // SAFETY: column i of the half is COL_BYTES bytes inside `half`.
        let dst = unsafe { base.add(i * COL_BYTES) }.cast::<u32>();
        for (k, &w) in src.iter().enumerate() {
            // SAFETY: k < COL_BYTES / 4.
            unsafe { dst.add(k).write_unaligned(w) };
        }
    }
    let t_clear = now_us();
    stats.clear_us = t_clear.wrapping_sub(t);

    // Merge-walk the two depth-sorted lists, farthest primitive first.
    let (mut i, mut j) = (0, 0);
    while i < a.len && j < b.len {
        if a.depth_at(i) >= b.depth_at(j) {
            draw_one(a, a.nth(i), half, x0, x1, textures, &mut stats);
            i += 1;
        } else {
            draw_one(b, b.nth(j), half, x0, x1, textures, &mut stats);
            j += 1;
        }
    }
    while i < a.len {
        draw_one(a, a.nth(i), half, x0, x1, textures, &mut stats);
        i += 1;
    }
    while j < b.len {
        draw_one(b, b.nth(j), half, x0, x1, textures, &mut stats);
        j += 1;
    }
    stats
}

/// Dispatches one primitive to its rasterizer, attributing its time.
#[inline(always)]
fn draw_one(
    list: &TriList,
    t: &ScreenTri,
    half: &mut [u8],
    x0: i32,
    x1: i32,
    textures: &Textures,
    stats: &mut RasterStats,
) {
    // Cheap reject of primitives entirely outside this part's columns.
    if i32::from(t.xr[1]) * 2 < x0 || i32::from(t.xr[0]) * 2 >= x1 {
        return;
    }
    let start = now_us();
    if t.tex == 0 {
        draw_tri(t, half, x0, x1);
        stats.flat_us = stats.flat_us.wrapping_add(now_us().wrapping_sub(start));
    } else if t.tex == GROUND_TEX {
        draw_ground(t, half, x0, x1, &list.ground[usize::from(t.aa)], &textures.ground);
        stats.ground_us = stats.ground_us.wrapping_add(now_us().wrapping_sub(start));
    } else {
        draw_sprite(t, half, x0, x1, &textures.maps[usize::from(t.tex - 1)]);
        stats.sprite_us = stats.sprite_us.wrapping_add(now_us().wrapping_sub(start));
    }
}

fn unpack565(c: u16) -> (i32, i32, i32) {
    (i32::from(c >> 11) & 31, i32::from(c >> 5) & 63, i32::from(c) & 31)
}

/// One boundary of a convex primitive: a chain of up to three x-monotonic
/// segments through `pts[0..n]`, stepped one column at a time in 16.16.
struct Chain {
    y: i32,
    d: i32,
    /// Index of the current segment's end point; the chain advances when a
    /// column reaches `next_x` (that point's x, rounded up).
    next: usize,
    next_x: i32,
    n: usize,
    pts: [(f32, f32); 4],
}

impl Chain {
    /// Seeds the chain at absolute column `x`.
    #[inline(always)]
    fn new(pts: [(f32, f32); 4], n: usize, x: i32) -> Self {
        let mut c = Self { y: 0, d: 0, next: 0, next_x: i32::MAX, n, pts };
        c.advance(x);
        c
    }

    /// Moves onto the first segment whose end lies beyond column `x` (or
    /// the last segment) and seeds y at `x`.
    #[inline(always)]
    fn advance(&mut self, x: i32) {
        let mut seg = self.next;
        while seg + 1 < self.n - 1 && x as f32 >= self.pts[seg + 1].0 {
            seg += 1;
        }
        let (ex, ey) = self.pts[seg];
        let (fx_, fy) = self.pts[seg + 1];
        self.next = seg + 1;
        // `x as f32 >= fx_` is exactly `x >= ceil(fx_)`; only the final
        // segment never switches.
        self.next_x = if self.next < self.n - 1 { ceil_i32(fx_) } else { i32::MAX };
        let dx = fx_ - ex;
        if dx < 0.5 {
            // Vertical (or empty) segment: hold at its far endpoint.
            self.y = fx16(fy);
            self.d = 0;
        } else {
            let slope = (fy - ey) / dx;
            // Seeding from absolute x (not the clipped range) is what makes
            // both cores' rounding identical at the split boundary.
            self.y = fx16(ey + (x as f32 - ex) * slope);
            self.d = fx16(slope);
        }
    }

    /// The boundary at column `x`, then steps to the next column.
    #[inline(always)]
    fn step(&mut self, x: i32) -> i32 {
        if x >= self.next_x {
            self.advance(x);
        }
        let y = self.y;
        self.y += self.d;
        y
    }
}

/// Column range `[xs, xe]` (inclusive) of a primitive spanning `ax..cx`
/// clipped to this half, or `None` if nothing to draw.
#[inline(always)]
fn column_range(ax: f32, cx: f32, x0: i32, x1: i32) -> Option<(i32, i32)> {
    if cx - ax < 0.5 || cx < x0 as f32 || ax >= x1 as f32 {
        return None;
    }
    let xs = ceil_i32(ax).max(x0);
    let xe = (ceil_i32(cx) - 1).min(x1 - 1);
    (xs <= xe).then_some((xs, xe))
}

/// Rasterizes one flat-shaded triangle into a framebuffer half with
/// anti-aliased silhouette run ends (see the module docs).
#[link_section = ".data.raster"]
#[inline(never)]
fn draw_tri(t: &ScreenTri, half: &mut [u8], x0: i32, x1: i32) {
    // Sort the vertices by x, keeping the input index for the edge mask.
    let mut order = [0usize, 1, 2];
    if t.x[order[0]] > t.x[order[1]] {
        order.swap(0, 1);
    }
    if t.x[order[1]] > t.x[order[2]] {
        order.swap(1, 2);
    }
    if t.x[order[0]] > t.x[order[1]] {
        order.swap(0, 1);
    }
    let pt = |i: usize| (t.x[order[i]], t.y[order[i]]);
    let (a, b, c) = (pt(0), pt(1), pt(2));
    let Some((xs, xe)) = column_range(a.0, c.0, x0, x1) else {
        return;
    };

    // Input edge mask bit k = edge between input vertices k and (k + 1) % 3.
    let edge_bit = |i: usize, j: usize| {
        let (i, j) = (order[i], order[j]);
        let k = if (i + 1) % 3 == j { i } else { j };
        t.aa >> k & 1 != 0
    };
    let aa_long = edge_bit(0, 2);
    let aa_ab = edge_bit(0, 1);
    let aa_bc = edge_bit(1, 2);

    // Long edge a->c spans every column; the other boundary is a->b then b->c.
    let mut long = Chain::new([a, c, c, c], 2, xs);
    let mut short = Chain::new([a, b, c, c], 3, xs);

    let sw = t.color.swap_bytes();
    let base = half.as_mut_ptr();
    for x in xs..=xe {
        let yl = long.step(x);
        let ys_ = short.step(x);
        // After stepping, `next == 2` means the chain is on b->c.
        let aa_short = if short.next == 2 { aa_bc } else { aa_ab };
        let (top, bot, aa_top, aa_bot) = if yl <= ys_ {
            (yl, ys_, aa_long, aa_short)
        } else {
            (ys_, yl, aa_short, aa_long)
        };
        let top = top.max(0);
        let bot = bot.min(HEIGHT_FX);
        if bot <= top {
            continue;
        }
        // SAFETY: x is within [x0, x1) so the column lies inside `half`; every
        // pixel index below is clamped to 0..HEIGHT.
        let col = unsafe { base.add((x - x0) as usize * COL_BYTES) };
        // Solid rows [first, last]; a blended pixel may sit on either side.
        // A shared (non-AA) end uses the pixel-center ownership rule; an AA
        // end covers its partial pixel by the fractional amount.
        let (first, top_alpha) = if aa_top {
            ((top >> 16) + 1, 32 - ((top & 0xffff) >> 11) as u32)
        } else {
            ((top + 0x8000) >> 16, 0)
        };
        let (last, bot_alpha) = if aa_bot {
            ((bot >> 16) - 1, ((bot & 0xffff) >> 11) as u32)
        } else {
            (((bot + 0x8000) >> 16) - 1, 0)
        };
        if first > last + 1 {
            // Sliver inside a single pixel (both ends AA): alpha is the
            // covered fraction.
            if aa_top && aa_bot {
                let alpha = ((bot - top) >> 11) as u32;
                // SAFETY: top >> 16 is in 0..HEIGHT.
                unsafe { blend_px(col.add((top >> 16) as usize * 2), t.color, alpha) };
            }
            continue;
        }
        if top_alpha > 0 {
            // SAFETY: first - 1 == top >> 16, in 0..HEIGHT.
            unsafe { blend_px(col.add((first - 1) as usize * 2), t.color, top_alpha) };
        }
        if first <= last {
            // SAFETY: first..=last lies within 0..HEIGHT.
            unsafe { fill_run(col.add(first as usize * 2), (last - first + 1) as usize, sw) };
        }
        if bot_alpha > 0 && last + 1 < HEIGHT as i32 {
            // SAFETY: last + 1 < HEIGHT checked.
            unsafe { blend_px(col.add((last + 1) as usize * 2), t.color, bot_alpha) };
        }
    }
}

/// Rasterizes one textured sprite (parallelogram; see [`ScreenTri`]):
/// nearest-sampled shade map walked by this core's `INTERP0`, transparent /
/// edge / shade texels resolved against a palette derived from the
/// sprite's base color.
#[link_section = ".data.raster"]
#[inline(never)]
fn draw_sprite(t: &ScreenTri, half: &mut [u8], x0: i32, x1: i32, map: &TexMap) {
    let tl = (t.x[0], t.y[0]);
    let tr = (t.x[1], t.y[1]);
    let br = (t.x[2], t.y[2]);
    let bl = (tl.0 + br.0 - tr.0, tl.1 + br.1 - tr.1);

    // Leftmost / rightmost corners; the other two split one per boundary.
    let pts = [tl, tr, br, bl];
    let (mut li, mut ri) = (0, 0);
    for (i, p) in pts.iter().enumerate() {
        if p.0 < pts[li].0 {
            li = i;
        }
        if p.0 > pts[ri].0 {
            ri = i;
        }
    }
    if li == ri {
        return;
    }
    let (l, r) = (pts[li], pts[ri]);
    let Some((xs, xe)) = column_range(l.0, r.0, x0, x1) else {
        return;
    };
    // Boundaries as chains L -> mid -> R. A corner above the L-R line (screen
    // y smaller) belongs to the upper chain, otherwise the lower one; a
    // degenerate case with both on one side becomes a 3-segment chain.
    let mut upper = [l, r, r, r];
    let mut lower = [l, r, r, r];
    let (mut nu, mut nl) = (1usize, 1usize);
    for (i, p) in pts.iter().enumerate() {
        if i == li || i == ri {
            continue;
        }
        let side = (r.0 - l.0) * (p.1 - l.1) - (r.1 - l.1) * (p.0 - l.0);
        if side < 0.0 {
            upper[nu] = *p;
            nu += 1;
        } else {
            lower[nl] = *p;
            nl += 1;
        }
    }
    // Chains with two mids must be x-ordered.
    if nu == 3 && upper[1].0 > upper[2].0 {
        upper.swap(1, 2);
    }
    if nl == 3 && lower[1].0 > lower[2].0 {
        lower.swap(1, 2);
    }
    upper[nu] = r;
    lower[nl] = r;
    let mut upper = Chain::new(upper, nu + 1, xs);
    let mut lower = Chain::new(lower, nl + 1, xs);

    // Texel coordinates at the corners: the content rect maps onto the quad.
    let side = f32::from(1u16 << map.log2);
    let [rx0, ry0, rx1, ry1] = content_rect(Textures::shape_of(t.tex - 1));
    let unit = |c: f32| (c + 1.0) * 0.5 * side;
    // Screen y grows downward while the rect's y grows upward.
    let (u_lo, u_hi, v_lo, v_hi) = (unit(rx0), unit(rx1), unit(-ry1), unit(-ry0));
    // Affine gradients from the plane through tl (u_lo, v_lo), tr (u_hi,
    // v_lo), br (u_hi, v_hi).
    let (ax, ay) = tl;
    let (bx, by) = tr;
    let (cx, cy) = br;
    let area = (bx - ax) * (cy - ay) - (cx - ax) * (by - ay);
    if area.abs() < 0.25 {
        return;
    }
    let inv = 1.0 / area;
    let du = u_hi - u_lo;
    let dv = v_hi - v_lo;
    // General plane gradients with (bu - au) = (cu - au) = du and
    // (bv - av) = 0, (cv - av) = dv substituted.
    let dudx = du * (cy - by) * inv;
    let dudy = du * (bx - cx) * inv;
    let dvdx = -dv * (by - ay) * inv;
    let dvdy = dv * (bx - ax) * inv;

    // Shade palette, byte-swapped for direct little-endian u16 stores into
    // the big-endian framebuffer: [-, -, dark, mid, light, ...]. Eight
    // entries so a `& 7` index needs no bounds check in the pixel loop.
    let mid = t.color;
    let pal: [u16; 8] = [
        0,
        0,
        scale565(mid, 22).swap_bytes(),
        mid.swap_bytes(),
        scale565(mid, 42).swap_bytes(),
        0,
        0,
        0,
    ];
    // Half-intensity mid tone for the 50% edge average.
    let mid_half = mid & 0xF7DE;

    // Configure this core's interpolator 0 as a texel address generator:
    // lane 0 extracts the u texel index, lane 1 the v index pre-shifted into
    // its row position, and POP_FULL adds them to the map base address.
    // ADD_RAW makes each pop advance the accumulators by the raw base
    // registers (the per-pixel gradients) rather than the masked results.
    let interp = pac::SIO.interp(0);
    let log2 = map.log2;
    interp.ctrl_lane0().write(|w| {
        w.set_shift(16);
        w.set_mask_lsb(0);
        w.set_mask_msb(log2 - 1);
        w.set_add_raw(true);
    });
    interp.ctrl_lane1().write(|w| {
        w.set_shift(16 - log2);
        w.set_mask_lsb(log2);
        w.set_mask_msb(2 * log2 - 1);
        w.set_add_raw(true);
    });
    interp.base0().write_value(fx16(dudy) as u32);
    interp.base1().write_value(fx16(dvdy) as u32);
    interp.base2().write_value(map.data.as_ptr() as u32);
    let pop_full = interp.pop_full();

    let base = half.as_mut_ptr();
    for x in xs..=xe {
        let yu = upper.step(x);
        let yl = lower.step(x);
        let (top, bot) = if yu <= yl { (yu, yl) } else { (yl, yu) };
        let top = top.max(0);
        let bot = bot.min(HEIGHT_FX);
        // Run of whole pixels whose centers lie inside [top, bot).
        let ys = (top + 0x8000) >> 16;
        let ye = ((bot - 0x8000) >> 16).min(HEIGHT as i32 - 1);
        if ys > ye {
            continue;
        }
        // Seed u/v at this column's first pixel center (absolute x, so both
        // cores sample identically at the split).
        let fx = x as f32 + 0.5 - ax;
        let fy = ys as f32 + 0.5 - ay;
        interp.accum0().write_value(fx16(u_lo + dudx * fx + dudy * fy) as u32);
        interp.accum1().write_value(fx16(v_lo + dvdx * fx + dvdy * fy) as u32);

        // SAFETY: x in [x0, x1) and ys..=ye in 0..HEIGHT keep every write
        // inside `half`.
        let mut p = unsafe { base.add((x - x0) as usize * COL_BYTES + ys as usize * 2) };
        let mut n = (ye - ys + 1) as u32;
        while n != 0 {
            let addr = pop_full.read();
            // SAFETY: the interpolator's lane masks bound the offset to
            // side*side - 1 texels above `map.data`, which is `MAX_TEXELS`
            // long for every map size.
            let texel = unsafe { *(addr as *const u8) };
            if texel > EDGE {
                // SAFETY: see above.
                unsafe { p.cast::<u16>().write_unaligned(pal[usize::from(texel & 7)]) };
            } else if texel != TRANSPARENT {
                // 50% edge: the RGB565 average trick (drop each channel's
                // low bit, add, halve) on the logical pixel value.
                // SAFETY: see above.
                unsafe {
                    let dst = p.cast::<u16>().read_unaligned().swap_bytes();
                    let avg = ((dst & 0xF7DE) + mid_half) >> 1;
                    p.cast::<u16>().write_unaligned(avg.swap_bytes());
                }
            }
            // SAFETY: stays within the column run.
            p = unsafe { p.add(2) };
            n -= 1;
        }
    }
}

/// Rasterizes one perspective-textured ground triangle (see the module
/// docs): grass tile sampled by this core's `INTERP0`, eight shade levels
/// resolved against a palette derived from the triangle's tint.
#[link_section = ".data.raster"]
#[inline(never)]
fn draw_ground(
    t: &ScreenTri,
    half: &mut [u8],
    x0: i32,
    x1: i32,
    attr: &GroundAttr,
    maps: &[GroundMap; GROUND_LEVELS],
) {
    // Sort the vertices by x, carrying the attribute index.
    let mut order = [0usize, 1, 2];
    if t.x[order[0]] > t.x[order[1]] {
        order.swap(0, 1);
    }
    if t.x[order[1]] > t.x[order[2]] {
        order.swap(1, 2);
    }
    if t.x[order[0]] > t.x[order[1]] {
        order.swap(0, 1);
    }
    let pt = |i: usize| (t.x[order[i]], t.y[order[i]]);
    let (a, b, c) = (pt(0), pt(1), pt(2));
    let Some((xs, xe)) = column_range(a.0, c.0, x0, x1) else {
        return;
    };
    let (ax, ay) = a;
    let (bx, by) = b;
    let (cx, cy) = c;
    let area = (bx - ax) * (cy - ay) - (cx - ax) * (by - ay);
    if area.abs() < 0.25 {
        return;
    }
    let inv = 1.0 / area;

    // Mip from the triangle's mean depth: texels per pixel ~ side * z / f.
    let iz_mean = (attr.iz[0] + attr.iz[1] + attr.iz[2]) * (1.0 / 3.0);
    let texels_per_px = 64.0 / (iz_mean * FOCAL);
    let level = if texels_per_px < 1.0 {
        0
    } else if texels_per_px < 2.0 {
        1
    } else if texels_per_px < 4.0 {
        2
    } else {
        3
    };
    let map = &maps[level];
    let side = f32::from(1u16 << map.log2);

    // Screen-space gradients of u/z (in texels of this level), v/z, 1/z.
    let grad = |v: [f32; 3], scale: f32| {
        let (va, vb, vc) = (v[order[0]] * scale, v[order[1]] * scale, v[order[2]] * scale);
        let dx = ((vb - va) * (cy - ay) - (vc - va) * (by - ay)) * inv;
        let dy = ((vc - va) * (bx - ax) - (vb - va) * (cx - ax)) * inv;
        (va, dx, dy)
    };
    let (uz_a, duz_dx, duz_dy) = grad(attr.uz, side);
    let (vz_a, dvz_dx, dvz_dy) = grad(attr.vz, side);
    let (iz_a, diz_dx, diz_dy) = grad(attr.iz, 1.0);

    // Eight shade levels around the triangle's tint, byte-swapped for
    // direct stores.
    const FACTORS: [u32; 8] = [23, 26, 29, 32, 35, 38, 41, 44];
    let mut pal = [0u16; 8];
    for (p, &f) in pal.iter_mut().zip(FACTORS.iter()) {
        *p = scale565(t.color, f).swap_bytes();
    }

    let mut long = Chain::new([a, c, c, c], 2, xs);
    let mut short = Chain::new([a, b, c, c], 3, xs);

    let interp = pac::SIO.interp(0);
    let log2 = map.log2;
    interp.ctrl_lane0().write(|w| {
        w.set_shift(16);
        w.set_mask_lsb(0);
        w.set_mask_msb(log2 - 1);
        w.set_add_raw(true);
    });
    interp.ctrl_lane1().write(|w| {
        w.set_shift(16 - log2);
        w.set_mask_lsb(log2);
        w.set_mask_msb(2 * log2 - 1);
        w.set_add_raw(true);
    });
    interp.base2().write_value(map.data.as_ptr() as u32);
    let pop_full = interp.pop_full();

    // Reciprocals of chunk lengths 0..=8.
    const INV: [f32; 9] = [0.0, 1.0, 0.5, 1.0 / 3.0, 0.25, 0.2, 1.0 / 6.0, 1.0 / 7.0, 0.125];

    let base = half.as_mut_ptr();
    for x in xs..=xe {
        let yl = long.step(x);
        let ys_ = short.step(x);
        let (top, bot) = if yl <= ys_ { (yl, ys_) } else { (ys_, yl) };
        let top = top.max(0);
        let bot = bot.min(HEIGHT_FX);
        // Pixel-center ownership: every ground edge is shared.
        let ys = (top + 0x8000) >> 16;
        let ye = (((bot + 0x8000) >> 16) - 1).min(HEIGHT as i32 - 1);
        if ys > ye {
            continue;
        }
        let fx = x as f32 + 0.5 - ax;
        let fy = ys as f32 + 0.5 - ay;
        let mut uz = uz_a + duz_dx * fx + duz_dy * fy;
        let mut vz = vz_a + dvz_dx * fx + dvz_dy * fy;
        let mut iz = iz_a + diz_dx * fx + diz_dy * fy;
        let z = 1.0 / iz;
        let mut u = uz * z;
        let mut v = vz * z;

        // SAFETY: x in [x0, x1) and ys..=ye in 0..HEIGHT keep every write
        // inside `half`.
        let mut p = unsafe { base.add((x - x0) as usize * COL_BYTES + ys as usize * 2) };
        let mut y = ys;
        while y <= ye {
            let n = (ye - y + 1).min(8);
            let nf = n as f32;
            // Exact coordinates at the start of the next chunk; linear
            // in between.
            uz += duz_dy * nf;
            vz += dvz_dy * nf;
            iz += diz_dy * nf;
            let z1 = 1.0 / iz;
            let u1 = uz * z1;
            let v1 = vz * z1;
            let inv_n = INV[n as usize];
            interp.accum0().write_value(fx16(u) as u32);
            interp.accum1().write_value(fx16(v) as u32);
            interp.base0().write_value(fx16((u1 - u) * inv_n) as u32);
            interp.base1().write_value(fx16((v1 - v) * inv_n) as u32);
            let mut k = n;
            while k != 0 {
                let addr = pop_full.read();
                // SAFETY: the lane masks bound the offset to side*side - 1
                // texels above `map.data`, which is 64*64 long for every
                // level.
                let texel = unsafe { *(addr as *const u8) };
                // SAFETY: within the column run (see above).
                unsafe { p.cast::<u16>().write_unaligned(pal[usize::from(texel & 7)]) };
                // SAFETY: stays within the column run.
                p = unsafe { p.add(2) };
                k -= 1;
            }
            u = u1;
            v = v1;
            y += n;
        }
    }
}

/// Fills `n` pixels from `p` with the byte-swapped color `sw`, two pixels
/// per store once aligned.
///
/// # Safety
/// `p..p + 2n` must lie inside the framebuffer half.
#[inline(always)]
unsafe fn fill_run(mut p: *mut u8, mut n: usize, sw: u16) {
    // SAFETY: caller guarantees the range.
    unsafe {
        if n > 0 && (p as usize) & 2 != 0 {
            p.cast::<u16>().write_unaligned(sw);
            p = p.add(2);
            n -= 1;
        }
        let pair = u32::from(sw) | u32::from(sw) << 16;
        while n >= 2 {
            p.cast::<u32>().write_unaligned(pair);
            p = p.add(4);
            n -= 2;
        }
        if n == 1 {
            p.cast::<u16>().write_unaligned(sw);
        }
    }
}

/// Blends `color` over the RGB565 pixel at `p` with `alpha` in 0..=32,
/// using the two-word channel-split trick: one multiply per operand.
///
/// # Safety
/// `p` must point at a pixel inside the framebuffer half.
#[inline(always)]
unsafe fn blend_px(p: *mut u8, color: u16, alpha: u32) {
    // SAFETY: caller guarantees `p` is a valid pixel.
    unsafe {
        if alpha >= 32 {
            p.cast::<u16>().write_unaligned(color.swap_bytes());
            return;
        }
        if alpha == 0 {
            return;
        }
        let dst = u32::from(p.cast::<u16>().read_unaligned().swap_bytes());
        let d = (dst | dst << 16) & 0x07E0_F81F;
        let s = (u32::from(color) | u32::from(color) << 16) & 0x07E0_F81F;
        let r = ((d * (32 - alpha) + s * alpha) >> 5) & 0x07E0_F81F;
        let out = (r | r >> 16) as u16;
        p.cast::<u16>().write_unaligned(out.swap_bytes());
    }
}

/// Scales every channel of an RGB565 color by `f / 32`, saturating.
#[inline(always)]
fn scale565(c: u16, f: u32) -> u16 {
    let r = ((u32::from(c >> 11) & 31) * f / 32).min(31);
    let g = ((u32::from(c >> 5) & 63) * f / 32).min(63);
    let b = ((u32::from(c) & 31) * f / 32).min(31);
    (r << 11 | g << 5 | b) as u16
}

#[inline(always)]
fn fx16(v: f32) -> i32 {
    (v * 65536.0) as i32
}

#[inline(always)]
fn ceil_i32(v: f32) -> i32 {
    let i = v as i32;
    if v > i as f32 { i + 1 } else { i }
}

// Rust guideline compliant 2026-08-29
