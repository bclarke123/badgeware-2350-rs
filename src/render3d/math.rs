//! Minimal f32 3D math: vectors, affine transforms, orbit camera, projection.
//!
//! Everything is `f32` — both RP2350 cores have a single-precision FPU, so
//! add/mul are ~1 cycle and divide ~14. Trig and square roots use fast f32
//! approximations below rather than `libm`: libm's `sinf`/`cosf` evaluate
//! their polynomials in `f64`, which the M33 must emulate in software at
//! microseconds per call — measured as a 20x geometry slowdown with the
//! tree's ~900 sway calls per frame. The ~0.1% error here is invisible in
//! animation and lighting.

/// A 3-component vector / point.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Shorthand constructor.
pub const fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

impl core::ops::Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl core::ops::Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

// The inherent `add`/`sub` keep call sites method-chained (`a.add(b.scale(t))`)
// without importing the ops traits everywhere; the traits above make the
// operators available too.
#[expect(clippy::should_implement_trait, reason = "ops traits are implemented as well; inherent forms keep call sites free of trait imports")]
impl Vec3 {
    pub fn add(self, o: Vec3) -> Vec3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }

    pub fn sub(self, o: Vec3) -> Vec3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }

    pub fn scale(self, s: f32) -> Vec3 {
        v3(self.x * s, self.y * s, self.z * s)
    }

    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        v3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// Unit vector (~0.0001 length error); zero for near-zero input, not NaN.
    #[link_section = ".data.geom"]
    pub fn normalize(self) -> Vec3 {
        let len_sq = self.dot(self);
        if len_sq < 1e-12 {
            return Vec3::default();
        }
        self.scale(inv_sqrt(len_sq))
    }
}

/// Fast reciprocal square root: bit-trick seed plus two Newton iterations.
///
/// Runs in a handful of FPU cycles; relative error is under 5e-6 after the
/// second iteration — far below anything visible in shading or geometry.
#[link_section = ".data.geom"]
pub fn inv_sqrt(x: f32) -> f32 {
    let i = 0x5f37_59df - (x.to_bits() >> 1);
    let mut y = f32::from_bits(i);
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    y
}

/// Fast sine (max error ~1e-3): range-reduce to one period, then a parabola
/// with an odd correction term. Accurate enough for animation and lighting;
/// do not use where phase must stay exact over unbounded time.
#[link_section = ".data.geom"]
pub fn fast_sin(x: f32) -> f32 {
    const TAU: f32 = core::f32::consts::TAU;
    // Wrap to [-pi, pi]. The i32 cast bounds usable |x| to ~2^31/tau, far
    // beyond any animation clock this firmware produces.
    let turns = x * (1.0 / TAU);
    let wrapped = x - (round_i32(turns) as f32) * TAU;

    // Parabolic approximation, then one refinement pass.
    const B: f32 = 4.0 / core::f32::consts::PI;
    const C: f32 = -4.0 / (core::f32::consts::PI * core::f32::consts::PI);
    let y = B * wrapped + C * wrapped * abs(wrapped);
    0.225 * (y * abs(y) - y) + y
}

/// Fast cosine via the sine phase shift.
#[link_section = ".data.geom"]
pub fn fast_cos(x: f32) -> f32 {
    fast_sin(x + core::f32::consts::FRAC_PI_2)
}

fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7fff_ffff)
}

fn round_i32(x: f32) -> i32 {
    if x >= 0.0 { (x + 0.5) as i32 } else { (x - 0.5) as i32 }
}

/// Row-major 3x4 affine transform (rotation/scale in columns 0-2, translation
/// in column 3). Enough for model/view; projection is done separately.
#[derive(Debug, Clone, Copy)]
pub struct Mat34(pub [f32; 12]);

impl Mat34 {
    pub const IDENTITY: Mat34 = Mat34([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);

    pub fn translate(t: Vec3) -> Mat34 {
        let mut m = Self::IDENTITY;
        m.0[3] = t.x;
        m.0[7] = t.y;
        m.0[11] = t.z;
        m
    }

    pub fn rotate_x(a: f32) -> Mat34 {
        let (s, c) = (fast_sin(a), fast_cos(a));
        Mat34([1.0, 0.0, 0.0, 0.0, 0.0, c, -s, 0.0, 0.0, s, c, 0.0])
    }

    pub fn rotate_y(a: f32) -> Mat34 {
        let (s, c) = (fast_sin(a), fast_cos(a));
        Mat34([c, 0.0, s, 0.0, 0.0, 1.0, 0.0, 0.0, -s, 0.0, c, 0.0])
    }

    /// `self * other` (apply `other` first, then `self`).
    pub fn mul(&self, o: &Mat34) -> Mat34 {
        let a = &self.0;
        let b = &o.0;
        let mut r = [0.0f32; 12];
        for row in 0..3 {
            for col in 0..4 {
                let mut acc = a[row * 4] * b[col]
                    + a[row * 4 + 1] * b[4 + col]
                    + a[row * 4 + 2] * b[8 + col];
                if col == 3 {
                    acc += a[row * 4 + 3];
                }
                r[row * 4 + col] = acc;
            }
        }
        Mat34(r)
    }

    #[link_section = ".data.geom"]
    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        let m = &self.0;
        v3(
            m[0] * p.x + m[1] * p.y + m[2] * p.z + m[3],
            m[4] * p.x + m[5] * p.y + m[6] * p.z + m[7],
            m[8] * p.x + m[9] * p.y + m[10] * p.z + m[11],
        )
    }

    /// Rotation only (for normals / directions).
    #[allow(dead_code, reason = "needed once lighting moves to world-space normals")]
    pub fn transform_dir(&self, p: Vec3) -> Vec3 {
        let m = &self.0;
        v3(
            m[0] * p.x + m[1] * p.y + m[2] * p.z,
            m[4] * p.x + m[5] * p.y + m[6] * p.z,
            m[8] * p.x + m[9] * p.y + m[10] * p.z,
        )
    }
}

/// Orbit camera around a target point; view space looks down +z.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub target: Vec3,
}

impl Camera {
    /// World → view transform for the current orbit position.
    pub fn view(&self) -> Mat34 {
        // Orbit = translate target to origin, yaw, pitch, then push the world
        // `dist` units in front of the camera (+z forward).
        Mat34::translate(v3(0.0, 0.0, self.dist))
            .mul(&Mat34::rotate_x(self.pitch))
            .mul(&Mat34::rotate_y(self.yaw))
            .mul(&Mat34::translate(self.target.scale(-1.0)))
    }
}

// Rust guideline compliant 2026-08-21
