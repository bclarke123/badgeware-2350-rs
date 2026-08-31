//! Quantizing 8-bit grey to the e-paper's four levels.
//!
//! Everything happens in linear light: the panel's greys are reflectances,
//! not sRGB steps, so a canvas value is first linearized and then placed
//! between the two nearest *measured* panel levels ([`PANEL`]). Getting that
//! table right is what makes a ramp look evenly spaced and a mid-grey land
//! on the step it should.
//!
//! Two families of dither:
//! * **Ordered** (Bayer 4x4 / 8x8): a pixel's output depends only on its own
//!   value and position, so it is stable frame to frame and under partial
//!   refreshes, and exact levels (text, flat fills) come out unpatterned.
//!   The e-paper default.
//! * **Floyd–Steinberg** error diffusion: smoother gradients and photo-like
//!   tone, but non-local — a one-pixel change ripples across the row — so
//!   only for still images that are refreshed whole.

use embedded_graphics::primitives::Rectangle;
use portable_atomic::{AtomicU16, Ordering};

use super::grey::Grey;

/// Number of panel greys.
pub const LEVELS: usize = 4;

/// Linear reflectance of each panel level, 0..=4095 (level 0 = black),
/// adjustable at runtime (the `epd_test` card calibrates by eye). The
/// Badger's level 1 is far darker than a nominal third — with even spacing
/// everything below mid-grey dithered toward black. These working values
/// (~10% / ~51%, judged on hardware 2026-08-30) fix that; the TURBO and SLOW
/// waveforms place the mid greys slightly differently, so fine calibration
/// is per waveform and was left at this.
static PANEL: [AtomicU16; LEVELS] =
    [AtomicU16::new(0), AtomicU16::new(400), AtomicU16::new(2100), AtomicU16::new(4095)];

/// A snapshot of the panel calibration, taken once per quantize pass.
#[derive(Debug, Clone, Copy)]
pub struct Panel(pub [u16; LEVELS]);

impl Panel {
    /// The current calibration.
    pub fn current() -> Self {
        Self([0, 1, 2, 3].map(|i| PANEL[i].load(Ordering::Relaxed)))
    }

    /// Sets level `i`'s linear reflectance (clamped between its neighbours).
    pub fn set_level(i: usize, value: u16) {
        if (1..LEVELS - 1).contains(&i) {
            let lo = PANEL[i - 1].load(Ordering::Relaxed) + 1;
            let hi = PANEL[i + 1].load(Ordering::Relaxed) - 1;
            PANEL[i].store(value.clamp(lo, hi), Ordering::Relaxed);
        }
    }
}

/// Widest canvas the diffusion path supports (error rows are stack arrays).
const MAX_WIDTH: usize = 528;

/// How to quantize a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Nearest panel level, no dither (for flat fills and text you want
    /// pixel-sharp).
    Nearest,
    /// Bayer 4x4 ordered dither.
    Ordered4,
    /// Bayer 8x8 ordered dither (finer, less visible pattern).
    Ordered8,
    /// Floyd–Steinberg error diffusion.
    FloydSteinberg,
}

const BAYER4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
const BAYER8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// sRGB luma to linear light, 0..=4095. Gamma 2.0 is within a few percent
/// of the sRGB curve over the range that matters here and needs no `powf`.
#[inline]
pub fn to_linear(luma: u8) -> u16 {
    let v = u32::from(luma);
    (v * v * 4095 / (255 * 255)) as u16
}

/// The panel level below `lin` and the fraction of the way to the next one
/// (0..=4095), clamped at the top level.
#[inline]
fn bracket(panel: &Panel, lin: u16) -> (u8, u32) {
    let p = &panel.0;
    let mut i = 0;
    while i + 2 < LEVELS && lin >= p[i + 1] {
        i += 1;
    }
    let (lo, hi) = (u32::from(p[i]), u32::from(p[i + 1]));
    let f = (u32::from(lin).clamp(lo, hi) - lo) * 4095 / (hi - lo);
    (i as u8, f)
}

/// Ordered-dither one pixel: `threshold` in 0..=4095.
#[inline]
pub fn ordered_level(panel: &Panel, luma: u8, threshold: u32) -> u8 {
    let (i, f) = bracket(panel, to_linear(luma));
    i + u8::from(f > threshold)
}

/// Bayer 8x8 threshold for a pixel position, 0..=4095.
#[inline]
pub fn bayer8_threshold(x: usize, y: usize) -> u32 {
    (u32::from(BAYER8[y & 7][x & 7]) * 64 + 32) - 1
}

/// Quantizes `rect` of `src` into `levels` (row-major, same dimensions as
/// `src`, values 0..LEVELS) with `method`. Regions outside `rect` are left
/// untouched, so one canvas can mix methods.
pub fn quantize(src: &Grey<'_>, rect: Rectangle, method: Method, levels: &mut [u8]) {
    let w = src.width();
    let area = rect.intersection(&Rectangle::new(
        embedded_graphics::prelude::Point::zero(),
        embedded_graphics::prelude::Size::new(w as u32, src.height() as u32),
    ));
    if area.size.width == 0 || area.size.height == 0 {
        return;
    }
    let (x0, y0) = (area.top_left.x as usize, area.top_left.y as usize);
    let (x1, y1) = (x0 + area.size.width as usize, y0 + area.size.height as usize);
    let panel = Panel::current();

    match method {
        Method::Nearest => {
            for y in y0..y1 {
                for x in x0..x1 {
                    levels[y * w + x] = ordered_level(&panel, src.get(x, y), 2047);
                }
            }
        }
        Method::Ordered4 => {
            for y in y0..y1 {
                for x in x0..x1 {
                    let t = u32::from(BAYER4[y & 3][x & 3]) * 256 + 128 - 1;
                    levels[y * w + x] = ordered_level(&panel, src.get(x, y), t);
                }
            }
        }
        Method::Ordered8 => {
            for y in y0..y1 {
                for x in x0..x1 {
                    levels[y * w + x] = ordered_level(&panel, src.get(x, y), bayer8_threshold(x, y));
                }
            }
        }
        Method::FloydSteinberg => {
            assert!(w <= MAX_WIDTH, "canvas too wide for the diffusion buffers");
            // Two rows of error (this row, next row), in linear units, with a
            // one-pixel guard on each side.
            let mut err = [[0i32; MAX_WIDTH + 2]; 2];
            for y in y0..y1 {
                let (cur, next) = {
                    let (a, b) = err.split_at_mut(1);
                    (&mut a[0], &mut b[0])
                };
                next.fill(0);
                for x in x0..x1 {
                    let lin = i32::from(to_linear(src.get(x, y))) + cur[x + 1];
                    let target = lin.clamp(0, 4095) as u16;
                    // Nearest panel level in linear light.
                    let (i, f) = bracket(&panel, target);
                    let level = i + u8::from(f > 2047);
                    let e = lin - i32::from(panel.0[usize::from(level)]);
                    levels[y * w + x] = level;
                    cur[x + 2] += e * 7 / 16;
                    next[x] += e * 3 / 16;
                    next[x + 1] += e * 5 / 16;
                    next[x + 2] += e / 16;
                }
                err.swap(0, 1);
            }
        }
    }
}

/// Quantizes `rect` of `src` to two levels (0 black, 1 white) for 1-bit
/// panels, in linear light. The four-grey [`quantize`] leans on the panel
/// calibration; mono has no calibration to lean on, just a 50% threshold.
pub fn quantize_mono(src: &Grey<'_>, rect: Rectangle, method: Method, levels: &mut [u8]) {
    let w = src.width();
    let area = rect.intersection(&Rectangle::new(
        embedded_graphics::prelude::Point::zero(),
        embedded_graphics::prelude::Size::new(w as u32, src.height() as u32),
    ));
    if area.size.width == 0 || area.size.height == 0 {
        return;
    }
    let (x0, y0) = (area.top_left.x as usize, area.top_left.y as usize);
    let (x1, y1) = (x0 + area.size.width as usize, y0 + area.size.height as usize);

    match method {
        Method::Nearest => {
            for y in y0..y1 {
                for x in x0..x1 {
                    levels[y * w + x] = u8::from(to_linear(src.get(x, y)) > 2047);
                }
            }
        }
        Method::Ordered4 => {
            for y in y0..y1 {
                for x in x0..x1 {
                    let t = u32::from(BAYER4[y & 3][x & 3]) * 256 + 128 - 1;
                    levels[y * w + x] = u8::from(u32::from(to_linear(src.get(x, y))) > t);
                }
            }
        }
        Method::Ordered8 => {
            for y in y0..y1 {
                for x in x0..x1 {
                    let t = bayer8_threshold(x, y);
                    levels[y * w + x] = u8::from(u32::from(to_linear(src.get(x, y))) > t);
                }
            }
        }
        Method::FloydSteinberg => {
            assert!(w <= MAX_WIDTH, "canvas too wide for the diffusion buffers");
            let mut err = [[0i32; MAX_WIDTH + 2]; 2];
            for y in y0..y1 {
                let (cur, next) = {
                    let (a, b) = err.split_at_mut(1);
                    (&mut a[0], &mut b[0])
                };
                next.fill(0);
                for x in x0..x1 {
                    let lin = i32::from(to_linear(src.get(x, y))) + cur[x + 1];
                    let level = u8::from(lin > 2047);
                    let e = lin - if level == 1 { 4095 } else { 0 };
                    levels[y * w + x] = level;
                    cur[x + 2] += e * 7 / 16;
                    next[x] += e * 3 / 16;
                    next[x + 1] += e * 5 / 16;
                    next[x + 2] += e / 16;
                }
                err.swap(0, 1);
            }
        }
    }
}

/// Writes one level directly into a rect of `levels` (for calibration
/// swatches and anything that must bypass quantization).
pub fn paint_level(levels: &mut [u8], width: usize, rect: Rectangle, level: u8) {
    let (x0, y0) = (rect.top_left.x.max(0) as usize, rect.top_left.y.max(0) as usize);
    for y in y0..y0 + rect.size.height as usize {
        let row = &mut levels[y * width..(y + 1) * width];
        let end = (x0 + rect.size.width as usize).min(width);
        row[x0.min(end)..end].fill(level);
    }
}

// Rust guideline compliant 2026-08-30
