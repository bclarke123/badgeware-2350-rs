//! Software 3D pipeline: dual-core tiled flat-shaded triangle rendering.
//!
//! Frame flow (strict fork-join, decided in planning):
//! 1. Core 0 assembles view-space triangles (emitters bake their own colors),
//!    near-clips, projects, and painter-sorts into one shared [`TriList`].
//! 2. Core 0 hands core 1 the RIGHT half of the framebuffer via [`core1`]'s
//!    signal handshake, rasterizes the LEFT half itself, then joins.
//! 3. Core 0 presents. The column-major framebuffer makes the two halves
//!    contiguous disjoint slices, so the split is a plain `split_at_mut`.
//!
//! Fill rate is the scarce resource; geometry is nearly free on the FPU, so
//! both cores rasterize ("tiled" split) rather than dedicating core 1 to all
//! of rendering.

//! Everything on the per-frame geometry path is linked into RAM
//! (`.data.geom`), like the rasterizer: both cores run it concurrently and
//! the shared XIP flash cache stalls them on each other's misses (measured
//! as +1.5 ms of geometry when the binary grew past the cache's comfort).

pub mod core1;
pub mod math;
pub mod raster;
pub mod texture;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use crate::bsp::screen::{HEIGHT, WIDTH};
use math::Vec3;

/// Maximum primitives per frame. 2048 x 32 bytes = 64 KiB; sized for a dense
/// tree (one entry per leaf/blossom sprite, up to 768 leaves, plus ~500
/// branch triangles) with headroom.
pub const MAX_TRIS: usize = 2048;

/// A view-space input triangle with its final (pre-lit) color.
#[derive(Debug, Clone, Copy)]
pub struct MeshTri {
    pub v: [Vec3; 3],
    pub color: Rgb565,
}

/// A screen-space primitive ready to rasterize (32 bytes): a flat triangle,
/// or (`tex != 0`) a textured parallelogram sprite whose three stored
/// vertices are its top-left, top-right and bottom-right corners in map
/// terms — the fourth is implied (`tl + br - tr`), and the map's content
/// rect spans the quad.
///
/// Coordinates stay `f32`: the rasterizer's edge anti-aliasing needs the
/// sub-pixel position (integer vertices would make a swaying leaf's edge snap
/// pixel to pixel), and the rasterizer works in f32 for setup anyway.
#[derive(Debug, Clone, Copy)]
pub struct ScreenTri {
    pub x: [f32; 3],
    pub y: [f32; 3],
    /// Quantized view-space centroid z; larger = farther.
    pub depth: u16,
    /// RGB565 with lighting already applied (flat), or the base tint the
    /// texture's shade levels are derived from (textured).
    pub color: u16,
    /// 0 = flat-shaded; otherwise `texture id + 1` into [`texture::Textures`].
    pub tex: u8,
    /// Which edges are silhouettes and get anti-aliased: bit 0 = v0-v1,
    /// bit 1 = v1-v2, bit 2 = v2-v0. Edges shared with a neighbouring
    /// triangle must be left unset so the two tile without a seam.
    pub aa: u8,
    /// Conservative screen x extent in 2-pixel units: `xr[0] * 2 <= min x`,
    /// `xr[1] * 2 >= max x` (saturating). Lets a core skip primitives that
    /// miss its columns without setting them up.
    pub xr: [u8; 2],
}

/// Packs a screen x range into [`ScreenTri::xr`].
#[link_section = ".data.geom"]
fn x_range(min: f32, max: f32) -> [u8; 2] {
    [(min * 0.5).clamp(0.0, 255.0) as u8, (max * 0.5 + 1.0).clamp(0.0, 255.0) as u8]
}

/// Every edge anti-aliased (a free-standing triangle).
pub const AA_ALL: u8 = 0b111;

/// `ScreenTri::tex` value marking a perspective-textured ground triangle;
/// its `aa` field then indexes [`TriList::ground`].
pub const GROUND_TEX: u8 = 0xFF;

/// Ground triangles per list (a 7x7 cell patch is 98, plus near-clip fans).
pub const MAX_GROUND: usize = 160;

/// Per-vertex perspective attributes of a ground triangle, in the same
/// vertex order as its [`ScreenTri`]: `u/z`, `v/z`, `1/z` (u, v in world
/// units — one grass tile per unit — and z the view depth). All three are
/// affine in screen space, which is what makes perspective-correct
/// texturing a reciprocal per pixel chunk (see [`raster`]).
#[derive(Debug, Clone, Copy)]
pub struct GroundAttr {
    pub uz: [f32; 3],
    pub vz: [f32; 3],
    pub iz: [f32; 3],
}

impl ScreenTri {
    const ZERO: ScreenTri = ScreenTri {
        x: [0.0; 3],
        y: [0.0; 3],
        depth: 0,
        color: 0,
        tex: 0,
        aa: 0,
        xr: [0; 2],
    };
}

/// A per-frame primitive list (single-buffered; the fork-join frame structure
/// means there is never a producer/consumer overlap to exploit).
///
/// Primitives are appended in emission order and never moved: the painter's
/// order lives in `order`, packed `depth << 16 | index` keys radix-sorted by
/// [`ListBuilder::finish`]. Sorting 4-byte keys instead of 32-byte entries
/// is what keeps the sort out of the frame budget (moving whole entries was
/// measured at ~6 us per primitive).
pub struct TriList {
    pub tris: [ScreenTri; MAX_TRIS],
    pub len: usize,
    /// `len` keys, farthest first: `depth << 16 | index into tris`.
    pub order: [u32; MAX_TRIS],
    /// Radix-sort scratch.
    scratch: [u32; MAX_TRIS],
    /// Attributes of ground triangles (indexed by their `aa` field).
    pub ground: [GroundAttr; MAX_GROUND],
    pub ground_len: usize,
}

impl TriList {
    pub const EMPTY: TriList = TriList {
        tris: [ScreenTri::ZERO; MAX_TRIS],
        len: 0,
        order: [0; MAX_TRIS],
        scratch: [0; MAX_TRIS],
        ground: [GroundAttr { uz: [0.0; 3], vz: [0.0; 3], iz: [0.0; 3] }; MAX_GROUND],
        ground_len: 0,
    };

    /// The `i`-th primitive in painter's (far-to-near) order.
    #[inline]
    pub fn nth(&self, i: usize) -> &ScreenTri {
        &self.tris[(self.order[i] & 0xffff) as usize]
    }

    /// Depth of the `i`-th primitive in painter's order.
    #[inline]
    pub fn depth_at(&self, i: usize) -> u32 {
        self.order[i] >> 16
    }
}

/// Anything closer than this is clipped (view space looks down +z).
const NEAR: f32 = 0.1;

/// Focal length in pixels for a ~70 degree horizontal field of view
/// (228 px at 320 wide), scaled with the board's screen width.
pub const FOCAL: f32 = WIDTH as f32 * 0.7125;

/// Push-based frame assembler: clips, projects, and depth-keys each pushed
/// view-space triangle, then depth-sorts on [`ListBuilder::finish`].
///
/// Push-based (rather than slice-based) so procedural emitters like the tree
/// stream triangles without materializing a mesh buffer. Triangles arrive
/// already in view space with final colors — emitters bake their lighting
/// (billboard/ribbon normals are near-constant, so per-triangle sun shading
/// was pure per-frame waste; measured as a large share of geometry time).
/// Triangles beyond [`MAX_TRIS`] are dropped silently; back-facing (clockwise
/// on screen) triangles are culled.
pub struct ListBuilder<'a> {
    out: &'a mut TriList,
}

impl<'a> ListBuilder<'a> {
    /// Starts a new frame of view-space triangles.
    pub fn new_view_space(out: &'a mut TriList) -> Self {
        out.len = 0;
        out.ground_len = 0;
        Self { out }
    }

    /// Adds one free-standing view-space triangle (all edges anti-aliased).
    #[link_section = ".data.geom"]
    pub fn push(&mut self, tri: MeshTri) {
        self.push_aa(tri, AA_ALL);
    }

    /// Adds one view-space triangle with an explicit silhouette-edge mask
    /// (see [`ScreenTri::aa`]); pass 0 for a triangle whose every edge is
    /// shared with a neighbour, e.g. a terrain cell.
    #[link_section = ".data.geom"]
    #[inline(never)]
    pub fn push_aa(&mut self, tri: MeshTri, aa: u8) {
        let color = embedded_graphics::pixelcolor::raw::RawU16::from(tri.color).into_inner();

        // Near-plane clip: emit the surviving polygon (0, 3, or 4 vertices).
        let mut poly = [Vec3::default(); 4];
        let n = clip_near(&tri.v, &mut poly);
        if n < 3 {
            return;
        }

        // Fan-triangulate the clipped polygon (n is 3 or 4). Clipping
        // re-orders the vertices, so a clipped triangle just keeps its mask
        // as-is — near-plane crossings are rare and brief.
        for i in 1..n - 1 {
            emit(self.out, [poly[0], poly[i], poly[i + 1]], color, 0, aa);
        }
    }

    /// Adds a quad `[a, b, c, d]` as two triangles with only its four outer
    /// edges anti-aliased (the diagonal is shared).
    #[link_section = ".data.geom"]
    pub fn push_quad(&mut self, v: [Vec3; 4], color: Rgb565) {
        let [a, b, c, d] = v;
        self.push_aa(MeshTri { v: [a, b, c], color }, 0b011);
        self.push_aa(MeshTri { v: [a, c, d], color }, 0b110);
    }

    /// Adds a perspective-textured ground triangle: `uv` are world-unit
    /// texture coordinates per vertex (the grass tile repeats every unit),
    /// `color` the tint the tile's shade levels are derived from. Near-plane
    /// clipped like a flat triangle, interpolating the coordinates; never
    /// anti-aliased (the patch's edges are all shared or off-screen).
    #[link_section = ".data.geom"]
    pub fn push_ground(&mut self, tri: [Vec3; 3], uv: [[f32; 2]; 3], color: Rgb565) {
        let color = embedded_graphics::pixelcolor::raw::RawU16::from(color).into_inner();
        let mut poly = [(Vec3::default(), [0.0f32; 2]); 4];
        let n = clip_near_uv(&tri, &uv, &mut poly);
        if n < 3 {
            return;
        }
        for i in 1..n - 1 {
            let (a, b, c) = (poly[0], poly[i], poly[i + 1]);
            let out = &mut *self.out;
            if out.len == MAX_TRIS || out.ground_len == MAX_GROUND {
                return;
            }
            let (ax, ay) = project(a.0);
            let (bx, by) = project(b.0);
            let (cx, cy) = project(c.0);
            if (bx - ax) * (cy - ay) - (by - ay) * (cx - ax) <= 0.0 {
                continue;
            }
            let iz = [1.0 / a.0.z, 1.0 / b.0.z, 1.0 / c.0.z];
            out.ground[out.ground_len] = GroundAttr {
                uz: [a.1[0] * iz[0], b.1[0] * iz[1], c.1[0] * iz[2]],
                vz: [a.1[1] * iz[0], b.1[1] * iz[1], c.1[1] * iz[2]],
                iz,
            };
            out.tris[out.len] = ScreenTri {
                x: [ax, bx, cx],
                y: [ay, by, cy],
                depth: depth_key((a.0.z + b.0.z + c.0.z) * (1.0 / 3.0)),
                color,
                tex: GROUND_TEX,
                aa: out.ground_len as u8,
                xr: x_range(ax.min(bx).min(cx), ax.max(bx).max(cx)),
            };
            out.ground_len += 1;
            out.len += 1;
        }
    }

    /// Adds a textured billboard sprite: `corners` are the view-space
    /// top-left, top-right and bottom-right of the quad spanning the map's
    /// content rect (see [`texture::content_rect`]); the bottom-left is
    /// implied. `tex` is a [`texture::Textures`] map id; `color` the tint the
    /// map's shade levels are derived from.
    ///
    /// Sprites are not near-clipped or winding-culled (billboards always
    /// face the camera): a quad straddling the near plane is dropped whole —
    /// they are small and sit near the tree, so this only happens for a frame
    /// or two when zoomed in hard.
    #[link_section = ".data.geom"]
    #[inline(never)]
    pub fn push_sprite(&mut self, corners: [Vec3; 3], tex: u8, color: Rgb565) {
        if corners.iter().any(|c| c.z < NEAR) || self.out.len == MAX_TRIS {
            return;
        }
        let color = embedded_graphics::pixelcolor::raw::RawU16::from(color).into_inner();
        let [tl, tr, br] = corners;
        let (x0, y0) = project(tl);
        let (x1, y1) = project(tr);
        let (x2, y2) = project(br);
        let x3 = x0 + x2 - x1; // implied bottom-left
        self.out.tris[self.out.len] = ScreenTri {
            x: [x0, x1, x2],
            y: [y0, y1, y2],
            depth: depth_key(tl.z),
            color,
            tex: tex + 1,
            aa: 0,
            xr: x_range(x0.min(x1).min(x2).min(x3), x0.max(x1).max(x2).max(x3)),
        };
        self.out.len += 1;
    }

    /// Sorts far-to-near (painter's algorithm) and ends the frame: an LSD
    /// radix sort on the 16-bit depth, two 8-bit passes, descending.
    #[link_section = ".data.geom"]
    #[inline(never)]
    pub fn finish(self) {
        let n = self.out.len;
        let out = &mut *self.out;
        for (i, key) in out.order[..n].iter_mut().enumerate() {
            *key = u32::from(out.tris[i].depth) << 16 | i as u32;
        }
        radix_pass(&out.order[..n], &mut out.scratch[..n], 16);
        radix_pass(&out.scratch[..n], &mut out.order[..n], 24);
    }
}

/// One stable counting-sort pass on the byte at `shift`, descending, so
/// after the low byte then the high byte the keys are far-to-near.
#[link_section = ".data.geom"]
fn radix_pass(src: &[u32], dst: &mut [u32], shift: u32) {
    let mut counts = [0u32; 256];
    for &k in src {
        counts[((k >> shift) & 0xff) as usize] += 1;
    }
    // Descending: bucket 255 starts at 0.
    let mut start = 0;
    for c in counts.iter_mut().rev() {
        let n = *c;
        *c = start;
        start += n;
    }
    for &k in src {
        let b = ((k >> shift) & 0xff) as usize;
        dst[counts[b] as usize] = k;
        counts[b] += 1;
    }
}

/// Scales an RGB565 color by `f`, clamping each channel (used by emitters to
/// bake lighting and tone variants at generation time).
#[link_section = ".data.geom"]
pub fn tint(color: Rgb565, f: f32) -> Rgb565 {
    let ch = |v: u8, max: u8| ((f32::from(v)) * f).min(f32::from(max)) as u8;
    Rgb565::new(ch(color.r(), 31), ch(color.g(), 63), ch(color.b(), 31))
}

/// Projects one clipped view-space triangle and appends it if front-facing.
#[link_section = ".data.geom"]
fn emit(out: &mut TriList, [a, b, c]: [Vec3; 3], color: u16, tex: u8, aa: u8) {
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
        x: [ax, bx, cx],
        y: [ay, by, cy],
        depth: depth_key(centroid_z),
        color,
        tex,
        aa,
        xr: x_range(ax.min(bx).min(cx), ax.max(bx).max(cx)),
    };
    out.len += 1;
}

/// Painter's sort key: 2048 units/step covers a ~0.1..32 unit scene in u16.
#[link_section = ".data.geom"]
fn depth_key(z: f32) -> u16 {
    (z * 2048.0).clamp(0.0, 65535.0) as u16
}

#[link_section = ".data.geom"]
fn project(v: Vec3) -> (f32, f32) {
    let inv_z = 1.0 / v.z;
    (
        WIDTH as f32 * 0.5 + FOCAL * v.x * inv_z,
        HEIGHT as f32 * 0.5 - FOCAL * v.y * inv_z,
    )
}

/// [`clip_near`] carrying per-vertex texture coordinates (linear in view
/// space, so plain interpolation is exact).
#[link_section = ".data.geom"]
fn clip_near_uv(tri: &[Vec3; 3], uv: &[[f32; 2]; 3], out: &mut [(Vec3, [f32; 2]); 4]) -> usize {
    let mut n = 0;
    for i in 0..3 {
        let j = (i + 1) % 3;
        let (cur, next) = (tri[i], tri[j]);
        let cur_in = cur.z >= NEAR;
        let next_in = next.z >= NEAR;
        if cur_in {
            out[n] = (cur, uv[i]);
            n += 1;
        }
        if cur_in != next_in {
            let t = (NEAR - cur.z) / (next.z - cur.z);
            let lerp = |a: f32, b: f32| a + (b - a) * t;
            out[n] = (
                cur.add(next.sub(cur).scale(t)),
                [lerp(uv[i][0], uv[j][0]), lerp(uv[i][1], uv[j][1])],
            );
            n += 1;
        }
    }
    n
}

/// Clips a triangle against the near plane, writing the result into `out`.
/// Returns the vertex count (0 when fully behind, 3 or 4 otherwise).
#[link_section = ".data.geom"]
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

// Rust guideline compliant 2026-08-21
