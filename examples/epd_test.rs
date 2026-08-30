//! E-paper test card for the Badger 2350: everything needed to judge the
//! four-grey pipeline on the real panel in one refresh.
//!
//! `cargo run --release --example epd_test --no-default-features --features badger`
//!
//! Rows, top to bottom:
//! 1. The four flat panel levels (raw, no dither) and a reflectance-linear
//!    ramp — the calibration reference for `gfx::dither::PANEL`.
//! 2. The same ramp quantized three ways: Bayer 4x4, Bayer 8x8,
//!    Floyd–Steinberg.
//! 3. Text, hairlines and a circle: drawn 1:1 in pure levels (left) versus
//!    drawn at 2x, box-filtered down and dithered (right).
//! 4. A shaded sphere "photo": Bayer 8x8 (left) versus Floyd–Steinberg (right).
//!
//! Buttons: **A** re-refreshes at TURBO (1.1 s), **B** at SLOW (3.7 s), so
//! waveform quality can be compared on the same image. **UP/DOWN** nudge the
//! selected panel level's calibrated reflectance and **C** switches between
//! level 1 and level 2 (each change re-quantizes and refreshes; the values
//! are logged — bake the good ones into `gfx::dither`). HOME held 2 s
//! reboots to BOOTSEL as always.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::{bind_interrupts, usb};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10, FONT_9X15_BOLD};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;
use static_cell::ConstStaticCell;

use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp;
use tufty_2350::gfx::dither::{self, Method, Panel};
use tufty_2350::gfx::grey::Grey;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// 1x grey canvas, 2x supersampling canvas, and the quantized levels.
static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static CANVAS2: ConstStaticCell<[u8; WIDTH * HEIGHT * 4]> = ConstStaticCell::new([0; WIDTH * HEIGHT * 4]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);

const BLACK: Gray8 = Gray8::new(0);
const WHITE: Gray8 = Gray8::new(255);

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut p = embassy_rp::init(Default::default());
    let power_en = Output::new(p.PIN_27, Level::High);
    core::mem::forget(power_en);
    bsp::power::sleep_if_reset_held(
        p.PIN_14.reborrow(),
        p.PIN_0.reborrow(),
        p.PIN_1.reborrow(),
        p.PIN_2.reborrow(),
        p.PIN_3.reborrow(),
    )
    .await;

    let usb_driver = usb::Driver::new(p.USB, Irqs);
    spawner.spawn(bsp::usb::logger_task(usb_driver).unwrap());
    log::info!("epd_test booting");

    let leds = RearLeds::new(p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3);
    spawner.spawn(bsp::leds::led_task(leds).unwrap());
    let buttons = ButtonPins {
        a: Input::new(p.PIN_7, Pull::Up),
        b: Input::new(p.PIN_9, Pull::Up),
        c: Input::new(p.PIN_10, Pull::Up),
        up: Input::new(p.PIN_11, Pull::Up),
        down: Input::new(p.PIN_6, Pull::Up),
        home: Input::new(p.PIN_22, Pull::Up),
    };
    spawner.spawn(bsp::buttons::button_task(buttons).unwrap());

    let mut epd = Epd::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_17, p.PIN_20, p.PIN_21, p.PIN_16);
    epd.init().await;

    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());
    let mut canvas2 = Grey::new(WIDTH * 2, HEIGHT * 2, CANVAS2.take());
    draw_card(&mut canvas, &mut canvas2, levels);

    epd.set_speed(Speed::Slow);
    epd.present_levels(levels).await;
    log::info!("test card shown (SLOW); A/B = TURBO/SLOW refresh, C = select level, UP/DOWN = adjust");

    // Calibration state: which panel level UP/DOWN adjusts.
    let mut selected = 1usize;
    const STEP: u16 = 80; // ~2% of full scale

    loop {
        let ButtonEvent::Pressed(button) = EVENTS.receive().await else {
            continue;
        };
        let speed = match button {
            Button::A => Speed::Turbo,
            Button::B => Speed::Slow,
            Button::C => {
                selected = if selected == 1 { 2 } else { 1 };
                log::info!("adjusting level {}", selected);
                continue;
            }
            Button::Up | Button::Down => {
                let p = Panel::current();
                let cur = p.0[selected];
                let next = if button == Button::Up { cur.saturating_add(STEP) } else { cur.saturating_sub(STEP) };
                Panel::set_level(selected, next);
                let p = Panel::current();
                log::info!(
                    "panel = [{}, {}, {}, {}]  ({:.1}% / {:.1}%)",
                    p.0[0],
                    p.0[1],
                    p.0[2],
                    p.0[3],
                    f32::from(p.0[1]) * 100.0 / 4095.0,
                    f32::from(p.0[2]) * 100.0 / 4095.0
                );
                draw_card(&mut canvas, &mut canvas2, levels);
                Speed::Turbo
            }
            Button::Home => continue,
        };
        epd.set_speed(speed);
        let t = embassy_time::Instant::now();
        epd.present_levels(levels).await;
        log::info!("refresh {:?}: {} ms", speed, t.elapsed().as_millis());
    }
}

/// Lays out the card into `levels`.
fn draw_card(canvas: &mut Grey<'_>, canvas2: &mut Grey<'_>, levels: &mut [u8]) {
    let w = WIDTH as i32;
    let rect = |x: i32, y: i32, wd: i32, ht: i32| Rectangle::new(Point::new(x, y), Size::new(wd as u32, ht as u32));
    canvas.fill(255);
    levels.fill(3);

    // ---- Row 1 (y 0..30): flat panel levels, then a reflectance-linear ramp.
    for level in 0..4u8 {
        dither::paint_level(levels, WIDTH, rect(i32::from(level) * 40, 0, 40, 30), level);
    }
    ramp(canvas, rect(160, 0, w - 160, 30));
    dither::quantize(canvas, rect(160, 0, w - 160, 30), Method::Ordered8, levels);

    // ---- Row 2 (y 30..60): the ramp three ways.
    let third = w / 3;
    ramp(canvas, rect(0, 30, w, 30));
    dither::quantize(canvas, rect(0, 30, third, 30), Method::Ordered4, levels);
    dither::quantize(canvas, rect(third, 30, third, 30), Method::Ordered8, levels);
    dither::quantize(canvas, rect(2 * third, 30, w - 2 * third, 30), Method::FloydSteinberg, levels);

    // ---- Row 3 (y 60..116): 1:1 pure (left) vs 2x supersampled (right).
    let half = w / 2;
    let _ = canvas.fill_solid(&rect(0, 60, w, 56), WHITE);
    draw_linework(canvas, Point::new(4, 60), 1);
    dither::quantize(canvas, rect(0, 60, half, 56), Method::Nearest, levels);

    canvas2.fill(255);
    draw_linework(canvas2, Point::new(2 * (half + 4), 2 * 60), 2);
    canvas2.downsample_into(canvas);
    dither::quantize(canvas, rect(half, 60, w - half, 56), Method::Ordered8, levels);

    // ---- Row 4 (y 116..176): shaded sphere, Bayer 8x8 vs Floyd–Steinberg.
    let row = rect(0, 116, w, HEIGHT as i32 - 116);
    let _ = canvas.fill_solid(&row, Gray8::new(200));
    sphere(canvas, Point::new(half / 2, 146), 26);
    sphere(canvas, Point::new(half + half / 2, 146), 26);
    dither::quantize(canvas, rect(0, 116, half, 60), Method::Ordered8, levels);
    dither::quantize(canvas, rect(half, 116, w - half, 60), Method::FloydSteinberg, levels);
}

/// A horizontal ramp that is linear in *reflectance* (so it should read as
/// evenly stepped once `PANEL` is calibrated): sRGB value = 255 * sqrt(t)
/// under the gamma-2 model in `dither`.
fn ramp(canvas: &mut Grey<'_>, area: Rectangle) {
    let (x0, y0) = (area.top_left.x as usize, area.top_left.y as usize);
    let (wd, ht) = (area.size.width as usize, area.size.height as usize);
    for x in 0..wd {
        let t = x as f32 / (wd - 1) as f32;
        let v = (255.0 * sqrt(t)) as u8;
        for y in 0..ht {
            canvas.set(x0 + x, y0 + y, v);
        }
    }
}

/// Text at three sizes, hairlines and a circle, at `scale` (1 or 2).
fn draw_linework(canvas: &mut Grey<'_>, origin: Point, scale: i32) {
    let s = scale;
    let big = MonoTextStyle::new(&FONT_10X20, BLACK);
    let bold = MonoTextStyle::new(&FONT_9X15_BOLD, BLACK);
    let small = MonoTextStyle::new(&FONT_6X10, BLACK);
    // Mono fonts do not scale, so draw each glyph pixel as an s x s block.
    let mut draw_text = |text: &str, style: MonoTextStyle<'_, Gray8>, at: Point| {
        let at = origin + at * s;
        let mut sink = Scaled { canvas, origin: at, scale: s };
        let _ = Text::new(text, at, style).draw(&mut sink);
    };
    draw_text("Badger 2350", big, Point::new(0, 16));
    draw_text("embassy / eink", bold, Point::new(0, 32));
    draw_text("quick brown fox 0123", small, Point::new(0, 44));
    let stroke = PrimitiveStyle::with_stroke(BLACK, s as u32);
    let _ = Line::new(origin + Point::new(0, 50) * s, origin + Point::new(60, 52) * s).into_styled(stroke).draw(canvas);
    let _ = Line::new(origin + Point::new(0, 52) * s, origin + Point::new(60, 46) * s).into_styled(stroke).draw(canvas);
    let _ = Circle::new(origin + Point::new(96, 34) * s, 20 * s as u32).into_styled(stroke).draw(canvas);
    let _ = Circle::new(origin + Point::new(112, 40) * s, 12 * s as u32)
        .into_styled(PrimitiveStyle::with_fill(Gray8::new(128)))
        .draw(canvas);
}

/// Draws into a canvas with every pixel expanded to a `scale` x `scale`
/// block relative to `origin`, so unscalable mono fonts can be
/// supersampled: `Text` positions glyph pixels at `origin + offset`, and
/// this maps them to `origin + offset * scale`.
struct Scaled<'c, 'a> {
    canvas: &'c mut Grey<'a>,
    origin: Point,
    scale: i32,
}

impl OriginDimensions for Scaled<'_, '_> {
    fn size(&self) -> Size {
        self.canvas.size()
    }
}

impl DrawTarget for Scaled<'_, '_> {
    type Color = Gray8;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let s = self.scale;
        for Pixel(p, c) in pixels {
            let rel = p - self.origin;
            let base = self.origin + rel * s;
            for dy in 0..s {
                for dx in 0..s {
                    let (x, y) = (base.x + dx, base.y + dy);
                    if x >= 0 && y >= 0 {
                        self.canvas.set(x as usize, y as usize, c.luma());
                    }
                }
            }
        }
        Ok(())
    }
}

/// A lambert-shaded sphere with a soft shadow, mid-grey highlight to
/// black terminator.
fn sphere(canvas: &mut Grey<'_>, center: Point, r: i32) {
    let (cx, cy) = (center.x, center.y);
    // Shadow ellipse.
    for y in -8..=8 {
        for x in -(r + 6)..=(r + 6) {
            let fx = x as f32 / (r + 6) as f32;
            let fy = y as f32 / 8.0;
            let d = fx * fx + fy * fy;
            if d < 1.0 {
                let px = (cx + x + 6) as usize;
                let py = (cy + r + 2 + y) as usize;
                let v = canvas.get(px, py) as f32 * (0.45 + 0.55 * d);
                canvas.set(px, py, v as u8);
            }
        }
    }
    for y in -r..=r {
        for x in -r..=r {
            let fx = x as f32 / r as f32;
            let fy = y as f32 / r as f32;
            let d = fx * fx + fy * fy;
            if d <= 1.0 {
                let nz = sqrt(1.0 - d);
                // Light from upper-left.
                let l = (-fx * 0.5 - fy * 0.6 + nz * 0.62).max(0.0);
                let spec = (l * l * l * l * l * l) * 0.5;
                let lin = 0.04 + 0.7 * l + spec;
                let v = (255.0 * sqrt(lin.min(1.0))) as u8;
                canvas.set((cx + x) as usize, (cy + y) as usize, v);
            }
        }
    }
}

fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    // Newton from a bit-hack seed; plenty for shading.
    let mut y = f32::from_bits(0x1fbd_1df5 + (x.to_bits() >> 1));
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

// Rust guideline compliant 2026-08-30
