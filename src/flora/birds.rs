//! Distant birds: flapping two-triangle silhouettes crossing the sky.
//!
//! Birds live in view space (like the stars) at far z, flying a straight
//! line across the frame with a gentle bob and a wing flap. They spawn just
//! outside one edge during daytime only, and always *finish* their crossing —
//! leaving by exiting the frame, never by fading, because birds do not fade.

use embassy_rp::clocks::RoscRng;
use embedded_graphics::pixelcolor::Rgb565;

use crate::render3d::math::{fast_sin, v3};
use crate::render3d::{ListBuilder, MeshTri};

/// Concurrent bird cap (2 triangles each).
const MAX_BIRDS: usize = 4;

/// View-space x beyond which a bird has fully left the widest frame.
const EXIT_X: f32 = 14.0;

/// Silhouette color: near-black against any daytime sky.
const SILHOUETTE: Rgb565 = Rgb565::new(1, 3, 3);

#[derive(Clone, Copy, Default)]
struct Bird {
    active: bool,
    x: f32,
    y: f32,
    z: f32,
    /// Signed horizontal speed (units/s); sign is the flight direction.
    speed: f32,
    /// Wingspan scale, 0.3 (a speck) to 1.15 (a near bird).
    size: f32,
    flap_phase: f32,
    /// Earliest time (seconds) this slot may spawn again.
    next_spawn_s: f32,
}

/// A small pool of birds, updated and emitted once per frame.
pub struct Flock {
    birds: [Bird; MAX_BIRDS],
}

impl Flock {
    pub fn new() -> Self {
        Self {
            birds: [Bird::default(); MAX_BIRDS],
        }
    }

    /// Advances flight paths; spawns replacements only while `daytime`.
    pub fn update(&mut self, dt: f32, time_s: f32, daytime: bool) {
        for bird in &mut self.birds {
            if bird.active {
                bird.x += bird.speed * dt;
                if bird.x.abs() > EXIT_X {
                    bird.active = false;
                    // 4-16 s until this slot may fly again.
                    bird.next_spawn_s = time_s + 4.0 + rand_unit() * 12.0;
                }
            } else if daytime && time_s >= bird.next_spawn_s {
                let leftward = RoscRng.next_u32() & 1 == 0;
                bird.z = 12.0 + rand_unit() * 8.0;
                bird.y = 2.0 + rand_unit() * 3.5;
                bird.speed = (1.2 + rand_unit() * 1.2) * if leftward { -1.0 } else { 1.0 };
                bird.x = if leftward { EXIT_X } else { -EXIT_X };
                // Uniform down to near-imperceptible specks.
                bird.size = 0.3 + rand_unit() * 0.85;
                bird.flap_phase = rand_unit() * 6.3;
                bird.active = true;
            }
        }
    }

    /// Emits active birds (view space, far depth — occluded by everything).
    pub fn emit(&self, time_s: f32, out: &mut ListBuilder<'_>) {
        for bird in self.birds.iter().filter(|b| b.active) {
            // Wing flap plus a slight flight bob on the same clock; smaller
            // birds flap faster, as real ones do.
            let flap_hz = 6.5 + 5.0 * (1.15 - bird.size);
            let flap = fast_sin(time_s * flap_hz + bird.flap_phase);
            let bob = 0.12 * fast_sin(time_s * 2.3 + bird.flap_phase);
            let p = v3(bird.x, bird.y + bob, bird.z);

            // Base span tracks distance (constant angular size), then the
            // per-bird size factor varies it from speck to companion.
            let w = 0.028 * bird.z * bird.size;
            let body_h = 0.12 * w;
            let tip_dy = 0.55 * w * flap;
            let b_top = p.add(v3(0.0, body_h, 0.0));
            let b_bot = p.add(v3(0.0, -body_h, 0.0));
            let tip_l = p.add(v3(-w, tip_dy, 0.0));
            let tip_r = p.add(v3(w, tip_dy, 0.0));

            // Winding toward the camera (see the billboard convention).
            out.push(MeshTri { v: [b_top, b_bot, tip_l], color: SILHOUETTE });
            out.push(MeshTri { v: [b_top, tip_r, b_bot], color: SILHOUETTE });
        }
    }
}

/// Uniform-ish 0..1 from the hardware entropy source (wildlife may be truly
/// random; only trees must be reproducible).
fn rand_unit() -> f32 {
    (RoscRng.next_u32() >> 8) as f32 / 16_777_216.0
}

// Rust guideline compliant 2026-08-21
