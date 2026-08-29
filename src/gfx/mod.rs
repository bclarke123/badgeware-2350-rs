//! Full-screen RGB565 framebuffer with an [`embedded_graphics`] `DrawTarget`.
//!
//! The buffer stores pixels as big-endian byte pairs in **panel scan order**
//! (column-major: the panel physically refreshes as 240x320 portrait, and the
//! driver streams frames in that same order so a vsync'd write never crosses
//! the refresh beam — this is what prevents tearing). Drawing is still plain
//! landscape (x right, y down); only the index math differs, paid once per
//! pixel write instead of with a whole-frame rotation pass at present time.
//! 320x240x2 = 150 KiB, held in `.bss`.

use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

use crate::bsp::display::{HEIGHT, WIDTH};

/// Size of the framebuffer in bytes.
pub const FB_BYTES: usize = WIDTH * HEIGHT * 2;

/// A drawable, DMA-ready full-screen frame.
pub struct FrameBuffer {
    buf: &'static mut [u8; FB_BYTES],
}

impl FrameBuffer {
    /// Wraps the statically allocated pixel storage.
    pub fn new(buf: &'static mut [u8; FB_BYTES]) -> Self {
        Self { buf }
    }

    /// The raw bytes to hand to [`crate::bsp::display::Display::present`].
    pub fn bytes(&self) -> &[u8] {
        self.buf
    }

    /// Splits the frame into the columns left of `x` and the rest.
    ///
    /// Column-major storage makes each part one contiguous slice, which is
    /// what lets the two cores rasterize disjoint parts with no aliasing.
    pub fn split_at_column(&mut self, x: usize) -> (&mut [u8], &mut [u8]) {
        self.buf.split_at_mut(x * HEIGHT * 2)
    }

    /// Start of the pixel storage (for the presentation DMA).
    pub fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }

    #[inline]
    fn set_pixel(&mut self, x: usize, y: usize, color: Rgb565) {
        let raw = RawU16::from(color).into_inner();
        // Panel scan order: landscape column x is a contiguous 240-pixel run.
        let idx = (x * HEIGHT + y) * 2;
        self.buf[idx] = (raw >> 8) as u8;
        self.buf[idx + 1] = raw as u8;
    }
}

impl OriginDimensions for FrameBuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for FrameBuffer {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if (0..WIDTH as i32).contains(&point.x) && (0..HEIGHT as i32).contains(&point.y) {
                self.set_pixel(point.x as usize, point.y as usize, color);
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let Some(area) = area.intersection(&self.bounding_box()).size_nonzero() else {
            return Ok(());
        };
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = raw as u8;
        let x0 = area.top_left.x as usize;
        let y0 = area.top_left.y as usize;
        // Column-major storage: each landscape column of the rect is one
        // contiguous run, so this stays a tight sequential fill.
        for x in x0..x0 + area.size.width as usize {
            let col = (x * HEIGHT + y0) * 2;
            let col = &mut self.buf[col..col + area.size.height as usize * 2];
            for pair in col.chunks_exact_mut(2) {
                pair[0] = hi;
                pair[1] = lo;
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = raw as u8;
        for pair in self.buf.chunks_exact_mut(2) {
            pair[0] = hi;
            pair[1] = lo;
        }
        Ok(())
    }
}

/// Extension trait: `Rectangle::size_nonzero` does not exist upstream; this
/// filters out degenerate rectangles after clipping.
trait NonZeroExt {
    fn size_nonzero(self) -> Option<Rectangle>;
}

impl NonZeroExt for Rectangle {
    fn size_nonzero(self) -> Option<Rectangle> {
        if self.size.width == 0 || self.size.height == 0 {
            None
        } else {
            Some(self)
        }
    }
}

// Rust guideline compliant 2026-08-29
