//! Procedural billboard textures: tiny shade maps for leaves and flowers.
//!
//! A texture is not a color image but a *shade map*: each texel is one of
//! [`TRANSPARENT`], [`EDGE`], or a shade level 2..=4 (dark, mid, light). The
//! rasterizer turns levels into RGB565 per triangle from the triangle's own
//! base color, so every leaf keeps its individual tint for free and one map
//! serves every green and every pink. `EDGE` texels are half-covered by the
//! shape: the rasterizer blends them 50% with what is behind, which softens
//! the silhouette — a poor man's alpha edge in one extra blend per rim pixel.
//!
//! Maps are generated once at boot by supersampling analytic shapes (16
//! sub-samples per texel), in two sizes: 16x16 for leaves that project large
//! on screen and an 8x8 mip for the (very common) 4-8 px ones — nearest
//! sampling a 16-wide map for a 5 px sprite skips most texels and shimmers
//! as the leaf flutters; the mip keeps the sampling ratio near 1:1.
//!
//! Layout is row-major with a guaranteed transparent 1-texel border, so the
//! interpolator's wrap-around masking (see [`super::raster`]) can never turn
//! rounding slop at a quad edge into a visible texel.

use super::math::{fast_cos, fast_sin, inv_sqrt};

/// Texel classes.
pub const TRANSPARENT: u8 = 0;
pub const EDGE: u8 = 1;
const DARK: u8 = 2;
const MID: u8 = 3;
const LIGHT: u8 = 4;

/// Largest map side (16x16); the storage size of every map.
const MAX_SIDE: usize = 16;
const MAX_TEXELS: usize = MAX_SIDE * MAX_SIDE;

/// One shade map. `log2` is the side length exponent (4 = 16x16, 3 = 8x8).
#[derive(Clone, Copy)]
pub struct TexMap {
    pub log2: u8,
    pub data: [u8; MAX_TEXELS],
}

/// Texture ids as stored in a [`super::ScreenTri`] (`tex` field is id + 1;
/// 0 means flat-shaded).
pub const LEAF: u8 = 0;
pub const BLOSSOM: u8 = 1;
pub const TUFT: u8 = 2;
/// Number of distinct shapes; each has a full and a half-size map.
pub const SHAPES: usize = 3;

/// Bounding rect of each shape's content in unit-square coordinates
/// `[x_lo, y_lo, x_hi, y_hi]` (x right, y up). Sprites are emitted as quads
/// spanning only this rect, so the rasterizer never walks the fully
/// transparent margin a square quad would carry (a leaf fills ~35% of its
/// square). Keep a small margin so the `EDGE` rim is included.
pub const fn content_rect(shape: u8) -> [f32; 4] {
    match shape {
        LEAF => [-0.44, -0.95, 0.44, 0.95],
        TUFT => [-0.97, -0.62, 0.97, 0.78],
        _ => [-0.98, -0.98, 0.98, 0.98],
    }
}

/// Number of ground mip levels: 64, 32, 16, 8 texels per world unit.
pub const GROUND_LEVELS: usize = 4;
/// Side of the largest ground map.
pub const GROUND_SIDE: usize = 64;
/// Shade levels in a ground map (texel values `0..GROUND_SHADES`).
pub const GROUND_SHADES: usize = 8;

/// One ground tile mip: a `2^log2`-sided map of shade levels 0..=7, tiling
/// one world unit. No transparency — every texel is ground.
#[derive(Clone, Copy)]
pub struct GroundMap {
    pub log2: u8,
    pub data: [u8; GROUND_SIDE * GROUND_SIDE],
}

/// All maps: `maps[shape * 2]` is 16x16, `maps[shape * 2 + 1]` the 8x8 mip;
/// `ground[level]` is the grass tile at 64 >> level texels per unit.
pub struct Textures {
    pub maps: [TexMap; SHAPES * 2],
    pub ground: [GroundMap; GROUND_LEVELS],
}

impl Textures {
    pub const EMPTY: Textures = Textures {
        maps: [TexMap { log2: 4, data: [0; MAX_TEXELS] }; SHAPES * 2],
        ground: [GroundMap { log2: 6, data: [0; GROUND_SIDE * GROUND_SIDE] }; GROUND_LEVELS],
    };

    /// Generates every map. Call once at boot, before core 1 rasterizes.
    pub fn generate(&mut self) {
        let shapes: [fn(f32, f32) -> u8; SHAPES] = [leaf, blossom, tuft];
        for (shape, f) in shapes.iter().enumerate() {
            bake(&mut self.maps[shape * 2], 4, *f);
            bake(&mut self.maps[shape * 2 + 1], 3, *f);
        }
        bake_grass(&mut self.ground[0]);
        for level in 1..GROUND_LEVELS {
            let (lo, hi) = self.ground.split_at_mut(level);
            downsample(&lo[level - 1], &mut hi[0]);
        }
    }

    /// The map id for `shape` at the size best matching `px` (the sprite's
    /// projected full-square width in pixels).
    pub fn pick(shape: u8, px: f32) -> u8 {
        shape * 2 + u8::from(px < 11.0)
    }

    /// The shape a map id belongs to.
    pub const fn shape_of(map: u8) -> u8 {
        map / 2
    }
}

/// Supersamples `shape` over the unit square (x right, y up, both -1..=1)
/// into a `2^log2`-sided map. Coverage decides the texel class; a covered
/// texel takes the majority shade of its covered sub-samples. The 8x8 mip
/// uses lower coverage thresholds so thin features (a leaf's waist) survive
/// instead of collapsing to a stalk.
fn bake(map: &mut TexMap, log2: u8, shape: fn(f32, f32) -> u8) {
    const SS: usize = 4;
    const SAMPLES: u32 = (SS * SS) as u32;
    let side = 1usize << log2;
    let (solid_pct, edge_pct) = if log2 >= 4 { (55, 20) } else { (30, 10) };
    map.log2 = log2;
    map.data = [TRANSPARENT; MAX_TEXELS];
    let scale = 2.0 / side as f32;
    for row in 1..side - 1 {
        for col in 1..side - 1 {
            let mut counts = [0u32; 5];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = -1.0 + (col as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = 1.0 - (row as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    counts[usize::from(shape(x, y))] += 1;
                }
            }
            let covered = SAMPLES - counts[0];
            let shade = (DARK..=LIGHT)
                .max_by_key(|&k| counts[usize::from(k)])
                .unwrap_or(MID);
            map.data[row * side + col] = if covered * 100 >= SAMPLES * solid_pct {
                shade
            } else if covered * 100 >= SAMPLES * edge_pct {
                EDGE
            } else {
                TRANSPARENT
            };
        }
    }
}

/// A single leaf: a teardrop pointing up, dark left half, lit right half, a
/// bright vein down the middle.
fn leaf(x: f32, y: f32) -> u8 {
    leaf_at(x, y, 1.0)
}

/// Leaf shape scaled by `size` about the origin (shared with the tuft).
fn leaf_at(x: f32, y: f32, size: f32) -> u8 {
    let (x, y) = (x / size, y / size);
    const HALF_LEN: f32 = 0.92;
    if y.abs() >= HALF_LEN {
        return TRANSPARENT;
    }
    // Teardrop: widest a third of the way up, pointed tip, rounded base.
    let t = (y + HALF_LEN) / (2.0 * HALF_LEN); // 0 at base, 1 at tip
    let width = 0.40 * pow075(fast_sin(core::f32::consts::PI * t)) * (1.0 - 0.25 * t);
    if x.abs() >= width {
        return TRANSPARENT;
    }
    if x.abs() < 0.045 {
        LIGHT
    } else if x < 0.0 {
        DARK
    } else {
        MID
    }
}

/// A sakura blossom: five notched petals around a darker center.
fn blossom(x: f32, y: f32) -> u8 {
    let r2 = x * x + y * y;
    if r2 < 0.16 * 0.16 {
        return DARK; // stamens / deep-pink heart
    }
    for k in 0..5 {
        let a = core::f32::consts::TAU * k as f32 / 5.0 + core::f32::consts::FRAC_PI_2;
        let (dx, dy) = (fast_cos(a), fast_sin(a));
        // Petal frame: radial `r` out from the flower center, tangential `t`.
        let cx = 0.5 * dx;
        let cy = 0.5 * dy;
        let r = (x - cx) * dx + (y - cy) * dy;
        let t = -(x - cx) * dy + (y - cy) * dx;
        let inside = (r / 0.46) * (r / 0.46) + (t / 0.26) * (t / 0.26) <= 1.0;
        if !inside {
            continue;
        }
        // The characteristic notch at the petal tip.
        if r > 0.16 && t.abs() < (r - 0.16) * 0.55 {
            continue;
        }
        return if r > -0.05 { LIGHT } else { MID };
    }
    TRANSPARENT
}

/// A foliage tuft: three overlapping leaves fanned out, so one billboard
/// reads as a small cluster — denser crowns for the same triangle count.
fn tuft(x: f32, y: f32) -> u8 {
    const PARTS: [(f32, f32, f32); 3] = [
        (-0.46, -0.2, -0.95),
        (0.46, -0.2, 0.95),
        (0.0, 0.12, 0.0),
    ];
    let mut shade = TRANSPARENT;
    for &(ox, oy, angle) in &PARTS {
        let (c, s) = (fast_cos(angle), fast_sin(angle));
        let lx = (x - ox) * c + (y - oy) * s;
        let ly = -(x - ox) * s + (y - oy) * c;
        let hit = leaf_at(lx, ly, 0.7);
        if hit != TRANSPARENT {
            shade = hit; // later parts are drawn in front
        }
    }
    shade
}

/// The grass tile: three octaves of tiling value noise quantized to eight
/// shade levels (soft light/dark patches), then sparse two-texel vertical
/// "blade" flecks. Everything tiles at the map's period so the world-space
/// mapping never shows a seam.
fn bake_grass(map: &mut GroundMap) {
    const SIDE: usize = GROUND_SIDE;
    map.log2 = 6;
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    let mut field = [0f32; SIDE * SIDE];
    for (i, v) in field.iter_mut().enumerate() {
        let (x, y) = ((i % SIDE) as f32, (i / SIDE) as f32);
        // Octave lattices of 8, 16 and 32 cells across the tile.
        let n = value_noise(x, y, 8, 1) + 0.5 * value_noise(x, y, 16, 2) + 0.3 * value_noise(x, y, 32, 3);
        lo = lo.min(n);
        hi = hi.max(n);
        *v = n;
    }
    let span = (hi - lo).max(1e-3);
    for (i, &n) in field.iter().enumerate() {
        let level = ((n - lo) / span * (GROUND_SHADES as f32 - 0.01)) as u8;
        map.data[i] = level.min(GROUND_SHADES as u8 - 1);
    }
    // Blade flecks: ~6% of texels start a bright or dark 2-texel dash.
    for y in 0..SIDE {
        for x in 0..SIDE {
            let h = hash(x as u32, y as u32, 7);
            if h % 100 < 6 {
                let delta: i32 = if h & 0x100 != 0 { 2 } else { -2 };
                for dy in 0..2 {
                    let i = ((y + dy) % SIDE) * SIDE + x;
                    map.data[i] = (i32::from(map.data[i]) + delta).clamp(0, GROUND_SHADES as i32 - 1) as u8;
                }
            }
        }
    }
}

/// Halves a ground map by averaging 2x2 texel shade levels — the far mips
/// go smooth rather than sparkling.
fn downsample(src: &GroundMap, dst: &mut GroundMap) {
    let s_side = 1usize << src.log2;
    let d_side = s_side / 2;
    dst.log2 = src.log2 - 1;
    for y in 0..d_side {
        for x in 0..d_side {
            let at = |dx: usize, dy: usize| u32::from(src.data[(2 * y + dy) * s_side + 2 * x + dx]);
            let sum = at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1);
            dst.data[y * d_side + x] = ((sum + 2) / 4) as u8;
        }
    }
}

/// Tiling value noise: a `cells`-wide lattice of hashed values over a
/// [`GROUND_SIDE`] period, smoothstep-interpolated.
fn value_noise(x: f32, y: f32, cells: u32, seed: u32) -> f32 {
    let scale = cells as f32 / GROUND_SIDE as f32;
    let (fx, fy) = (x * scale, y * scale);
    let (x0, y0) = (fx as u32, fy as u32);
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let at = |i: u32, j: u32| (hash(i % cells, j % cells, seed) & 0xffff) as f32 / 65535.0;
    let a = at(x0, y0);
    let b = at(x0 + 1, y0);
    let c = at(x0, y0 + 1);
    let d = at(x0 + 1, y0 + 1);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sy
}

/// Small integer hash (Wang-style mix) for noise lattices.
fn hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x.wrapping_mul(0x27d4_eb2d) ^ y.wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    h
}

/// `x^0.75` for `x` in 0..=1 (`sqrt(x) * sqrt(sqrt(x))`), 0 below.
fn pow075(x: f32) -> f32 {
    if x <= 1e-6 {
        return 0.0;
    }
    let s = x * inv_sqrt(x);
    s * (s * inv_sqrt(s))
}

// Rust guideline compliant 2026-08-29
