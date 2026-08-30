//! Terrain: a gently bumpy, subtly mottled ground patch.
//!
//! A 7x7 cell grid (~98 triangles) carries small height jitter — flattened
//! under the trunk so the tree sits naturally — and faint per-triangle color
//! mottling; each triangle is perspective-textured with the grass tile
//! (world x/z as texture coordinates, one tile per unit) and fogged toward
//! the horizon colour with distance. The patch is fixed (one landscape,
//! generated once at boot from a constant seed) and sized so its edges stay
//! outside the frame at every orbit angle and zoom.

use embedded_graphics::pixelcolor::Rgb565;

use super::anim::lerp_color;
use super::tree::XorShift;
use crate::render3d::math::{v3, Mat34, Vec3};
use crate::render3d::{tint, ListBuilder};

/// Grid cells per side (triangle count: CELLS^2 * 2).
const CELLS: usize = 7;
const VERTS: usize = CELLS + 1;

/// Half-extent of the ground patch in world units. Sized so the patch edge
/// (not just its corners) stays beyond the bottom of frame at maximum zoom-out
/// and every yaw — no sky visible under the ground.
const EXTENT: f32 = 4.2;

/// Ground level (matches the tree base).
const GROUND_Y: f32 = -0.95;

/// Base grass color (pre-baked sun intensity, like the old flat ground).
const GRASS: Rgb565 = Rgb565::new(4, 13, 4);

/// A generated ground patch, ready to emit each frame (lives in a static).
pub struct Terrain {
    heights: [[f32; VERTS]; VERTS],
    /// Per-cell tint pair: one per triangle, for a faceted mottle.
    shades: [[(Rgb565, Rgb565); CELLS]; CELLS],
}

impl Terrain {
    pub const EMPTY: Terrain = Terrain {
        heights: [[0.0; VERTS]; VERTS],
        shades: [[(GRASS, GRASS); CELLS]; CELLS],
    };

    /// Generates the (one, fixed) landscape. Call once at boot.
    pub fn generate(&mut self) {
        // Constant seed: the meadow is part of the set dressing, not the tree.
        let mut rng = XorShift(0x00C0_FFEE);

        for i in 0..VERTS {
            for j in 0..VERTS {
                let x = grid_x(i);
                let z = grid_x(j);
                // Flatten near the trunk; fade bumps in toward the rim.
                let r = (x * x + z * z) * 0.5;
                let lift = (r - 0.15).clamp(0.0, 1.0);
                self.heights[i][j] = rng.range(-0.04, 0.10) * lift;
            }
        }

        for i in 0..CELLS {
            for j in 0..CELLS {
                // Very subtle mottle: a few percent per cell, less within it.
                let a = tint(GRASS, rng.range(0.95, 1.08));
                let b = tint(a, rng.range(0.98, 1.03));
                self.shades[i][j] = (a, b);
            }
        }
    }

    /// Emits the patch; `dim` scales colors (night darkening) and `horizon`
    /// is the sky colour distant ground fogs toward.
    #[link_section = ".data.geom"]
    #[inline(never)]
    pub fn emit(&self, view: &Mat34, dim: f32, horizon: Rgb565, out: &mut ListBuilder<'_>) {
        // Transform the vertex grid once (64 points).
        let mut pv = [[Vec3::default(); VERTS]; VERTS];
        for (i, row) in pv.iter_mut().enumerate() {
            for (j, p) in row.iter_mut().enumerate() {
                *p = view.transform_point(v3(grid_x(i), GROUND_Y + self.heights[i][j], grid_x(j)));
            }
        }
        // World x/z double as grass texture coordinates (one tile per unit).
        let uv = |i: usize, j: usize| [grid_x(i), grid_x(j)];

        // Cells wound +y outward: (a, b, c) + (a, c, d), matching the old quad.
        for i in 0..CELLS {
            for j in 0..CELLS {
                let a = pv[i][j];
                let b = pv[i][j + 1];
                let c = pv[i + 1][j + 1];
                let d = pv[i + 1][j];
                let (s0, s1) = self.shades[i][j];
                // Distance fog toward the horizon colour, per triangle: the
                // far rows sink into the sky instead of ending in a hard rim.
                let fog = |z: f32| ((z - 3.0) * (1.0 / 8.0)).clamp(0.0, 0.8);
                let color = |s: Rgb565, z: f32| lerp_color(tint(s, dim), horizon, fog(z));
                let z0 = (a.z + b.z + c.z) * (1.0 / 3.0);
                let z1 = (a.z + c.z + d.z) * (1.0 / 3.0);
                out.push_ground([a, b, c], [uv(i, j), uv(i, j + 1), uv(i + 1, j + 1)], color(s0, z0));
                out.push_ground([a, c, d], [uv(i, j), uv(i + 1, j + 1), uv(i + 1, j)], color(s1, z1));
            }
        }
    }
}

fn grid_x(i: usize) -> f32 {
    -EXTENT + (2.0 * EXTENT) * i as f32 / CELLS as f32
}

// Rust guideline compliant 2026-08-30
