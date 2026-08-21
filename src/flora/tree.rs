//! Procedural tree: recursive branching skeleton, ribbon-quad rendering,
//! staggered eased growth, billboard leaves, and wind sway.
//!
//! A tree is generated once per seed (deterministic: same seed byte, same
//! tree) into static storage, then *emitted* every frame: each branch becomes
//! a camera-facing tapered ribbon (2 triangles), each leaf a screen-aligned
//! billboard quad. Camera-facing emission happens in view space, which is why
//! [`Tree::emit`] takes the view matrix and pushes into a [`ListBuilder`]
//! constructed with identity transforms.
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

use super::anim::{ease_out_cubic, Timeline};
use crate::render3d::math::{v3, Mat34, Vec3};
use crate::render3d::{ListBuilder, MeshTri};

/// Deepest branch generation (trunk is depth 0; leaves attach at this depth).
const MAX_DEPTH: u8 = 5;

/// Branch capacity; generation prunes silently when full (a bushy seed just
/// gets a slightly thinner tree).
pub const MAX_BRANCHES: usize = 192;
/// Leaf capacity, pruned the same way.
pub const MAX_LEAVES: usize = 288;

/// Per-depth growth stagger and per-branch growth duration.
const GROW_STAGGER_MS: u64 = 850;
const GROW_LEN_MS: u64 = 1300;
/// Leaves start after the last branch depth and pop in over this long.
const LEAF_LEN_MS: u64 = 700;

/// Total time from seed to full bloom (for callers pacing a replay).
pub const TOTAL_GROW_MS: u64 =
    (MAX_DEPTH as u64 + 1) * GROW_STAGGER_MS + GROW_LEN_MS + LEAF_LEN_MS;

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
        // Sakura-ish seeds carry blossoms; the rest stay leafy green.
        let blossoms = rng.unit() < 0.4;
        self.grow(
            v3(0.0, -0.95, 0.0),
            v3(0.0, 1.0, 0.0),
            0.75,
            0.075,
            0,
            blossoms,
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
        blossoms: bool,
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
            let leaves = 2 + (rng.next() % 2) as usize;
            for _ in 0..leaves {
                let jitter = v3(
                    rng.range(-0.08, 0.08),
                    rng.range(-0.03, 0.10),
                    rng.range(-0.08, 0.08),
                );
                let color = if blossoms && rng.unit() < 0.65 {
                    // Blossom pinks.
                    Rgb565::new(29, 34 + (rng.next() % 14) as u8, 24)
                } else {
                    // Leaf greens.
                    Rgb565::new(4 + (rng.next() % 6) as u8, 34 + (rng.next() % 20) as u8, 6)
                };
                let leaf = Leaf {
                    pos: end.add(jitter),
                    size: rng.range(0.045, 0.085),
                    color,
                    phase: rng.unit(),
                };
                if self.leaves.push(leaf).is_err() {
                    return;
                }
            }
            return;
        }

        // 2-3 children fanned around the parent direction.
        let children = if depth == 0 { 3 } else { 2 + u32::from(rng.unit() < 0.35) };
        let (u, v) = basis(dir);
        let azimuth0 = rng.range(0.0, core::f32::consts::TAU);
        for k in 0..children {
            let azimuth = azimuth0 + k as f32 * core::f32::consts::TAU / children as f32
                + rng.range(-0.3, 0.3);
            let pitch = rng.range(0.35, 0.65);
            let lateral = u.scale(fast_cos(azimuth)).add(v.scale(fast_sin(azimuth)));
            // Slight upward bias keeps the crown from drooping.
            let child_dir = dir
                .scale(fast_cos(pitch))
                .add(lateral.scale(fast_sin(pitch)))
                .add(v3(0.0, 0.12, 0.0))
                .normalize();
            self.grow(
                end,
                child_dir,
                length * rng.range(0.62, 0.78),
                radius * 0.62,
                depth + 1,
                blossoms,
                rng,
            );
        }
    }

    /// Emits the tree at the current growth instant into `out`.
    ///
    /// `out` must have been built with identity model/view matrices: ribbons
    /// and leaf billboards are camera-facing, so this function does the view
    /// transform itself and pushes view-space triangles.
    pub fn emit(&self, view: &Mat34, growth: &Timeline, time_s: f32, out: &mut ListBuilder<'_>) {
        for b in &self.branches {
            let t = ease_out_cubic(growth.segment(
                Duration::from_millis(u64::from(b.depth) * GROW_STAGGER_MS),
                Duration::from_millis(GROW_LEN_MS),
            ));
            if t <= 0.0 {
                continue;
            }
            let tip = b.start.add(b.end.sub(b.start).scale(t));
            let p0 = view.transform_point(sway(b.start, time_s));
            let p1 = view.transform_point(sway(tip, time_s));

            // Camera-facing ribbon: side is perpendicular to both the branch
            // axis and the line of sight, so the quad always shows its face.
            let axis = p1.sub(p0);
            let mid = p0.add(p1).scale(0.5);
            let mut side = axis.cross(mid).normalize();
            if side.dot(side) < 0.5 {
                side = v3(1.0, 0.0, 0.0); // camera exactly on the axis
            }
            // Radius eases in with growth so twigs sprout thin.
            let r0 = b.r0 * (0.3 + 0.7 * t);
            let r1 = b.r1 * (0.3 + 0.7 * t);
            let color = bark_color(b.depth);
            push_quad(
                out,
                p0.sub(side.scale(r0)),
                p1.sub(side.scale(r1)),
                p1.add(side.scale(r1)),
                p0.add(side.scale(r0)),
                color,
            );
        }

        let leaf_delay_base = u64::from(MAX_DEPTH + 1) * GROW_STAGGER_MS;
        for leaf in &self.leaves {
            let delay = leaf_delay_base + (leaf.phase * 900.0) as u64;
            let t = ease_out_cubic(growth.segment(
                Duration::from_millis(delay),
                Duration::from_millis(LEAF_LEN_MS),
            ));
            if t <= 0.0 {
                continue;
            }
            let p = view.transform_point(sway(leaf.pos, time_s));
            let s = leaf.size * t;
            // Screen-aligned billboard, wound toward the camera (-z normal).
            push_quad(
                out,
                p.add(v3(-s, -s, 0.0)),
                p.add(v3(-s, s, 0.0)),
                p.add(v3(s, s, 0.0)),
                p.add(v3(s, -s, 0.0)),
                leaf.color,
            );
        }
    }
}

/// Wind: a smooth displacement field over height and time. Being a pure
/// function of position, parent tips and child bases displace identically —
/// joints cannot separate.
fn sway(p: Vec3, time_s: f32) -> Vec3 {
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
    // Trunk dark brown, lightening toward the twigs.
    let d = u8::min(depth, 5);
    Rgb565::new(10 + d * 2, 16 + d * 4, 5 + d)
}

/// Pushes a quad as two triangles (a,b,c) + (a,c,d).
fn push_quad(out: &mut ListBuilder<'_>, a: Vec3, b: Vec3, c: Vec3, d: Vec3, color: Rgb565) {
    out.push(MeshTri { v: [a, b, c], color });
    out.push(MeshTri { v: [a, c, d], color });
}

/// An orthonormal basis perpendicular to `dir`.
fn basis(dir: Vec3) -> (Vec3, Vec3) {
    let helper = if dir.y.abs() < 0.9 { v3(0.0, 1.0, 0.0) } else { v3(1.0, 0.0, 0.0) };
    let u = dir.cross(helper).normalize();
    let v = dir.cross(u);
    (u, v)
}

/// Tiny deterministic PRNG (xorshift32) for repeatable trees per seed.
struct XorShift(u32);

impl XorShift {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform-ish in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / 16_777_216.0
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

// Rust guideline compliant 2026-08-21
