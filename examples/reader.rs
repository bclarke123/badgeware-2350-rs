//! E-reader for the Badger 2350 — and the PSRAM demo: downloads an entire
//! public-domain book from Project Gutenberg over TLS into the 8 MB QSPI
//! PSRAM (a novel does not fit in 520 KB of SRAM), then paginates it on the
//! e-paper.
//!
//! `cargo run --release --example reader --no-default-features --features badger`
//!
//! **DOWN/UP** turn pages (TURBO refresh, with a clean SLOW refresh every
//! dozen turns); **A** re-downloads the book. The reading position is saved
//! to the settings sector after every turn, so a power cycle re-fetches the
//! text (PSRAM is volatile) and reopens at your page. WiFi credentials come
//! from the shared settings sector (provision with the `weather` example);
//! the radio disassociates once the book has landed.
//!
//! Change [`BOOK_HOST`]/[`BOOK_PATH`] for a different text — anything at
//! gutenberg.org's `cache/epub` works. The Gutenberg licence header and
//! footer are trimmed automatically.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::{bind_interrupts, dma, pio, usb};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_io_async::Read as _;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::Method;
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::battery::Battery;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp::settings::{Settings, MAX_VAL};
use tufty_2350::bsp::wifi;
use tufty_2350::gfx::dither::{self, Method as Dither};
use tufty_2350::gfx::grey::Grey;

/// The book. Moby Dick: ~1.28 MB of plain text.
const BOOK_HOST: &str = "www.gutenberg.org";
const BOOK_PATH: &str = "/cache/epub/2701/pg2701.txt";
const BOOK_TITLE: &str = "Moby Dick";

/// Page geometry.
const MARGIN: i32 = 7;
const LINE_H: i32 = 15;
const TEXT_TOP: i32 = 16;
/// Lowest allowed baseline: clear of the footer rule plus descenders.
const TEXT_BOTTOM: i32 = HEIGHT as i32 - 26;

/// Clean SLOW refresh every this many page turns.
const CLEAN_EVERY: u32 = 12;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static TLS_RX: ConstStaticCell<[u8; 16640]> = ConstStaticCell::new([0; 16640]);
static TLS_TX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static HTTP_RX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static TCP_STATE: ConstStaticCell<TcpClientState<1, 4096, 4096>> = ConstStaticCell::new(TcpClientState::new());

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
    log::info!("reader booting");

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
    epd.set_speed(Speed::Slow);
    let mut settings = Settings::new(p.FLASH);
    let mut battery = Battery::new(p.ADC, p.PIN_26, p.PIN_28, p.PIN_12);
    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());

    // ---- The whole point: 8 MB of runtime-writable memory.
    let Some(psram) = bsp::psram::init() else {
        draw_status(&mut canvas, levels, "No PSRAM", "this demo needs the 8 MB chip");
        epd.present_levels(levels).await;
        loop {
            EVENTS.receive().await;
        }
    };

    // ---- WiFi + download.
    let mut cred = [0u8; MAX_VAL];
    let cred_len = settings.get("wifi", &mut cred).map(<[u8]>::len).unwrap_or(0);
    let (ssid, pass) = split_cred(&cred[..cred_len]);
    draw_status(&mut canvas, levels, BOOK_TITLE, "connecting...");
    epd.present_levels(levels).await;

    let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
    let mut net = match wifi::connect(
        spawner, p.PIO0, Irqs, dma_ch, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, ssid, pass,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            log::warn!("wifi failed: {:?}", e);
            draw_status(&mut canvas, levels, "No WiFi", "provision with the weather example");
            epd.present_levels(levels).await;
            loop {
                EVENTS.receive().await;
            }
        }
    };

    let tcp_state = TCP_STATE.take();
    let (tls_rx, tls_tx, http_rx) = (TLS_RX.take(), TLS_TX.take(), HTTP_RX.take());

    let mut book: &[u8] = &[];
    loop {
        // ---- Fetch (or re-fetch on A) into PSRAM.
        if book.is_empty() {
            draw_status(&mut canvas, levels, BOOK_TITLE, "downloading...");
            epd.set_speed(Speed::Turbo);
            epd.present_levels(levels).await;
            match download(net.stack, tcp_state, tls_rx, tls_tx, http_rx, psram, &mut canvas, levels, &mut epd).await {
                Some(n) => {
                    let trimmed = trim_gutenberg(&psram[..n]);
                    log::info!("book: {} bytes ({} after trim)", n, trimmed.len());
                    book = trimmed;
                    // Reading done over the (cached) XIP window; drop the radio.
                    net.leave().await;
                }
                None => {
                    draw_status(&mut canvas, levels, "Download failed", "press A to retry");
                    epd.present_levels(levels).await;
                    wait_for(Button::A).await;
                    continue;
                }
            }
        }

        // ---- Restore the bookmark and read.
        let mut buf = [0u8; MAX_VAL];
        let mut pos: usize = settings
            .get("reader", &mut buf)
            .and_then(|b| core::str::from_utf8(b).ok())
            .and_then(|s| s.parse().ok())
            .filter(|&o| o < book.len())
            .unwrap_or(0);

        let mut turns = 0u32;
        'reading: loop {
            let text = core::str::from_utf8(book).unwrap_or("");
            let end = draw_page(&mut canvas, levels, text, pos, battery.percent());
            epd.set_speed(if turns.is_multiple_of(CLEAN_EVERY) { Speed::Slow } else { Speed::Turbo });
            epd.present_levels(levels).await;
            turns += 1;

            let mut save = heapless::String::<16>::new();
            let _ = write!(save, "{}", pos);
            let _ = settings.set("reader", save.as_bytes());

            loop {
                match EVENTS.receive().await {
                    ButtonEvent::Pressed(Button::Down) => {
                        if end < book.len() {
                            pos = end;
                        }
                        continue 'reading;
                    }
                    ButtonEvent::Pressed(Button::Up) => {
                        if pos > 0 {
                            // Anchor ~a page of bytes back on a line start,
                            // then lay out forward to the page that ends
                            // here. Layout is deterministic, so no history
                            // stack is needed.
                            let face = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
                            let target = pos.saturating_sub(600);
                            let anchor = text.as_bytes()[..target]
                                .iter()
                                .rposition(|&b| b == b'\n')
                                .map_or(0, |i| i + 1);
                            let mut p = anchor;
                            loop {
                                let n = next_page_start(&face, text, p);
                                if n >= pos || n <= p {
                                    break;
                                }
                                p = n;
                            }
                            pos = p;
                        }
                        continue 'reading;
                    }
                    ButtonEvent::Pressed(Button::A) => {
                        book = &[];
                        let _ = settings.set("reader", b"0");
                        break 'reading;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Splits the stored `ssid\0pass` credential blob.
fn split_cred(cred: &[u8]) -> (&str, &str) {
    let split = cred.iter().position(|&b| b == 0).unwrap_or(cred.len());
    (
        core::str::from_utf8(&cred[..split]).unwrap_or(""),
        core::str::from_utf8(cred.get(split + 1..).unwrap_or(&[])).unwrap_or(""),
    )
}

async fn wait_for(wanted: Button) {
    loop {
        if let ButtonEvent::Pressed(b) = EVENTS.receive().await {
            if b == wanted {
                return;
            }
        }
    }
}

/// Bytes between download progress refreshes (each is a ~1 s e-paper pass).
const PROGRESS_STEP: usize = 128 * 1024;

/// Streams the book over HTTPS into `psram`; returns the byte count.
#[allow(clippy::too_many_arguments)]
async fn download(
    stack: embassy_net::Stack<'static>,
    tcp_state: &'static TcpClientState<1, 4096, 4096>,
    tls_rx: &mut [u8],
    tls_tx: &mut [u8],
    http_rx: &mut [u8],
    psram: &mut [u8],
    canvas: &mut Grey<'_>,
    levels: &mut [u8],
    epd: &mut Epd,
) -> Option<usize> {
    let tcp = TcpClient::new(stack, tcp_state);
    let dns = DnsSocket::new(stack);
    let tls = TlsConfig::new(RoscRng.next_u64(), tls_rx, tls_tx, TlsVerify::None);
    let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);

    let mut url = heapless::String::<128>::new();
    let _ = write!(url, "https://{}{}", BOOK_HOST, BOOK_PATH);
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("reader: connect failed: {:?}", e);
            return None;
        }
    };
    let response = match request.send(http_rx).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("reader: request failed: {:?}", e);
            return None;
        }
    };
    log::info!("reader: http {:?}", response.status);
    let total = response.content_length;
    let mut body = response.body().reader();
    let mut n = 0usize;
    let mut shown = 0usize;
    loop {
        match body.read(&mut psram[n..]).await {
            Ok(0) => break,
            Ok(got) => {
                n += got;
                if n / PROGRESS_STEP > shown {
                    shown = n / PROGRESS_STEP;
                    log::info!("reader: {} KB", n / 1024);
                    draw_progress(canvas, levels, n, total);
                    epd.present_levels(levels).await;
                }
                if n == psram.len() {
                    break;
                }
            }
            Err(e) => {
                log::warn!("reader: read failed after {} bytes: {:?}", n, e);
                return (n > 0).then_some(n);
            }
        }
    }
    Some(n)
}

/// Strips the Project Gutenberg licence header and footer.
fn trim_gutenberg(book: &[u8]) -> &[u8] {
    let start = find(book, b"*** START OF")
        .and_then(|at| find(&book[at..], b"\n").map(|nl| at + nl + 1))
        .unwrap_or(0);
    let end = find(book, b"*** END OF").unwrap_or(book.len());
    &book[start..end]
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Lays out and draws one page starting at byte `pos`; returns the offset
/// where the next page starts. Gutenberg text is hard-wrapped, so lone
/// newlines join into flowing text and blank lines break paragraphs.
fn draw_page(canvas: &mut Grey<'_>, levels: &mut [u8], text: &str, pos: usize, batt: u8) -> usize {
    canvas.fill(255);
    let face = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let max_w = WIDTH as i32 - 2 * MARGIN;

    let mut cursor = pos;
    let mut y = TEXT_TOP;
    while y <= TEXT_BOTTOM {
        let (line, next, para_break) = take_line(&face, text, cursor, max_w);
        if !line.is_empty() {
            let _ = face.render_aligned(
                sanitize(line).as_str(),
                Point::new(MARGIN, y),
                VerticalPosition::Baseline,
                HorizontalAlignment::Left,
                FontColor::Transparent(BLACK),
                canvas,
            );
        }
        cursor = next;
        y += LINE_H;
        if para_break {
            y += LINE_H / 3;
        }
        if cursor >= text.len() {
            break;
        }
    }

    // Footer: progress and battery.
    let _ = Line::new(
        Point::new(MARGIN, HEIGHT as i32 - 18),
        Point::new(WIDTH as i32 - MARGIN, HEIGHT as i32 - 18),
    )
    .into_styled(PrimitiveStyle::with_stroke(BLACK, 1))
    .draw(canvas);
    let mut foot = heapless::String::<48>::new();
    let permille = if text.is_empty() { 0 } else { cursor as u64 * 1000 / text.len() as u64 };
    let _ = write!(foot, "{} - {}.{}%", BOOK_TITLE, permille / 10, permille % 10);
    let _ = face.render_aligned(
        foot.as_str(),
        Point::new(MARGIN, HEIGHT as i32 - 4),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let mut b = heapless::String::<8>::new();
    let _ = write!(b, "{}%", batt);
    let _ = face.render_aligned(
        b.as_str(),
        Point::new(WIDTH as i32 - MARGIN, HEIGHT as i32 - 4),
        VerticalPosition::Baseline,
        HorizontalAlignment::Right,
        FontColor::Transparent(BLACK),
        canvas,
    );

    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Dither::Nearest,
        levels,
    );
    cursor
}

/// Greedily fills one display line from `text[pos..]`: returns the line, the
/// next read position, and whether a paragraph break follows.
fn take_line<'a>(face: &FontRenderer, text: &'a str, pos: usize, max_w: i32) -> (&'a str, usize, bool) {
    let bytes = text.as_bytes();
    let mut cursor = pos;
    // Skip leading whitespace, counting newlines (2+ = paragraph gap eaten).
    while cursor < bytes.len() && (bytes[cursor] as char).is_ascii_whitespace() {
        cursor += 1;
    }
    let line_start = cursor;
    let mut line_end = cursor;
    while cursor < bytes.len() {
        // The next word.
        let word_start = cursor;
        while cursor < bytes.len() && !(bytes[cursor] as char).is_ascii_whitespace() {
            cursor += 1;
        }
        if width_of(face, text, line_start, cursor) > max_w && line_end > line_start {
            // Word does not fit: line ends before it.
            return (text[line_start..line_end].trim(), word_start, false);
        }
        line_end = cursor;
        // Consume whitespace after the word; a blank line ends the paragraph.
        let mut newlines = 0;
        let after_word = cursor;
        while cursor < bytes.len() && (bytes[cursor] as char).is_ascii_whitespace() {
            if bytes[cursor] == b'\n' {
                newlines += 1;
            }
            cursor += 1;
        }
        if newlines >= 2 {
            return (text[line_start..after_word].trim(), cursor, true);
        }
    }
    (text[line_start..line_end].trim(), cursor, false)
}

/// Pure layout: where the page starting at `pos` ends. MUST mirror
/// `draw_page`'s line loop exactly, or rebuilt back-navigation pages drift
/// from drawn ones.
fn next_page_start(face: &FontRenderer, text: &str, pos: usize) -> usize {
    let max_w = WIDTH as i32 - 2 * MARGIN;
    let mut cursor = pos;
    let mut y = TEXT_TOP;
    while y <= TEXT_BOTTOM {
        let (_line, next, para_break) = take_line(face, text, cursor, max_w);
        cursor = next;
        y += LINE_H;
        if para_break {
            y += LINE_H / 3;
        }
        if cursor >= text.len() {
            break;
        }
    }
    cursor
}

/// Renderable copy of a line: newlines become spaces, and typography the
/// Latin-1 font lacks (curly quotes, em dashes, ellipses) is mapped to its
/// ASCII spelling — a glyph the font can't draw would otherwise abort the
/// whole line. Measure and render MUST both go through this or they
/// disagree about layout.
fn sanitize(src: &str) -> heapless::String<96> {
    let mut out = heapless::String::new();
    for c in src.chars() {
        let pushed = match c {
            '\n' | '\r' => out.push(' '),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2010}' | '\u{2013}' | '\u{2014}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            c if (c as u32) < 256 => out.push(c),
            _ => out.push('?'),
        };
        if pushed.is_err() {
            break;
        }
    }
    out
}

/// Measured width of `text[start..end]` as it will actually render.
fn width_of(face: &FontRenderer, text: &str, start: usize, end: usize) -> i32 {
    let buf = sanitize(&text[start..end]);
    face.get_rendered_dimensions(buf.as_str(), Point::zero(), VerticalPosition::Baseline)
        .ok()
        .and_then(|d| d.bounding_box)
        .map_or(0, |bb| bb.size.width as i32)
}

/// The download screen: title, a progress bar, and a byte count.
fn draw_progress(canvas: &mut Grey<'_>, levels: &mut [u8], done: usize, total: Option<usize>) {
    canvas.fill(255);
    let title = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let _ = title.render_aligned(
        BOOK_TITLE,
        Point::new(WIDTH as i32 / 2, 80),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let (bar_w, bar_h) = (200u32, 12u32);
    let bar = Point::new((WIDTH as i32 - bar_w as i32) / 2, 96);
    let mut label = heapless::String::<32>::new();
    if let Some(total) = total.filter(|&t| t > 0) {
        let _ = Rectangle::new(bar, Size::new(bar_w, bar_h))
            .into_styled(PrimitiveStyle::with_stroke(BLACK, 1))
            .draw(canvas);
        let fill = (done.min(total) as u32).saturating_mul(bar_w - 4) / total as u32;
        let _ = Rectangle::new(bar + Point::new(2, 2), Size::new(fill, bar_h - 4))
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(canvas);
        let _ = write!(label, "{} / {} KB", done / 1024, total / 1024);
    } else {
        // No Content-Length: just count up.
        let _ = write!(label, "{} KB", done / 1024);
    }
    let _ = small.render_aligned(
        label.as_str(),
        Point::new(WIDTH as i32 / 2, 128),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Dither::Nearest,
        levels,
    );
}

/// A title + status screen.
fn draw_status(canvas: &mut Grey<'_>, levels: &mut [u8], head: &str, detail: &str) {
    canvas.fill(255);
    let title = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let _ = title.render_aligned(
        head,
        Point::new(WIDTH as i32 / 2, 80),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let _ = small.render_aligned(
        detail,
        Point::new(WIDTH as i32 / 2, 104),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Dither::Nearest,
        levels,
    );
}

// Rust guideline compliant 2026-08-31
