//! Conference badge for the Badger 2350: name, email and a scannable QR
//! code, rendered once and left on the (zero-power) e-paper.
//!
//! `cargo run --release --example badge --no-default-features --features badger`
//!
//! All personal data comes from `examples/badge.vcf` (`include_str!`): the
//! QR encodes the vCard verbatim (phones import it as a contact), and the
//! displayed name and email are parsed from its `FN:` and `EMAIL:` lines.
//! Put your own details in that file to personalize — and swap back to the
//! anonymous template before committing:
//!
//! ```text
//! BEGIN:VCARD
//! VERSION:3.0
//! N:Badger;Ada;;;
//! FN:Ada Badger
//! EMAIL:ada@example.com
//! URL:https://github.com/ada
//! X-SOCIALPROFILE;TYPE=instagram:https://instagram.com/ada
//! END:VCARD
//! ```
//!
//! Every `URL:` and `X-SOCIALPROFILE...:` line is listed under the email
//! (scheme stripped), and the QR carries them all — but a longer vCard
//! means a denser QR, so keep it to a few short links if scanning matters.
//!
//! Text uses u8g2 faces (crox5 bold for the name, crox1 for the email)
//! drawn 1:1 and quantized without dither; the QR is painted as raw levels
//! — pixel-sharp black on white, which reflective e-paper makes ideal for
//! scanners.
//!
//! **A** re-renders and refreshes (SLOW, the cleanest waveform). HOME held
//! 2 s reboots to BOOTSEL.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::{bind_interrupts, usb};
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::gfx::dither::{self, Method};
use tufty_2350::gfx::grey::Grey;

/// The vCard: encoded in the QR verbatim, and the source of the displayed
/// name (`FN:`) and email (`EMAIL`).
const VCARD: &str = include_str!("badge.vcf");

/// The values of every line starting with `key` (parameters like
/// `;TYPE=github` are skipped along with the `:`), trimmed and non-empty.
fn vcard_fields<'a>(vcard: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> {
    vcard
        .lines()
        .filter_map(move |l| {
            let l = l.trim();
            (l.len() > key.len() && l[..key.len()].eq_ignore_ascii_case(key))
                .then(|| l.split_once(':').map_or("", |(_, v)| v.trim()))
        })
        .filter(|v| !v.is_empty())
}

/// The first such value; empty if absent.
fn vcard_field<'a>(vcard: &'a str, key: &'a str) -> &'a str {
    vcard_fields(vcard, key).next().unwrap_or("")
}

/// A display-friendly social line: known networks become `LABEL: handle`
/// (`GH: octocat`, `IG: @cat`...), anything else is shown de-schemed.
fn social_display(url: &str) -> heapless::String<48> {
    let bare = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_end_matches('/');
    // (host+path prefix, label, prepend '@')
    const NETWORKS: [(&str, &str, bool); 10] = [
        ("github.com/", "GH: ", false),
        ("linkedin.com/in/", "LI: ", false),
        ("instagram.com/", "IG: ", true),
        ("twitter.com/", "X: ", true),
        ("x.com/", "X: ", true),
        ("tiktok.com/", "TT: ", true),
        ("youtube.com/", "YT: ", true),
        ("bsky.app/profile/", "BSKY: ", false),
        ("facebook.com/", "FB: ", false),
        ("twitch.tv/", "Twitch: ", false),
    ];
    let mut out = heapless::String::new();
    for (prefix, label, at) in NETWORKS {
        if let Some(handle) = bare.strip_prefix(prefix) {
            let handle = handle.trim_start_matches('@');
            let _ = out.push_str(label);
            if at {
                let _ = out.push('@');
            }
            let _ = out.push_str(handle);
            return out;
        }
    }
    let _ = out.push_str(&bare[..bare.len().min(out.capacity())]);
    out
}

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
/// QR working storage, sized for the largest version so any payload fits.
static QR_TEMP: ConstStaticCell<[u8; Version::MAX.buffer_len()]> =
    ConstStaticCell::new([0; Version::MAX.buffer_len()]);
static QR_OUT: ConstStaticCell<[u8; Version::MAX.buffer_len()]> =
    ConstStaticCell::new([0; Version::MAX.buffer_len()]);

const BLACK: Gray8 = Gray8::new(0);

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
    log::info!("badge booting");

    let leds = RearLeds::new(p.PWM_SLICE0, p.PWM_SLICE1, p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3);
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
    let (qr_temp, qr_out) = (QR_TEMP.take(), QR_OUT.take());
    draw_badge(&mut canvas, levels, qr_temp, qr_out);

    epd.set_speed(Speed::Slow);
    epd.present_levels(levels).await;
    log::info!("badge shown; A = refresh");

    loop {
        if let ButtonEvent::Pressed(Button::A) = EVENTS.receive().await {
            draw_badge(&mut canvas, levels, qr_temp, qr_out);
            epd.present_levels(levels).await;
            log::info!("refreshed");
        }
    }
}

/// Lays the badge out into `levels` (`qr_temp`/`qr_out` are the encoder's
/// working storage).
fn draw_badge(canvas: &mut Grey<'_>, levels: &mut [u8], qr_temp: &mut [u8], qr_out: &mut [u8]) {
    let w = WIDTH as i32;
    levels.fill(3);
    canvas.fill(255);

    // ---- QR first: its geometry sets the text column's right edge.
    let qr = QrCode::encode_text(
        VCARD.trim(),
        qr_temp,
        qr_out,
        // Low keeps the longer vCard payload at a scannable module size.
        QrCodeEcc::Low,
        Version::MIN,
        Version::MAX,
        None,
        true,
    )
    .expect("QR payload too long");
    let size = qr.size();
    // No explicit quiet zone: the layout keeps solid white around the code
    // (margins, and the gap below the rule), so every pixel of the area under
    // the rule goes to modules. Integer scale: 3 px/module up to version 5
    // (a ~106-byte vCard); longer payloads drop to 2 and shrink — keep the
    // link list short if scanning matters.
    let avail = HEIGHT as i32 - 56;
    let scale = (avail / size).max(1);
    let px = size * scale;
    let x0 = w - px - 6;
    let qr_x0 = x0;
    let y0 = 56 + (avail - px) / 2;
    // ---- Text, all 1:1 in real faces, quantized without dither.
    let name = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let _ = name.render_aligned(
        vcard_field(VCARD, "FN"),
        Point::new(6, 36),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let small_face = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let mut y = 66;
    // Email, then every URL / X-SOCIALPROFILE line as a short social label,
    // until we run out of column. Lines are truncated to stay clear of the
    // QR (~8 px per character in this face).
    let max_chars = ((qr_x0 - 16) / 8).max(4) as usize;
    let email = vcard_field(VCARD, "EMAIL");
    let links = vcard_fields(VCARD, "URL").chain(vcard_fields(VCARD, "X-SOCIALPROFILE"));
    let mut email_line = heapless::String::<48>::new();
    let _ = email_line.push_str(email);
    for text in core::iter::once(email_line).chain(links.map(social_display)) {
        if y > 144 {
            break;
        }
        let clipped = &text[..text.len().min(max_chars)];
        let _ = small_face.render_aligned(
            clipped,
            Point::new(6, y),
            VerticalPosition::Baseline,
            HorizontalAlignment::Left,
            FontColor::Transparent(BLACK),
            canvas,
        );
        y += 14;
    }
    let small = MonoTextStyle::new(&FONT_6X10, BLACK);
    let _ = Line::new(Point::new(6, 48), Point::new(w - 6, 48))
        .into_styled(PrimitiveStyle::with_stroke(BLACK, 1))
        .draw(canvas);
    let _ = Text::new("Badger 2350", Point::new(6, 156), small).draw(canvas);
    let _ = Text::new("embassy-rs", Point::new(6, 168), small).draw(canvas);
    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Method::Nearest,
        levels,
    );

    // ---- Paint the QR modules last (directly as levels, over the quantized
    // white background): pure black on white, no dither.
    let rect = |x: i32, y: i32, s: i32| Rectangle::new(Point::new(x, y), Size::new(s as u32, s as u32));
    for my in 0..size {
        for mx in 0..size {
            if qr.get_module(mx, my) {
                dither::paint_level(levels, WIDTH, rect(x0 + mx * scale, y0 + my * scale, scale), 0);
            }
        }
    }

    log::info!("qr: version {} ({} modules) at {} px/module", (size - 17) / 4, size, scale);
}

// Rust guideline compliant 2026-08-30
