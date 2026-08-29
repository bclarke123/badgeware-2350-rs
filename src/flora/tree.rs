//! Procedural tree: recursive branching skeleton, ribbon-quad rendering,
//! staggered eased growth, billboard leaves, and wind sway.
//!
//! A tree is generated once per seed (deterministic: same seed byte, same
//! tree) into static storage, then *emitted* every frame: each branch becomes
//! a camera-facing tapered ribbon (2 triangles), each leaf a screen-aligned
//! billboard quad. Camera-facing emission happens in view space, which is why
//! [`Tree::emit`] takes the view matrix and pushes into a [`ListBuilder`]
//! constructed with [`ListBuilder::new_view_space`].
//!
//! Growth is time-based: depth `d` branches grow during their own window of
//! the shared [`Timeline`] (via `Timeline::segment`), so the trunk finishes
//! before its children sprout and a child's fixed base point is already final
//! when it starts growing. Leaves pop in last. Wind is a continuous
//! displacement field over height — a function of `(y, t)` applied to every
//! point — so joints can never visibly separate.

use embassy_time::Duration;
use embedded_graphics::pixelcolor::Rgb565;
use heapless::Vec;
use crate::render3d::math::{fast_cos, fast_sin};

use super::anim::{ease_out_cubic, segment_progress};
use crate::render3d::math::{v3, Mat34, Vec3};
use crate::render3d::texture::{self, Textures};
use crate::render3d::{tint, ListBuilder, FOCAL};

/// Deepest branch generation (trunk is depth 0; leaves attach at this depth).
const MAX_DEPTH: u8 = 5;

/// Where the trunk meets the ground; despawn scaling shrinks toward here.
const TREE_BASE: Vec3 = v3(0.0, -0.95, 0.0);

/// Branch capacity; generation prunes silently when full (a bushy seed just
/// gets a slightly thinner tree). Sized so the densest species (the bush,
/// ~240 branches / ~670 leaves wanted) fits without pruning: worst case
/// ~1,930 triangles ~= 15.2 ms/frame by measured marginal cost — inside the
/// 16.7 ms vsync budget.
pub const MAX_BRANCHES: usize = 256;
/// Leaf capacity, pruned the same way.
pub const MAX_LEAVES: usize = 768;

/// Per-depth growth stagger and per-branch growth duration.
const GROW_STAGGER_MS: u64 = 850;
const GROW_LEN_MS: u64 = 1300;
/// Leaves start after the last branch depth and pop in over this long.
const LEAF_LEN_MS: u64 = 700;

/// Total time from seed to full bloom (for callers pacing a replay).
pub const TOTAL_GROW_MS: u64 =
    (MAX_DEPTH as u64 + 1) * GROW_STAGGER_MS + GROW_LEN_MS + LEAF_LEN_MS;

/// Per-seed shape parameters rolled once in [`Tree::generate`].
///
/// Three kinds share one parameter set: sakura (pink blossoms), the classic
/// green tree, and the bush — a shorter, wider, denser green. Every leaf or
/// blossom is one textured quad (2 triangles); the branch/leaf capacity caps
/// bound the worst case regardless.
struct Species {
    blossoms: bool,
    /// Branch-pitch multiplier: <1.0 tall and narrow, >1.0 wide and squat.
    spread: f32,
    /// Chance of a third child per branch node.
    extra_child_p: f32,
    /// Minimum leaves per twig (a coin flip adds one more).
    leaves_per_twig: u32,
    /// Trunk length multiplier (bushes start low).
    trunk_scale: f32,
}

#[derive(Clone, Copy)]
struct Branch {
    start: Vec3,
    end: Vec3,
    r0: f32,
    r1: f32,
    depth: u8,
}

#[derive(Clone, Copy)]
struct Leaf {
    pos: Vec3,
    size: f32,
    color: Rgb565,
    /// 0..1 jitter spreading the leaf pop-in inside its window.
    phase: f32,
    /// Base orientation of the billboard shape (radians).
    angle: f32,
    /// Texture shape id ([`texture::LEAF`], [`texture::BLOSSOM`], ...).
    shape: u8,
}

/// A generated tree, ready to emit each frame. Lives in a static (it is a few
/// KiB — too big for a task future).
pub struct Tree {
    branches: Vec<Branch, MAX_BRANCHES>,
    leaves: Vec<Leaf, MAX_LEAVES>,
}

impl Tree {
    pub const EMPTY: Tree = Tree {
        branches: Vec::new(),
        leaves: Vec::new(),
    };

    /// Regenerates this tree from a seed (deterministic).
    pub fn generate(&mut self, seed: u32) {
        self.branches.clear();
        self.leaves.clear();
        // Knuth multiplicative hash spreads consecutive seed bytes into very
        // different trees; |1 keeps xorshift out of its zero fixed point.
        let mut rng = XorShift(seed.wrapping_mul(2654435761).wrapping_add(0x9e37_79b9) | 1);
        // Per-seed species character; within each kind, silhouettes still
        // range tall-and-narrow to wide-and-squat via `spread`.
        let roll = rng.unit();
        let species = if roll < 0.35 {
            // Sakura: pink blossoms, classic shape.
            Species {
                blossoms: true,
                spread: rng.range(0.75, 1.4),
                extra_child_p: 0.35,
                // Sprites halved the blossom's triangle cost (4 -> 2), so
                // sakura can carry a denser bloom for the same budget.
                leaves_per_twig: 4,
                trunk_scale: 1.0,
            }
        } else if roll < 0.7 {
            // Classic green tree. Half its foliage is three-leaf tufts, so
            // fewer sprites read as a fuller crown than the old 3 per twig.
            Species {
                blossoms: false,
                spread: rng.range(0.75, 1.4),
                extra_child_p: 0.35,
                leaves_per_twig: 2,
                trunk_scale: 1.0,
            }
        } else {
            // Bush: low trunk, wide and dense green crown.
            Species {
                blossoms: false,
                spread: rng.range(1.1, 1.55),
                // 0.65 measured ~1,950 tris = 16.5 ms — the exact frame
                // budget; 0.55 sheds ~150 tris for reliable 60 fps headroom.
                extra_child_p: 0.55,
                // Tuft sprites carry three leaves each: 3 per twig here is
                // denser on screen than the old 4 flat kites.
                leaves_per_twig: 3,
                trunk_scale: 0.72,
            }
        };
        let trunk_len = rng.range(0.55, 0.85) * species.trunk_scale;
        let trunk_radius = 0.075 * rng.range(0.9, 1.15);
        self.grow(
            TREE_BASE,
            v3(0.0, 1.0, 0.0),
            trunk_len,
            trunk_radius,
            0,
            &species,
            &mut rng,
        );
    }

    #[expect(clippy::too_many_arguments, reason = "internal recursion state, not an API")]
    fn grow(
        &mut self,
        start: Vec3,
        dir: Vec3,
        length: f32,
        radius: f32,
        depth: u8,
        species: &Species,
        rng: &mut XorShift,
    ) {
        let end = start.add(dir.scale(length));
        let branch = Branch {
            start,
            end,
            r0: radius,
            r1: radius * 0.62,
            depth,
        };
        if self.branches.push(branch).is_err() {
            return; // capacity pruning
        }

        if depth == MAX_DEPTH {
            let leaves = (species.leaves_per_twig + rng.next() % 2) as usize;
            for _ in 0..leaves {
                let jitter = v3(
                    rng.range(-0.08, 0.08),
                    rng.range(-0.03, 0.10),
                    rng.range(-0.08, 0.08),
                );
                let blossom = species.blossoms && rng.unit() < 0.65;
                // The 0.75 tint bakes the retired per-triangle sun lighting.
                // Shade maps add their own dark/light variation on top, so
                // these are the "mid" tones.
                let (color, shape) = if blossom {
                    // Blossom pinks.
                    (tint(Rgb565::new(30, 34 + (rng.next() % 14) as u8, 24), 0.8), texture::BLOSSOM)
                } else {
                    // Leaf greens; every other green leaf is a three-leaf
                    // tuft, which reads as a denser crown for no extra cost.
                    let shape = if rng.unit() < 0.5 { texture::TUFT } else { texture::LEAF };
                    (
                        tint(Rgb565::new(4 + (rng.next() % 6) as u8, 34 + (rng.next() % 20) as u8, 6), 0.8),
                        shape,
                    )
                };
                // Tufts and blossoms are clusters, so they get more room.
                let size_mul = if shape == texture::LEAF { 1.0 } else { 1.05 };
                let leaf = Leaf {
                    pos: end.add(jitter),
                    size: rng.range(0.055, 0.10) * size_mul,
                    color,
                    phase: rng.unit(),
                    angle: rng.range(0.0, core::f32::consts::TAU),
                    shape,
                };
                if self.leaves.push(leaf).is_err() {
                    return;
                }
            }
            return;
        }

        // 2-3 children fanned around the parent direction; denser kinds roll
        // the third child more often.
        let children = if depth == 0 {
            3
        } else {
            2 + u32::from(rng.unit() < species.extra_child_p)
        };
        let (u, v) = basis(dir);
        let azimuth0 = rng.range(0.0, core::f32::consts::TAU);
        for k in 0..children {
            let azimuth = azimuth0 + k as f32 * core::f32::consts::TAU / children as f32
                + rng.range(-0.3, 0.3);
            // Spread widens (or steepens) the whole silhouette; the upward
            // bias shrinks as spread grows so squat trees actually stay squat.
            let pitch = (rng.range(0.35, 0.65) * species.spread).min(1.15);
            let lateral = u.scale(fast_cos(azimuth)).add(v.scale(fast_sin(azimuth)));
            // Slight upward bias keeps the crown from drooping.
            let child_dir = dir
                .scale(fast_cos(pitch))
                .add(lateral.scale(fast_sin(pitch)))
                .add(v3(0.0, 0.12 / species.spread, 0.0))
                .normalize();
            self.grow(
                end,
                child_dir,
                length * rng.range(0.62, 0.78),
                radius * 0.62,
                depth + 1,
                species,
                rng,
            );
        }
    }

    /// Emits the tree at the current growth instant into `out`.
    ///
    /// `out` must be a view-space builder: ribbons and leaf billboards are
    /// camera-facing, so this function does the view transform itself and
    /// pushes view-space triangles with pre-baked colors.
    /// `scale` (0..=1) shrinks the whole tree toward its base — the despawn
    /// animation. Positions contract toward [`TREE_BASE`], which also quiets
    /// the wind sway naturally (its amplitude grows with height).
    /// `parity` selects every other branch/leaf (0 or 1) so the two cores can
    /// each emit half of the tree into their own list.
    pub fn emit(
        &self,
        view: &Mat34,
        growth_elapsed: Duration,
        time_s: f32,
        scale: f32,
        parity: usize,
        out: &mut ListBuilder<'_>,
    ) {
        if scale <= 0.01 {
            return;
        }
        let shrink = |p: Vec3| TREE_BASE.add(p.sub(TREE_BASE).scale(scale));
        for (_, b) in self
            .branches
            .iter()
            .enumerate()
            .filter(|(i, _)| i & 1 == parity)
        {
            let t = ease_out_cubic(segment_progress(
                growth_elapsed,
                Duration::from_millis(u64::from(b.depth) * GROW_STAGGER_MS),
                Duration::from_millis(GROW_LEN_MS),
            ));
            if t <= 0.0 {
                continue;
            }
            let tip = b.start.add(b.end.sub(b.start).scale(t));
            let p0 = view.transform_point(sway(shrink(b.start), time_s));
            let p1 = view.transform_point(sway(shrink(tip), time_s));

            // Camera-facing ribbon: side is perpendicular to both the branch
            // axis and the line of sight, so the quad always shows its face.
            let axis = p1.sub(p0);
            let mid = p0.add(p1).scale(0.5);
            let mut side = axis.cross(mid).normalize();
            if side.dot(side) < 0.5 {
                side = v3(1.0, 0.0, 0.0); // camera exactly on the axis
            }
            // Radius eases in with growth so twigs sprout thin.
            let r0 = b.r0 * (0.3 + 0.7 * t) * scale;
            let r1 = b.r1 * (0.3 + 0.7 * t) * scale;
            let color = bark_color(b.depth);
            out.push_quad(
                [
                    p0.sub(side.scale(r0)),
                    p1.sub(side.scale(r1)),
                    p1.add(side.scale(r1)),
                    p0.add(side.scale(r0)),
                ],
                color,
            );
        }

        let leaf_delay_base = u64::from(MAX_DEPTH + 1) * GROW_STAGGER_MS;
        for (_, leaf) in self
            .leaves
            .iter()
            .enumerate()
            .filter(|(i, _)| i & 1 == parity)
        {
            let delay = leaf_delay_base + (leaf.phase * 900.0) as u64;
            let t = ease_out_cubic(segment_progress(
                growth_elapsed,
                Duration::from_millis(delay),
                Duration::from_millis(LEAF_LEN_MS),
            ));
            if t <= 0.0 {
                continue;
            }
            let p = view.transform_point(sway(shrink(leaf.pos), time_s));
            let s = leaf.size * t * scale;
            // Gentle flutter on top of the leaf's fixed orientation.
            let angle = leaf.angle + 0.18 * fast_sin(time_s * 2.1 + leaf.phase * 6.3);
            emit_sprite(out, p, s, angle, leaf.color, leaf.shape);
        }
    }
}

/// A leaf or blossom: one screen-aligned textured sprite, rotated by `angle`
/// about its view-space center `p`; `s` is the half-size of the (unit) map
/// square, and the quad spans only the map's content rect. The map size is
/// picked from the projected width so small, distant sprites sample the 8x8
/// mip instead of shimmering through a 16x16 one.
fn emit_sprite(out: &mut ListBuilder<'_>, p: Vec3, s: f32, angle: f32, color: Rgb565, shape: u8) {
    if p.z <= 0.0 {
        return;
    }
    let rot = rotator(p, angle);
    let [x_lo, y_lo, x_hi, y_hi] = texture::content_rect(shape);
    let corners = [rot(x_lo * s, y_hi * s, 0.0), rot(x_hi * s, y_hi * s, 0.0), rot(x_hi * s, y_lo * s, 0.0)];
    let px = 2.0 * s * FOCAL / p.z;
    out.push_sprite(corners, Textures::pick(shape, px), color);
}

/// Billboard-space point placement: rotates local (x, y) by `angle` about the
/// view-space center `p`, with an optional depth nudge.
fn rotator(p: Vec3, angle: f32) -> impl Fn(f32, f32, f32) -> Vec3 {
    let (c, s) = (fast_cos(angle), fast_sin(angle));
    move |x: f32, y: f32, z: f32| p.add(v3(x * c - y * s, x * s + y * c, z))
}


/// Wind: a smooth displacement field over height and time. Being a pure
/// function of position, parent tips and child bases displace identically —
/// joints cannot separate.
pub(super) fn sway(p: Vec3, time_s: f32) -> Vec3 {
    // Amplitude grows with height above the roots so the trunk barely moves.
    let lever = (p.y + 0.95).max(0.0);
    let a = 0.022 * lever;
    v3(
        p.x + a * fast_sin(time_s * 1.3 + p.y * 1.2),
        p.y,
        p.z + 0.6 * a * fast_sin(time_s * 1.7 + p.y * 0.9 + 1.3),
    )
}

fn bark_color(depth: u8) -> Rgb565 {
    // Trunk dark brown, lightening toward the twigs. The 0.75 factor bakes
    // the sun intensity the old per-triangle lighting produced for
    // camera-facing surfaces, keeping the established look.
    let d = u8::min(depth, 5);
    tint(Rgb565::new(10 + d * 2, 16 + d * 4, 5 + d), 0.75)
}

/// An orthonormal basis perpendicular to `dir`.
fn basis(dir: Vec3) -> (Vec3, Vec3) {
    let helper = if dir.y.abs() < 0.9 { v3(0.0, 1.0, 0.0) } else { v3(1.0, 0.0, 0.0) };
    let u = dir.cross(helper).normalize();
    let v = dir.cross(u);
    (u, v)
}

/// Tiny deterministic PRNG (xorshift32) for repeatable trees per seed.
pub(super) struct XorShift(pub(super) u32);

impl XorShift {
    pub(super) fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform-ish in [0, 1).
    pub(super) fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / 16_777_216.0
    }

    pub(super) fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

// Rust guideline compliant 2026-08-29
