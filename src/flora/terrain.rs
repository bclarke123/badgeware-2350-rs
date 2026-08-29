//! Terrain: a gently bumpy, subtly mottled ground patch.
//!
//! Replaces the flat single-quad ground. A 7x7 cell grid (~98 triangles)
//! carries small height jitter — flattened under the trunk so the tree sits
//! naturally — and faint per-triangle color mottling. The patch is fixed (one
//! landscape, generated once at boot from a constant seed) and sized so its
//! edges stay outside the frame at every orbit angle and zoom.

use embedded_graphics::pixelcolor::Rgb565;

use super::tree::XorShift;
use crate::render3d::math::{v3, Mat34, Vec3};
use crate::render3d::{tint, ListBuilder, MeshTri};

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

    /// Emits the patch; `dim` scales colors (night darkening).
    pub fn emit(&self, view: &Mat34, dim: f32, out: &mut ListBuilder<'_>) {
        // Transform the vertex grid once (64 points).
        let mut pv = [[Vec3::default(); VERTS]; VERTS];
        for (i, row) in pv.iter_mut().enumerate() {
            for (j, p) in row.iter_mut().enumerate() {
                *p = view.transform_point(v3(grid_x(i), GROUND_Y + self.heights[i][j], grid_x(j)));
            }
        }

        // Cells wound +y outward: (a, b, c) + (a, c, d), matching the old quad.
        for i in 0..CELLS {
            for j in 0..CELLS {
                let a = pv[i][j];
                let b = pv[i][j + 1];
                let c = pv[i + 1][j + 1];
                let d = pv[i + 1][j];
                let (s0, s1) = self.shades[i][j];
                // Every edge is shared with a neighbouring cell (the patch
                // rim stays off-screen), so none is anti-aliased: that keeps
                // the grid seamless instead of drawing a faint lattice.
                out.push_aa(MeshTri { v: [a, b, c], color: tint(s0, dim) }, 0);
                out.push_aa(MeshTri { v: [a, c, d], color: tint(s1, dim) }, 0);
            }
        }

    }
}

fn grid_x(i: usize) -> f32 {
    -EXTENT + (2.0 * EXTENT) * i as f32 / CELLS as f32
}

// Rust guideline compliant 2026-08-21
