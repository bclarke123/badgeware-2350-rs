//! Tiny animation toolkit: cubic easing, lerps, and a wall-clock timeline.
//!
//! Everything is driven by real elapsed time rather than frame counts, so
//! animation speed is independent of present rate (which is vsync-locked and
//! can stretch if a frame waits on vblank). The RP2350's Cortex-M33 has a
//! single-precision FPU, so `f32` math here is cheap.

use embassy_time::{Duration, Instant};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

/// Decelerating to one: `1 - (1-t)^3`.
pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Slow-fast-slow; the classic smooth in-out cubic.
pub fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let u = -2.0 * t + 2.0;
        1.0 - u * u * u / 2.0
    }
}

/// Linear interpolation between `a` and `b`.
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Per-channel RGB565 interpolation between two colors.
pub fn lerp_color(a: Rgb565, b: Rgb565, t: f32) -> Rgb565 {
    let ch = |a: u8, b: u8| lerp(f32::from(a), f32::from(b), t) as u8;
    Rgb565::new(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// Scales `rect` about its center by `s` (0.0 collapses it, 1.0 is identity).
pub fn scale_rect(rect: Rectangle, s: f32) -> Rectangle {
    let s = s.max(0.0);
    let w = (rect.size.width as f32 * s) as u32;
    let h = (rect.size.height as f32 * s) as u32;
    let center = rect.center();
    Rectangle::with_center(center, Size::new(w, h))
}

/// A wall-clock animation window.
pub struct Timeline {
    start: Instant,
    duration: Duration,
}

impl Timeline {
    /// Starts a timeline of the given duration, beginning now.
    pub fn new(duration: Duration) -> Self {
        Self {
            start: Instant::now(),
            duration,
        }
    }

    /// Raw progress 0.0..=1.0 (clamped; pass through an easing fn to shape it).
    pub fn progress(&self) -> f32 {
        let elapsed = self.start.elapsed().as_micros() as f32;
        let total = self.duration.as_micros() as f32;
        (elapsed / total).clamp(0.0, 1.0)
    }

    /// True once the window has fully elapsed.
    pub fn finished(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// Progress of a sub-window starting `delay` in and lasting `length`.
    ///
    /// Returns 0.0 before the sub-window opens and 1.0 after it closes; used
    /// for staggered per-element animations inside one timeline.
    pub fn segment(&self, delay: Duration, length: Duration) -> f32 {
        let elapsed = self.start.elapsed();
        if elapsed <= delay {
            return 0.0;
        }
        let local = (elapsed - delay).as_micros() as f32;
        (local / length.as_micros() as f32).clamp(0.0, 1.0)
    }
}

// Rust guideline compliant 2026-08-21
