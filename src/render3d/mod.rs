//! Software 3D pipeline: dual-core tiled flat-shaded triangle rendering.
//!
//! Frame flow (strict fork-join, decided in planning):
//! 1. Core 0 transforms model triangles to view space, near-clips, projects,
//!    lights, and painter-sorts them into one shared [`TriList`].
//! 2. Core 0 hands core 1 the RIGHT half of the framebuffer via [`core1`]'s
//!    signal handshake, rasterizes the LEFT half itself, then joins.
//! 3. Core 0 presents. The column-major framebuffer makes the two halves
//!    contiguous disjoint slices, so the split is a plain `split_at_mut`.
//!
//! Fill rate is the scarce resource; geometry is nearly free on the FPU, so
//! both cores rasterize ("tiled" split) rather than dedicating core 1 to all
//! of rendering.

pub mod core1;
pub mod math;
pub mod raster;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use crate::bsp::display::{HEIGHT, WIDTH};
use math::{Mat34, Vec3};

/// Maximum triangles per frame. 1024 x 16 bytes = 16 KiB; more than can be
/// usefully distinguished on a 320x240 panel.
pub const MAX_TRIS: usize = 1024;

/// A model-space input triangle (counter-clockwise winding faces the camera).
#[derive(Debug, Clone, Copy)]
pub struct MeshTri {
    pub v: [Vec3; 3],
    pub color: Rgb565,
}

/// A screen-space triangle ready to rasterize (16 bytes).
#[derive(Debug, Clone, Copy)]
pub struct ScreenTri {
    pub x: [i16; 3],
    pub y: [i16; 3],
    /// Quantized view-space centroid z; larger = farther.
    pub depth: u16,
    /// RGB565 with lighting already applied.
    pub color: u16,
}

/// The shared per-frame triangle list (single-buffered; the fork-join frame
/// structure means there is never a producer/consumer overlap to exploit).
pub struct TriList {
    pub tris: [ScreenTri; MAX_TRIS],
    pub len: usize,
}

impl TriList {
    pub const EMPTY: TriList = TriList {
        tris: [ScreenTri { x: [0; 3], y: [0; 3], depth: 0, color: 0 }; MAX_TRIS],
        len: 0,
    };
}

/// Anything closer than this is clipped (view space looks down +z).
const NEAR: f32 = 0.1;

/// Focal length in pixels for a ~70 degree horizontal field of view.
const FOCAL: f32 = 228.0;

/// Directional light (view-space-ish, normalized in `build_list`).
const SUN: Vec3 = math::v3(0.45, -0.8, -0.4);

/// Push-based frame assembler: transforms, clips, lights, projects each
/// pushed triangle, then depth-sorts on [`ListBuilder::finish`].
///
/// Push-based (rather than slice-based) so procedural emitters like the tree
/// stream triangles without materializing a mesh buffer. Triangles beyond
/// [`MAX_TRIS`] are dropped silently (the cap is far above any real scene);
/// back-facing (clockwise on screen) triangles are culled.
pub struct ListBuilder<'a> {
    out: &'a mut TriList,
    mv: Mat34,
    sun: Vec3,
}

impl<'a> ListBuilder<'a> {
    /// Starts a new frame. Pass identity matrices to push view-space
    /// triangles directly (the camera-facing tree ribbons do this).
    pub fn new(model: &Mat34, view: &Mat34, out: &'a mut TriList) -> Self {
        out.len = 0;
        Self {
            out,
            mv: view.mul(model),
            sun: SUN.normalize(),
        }
    }

    /// Adds one triangle to the frame.
    pub fn push(&mut self, tri: MeshTri) {
        let a = self.mv.transform_point(tri.v[0]);
        let b = self.mv.transform_point(tri.v[1]);
        let c = self.mv.transform_point(tri.v[2]);

        // Lighting from the view-space face normal, computed before clipping.
        let normal = b.sub(a).cross(c.sub(a)).normalize();
        let intensity = 0.55 + 0.45 * normal.dot(self.sun).max(0.0);
        let color = shade(tri.color, intensity);

        // Near-plane clip: emit the surviving polygon (0, 3, or 4 vertices).
        let mut poly = [Vec3::default(); 4];
        let n = clip_near(&[a, b, c], &mut poly);
        if n < 3 {
            return;
        }

        // Fan-triangulate the clipped polygon (n is 3 or 4).
        for i in 1..n - 1 {
            emit(self.out, poly[0], poly[i], poly[i + 1], color);
        }
    }

    /// Sorts far-to-near (painter's algorithm) and ends the frame.
    pub fn finish(self) {
        self.out.tris[..self.out.len].sort_unstable_by_key(|t| core::cmp::Reverse(t.depth));
    }
}

/// Projects one clipped view-space triangle and appends it if front-facing.
fn emit(out: &mut TriList, a: Vec3, b: Vec3, c: Vec3, color: u16) {
    if out.len == MAX_TRIS {
        return;
    }
    let (ax, ay) = project(a);
    let (bx, by) = project(b);
    let (cx, cy) = project(c);

    // Screen-space winding cull: skip clockwise (back-facing) triangles.
    if (bx - ax) * (cy - ay) - (by - ay) * (cx - ax) <= 0.0 {
        return;
    }

    let centroid_z = (a.z + b.z + c.z) * (1.0 / 3.0);
    out.tris[out.len] = ScreenTri {
        x: [ax as i16, bx as i16, cx as i16],
        y: [ay as i16, by as i16, cy as i16],
        // 2048 units/step covers a ~0.1..32 unit scene in u16.
        depth: (centroid_z * 2048.0).clamp(0.0, 65535.0) as u16,
        color,
    };
    out.len += 1;
}

fn project(v: Vec3) -> (f32, f32) {
    let inv_z = 1.0 / v.z;
    (
        WIDTH as f32 * 0.5 + FOCAL * v.x * inv_z,
        HEIGHT as f32 * 0.5 - FOCAL * v.y * inv_z,
    )
}

/// Clips a triangle against the near plane, writing the result into `out`.
/// Returns the vertex count (0 when fully behind, 3 or 4 otherwise).
fn clip_near(tri: &[Vec3; 3], out: &mut [Vec3; 4]) -> usize {
    let mut n = 0;
    for i in 0..3 {
        let cur = tri[i];
        let next = tri[(i + 1) % 3];
        let cur_in = cur.z >= NEAR;
        let next_in = next.z >= NEAR;
        if cur_in {
            out[n] = cur;
            n += 1;
        }
        if cur_in != next_in {
            let t = (NEAR - cur.z) / (next.z - cur.z);
            out[n] = cur.add(next.sub(cur).scale(t));
            n += 1;
        }
    }
    n
}

/// Scales an RGB565 color by a 0..=1 intensity, per channel.
fn shade(color: Rgb565, intensity: f32) -> u16 {
    let scale = |v: u8| (f32::from(v) * intensity) as u8;
    let shaded = Rgb565::new(scale(color.r()), scale(color.g()), scale(color.b()));
    use embedded_graphics::pixelcolor::raw::RawU16;
    RawU16::from(shaded).into_inner()
}

// Rust guideline compliant 2026-08-21
