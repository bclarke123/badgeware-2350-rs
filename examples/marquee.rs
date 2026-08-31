//! Message board for the Badger 2350: shows a message as big as it will fit
//! on the zero-power e-paper, settable **from your phone** with no app, no
//! shared network and no typing of URLs — for finding your friends at a
//! loud, crowded gig ("BEN WHERE ARE YOU", held overhead).
//!
//! `cargo run --release --example marquee --no-default-features --features badger`
//!
//! Hold **A** for three seconds (the rear LEDs blink to confirm): the badge
//! becomes a WPA2 hotspot and shows a join-QR. Scan it with your phone
//! camera to join; the badge answers the phone's connectivity probes with a
//! redirect, so the OS pops the message form up as a captive "sign in" page
//! automatically (this also stops the phone from dropping the internet-less
//! network). Submit, the LEDs celebrate, the hotspot shuts down, and the
//! message is on the panel — persisted in flash, so it survives power-off.
//!
//! Under the hood the badge runs three tiny servers during setup: a
//! single-lease DHCP server, a wildcard DNS resolver (everything resolves to
//! the badge), and the HTTP form. Setup times out after three minutes.

#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpEndpoint, IpListenEndpoint};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::{bind_interrupts, dma, pio, usb};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_io_async::Write as _;
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::{cue, LedCue, RearLeds};
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp::settings::{Settings, MAX_VAL};
use tufty_2350::bsp::wifi::{self, AP_ADDR};
use tufty_2350::gfx::dither::{self, Method};
use tufty_2350::gfx::grey::Grey;

/// Hotspot credentials (WPA2 wants >= 8 characters).
const AP_SSID: &str = "badger-marquee";
const AP_PASS: &str = "badger42";
const AP_CHANNEL: u8 = 6;

/// Give up on setup after this long without a message.
const SETUP_TIMEOUT: Duration = Duration::from_secs(180);

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
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
    log::info!("marquee booting");

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
    let mut settings = Settings::new(p.FLASH);
    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());
    // Taken once: ConstStaticCell::take panics if called again, and setup
    // can run any number of times per boot.
    let (qr_temp, qr_out) = (QR_TEMP.take(), QR_OUT.take());

    // Radio up, hotspot off until asked for.
    let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
    let mut wifi =
        wifi::access_point(spawner, p.PIO0, Irqs, dma_ch, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29).await;

    let mut message = heapless::String::<{ MAX_VAL }>::new();
    let mut buf = [0u8; MAX_VAL];
    let _ = message.push_str(
        settings
            .get("marquee", &mut buf)
            .and_then(|b| core::str::from_utf8(b).ok())
            .unwrap_or("HOLD A TO SET A MESSAGE"),
    );

    epd.set_speed(Speed::Slow);
    draw_message(&mut canvas, levels, &message);
    epd.present_levels(levels).await;

    loop {
        // Wait for a 3-second hold of A.
        let ButtonEvent::Pressed(Button::A) = EVENTS.receive().await else { continue };
        if !held(Button::A, Duration::from_secs(3)).await {
            continue;
        }
        cue(LedCue::Blink);

        // ---- Setup session: hotspot + captive portal.
        wifi.ap_on(AP_SSID, AP_PASS, AP_CHANNEL).await;
        draw_setup(&mut canvas, levels, qr_temp, qr_out);
        epd.set_speed(Speed::Turbo);
        epd.present_levels(levels).await;

        let got = with_timeout(SETUP_TIMEOUT, portal_session(wifi.stack, &mut message)).await;
        wifi.ap_off().await;

        match got {
            Ok(()) => {
                cue(LedCue::Celebrate);
                if !settings.set("marquee", message.as_bytes()) {
                    log::warn!("could not persist message");
                }
                log::info!("message set: {}", message);
            }
            Err(_) => {
                cue(LedCue::Error);
                log::info!("setup timed out");
            }
        }
        epd.set_speed(Speed::Slow);
        draw_message(&mut canvas, levels, &message);
        epd.present_levels(levels).await;
    }
}

/// True if `button` stays held for `dur` (consumes its release event).
async fn held(button: Button, dur: Duration) -> bool {
    let deadline = Timer::after(dur);
    let wait_release = async {
        loop {
            if let ButtonEvent::Released(b) = EVENTS.receive().await {
                if b == button {
                    break;
                }
            }
        }
    };
    matches!(
        embassy_futures::select::select(deadline, wait_release).await,
        embassy_futures::select::Either::First(())
    )
}

/// Runs DHCP + DNS + HTTP until a message is submitted (writes it into
/// `message`).
async fn portal_session(stack: embassy_net::Stack<'static>, message: &mut heapless::String<{ MAX_VAL }>) {
    let dhcp = dhcp_server(stack);
    let dns = dns_server(stack);
    let http = http_server(stack, message);
    // http completes when a message lands; the servers never do.
    match embassy_futures::select::select3(http, dhcp, dns).await {
        embassy_futures::select::Either3::First(()) => {}
        _ => unreachable!("infinite servers returned"),
    }
}

/// A one-lease DHCP server: every client is offered 192.168.4.2. Enough for
/// the single phone a setup session involves.
async fn dhcp_server(stack: embassy_net::Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 640];
    let mut tx = [0u8; 640];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    sock.bind(67).expect("bind dhcp");
    let mut packet = [0u8; 576];
    loop {
        let Ok((n, _peer)) = sock.recv_from(&mut packet).await else { continue };
        // BOOTREQUEST with the magic cookie?
        if n < 244 || packet[0] != 1 || packet[236..240] != [0x63, 0x82, 0x53, 0x63] {
            continue;
        }
        // Find option 53 (message type).
        let mut msg_type = 0u8;
        let mut i = 240;
        while i + 1 < n {
            let (opt, len) = (packet[i], usize::from(packet[i + 1]));
            if opt == 255 {
                break;
            }
            if opt == 53 && len == 1 && i + 2 < n {
                msg_type = packet[i + 2];
            }
            i += 2 + len;
        }
        let reply_type = match msg_type {
            1 => 2, // DISCOVER -> OFFER
            3 => 5, // REQUEST -> ACK
            _ => continue,
        };
        let mut reply = [0u8; 300];
        reply[0] = 2; // BOOTREPLY
        reply[1..3].copy_from_slice(&packet[1..3]); // htype, hlen
        reply[4..8].copy_from_slice(&packet[4..8]); // xid
        reply[10] = packet[10]; // broadcast flag
        reply[16..20].copy_from_slice(&[192, 168, 4, 2]); // yiaddr
        reply[20..24].copy_from_slice(&AP_ADDR.octets()); // siaddr
        reply[28..44].copy_from_slice(&packet[28..44]); // chaddr
        reply[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        let opts: &[u8] = &[
            53, 1, reply_type, // message type
            54, 4, 192, 168, 4, 1, // server id
            51, 4, 0, 0, 0x0e, 0x10, // lease 3600 s
            1, 4, 255, 255, 255, 0, // subnet
            3, 4, 192, 168, 4, 1, // router
            6, 4, 192, 168, 4, 1, // dns
            255,
        ];
        reply[240..240 + opts.len()].copy_from_slice(opts);
        // Reply via broadcast: the client has no address yet.
        let bcast = IpEndpoint::new(Ipv4Addr::BROADCAST.into(), 68);
        let _ = sock.send_to(&reply[..240 + opts.len()], bcast).await;
        log::info!("dhcp: {} -> 192.168.4.2", if reply_type == 2 { "offer" } else { "ack" });
    }
}

/// A wildcard DNS server: every A query resolves to the badge, which is what
/// makes the phone's connectivity probe land on our HTTP server and pop the
/// captive-portal sheet.
async fn dns_server(stack: embassy_net::Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 640];
    let mut tx = [0u8; 640];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    sock.bind(53).expect("bind dns");
    let mut packet = [0u8; 512];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut packet).await else { continue };
        if n < 12 {
            continue;
        }
        // Walk the first question's labels.
        let mut i = 12;
        while i < n && packet[i] != 0 {
            i += 1 + usize::from(packet[i]);
        }
        let q_end = i + 5; // zero byte + qtype + qclass
        if i >= n || q_end > n {
            continue;
        }
        let mut reply = [0u8; 512];
        reply[..q_end].copy_from_slice(&packet[..q_end]);
        reply[2] = 0x81; // response, recursion desired
        reply[3] = 0x80; // recursion available
        reply[6] = 0;
        reply[7] = 1; // one answer
        reply[8..12].fill(0);
        let answer: &[u8] = &[
            0xc0, 0x0c, // pointer to the question name
            0, 1, 0, 1, // type A, class IN
            0, 0, 0, 60, // ttl
            0, 4, 192, 168, 4, 1,
        ];
        reply[q_end..q_end + answer.len()].copy_from_slice(answer);
        let _ = sock.send_to(&reply[..q_end + answer.len()], peer).await;
    }
}

/// Serves the form (and captive-portal redirects) until a message arrives.
async fn http_server(stack: embassy_net::Stack<'static>, message: &mut heapless::String<{ MAX_VAL }>) {
    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 2048];
    let mut req = [0u8; 1024];
    loop {
        let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
        sock.set_timeout(Some(Duration::from_secs(8)));
        if sock.accept(IpListenEndpoint { addr: None, port: 80 }).await.is_err() {
            continue;
        }
        let mut n = 0usize;
        // Read at least the request line + headers (phones send small GETs).
        while n < req.len() {
            match sock.read(&mut req[n..]).await {
                Ok(0) | Err(_) => break,
                Ok(got) => {
                    n += got;
                    if req[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let head = core::str::from_utf8(&req[..n]).unwrap_or("");
        let path = head.split(' ').nth(1).unwrap_or("/");
        let done = if let Some(query) = path.strip_prefix("/set?m=") {
            let text = query.split('&').next().unwrap_or("");
            message.clear();
            percent_decode(text, message);
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <html><body style='font-family:sans-serif;text-align:center;padding-top:4em'>\
                      <h1>&#10003; On the badge!</h1><p>You can leave this network.</p></body></html>",
                )
                .await;
            !message.is_empty()
        } else if path == "/" || path.starts_with("/index") {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <html><head><meta name='viewport' content='width=device-width,initial-scale=1'></head>\
                      <body style='font-family:sans-serif;text-align:center;padding-top:3em'>\
                      <h1>Badger says...</h1><form action='/set'>\
                      <input name='m' maxlength='100' autofocus style='font-size:1.5em;width:90%'>\
                      <p><button style='font-size:1.5em;padding:0.3em 2em'>Show it</button></p>\
                      </form></body></html>",
                )
                .await;
            false
        } else {
            // Connectivity probes and anything else: redirect to the form —
            // this is what makes the phone pop the captive-portal sheet.
            let _ = sock
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://192.168.4.1/\r\nConnection: close\r\n\r\n",
                )
                .await;
            false
        };
        let _ = sock.flush().await;
        sock.close();
        Timer::after(Duration::from_millis(50)).await;
        if done {
            return;
        }
    }
}

/// Decodes `%XX` and `+` from a query value into `out` (truncating to fit).
fn percent_decode(input: &str, out: &mut heapless::String<{ MAX_VAL }>) {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = match bytes[i] {
            b'+' => b' ',
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 => {
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                if let (Some(h), Some(l)) = (
                    bytes.get(i + 1).copied().and_then(hex),
                    bytes.get(i + 2).copied().and_then(hex),
                ) {
                    i += 2;
                    h << 4 | l
                } else {
                    b'%'
                }
            }
            b => b,
        };
        if (0x20..0x7f).contains(&c) && out.push(c as char).is_err() {
            break;
        }
        i += 1;
    }
}

/// Renders the message as big as it fits: tries faces largest-first, greedy
/// word wrap, first whose wrapped lines fit both axes wins.
fn draw_message(canvas: &mut Grey<'_>, levels: &mut [u8], message: &str) {
    canvas.fill(255);
    let faces: [(FontRenderer, i32); 5] = [
        (FontRenderer::new::<fonts::u8g2_font_logisoso58_tf>(), 66),
        (FontRenderer::new::<fonts::u8g2_font_logisoso42_tf>(), 50),
        (FontRenderer::new::<fonts::u8g2_font_logisoso28_tf>(), 34),
        (FontRenderer::new::<fonts::u8g2_font_logisoso18_tf>(), 23),
        (FontRenderer::new::<fonts::u8g2_font_crox1h_tf>(), 14),
    ];
    let max_w = WIDTH as i32 - 8;
    let max_h = HEIGHT as i32 - 8;

    for (face, line_h) in &faces {
        let mut lines: heapless::Vec<(usize, usize), 12> = heapless::Vec::new(); // byte ranges
        let mut ok = true;
        let mut start = 0usize;
        while start < message.len() && ok {
            // Greedy: extend the line word by word while it measures short
            // enough.
            let rest = &message[start..];
            let mut end = start;
            for (off, _) in rest.match_indices(' ').chain(core::iter::once((rest.len(), ""))) {
                let candidate = message[start..start + off].trim_end();
                if candidate.is_empty() {
                    continue;
                }
                if text_width(face, candidate) <= max_w {
                    end = start + off;
                } else {
                    break;
                }
            }
            if end == start {
                // A single word wider than the panel: this face is too big.
                ok = false;
                break;
            }
            if lines.push((start, end)).is_err() {
                ok = false;
                break;
            }
            start = end + 1; // skip the space
        }
        if !ok || (lines.len() as i32) * line_h > max_h {
            continue;
        }
        // Fits: draw centered.
        let total = lines.len() as i32 * line_h;
        let mut y = (HEIGHT as i32 - total) / 2 + line_h - line_h / 5;
        for &(s, e) in &lines {
            let _ = face.render_aligned(
                message[s..e].trim(),
                Point::new(WIDTH as i32 / 2, y),
                VerticalPosition::Baseline,
                HorizontalAlignment::Center,
                FontColor::Transparent(BLACK),
                canvas,
            );
            y += line_h;
        }
        break;
    }

    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Method::Nearest,
        levels,
    );
}

/// Measured pixel width of `text` in `face` (0 if unmeasurable).
fn text_width(face: &FontRenderer, text: &str) -> i32 {
    face.get_rendered_dimensions(text, Point::zero(), VerticalPosition::Baseline)
        .ok()
        .and_then(|d| d.bounding_box)
        .map_or(0, |bb| bb.size.width as i32)
}

/// The setup screen: join QR plus instructions.
fn draw_setup(canvas: &mut Grey<'_>, levels: &mut [u8], qr_temp: &mut [u8], qr_out: &mut [u8]) {
    canvas.fill(255);
    let title = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let _ = title.render_aligned(
        "Set message",
        Point::new(6, 30),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let mut y = 56;
    for text in ["1. Scan to join", "2. Wait for the", "   form to pop up", "3. Type + send", "", AP_SSID, AP_PASS] {
        let _ = small.render_aligned(
            text,
            Point::new(6, y),
            VerticalPosition::Baseline,
            HorizontalAlignment::Left,
            FontColor::Transparent(BLACK),
            canvas,
        );
        y += 16;
    }
    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Method::Nearest,
        levels,
    );

    // Join QR (WIFI: format — phones join from the camera app).
    let mut payload = heapless::String::<96>::new();
    let _ = write!(payload, "WIFI:T:WPA;S:{};P:{};;", AP_SSID, AP_PASS);
    let qr = QrCode::encode_text(
        &payload,
        qr_temp,
        qr_out,
        QrCodeEcc::Medium,
        Version::MIN,
        Version::MAX,
        None,
        true,
    );
    if let Ok(qr) = qr {
        let size = qr.size();
        // Cap the code's width so it stays clear of the instruction column.
        let avail = (HEIGHT as i32 - 16).min(126);
        let scale = (avail / size).max(1);
        let px = size * scale;
        let x0 = WIDTH as i32 - px - 6;
        let y0 = (HEIGHT as i32 - px) / 2;
        for my in 0..size {
            for mx in 0..size {
                if qr.get_module(mx, my) {
                    dither::paint_level(
                        levels,
                        WIDTH,
                        Rectangle::new(
                            Point::new(x0 + mx * scale, y0 + my * scale),
                            Size::new(scale as u32, scale as u32),
                        ),
                        0,
                    );
                }
            }
        }
    }
}

// Rust guideline compliant 2026-08-30
