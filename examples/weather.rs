//! Weather station for the Badger 2350: geolocates by IP, fetches current
//! conditions from Open-Meteo over WiFi, and leaves them on the zero-power
//! e-paper, refreshing every 15 minutes.
//!
//! `cargo run --release --example weather --no-default-features --features badger`
//!
//! WiFi credentials live in the shared settings sector; if absent the badge
//! shows a setup screen and accepts, over its USB serial port:
//!
//! ```text
//! WIFI myssid my password with spaces
//! WIFI "ssid with spaces" my password
//! ```
//!
//! On every fetch cycle it also asks an NTP server for the time and sets the
//! battery-backed RTC — so running this once provisions the clock the TOTP
//! example uses. Plain HTTP only (ip-api.com and api.open-meteo.com both
//! serve it), so no TLS stack is needed.
//!
//! **A** fetches now; **B** re-enters WiFi setup. HOME held 2 s reboots to
//! BOOTSEL.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::{bind_interrupts, dma, pio, usb};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::Write as _;
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle, Triangle};
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::rtc::{DateTime, RtcRam};
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp::settings::{Settings, MAX_VAL};
use tufty_2350::bsp::wifi;
use tufty_2350::gfx::dither::{self, Method};
use tufty_2350::gfx::grey::Grey;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
/// HTTP response buffer.
static RESPONSE: ConstStaticCell<[u8; 8192]> = ConstStaticCell::new([0; 8192]);

const BLACK: Gray8 = Gray8::new(0);

/// Fetch cadence.
const REFRESH: Duration = Duration::from_secs(15 * 60);

/// One fetched report.
#[derive(Default)]
struct Report {
    /// Local-time offset from UTC in seconds (from the IP geolocation).
    utc_offset: i32,
    city: heapless::String<24>,
    temp: f32,
    hi: f32,
    lo: f32,
    wind: f32,
    humidity: f32,
    code: u16,
}

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
    spawner.spawn(bsp::usb::logger_task_with_input(usb_driver).unwrap());
    log::info!("weather booting");

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
    epd.set_speed(Speed::Slow);

    let mut rtc = RtcRam::new(p.I2C0, p.PIN_5, p.PIN_4);
    let mut settings = Settings::new(p.FLASH);

    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());
    let response = RESPONSE.take();

    // ---- WiFi credentials: stored, or provisioned over serial.
    let mut cred = [0u8; MAX_VAL];
    let mut cred_len = settings.get("wifi", &mut cred).map(<[u8]>::len).unwrap_or(0);
    if cred_len == 0 {
        cred_len = wifi_setup(&mut epd, &mut canvas, levels, &mut settings, &mut cred).await;
    }
    let (ssid, pass) = split_cred(&cred[..cred_len]);

    // ---- Radio up + join (one-shot; a failed join means bad credentials —
    // fall back to setup so the badge never wedges).
    draw_message(&mut canvas, levels, "Connecting", ssid);
    epd.present_levels(levels).await;
    let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
    let wifi = match wifi::connect(
        spawner, p.PIO0, Irqs, dma_ch, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, ssid, pass,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            log::warn!("join failed: {:?}", e);
            draw_message(&mut canvas, levels, "Join failed", "send WIFI <ssid> <pass>, then reboot");
            epd.present_levels(levels).await;
            // Accept new credentials forever; the user reboots after.
            loop {
                wifi_setup(&mut epd, &mut canvas, levels, &mut settings, &mut cred).await;
            }
        }
    };
    let stack = wifi.stack;

    // ---- Fetch/display loop.
    let mut geo: Option<(f32, f32, i32, heapless::String<24>)> = None;
    loop {
        // NTP first (also provisions the RTC for other apps).
        if let Some(unix) = sntp(stack).await {
            set_rtc_from_unix(&mut rtc, unix);
        }
        if geo.is_none() {
            geo = geolocate(stack, response).await;
        }
        let report = match &geo {
            Some((lat, lon, offset, city)) => {
                let mut r = fetch_weather(stack, response, *lat, *lon, city).await;
                if let Some(r) = &mut r {
                    r.utc_offset = *offset;
                }
                r
            }
            None => None,
        };
        match report {
            Some(r) => {
                let clock = rtc.read_datetime();
                draw_report(&mut canvas, levels, &r, clock.as_ref());
                epd.present_levels(levels).await;
            }
            None => {
                draw_message(&mut canvas, levels, "Fetch failed", "retrying in a minute");
                epd.present_levels(levels).await;
                Timer::after(Duration::from_secs(60)).await;
                continue;
            }
        }

        // Wait out the refresh interval, honouring buttons (A or the timer
        // both mean "fetch again"; B re-runs credential entry, which takes
        // effect after a reboot).
        let sleep = Timer::after(REFRESH);
        if let embassy_futures::select::Either::Second(ButtonEvent::Pressed(Button::B)) =
            embassy_futures::select::select(sleep, EVENTS.receive()).await
        {
            wifi_setup(&mut epd, &mut canvas, levels, &mut settings, &mut cred).await;
        }
    }
}

/// Splits the stored `ssid\0pass` credential blob.
fn split_cred(cred: &[u8]) -> (&str, &str) {
    let split = cred.iter().position(|&b| b == 0).unwrap_or(cred.len());
    let ssid = core::str::from_utf8(&cred[..split]).unwrap_or("");
    let pass = core::str::from_utf8(cred.get(split + 1..).unwrap_or(&[])).unwrap_or("");
    (ssid, pass)
}

/// Setup screen + serial loop for `WIFI <ssid> <password>` (password may
/// contain spaces; the ssid may not). Stores `ssid\0pass` and returns its
/// length.
async fn wifi_setup(
    epd: &mut Epd,
    canvas: &mut Grey<'_>,
    levels: &mut [u8],
    settings: &mut Settings,
    cred: &mut [u8; MAX_VAL],
) -> usize {
    draw_message(canvas, levels, "WiFi setup", "serial: WIFI <ssid> <password>");
    epd.present_levels(levels).await;
    log::info!("setup: send `WIFI <ssid> <password>`");
    let mut line = heapless::String::<128>::new();
    loop {
        let entered = bsp::usb::read_line(&mut line).await;
        let Some(rest) = entered
            .len()
            .checked_sub(5)
            .and_then(|_| entered.split_at_checked(5))
            .filter(|(head, _)| head.eq_ignore_ascii_case("WIFI "))
            .map(|(_, rest)| rest.trim())
        else {
            if !entered.is_empty() {
                log::warn!("unknown command: {}", entered);
            }
            continue;
        };
        // A quoted first token allows spaces in the SSID; passwords may
        // contain spaces either way (everything after the divider).
        let parsed = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.split_once('"').map(|(ssid, rest)| (ssid, rest.trim_start()))
        } else {
            rest.split_once(' ')
        };
        let Some((ssid, pass)) = parsed else {
            log::warn!("need both ssid and password (quote an ssid with spaces)");
            continue;
        };
        let (ssid, pass) = (ssid.trim(), pass.trim());
        let total = ssid.len() + 1 + pass.len();
        if ssid.is_empty() || total > MAX_VAL {
            log::warn!("credentials too long");
            continue;
        }
        cred[..ssid.len()].copy_from_slice(ssid.as_bytes());
        cred[ssid.len()] = 0;
        cred[ssid.len() + 1..total].copy_from_slice(pass.as_bytes());
        if settings.set("wifi", &cred[..total]) {
            log::info!("stored credentials for '{}'", ssid);
            return total;
        }
        log::warn!("flash write failed");
    }
}

/// SNTP over UDP: seconds since the Unix epoch, or `None` on any failure.
async fn sntp(stack: embassy_net::Stack<'static>) -> Option<u64> {
    let server = resolve(stack, "pool.ntp.org").await?;
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 128];
    let mut tx = [0u8; 128];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    sock.bind(0).ok()?;
    let mut packet = [0u8; 48];
    packet[0] = 0x1B; // LI=0, VN=3, mode=client
    let endpoint = IpEndpoint::new(server, 123);
    sock.send_to(&packet, endpoint).await.ok()?;
    let mut reply = [0u8; 48];
    let (n, _) = with_timeout(Duration::from_secs(5), sock.recv_from(&mut reply)).await.ok()?.ok()?;
    if n < 44 {
        return None;
    }
    let secs = u32::from_be_bytes(reply[40..44].try_into().ok()?);
    // NTP epoch (1900) to Unix epoch (1970).
    Some(u64::from(secs) - 2_208_988_800)
}

/// Writes a Unix timestamp into the RTC as UTC.
fn set_rtc_from_unix(rtc: &mut RtcRam, unix: u64) {
    // Days-to-civil for 2000..2099 (every 4th year leap).
    let days = unix / 86400;
    let secs = unix % 86400;
    let mut year = 1970;
    let mut remaining = days;
    loop {
        let len = if year % 4 == 0 { 366 } else { 365 };
        if remaining < len {
            break;
        }
        remaining -= len;
        year += 1;
    }
    let leap = year % 4 == 0;
    let months = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while remaining >= months[month] {
        remaining -= months[month];
        month += 1;
    }
    let dt = DateTime {
        year: year as u16,
        month: month as u8 + 1,
        day: remaining as u8 + 1,
        hour: (secs / 3600) as u8,
        minute: (secs / 60 % 60) as u8,
        second: (secs % 60) as u8,
    };
    if rtc.set_datetime(&dt) {
        log::info!("rtc set from ntp: {:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second);
    }
}

/// DNS A lookup.
async fn resolve(stack: embassy_net::Stack<'static>, host: &str) -> Option<IpAddress> {
    let addrs = stack.dns_query(host, DnsQueryType::A).await.ok()?;
    addrs.first().copied()
}

/// Plain-HTTP GET; returns the body slice inside `response`.
async fn http_get<'a>(
    stack: embassy_net::Stack<'static>,
    response: &'a mut [u8],
    host: &str,
    path: &str,
) -> Option<&'a [u8]> {
    let addr = match resolve(stack, host).await {
        Some(a) => a,
        None => {
            log::warn!("http {}: dns failed", host);
            return None;
        }
    };
    let mut rx = [0u8; 4096];
    let mut tx = [0u8; 1024];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(Duration::from_secs(10)));
    if let Err(e) = sock.connect((addr, 80)).await {
        log::warn!("http {}: connect {:?}: {:?}", host, addr, e);
        return None;
    }
    let mut req = heapless::String::<512>::new();
    let _ = write!(req, "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: badger2350\r\n\r\n", path, host);
    if let Err(e) = sock.write_all(req.as_bytes()).await {
        log::warn!("http {}: write: {:?}", host, e);
        return None;
    }
    let mut n = 0usize;
    loop {
        match with_timeout(Duration::from_secs(10), sock.read(&mut response[n..])).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(got)) => n += got,
            Ok(Err(e)) => {
                log::warn!("http {}: read after {} bytes: {:?}", host, n, e);
                break;
            }
        }
        if n == response.len() {
            break;
        }
    }
    let all = &response[..n];
    let status = all.split(|&b| b == b'\r').next().unwrap_or(&[]);
    log::info!(
        "http {}: {} bytes, {}",
        host,
        n,
        core::str::from_utf8(&status[..status.len().min(32)]).unwrap_or("?")
    );
    let header_end = find(all, b"\r\n\r\n")?;
    Some(&all[header_end + 4..])
}

/// Where `needle` starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A number following `"key":` in a JSON body (no nesting awareness — fine
/// for these flat responses).
fn json_number(body: &str, key: &str) -> Option<f32> {
    let mut pat = heapless::String::<40>::new();
    let _ = write!(pat, "\"{}\":", key);
    let at = body.find(pat.as_str())? + pat.len();
    let rest = body[at..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.' || c == 'e' || c == 'E' || c == '+'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// A string following `"key":"` in a JSON body.
fn json_string<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let mut pat = heapless::String::<40>::new();
    let _ = write!(pat, "\"{}\":\"", key);
    let at = body.find(pat.as_str())? + pat.len();
    let rest = &body[at..];
    Some(&rest[..rest.find('"')?])
}

/// IP geolocation: (lat, lon, utc offset seconds, city).
async fn geolocate(
    stack: embassy_net::Stack<'static>,
    response: &mut [u8],
) -> Option<(f32, f32, i32, heapless::String<24>)> {
    let body = http_get(stack, response, "ip-api.com", "/json/?fields=status,city,lat,lon,offset").await?;
    let body = core::str::from_utf8(body).ok()?;
    let lat = json_number(body, "lat")?;
    let lon = json_number(body, "lon")?;
    let offset = json_number(body, "offset").unwrap_or(0.0) as i32;
    let mut city = heapless::String::new();
    let _ = city.push_str(json_string(body, "city").unwrap_or("Somewhere"));
    log::info!("located: {} ({}, {}) utc{:+}", city, lat, lon, offset / 3600);
    Some((lat, lon, offset, city))
}

/// Current conditions + today's range from Open-Meteo.
async fn fetch_weather(
    stack: embassy_net::Stack<'static>,
    response: &mut [u8],
    lat: f32,
    lon: f32,
    city: &heapless::String<24>,
) -> Option<Report> {
    let mut path = heapless::String::<256>::new();
    let _ = write!(
        path,
        "/v1/forecast?latitude={:.4}&longitude={:.4}&current=temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m&daily=temperature_2m_max,temperature_2m_min&forecast_days=1",
        lat, lon
    );
    let body = http_get(stack, response, "api.open-meteo.com", &path).await?;
    let body = core::str::from_utf8(body).ok()?;
    // Anchor inside the `current` object: the response also carries a
    // `current_units` block whose values are strings (degree symbols and
    // the like), and it comes first — a naive whole-body key search finds
    // those instead and fails to parse a number.
    let current = &body[body.find("\"current\":")?..];
    let mut report = Report {
        temp: json_number(current, "temperature_2m")?,
        humidity: json_number(current, "relative_humidity_2m")?,
        wind: json_number(current, "wind_speed_10m")?,
        code: json_number(current, "weather_code")? as u16,
        ..Default::default()
    };
    // Daily arrays: first value after the key.
    let hi_at = body.find("temperature_2m_max\":[")? + "temperature_2m_max\":[".len();
    let lo_at = body.find("temperature_2m_min\":[")? + "temperature_2m_min\":[".len();
    report.hi = body[hi_at..].split(&[',', ']'][..]).next()?.parse().ok()?;
    report.lo = body[lo_at..].split(&[',', ']'][..]).next()?.parse().ok()?;
    let _ = report.city.push_str(city);
    log::info!("weather: {}C code {}", report.temp, report.code);
    Some(report)
}

/// WMO weather code, roughly.
fn describe(code: u16) -> &'static str {
    match code {
        0 => "Clear",
        1 | 2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51..=57 => "Drizzle",
        61..=67 => "Rain",
        71..=77 => "Snow",
        80..=82 => "Showers",
        85 | 86 => "Snow showers",
        95..=99 => "Thunderstorm",
        _ => "Weather",
    }
}

/// The weather icon (Open Iconic, MIT), 64 px, drawn in the right-hand
/// negative space; snow and thunder are composed from the closest glyphs.
fn draw_icon(canvas: &mut Grey<'_>, r: &Report, local_hour: u8) {
    let icons = FontRenderer::new::<fonts::u8g2_font_open_iconic_weather_8x_t>();
    let day = (7..19).contains(&local_hour);
    // 64 cloud, 65 sun+cloud, 66 moon, 67 rain, 68 sun.
    let (glyph, snow, bolt) = match r.code {
        0 => (if day { '\u{44}' } else { '\u{42}' }, false, false),
        1 | 2 => (if day { '\u{41}' } else { '\u{42}' }, false, false),
        3 | 45 | 48 => ('\u{40}', false, false),
        51..=67 | 80..=82 => ('\u{43}', false, false),
        71..=77 | 85 | 86 => ('\u{40}', true, false),
        95..=99 => ('\u{43}', false, true),
        _ => ('\u{40}', false, false),
    };
    // Vertically center the glyph's real bounding box in the free space
    // right of the temperature (icon shapes sit at different heights in
    // their em box, so anchor-based centering lands visibly low).
    // The icon column is clear of text from the top edge to the rule at
    // y=140 (the city line stays left of it), so center in that full span.
    let target = Point::new(WIDTH as i32 - 46, 72);
    let mut at = target;
    if let Ok(Some(bb)) = icons
        .get_rendered_dimensions_aligned(glyph, target, VerticalPosition::Center, HorizontalAlignment::Center)
    {
        at = target + (target - bb.center());
    }
    let center = at;
    let _ = icons.render_aligned(
        glyph,
        center,
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(BLACK),
        canvas,
    );
    if snow {
        // Snowflakes: asterisks under the cloud.
        let flakes = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
        let _ = flakes.render_aligned(
            "* * *",
            center + Point::new(0, 44),
            VerticalPosition::Baseline,
            HorizontalAlignment::Center,
            FontColor::Transparent(BLACK),
            canvas,
        );
    }
    if bolt {
        // Lightning: two filled triangles making a zigzag bolt.
        let b = center + Point::new(4, 26);
        let _ = Triangle::new(b + Point::new(-2, 0), b + Point::new(8, 0), b + Point::new(-8, 16))
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(canvas);
        let _ = Triangle::new(b + Point::new(-2, 6), b + Point::new(8, 6), b + Point::new(12, -10))
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(canvas);
    }
}

/// The report screen.
fn draw_report(canvas: &mut Grey<'_>, levels: &mut [u8], r: &Report, clock: Option<&DateTime>) {
    canvas.fill(255);
    let big = FontRenderer::new::<fonts::u8g2_font_logisoso42_tf>();
    let title = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
    let small = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();

    let _ = title.render_aligned(
        r.city.as_str(),
        Point::new(6, 30),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let mut temp = heapless::String::<16>::new();
    let _ = write!(temp, "{:.1}\u{00b0}C", r.temp);
    let _ = big.render_aligned(
        temp.as_str(),
        Point::new(6, 92),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let _ = title.render_aligned(
        describe(r.code),
        Point::new(6, 128),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    let _ = Line::new(Point::new(6, 140), Point::new(WIDTH as i32 - 6, 140))
        .into_styled(PrimitiveStyle::with_stroke(BLACK, 1))
        .draw(canvas);
    let mut foot = heapless::String::<96>::new();
    let _ = write!(
        foot,
        "H {:.0}\u{00b0}  L {:.0}\u{00b0}   wind {:.0} km/h   rh {:.0}%",
        r.hi, r.lo, r.wind, r.humidity
    );
    let _ = small.render_aligned(
        foot.as_str(),
        Point::new(6, 158),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(BLACK),
        canvas,
    );
    // Local time from the RTC (UTC) plus the geolocated offset.
    let local = clock.map(|t| {
        let secs = (i64::from(t.unix() as u32) + i64::from(r.utc_offset)).rem_euclid(86400);
        ((secs / 3600) as u8, (secs / 60 % 60) as u8)
    });
    draw_icon(canvas, r, local.map_or(12, |(h, _)| h));
    if let Some((hour, minute)) = local {
        let mut upd = heapless::String::<32>::new();
        let _ = write!(upd, "updated {:02}:{:02}", hour, minute);
        let _ = small.render_aligned(
            upd.as_str(),
            Point::new(WIDTH as i32 - 6, 172),
            VerticalPosition::Baseline,
            HorizontalAlignment::Right,
            FontColor::Transparent(BLACK),
            canvas,
        );
    }
    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Method::Nearest,
        levels,
    );
}

/// A title + one-liner status screen.
fn draw_message(canvas: &mut Grey<'_>, levels: &mut [u8], head: &str, detail: &str) {
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
        Method::Nearest,
        levels,
    );
}

// Rust guideline compliant 2026-08-30
