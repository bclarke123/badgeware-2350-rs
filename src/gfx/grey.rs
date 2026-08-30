//! An 8-bit greyscale canvas: the intermediate between whatever draws
//! (`embedded-graphics` text and shapes, or a converted RGB565 frame) and the
//! e-paper's four levels. Row-major, one byte per pixel, values are sRGB
//! luma (what `Gray8` means in `embedded-graphics`), so drawing code can
//! think in ordinary 0..=255 greys; [`crate::gfx::dither`] handles the
//! linear-light conversion when it quantizes.
//!
//! The canvas is slice-backed so a caller can keep one at the panel's size
//! and one at 2x for supersampled anti-aliasing ([`Grey::downsample_into`]).

use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

/// A row-major 8-bit grey image over borrowed storage.
pub struct Grey<'a> {
    width: usize,
    height: usize,
    buf: &'a mut [u8],
}

impl<'a> Grey<'a> {
    /// Wraps `buf`, which must hold at least `width * height` bytes.
    pub fn new(width: usize, height: usize, buf: &'a mut [u8]) -> Self {
        assert!(buf.len() >= width * height, "grey canvas storage too small");
        Self { width, height, buf }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The pixel bytes, row-major.
    pub fn pixels(&self) -> &[u8] {
        &self.buf[..self.width * self.height]
    }

    /// Fills the whole canvas with one grey.
    pub fn fill(&mut self, luma: u8) {
        self.buf[..self.width * self.height].fill(luma);
    }

    /// The grey at `(x, y)` (0 outside the canvas).
    #[inline]
    pub fn get(&self, x: usize, y: usize) -> u8 {
        if x < self.width && y < self.height {
            self.buf[y * self.width + x]
        } else {
            0
        }
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, luma: u8) {
        if x < self.width && y < self.height {
            self.buf[y * self.width + x] = luma;
        }
    }

    /// Box-filters this canvas 2:1 into `dst` (which must be half the size
    /// in each dimension): the supersampling resolve. Averaging four sRGB
    /// values is a slight approximation of a linear-light average; for
    /// anti-aliasing edges at these sizes it is indistinguishable.
    pub fn downsample_into(&self, dst: &mut Grey<'_>) {
        debug_assert_eq!(dst.width * 2, self.width);
        debug_assert_eq!(dst.height * 2, self.height);
        for y in 0..dst.height {
            let r0 = &self.buf[(2 * y) * self.width..(2 * y + 1) * self.width];
            let r1 = &self.buf[(2 * y + 1) * self.width..(2 * y + 2) * self.width];
            let out = &mut dst.buf[y * dst.width..(y + 1) * dst.width];
            for (x, o) in out.iter_mut().enumerate() {
                let sum = u32::from(r0[2 * x])
                    + u32::from(r0[2 * x + 1])
                    + u32::from(r1[2 * x])
                    + u32::from(r1[2 * x + 1]);
                *o = ((sum + 2) / 4) as u8;
            }
        }
    }

    /// Converts a big-endian RGB565 column-major framebuffer of the same
    /// dimensions (see [`crate::gfx::FrameBuffer`]) into luma.
    pub fn from_rgb565_columns(&mut self, frame: &[u8]) {
        for x in 0..self.width {
            let col = &frame[x * self.height * 2..(x + 1) * self.height * 2];
            for (y, px) in col.as_chunks::<2>().0.iter().enumerate() {
                let c = u16::from(px[0]) << 8 | u16::from(px[1]);
                let r = u32::from(c >> 11) & 31;
                let g = u32::from(c >> 5) & 63;
                let b = u32::from(c) & 31;
                // Rec.601 luma on channels expanded to 8 bits.
                let lum = (r * 8 * 77 + g * 4 * 151 + b * 8 * 28) >> 8;
                self.buf[y * self.width + x] = lum.min(255) as u8;
            }
        }
    }
}

impl OriginDimensions for Grey<'_> {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for Grey<'_> {
    type Color = Gray8;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 {
                self.set(p.x as usize, p.y as usize, c.luma());
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }
        let (x0, y0) = (area.top_left.x as usize, area.top_left.y as usize);
        for y in y0..y0 + area.size.height as usize {
            self.buf[y * self.width + x0..y * self.width + x0 + area.size.width as usize].fill(color.luma());
        }
        Ok(())
    }
}

// Rust guideline compliant 2026-08-30
