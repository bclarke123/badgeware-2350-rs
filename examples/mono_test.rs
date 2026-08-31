//! Badger 2040 W bring-up: a 1-bit calibration and test card.
//!
//! The mono cousin of `epd_test` — since a 1-bit panel has no grey levels
//! to calibrate, this card is about the two things that matter here:
//! how each dither method renders a grey ramp at one bit, and how the four
//! UC8151 waveform speeds trade quality for time.
//!
//! Controls: A cycles refresh speed (Default -> Medium -> Fast -> Turbo),
//! B redraws, C inverts the card (ghosting check), UP/DOWN cycle the
//! featured dither method. Reflash via the Pico W's BOOTSEL button.
//!
//! Build: cargo run --release --example mono_test --no-default-features \
//!   --features badger2040w --target thumbv6m-none-eabi

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::usb;
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, PrimitiveStyle, Rectangle};
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd_mono::{Epd, Speed};
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::gfx::dither::{self, Method};
use tufty_2350::gfx::grey::Grey;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);

const BLACK: Gray8 = Gray8::new(0);
const METHODS: [(Method, &str); 4] = [
    (Method::Ordered8, "bayer 8x8"),
    (Method::Ordered4, "bayer 4x4"),
    (Method::FloydSteinberg, "floyd-steinberg"),
    (Method::Nearest, "nearest"),
];

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // On battery the badge only stays powered while EN_3V3 is held high —
    // latch it on first, before anything slow.
    let power_en = Output::new(p.PIN_10, Level::High);
    core::mem::forget(power_en);

    let usb_driver = usb::Driver::new(p.USB, Irqs);
    spawner.spawn(bsp::usb::logger_task(usb_driver).unwrap());
    log::info!("mono_test booting (Badger 2040 W)");

    // Front buttons are active high on this board.
    let buttons = ButtonPins {
        a: Input::new(p.PIN_12, Pull::Down),
        b: Input::new(p.PIN_13, Pull::Down),
        c: Input::new(p.PIN_14, Pull::Down),
        up: Input::new(p.PIN_15, Pull::Down),
        down: Input::new(p.PIN_11, Pull::Down),
    };
    spawner.spawn(bsp::buttons::button_task(buttons).unwrap());

    // The user LED doubles as an "alive" indicator while the panel works.
    let mut led = Output::new(p.PIN_22, Level::High);

    let mut epd = Epd::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_17, p.PIN_20, p.PIN_21, p.PIN_26);
    epd.init().await;

    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());

    let mut method = 0usize;
    let mut inverted = false;
    loop {
        led.set_high();
        draw_card(&mut canvas, levels, METHODS[method], epd.speed(), inverted);
        let t0 = embassy_time::Instant::now();
        epd.present_levels(levels).await;
        log::info!(
            "refresh: {:?} {} ms (est {} ms), dither {}",
            epd.speed(),
            t0.elapsed().as_millis(),
            epd.speed().millis(),
            METHODS[method].1
        );
        led.set_low();

        match EVENTS.receive().await {
            ButtonEvent::Pressed(Button::A) => {
                let next = match epd.speed() {
                    Speed::Default => Speed::Medium,
                    Speed::Medium => Speed::Fast,
                    Speed::Fast => Speed::Turbo,
                    Speed::Turbo => Speed::Default,
                };
                epd.set_speed(next).await;
            }
            ButtonEvent::Pressed(Button::B) => {}
            ButtonEvent::Pressed(Button::C) => inverted = !inverted,
            ButtonEvent::Pressed(Button::Up) => method = (method + 3) % 4,
            ButtonEvent::Pressed(Button::Down) => method = (method + 1) % 4,
            _ => continue,
        }
    }
}

/// The test card: title, a grey ramp under the featured dither, pattern
/// swatches, and a footer naming the current speed and method.
fn draw_card(canvas: &mut Grey<'_>, levels: &mut [u8], method: (Method, &str), speed: Speed, inverted: bool) {
    canvas.fill(if inverted { 0 } else { 255 });
    let ink = if inverted { Gray8::new(255) } else { BLACK };
    let title = FontRenderer::new::<fonts::u8g2_font_crox2hb_tf>();
    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();

    let _ = title.render_aligned(
        "Badger 2040 W",
        Point::new(4, 14),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(ink),
        canvas,
    );

    // Grey ramp, 0..255 left to right, to be dithered by the featured method.
    let ramp = Rectangle::new(Point::new(4, 22), Size::new(WIDTH as u32 - 8, 40));
    for x in 0..ramp.size.width as usize {
        let luma = (x * 255 / (ramp.size.width as usize - 1)) as u8;
        let _ = Line::new(
            Point::new(ramp.top_left.x + x as i32, ramp.top_left.y),
            Point::new(ramp.top_left.x + x as i32, ramp.top_left.y + ramp.size.height as i32 - 1),
        )
        .into_styled(PrimitiveStyle::with_stroke(Gray8::new(luma), 1))
        .draw(canvas);
    }

    // Pattern swatches: hairlines, checker, discs — 1:1 detail the dither
    // must not eat.
    let y0 = 70;
    for i in 0..8 {
        let _ = Line::new(Point::new(4 + i * 8, y0), Point::new(4 + i * 8 + 4, y0 + 24))
            .into_styled(PrimitiveStyle::with_stroke(ink, 1))
            .draw(canvas);
    }
    for cy in 0..3 {
        for cx in 0..6 {
            if (cx + cy) % 2 == 0 {
                let _ = Rectangle::new(Point::new(80 + cx * 8, y0 + cy * 8), Size::new(8, 8))
                    .into_styled(PrimitiveStyle::with_fill(ink))
                    .draw(canvas);
            }
        }
    }
    for (i, d) in [24u32, 16, 10, 6].iter().enumerate() {
        let _ = Circle::new(Point::new(150 + i as i32 * 30, y0), *d)
            .into_styled(PrimitiveStyle::with_stroke(ink, 1))
            .draw(canvas);
    }

    // Footer.
    let mut foot = heapless::String::<64>::new();
    let _ = write!(foot, "{:?} ~{} ms - {}", speed, speed.millis(), method.1);
    let _ = small.render_aligned(
        foot.as_str(),
        Point::new(4, HEIGHT as i32 - 4),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(ink),
        canvas,
    );

    // Quantize: ramp under the featured method, everything else nearest
    // (text and patterns are already 1-bit-crisp).
    dither::quantize_mono(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Method::Nearest,
        levels,
    );
    dither::quantize_mono(canvas, ramp, method.0, levels);
}

// Rust guideline compliant 2026-08-31
