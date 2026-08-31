//! Small reusable status widgets drawn onto the grey canvas.

use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, Triangle};

use super::grey::Grey;

/// Footprint of [`draw_battery`] (width, height) in pixels.
pub const BATTERY_SIZE: (u32, u32) = (24, 11);

/// A battery gauge at `top_left`: a 22x11 outline with a terminal nub and a
/// proportional fill; on USB power a lightning bolt is knocked out of the
/// fill instead. Crisp at 1:1 — quantize its region without dither.
pub fn draw_battery(canvas: &mut Grey<'_>, top_left: Point, percent: u8, on_usb: bool) {
    let black = Gray8::new(0);
    let white = Gray8::new(255);
    let body = Rectangle::new(top_left, Size::new(21, 11));
    let _ = body.into_styled(PrimitiveStyle::with_stroke(black, 1)).draw(canvas);
    // Terminal nub.
    let _ = Rectangle::new(top_left + Point::new(21, 3), Size::new(3, 5))
        .into_styled(PrimitiveStyle::with_fill(black))
        .draw(canvas);
    // Fill: 0..=17 inner pixels.
    let fill = (u32::from(percent.min(100)) * 17).div_ceil(100);
    let _ = Rectangle::new(top_left + Point::new(2, 2), Size::new(fill, 7))
        .into_styled(PrimitiveStyle::with_fill(black))
        .draw(canvas);
    if on_usb {
        // Bolt: two triangles, white where they cross the fill, black
        // outside it, drawn white-over-fill for simplicity (reads as a bolt
        // either way at this size).
        let b = top_left + Point::new(10, 1);
        let _ = Triangle::new(b + Point::new(1, 0), b + Point::new(-3, 5), b + Point::new(1, 5))
            .into_styled(PrimitiveStyle::with_fill(white))
            .draw(canvas);
        let _ = Triangle::new(b + Point::new(0, 4), b + Point::new(4, 4), b + Point::new(0, 9))
            .into_styled(PrimitiveStyle::with_fill(white))
            .draw(canvas);
    }
}

// Rust guideline compliant 2026-08-30
