//! TOTP authenticator for the Badger 2350: six digits, big, on zero-power
//! e-paper, refreshed every 30-second window.
//!
//! `cargo run --release --example totp --no-default-features --features badger`
//!
//! Time lives in the battery-backed PCF85063A RTC and the secret in the last
//! flash sector, so both survive reflashing and power-off. If the RTC has
//! lost power (first boot, drained battery) or no secret is stored, the
//! badge shows a setup screen and listens on its USB serial port
//! (`screen /dev/cu.usbmodem* 115200`) for two lines:
//!
//! ```text
//! TIME 2026-08-30T21:04:05     (current UTC)
//! SECRET JBSWY3DPEHPK3PXP      (base32, from the QR/setup key)
//! ```
//!
//! **A** forces a clean SLOW refresh; **B** re-enters setup. HOME held 2 s
//! reboots to BOOTSEL.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::{bind_interrupts, usb};
use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
#[cfg(feature = "badger")]
use tufty_2350::bsp::epd::{Epd, Speed};
#[cfg(feature = "badger2040w")]
use tufty_2350::bsp::epd_mono::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::rtc::{DateTime, RtcRam};
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp::settings::{Settings, MAX_VAL};
use tufty_2350::gfx::dither::{self, Method};
use tufty_2350::gfx::grey::Grey;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);

const BLACK: Gray8 = Gray8::new(0);

/// Full clean refresh every this many code windows (ghosting hygiene).
const FULL_EVERY: u32 = 20;

/// The countdown bar: a vertical strip on the right edge, draining downward.
/// Vertical because the panel's partial-refresh axis is our horizontal one —
/// this whole strip re-scans alone in a fraction of a TURBO refresh.
#[cfg(feature = "badger")]
mod layout {
    pub const BAR_X0: i32 = 246;
    pub const BAR_X1: i32 = 258;
    pub const BAR_Y0: i32 = 16;
    pub const BAR_Y1: i32 = 160;
    pub const DIGITS_Y: i32 = 110;
    pub const CLOCK_Y: i32 = 30;
    pub const SETUP_TITLE_Y: i32 = 40;
    pub const SETUP_BODY_Y0: i32 = 76;
    pub const SETUP_STEP: i32 = 18;
    /// The level value that renders as paper-white.
    pub const WHITE: u8 = 3;
}
#[cfg(feature = "badger2040w")]
mod layout {
    pub const BAR_X0: i32 = 278;
    pub const BAR_X1: i32 = 290;
    pub const BAR_Y0: i32 = 12;
    pub const BAR_Y1: i32 = 116;
    pub const DIGITS_Y: i32 = 84;
    pub const CLOCK_Y: i32 = 22;
    pub const SETUP_TITLE_Y: i32 = 28;
    pub const SETUP_BODY_Y0: i32 = 48;
    pub const SETUP_STEP: i32 = 15;
    pub const WHITE: u8 = 1;
}
use layout::*;

/// Clean-refresh waveform (the fast one is Turbo on both panels).
#[cfg(feature = "badger")]
const SPEED_CLEAN: Speed = Speed::Slow;
#[cfg(feature = "badger2040w")]
const SPEED_CLEAN: Speed = Speed::Default;

/// Quantizes the whole canvas for the panel at hand.
fn quantize_full(canvas: &Grey<'_>, levels: &mut [u8]) {
    let full = Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32));
    #[cfg(feature = "badger")]
    dither::quantize(canvas, full, Method::Nearest, levels);
    #[cfg(feature = "badger2040w")]
    dither::quantize_mono(canvas, full, Method::Nearest, levels);
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut p = embassy_rp::init(Default::default());
    #[cfg(feature = "badger")]
    let power_en = Output::new(p.PIN_27, Level::High);
    #[cfg(feature = "badger2040w")]
    let power_en = Output::new(p.PIN_10, Level::High);
    core::mem::forget(power_en);
    #[cfg(feature = "badger")]
    bsp::power::sleep_if_reset_held(
        p.PIN_14.reborrow(),
        p.PIN_0.reborrow(),
        p.PIN_1.reborrow(),
        p.PIN_2.reborrow(),
        p.PIN_3.reborrow(),
    )
    .await;

    let usb_driver = usb::Driver::new(p.USB, Irqs);
    spawner.spawn(bsp::usb::logger_task_with_input(usb_driver).unwrap());
    log::info!("totp booting");

    #[cfg(feature = "badger")]
    let leds = RearLeds::new(p.PWM_SLICE0, p.PWM_SLICE1, p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3);
    #[cfg(feature = "badger2040w")]
    let leds = RearLeds::new(p.PWM_SLICE3, p.PIN_22);
    spawner.spawn(bsp::leds::led_task(leds).unwrap());
    #[cfg(feature = "badger")]
    let buttons = ButtonPins {
        a: Input::new(p.PIN_7, Pull::Up),
        b: Input::new(p.PIN_9, Pull::Up),
        c: Input::new(p.PIN_10, Pull::Up),
        up: Input::new(p.PIN_11, Pull::Up),
        down: Input::new(p.PIN_6, Pull::Up),
        home: Input::new(p.PIN_22, Pull::Up),
    };
    #[cfg(feature = "badger2040w")]
    let buttons = ButtonPins {
        a: Input::new(p.PIN_12, Pull::Down),
        b: Input::new(p.PIN_13, Pull::Down),
        c: Input::new(p.PIN_14, Pull::Down),
        up: Input::new(p.PIN_15, Pull::Down),
        down: Input::new(p.PIN_11, Pull::Down),
    };
    spawner.spawn(bsp::buttons::button_task(buttons).unwrap());

    #[cfg(feature = "badger")]
    let mut epd = Epd::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_17, p.PIN_20, p.PIN_21, p.PIN_16);
    #[cfg(feature = "badger2040w")]
    let mut epd = Epd::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_17, p.PIN_20, p.PIN_21, p.PIN_26);
    epd.init().await;

    let mut rtc = RtcRam::new(p.I2C0, p.PIN_5, p.PIN_4);
    let mut settings = Settings::new(p.FLASH);

    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());

    // ---- Load or provision the secret + clock.
    let mut stored = [0u8; MAX_VAL];
    let mut key = [0u8; 40];
    let mut key_len = settings
        .get("totp", &mut stored)
        .and_then(|s| base32_decode(s, &mut key))
        .unwrap_or(0);

    if rtc.lost_power() || key_len == 0 {
        key_len = setup(&mut epd, &mut canvas, levels, &mut rtc, &mut settings, &mut key).await;
    }

    // ---- Show codes forever.
    let mut windows = 0u32;
    loop {
        let Some(now) = rtc.read_datetime() else {
            Timer::after(Duration::from_secs(1)).await;
            continue;
        };
        let unix = now.unix();
        let code = totp(&key[..key_len], unix);
        let remaining = 30 - (unix % 30) as u32;

        draw_code(&mut canvas, levels, code, &now, remaining);
        let full = windows.is_multiple_of(FULL_EVERY);
        epd.set_speed(if full { SPEED_CLEAN } else { Speed::Turbo });
        epd.present_levels(levels).await;
        windows += 1;

        // Tick the countdown bar until the window rolls over, watching
        // buttons throughout. Full TURBO refreshes rather than partials: a
        // partial only re-scans the bar's columns (see `Epd::present_partial`)
        // but every update still wiggles the whole panel's source lines, so
        // undriven pixels fade a little per pass — the full refresh restores
        // the digits each tick instead.
        let window_id = unix / 30;
        epd.set_speed(Speed::Turbo);
        loop {
            let sleep = Timer::after(Duration::from_secs(5));
            match embassy_futures::select::select(sleep, EVENTS.receive()).await {
                embassy_futures::select::Either::First(()) => {
                    let Some(t) = rtc.read_datetime() else { break };
                    let u = t.unix();
                    if u / 30 != window_id {
                        break; // next code
                    }
                    draw_code(&mut canvas, levels, code, &t, 30 - (u % 30) as u32);
                    epd.present_levels(levels).await;
                }
                embassy_futures::select::Either::Second(ButtonEvent::Pressed(Button::A)) => {
                    windows = 0; // force a clean SLOW refresh now
                    break;
                }
                embassy_futures::select::Either::Second(ButtonEvent::Pressed(Button::B)) => {
                    key_len = setup(&mut epd, &mut canvas, levels, &mut rtc, &mut settings, &mut key).await;
                    windows = 0;
                    break;
                }
                _ => {}
            }
        }
    }
}

/// Paints the countdown bar directly as levels: black outline, black fill
/// from the top for the remaining fraction of the 30 s window.
fn paint_bar(levels: &mut [u8], remaining: u32) {
    let rect = |x: i32, y: i32, w: i32, h: i32| {
        Rectangle::new(Point::new(x, y), Size::new(w.max(0) as u32, h.max(0) as u32))
    };
    // Clear the strip, outline, fill.
    dither::paint_level(levels, WIDTH, rect(BAR_X0 - 2, BAR_Y0 - 2, BAR_X1 - BAR_X0 + 4, BAR_Y1 - BAR_Y0 + 4), WHITE);
    for (x0, y0, w, h) in [
        (BAR_X0, BAR_Y0, BAR_X1 - BAR_X0, 1),
        (BAR_X0, BAR_Y1 - 1, BAR_X1 - BAR_X0, 1),
        (BAR_X0, BAR_Y0, 1, BAR_Y1 - BAR_Y0),
        (BAR_X1 - 1, BAR_Y0, 1, BAR_Y1 - BAR_Y0),
    ] {
        dither::paint_level(levels, WIDTH, rect(x0, y0, w, h), 0);
    }
    // Anchored at the bottom so the bar drains downward as time runs out.
    let inner_h = (BAR_Y1 - BAR_Y0 - 4) * remaining.min(30) as i32 / 30;
    dither::paint_level(
        levels,
        WIDTH,
        rect(BAR_X0 + 2, BAR_Y1 - 2 - inner_h, BAR_X1 - BAR_X0 - 4, inner_h),
        0,
    );
}

/// The provisioning flow: instructions on the panel, `TIME`/`SECRET` lines
/// over USB serial. Returns the decoded key length once both are in order.
async fn setup(
    epd: &mut Epd,
    canvas: &mut Grey<'_>,
    levels: &mut [u8],
    rtc: &mut RtcRam,
    settings: &mut Settings,
    key: &mut [u8; 40],
) -> usize {
    draw_setup(canvas, levels, rtc.lost_power(), false);
    epd.set_speed(SPEED_CLEAN);
    epd.present_levels(levels).await;
    log::info!("setup: send `TIME 2026-08-30T21:04:05` (UTC) and `SECRET <base32>`");

    let mut time_ok = !rtc.lost_power();
    let mut key_len = 0usize;
    let mut line = heapless::String::<128>::new();
    loop {
        if time_ok && key_len > 0 {
            log::info!("setup complete");
            return key_len;
        }
        let entered = bsp::usb::read_line(&mut line).await;
        if let Some(t) = strip_key(entered, "TIME") {
            match parse_time(t) {
                Some(dt) if rtc.set_datetime(&dt) => {
                    time_ok = true;
                    log::info!("clock set: {:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second);
                }
                _ => log::warn!("bad TIME (want 2026-08-30T21:04:05, UTC)"),
            }
        } else if let Some(s) = strip_key(entered, "SECRET") {
            match base32_decode(s.as_bytes(), key) {
                Some(n) if n >= 10 => {
                    if settings.set("totp", s.as_bytes()) {
                        key_len = n;
                        log::info!("secret stored ({} bytes)", n);
                    } else {
                        log::warn!("flash write failed");
                    }
                }
                _ => log::warn!("bad SECRET (want base32, at least 16 chars)"),
            }
        } else if !entered.is_empty() {
            log::warn!("unknown command: {}", entered);
        }
        if time_ok && key_len > 0 {
            draw_setup(canvas, levels, false, true);
            epd.present_levels(levels).await;
        }
    }
}

/// `PREFIX value` (case-insensitive prefix, at least one space).
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (head, rest) = line.split_at_checked(key.len())?;
    head.eq_ignore_ascii_case(key).then(|| rest.trim()).filter(|v| !v.is_empty())
}

/// `2026-08-30T21:04:05` (also accepts a space instead of the `T`).
fn parse_time(s: &str) -> Option<DateTime> {
    let num = |s: &str| s.parse::<u16>().ok();
    let (date, time) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let (year, month, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let mut t = time.trim_end_matches(['Z', 'z']).split(':');
    let (hour, minute, second) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    if !(2025..2100).contains(&(year as i32)) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(DateTime {
        year,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
    })
}

/// RFC 4648 base32 (no padding, case-insensitive). Returns the byte count.
fn base32_decode(input: &[u8], out: &mut [u8; 40]) -> Option<usize> {
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut n = 0usize;
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            b'=' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 5 | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if n == out.len() {
                return None;
            }
            out[n] = (acc >> bits) as u8;
            n += 1;
        }
    }
    (n > 0).then_some(n)
}

/// RFC 6238 TOTP: 6 digits, 30-second window, HMAC-SHA1.
fn totp(key: &[u8], unix: u64) -> u32 {
    let counter = (unix / 30).to_be_bytes();
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&counter);
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let bin = u32::from_be_bytes(digest[offset..offset + 4].try_into().expect("4-byte slice"));
    (bin & 0x7fff_ffff) % 1_000_000
}

/// The main screen: seven-segment digits, the UTC time small, and the
/// countdown bar (painted separately so partial ticks reuse the code).
fn draw_code(canvas: &mut Grey<'_>, levels: &mut [u8], code: u32, now: &DateTime, remaining: u32) {
    canvas.fill(255);
    let digits = FontRenderer::new::<fonts::u8g2_font_7Segments_26x42_mn>();
    let mut text = heapless::String::<8>::new();
    let _ = core::fmt::write(&mut text, format_args!("{:03} {:03}", code / 1000, code % 1000));
    let _ = digits.render_aligned(
        text.as_str(),
        Point::new((BAR_X0 - 6) / 2, DIGITS_Y),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );

    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let mut clock = heapless::String::<40>::new();
    let _ = core::fmt::write(
        &mut clock,
        format_args!("{:02}:{:02}:{:02} UTC", now.hour, now.minute, now.second),
    );
    let _ = small.render_aligned(
        clock.as_str(),
        Point::new((BAR_X0 - 6) / 2, CLOCK_Y),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );

    quantize_full(canvas, levels);
    paint_bar(levels, remaining);
}

/// The setup screen.
fn draw_setup(canvas: &mut Grey<'_>, levels: &mut [u8], need_time: bool, done: bool) {
    canvas.fill(255);
    let title = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let body = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let _ = title.render_aligned(
        if done { "Ready" } else { "TOTP setup" },
        Point::new(WIDTH as i32 / 2, SETUP_TITLE_Y),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let lines: &[&str] = if done {
        &["Provisioned. Codes start", "with the next window."]
    } else if need_time {
        &[
            "Connect USB serial (115200) and send:",
            "",
            "TIME 2026-08-30T21:04:05   (UTC now)",
            "SECRET JBSWY3DPEHPK3PXP    (base32)",
        ]
    } else {
        &["Clock is set. Send over USB serial:", "", "SECRET JBSWY3DPEHPK3PXP    (base32)"]
    };
    let mut y = SETUP_BODY_Y0;
    for l in lines {
        let _ = body.render_aligned(
            *l,
            Point::new(12, y),
            VerticalPosition::Baseline,
            HorizontalAlignment::Left,
            FontColor::Transparent(BLACK),
            canvas,
        );
        y += SETUP_STEP;
    }
    quantize_full(canvas, levels);
}

// Rust guideline compliant 2026-08-30
