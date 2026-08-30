//! Chess on the Badger 2350, against Stockfish at <https://chess-api.com>
//! (HTTPS via TLS 1.3), with move selection on five buttons and the board
//! living on zero-power e-paper.
//!
//! `cargo run --release --example chess --no-default-features --features badger`
//!
//! You play White. **A/C/UP/DOWN** move the cursor over your movable pieces
//! (spatially, with wrap-around); **B** selects, after which the arrows jump
//! between that piece's legal destinations and **B** commits (**B** back on
//! the piece deselects; promotions auto-queen). Cursor movement uses partial
//! panel refreshes (only the touched column bands re-scan — no full-screen
//! blink); committed moves get a full TURBO refresh, with a SLOW clean every
//! several moves. Hold **B** two seconds for a new game.
//!
//! The game (FEN) is saved to the settings sector after every move, so it
//! survives power-off and reflashing. WiFi credentials come from the same
//! sector (provision with the `weather` example, or over serial here); the
//! radio joins lazily for the engine's reply and leaves after five idle
//! minutes — an engaged player never waits, an abandoned board goes quiet.
//! With no credentials stored, it degrades to two-player hot-seat.
//!
//! Legal moves, check and mate come from `cozy-chess` on-device; the server
//! only picks Black's replies (depth 12, with its eval shown).

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cozy_chess::{Board, Color, File, GameStatus, Move, Piece, Square};
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::{bind_interrupts, dma, pio, usb};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use static_cell::ConstStaticCell;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use tufty_2350::bsp;
use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::epd::{Epd, Speed};
use tufty_2350::bsp::leds::RearLeds;
use tufty_2350::bsp::screen::{HEIGHT, WIDTH};
use tufty_2350::bsp::settings::{Settings, MAX_VAL};
use tufty_2350::bsp::wifi::{self, Wifi};
use tufty_2350::gfx::dither::{self, Method as Dither};
use tufty_2350::gfx::grey::Grey;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
});

static CANVAS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
static LEVELS: ConstStaticCell<[u8; WIDTH * HEIGHT]> = ConstStaticCell::new([0; WIDTH * HEIGHT]);
/// TLS record buffers (a full 16 KiB record must fit) and the HTTP response.
static TLS_RX: ConstStaticCell<[u8; 16640]> = ConstStaticCell::new([0; 16640]);
static TLS_TX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static HTTP_RX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static TCP_STATE: ConstStaticCell<TcpClientState<1, 4096, 4096>> = ConstStaticCell::new(TcpClientState::new());

const BLACK: Gray8 = Gray8::new(0);
/// Dark squares: quantizes to the light-grey panel level.
const DARK_SQ: Gray8 = Gray8::new(190);

/// Board geometry: 8 x 20 px squares with a 2 px frame.
const SQ: i32 = 20;
const BX: i32 = 2;
const BY: i32 = 8;

/// Radio idle timeout before leaving the network.
const WIFI_IDLE: Duration = Duration::from_secs(5 * 60);

/// SLOW clean refresh every this many committed moves.
const CLEAN_EVERY: u32 = 8;

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
    log::info!("chess booting");

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

    let mut settings = Settings::new(p.FLASH);
    let levels = LEVELS.take();
    let mut canvas = Grey::new(WIDTH, HEIGHT, CANVAS.take());

    // ---- Restore the saved game, if any.
    let mut fen_buf = [0u8; MAX_VAL];
    let mut board = settings
        .get("chess", &mut fen_buf)
        .and_then(|f| core::str::from_utf8(f).ok())
        .and_then(|f| Board::from_fen(f, false).ok())
        .unwrap_or_default();

    // ---- WiFi: lazy. Credentials looked up now, joined on first engine call.
    let mut cred = [0u8; MAX_VAL];
    let cred_len = settings.get("wifi", &mut cred).map(<[u8]>::len).unwrap_or(0);
    let mut net = if cred_len > 0 {
        let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
        let (ssid, pass) = split_cred(&cred[..cred_len]);
        match wifi::connect(spawner, p.PIO0, Irqs, dma_ch, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, ssid, pass).await
        {
            Ok(w) => Some(Net { wifi: w, joined: true, last_use: Instant::now() }),
            Err(e) => {
                log::warn!("wifi unavailable ({:?}); hot-seat mode", e);
                None
            }
        }
    } else {
        log::info!("no wifi credentials; hot-seat mode (provision via the weather example)");
        None
    };
    let tcp_state = TCP_STATE.take();
    let (tls_rx, tls_tx, http_rx) = (TLS_RX.take(), TLS_TX.take(), HTTP_RX.take());

    let mut ui = Ui { cursor: Square::E2, selected: None, eval: None, thinking: false, note: heapless::String::new() };
    let mut moves_done = 0u32;
    epd.set_speed(Speed::Slow);
    full_draw(&mut canvas, levels, &board, &ui, net.as_ref());
    epd.present_levels(levels).await;

    loop {
        // ---- Engine (Black) to move?
        if board.side_to_move() == Color::Black && board.status() == GameStatus::Ongoing {
            if let Some(net) = &mut net {
                ui.thinking = true;
                full_draw(&mut canvas, levels, &board, &ui, Some(net));
                epd.set_speed(Speed::Turbo);
                epd.present_levels(levels).await;

                ensure_joined(net, &cred[..cred_len]).await;
                let reply = engine_move(net, tcp_state, tls_rx, tls_tx, http_rx, &board).await;
                net.last_use = Instant::now();
                ui.thinking = false;
                match reply {
                    Ok((mv, eval, mate)) => {
                        board.play(mv);
                        save_fen(&mut settings, &board);
                        ui.eval = Some((eval, mate));
                        ui.note.clear();
                        moves_done += 1;
                    }
                    Err(why) => {
                        ui.note.clear();
                        let _ = write!(ui.note, "engine: {}", why);
                    }
                }
                sync_cursor(&board, &mut ui);
                epd.set_speed(if moves_done.is_multiple_of(CLEAN_EVERY) { Speed::Slow } else { Speed::Turbo });
                full_draw(&mut canvas, levels, &board, &ui, Some(net));
                epd.present_levels(levels).await;
                if ui.note.is_empty() {
                    continue;
                }
            }
            // Hot-seat (or engine offline): the player moves Black too.
        }

        // ---- Wait for input; also expire the radio when idle.
        let event = loop {
            let timeout = Timer::after(Duration::from_secs(30));
            match embassy_futures::select::select(EVENTS.receive(), timeout).await {
                embassy_futures::select::Either::First(ev) => break ev,
                embassy_futures::select::Either::Second(()) => {
                    if let Some(net) = &mut net {
                        if net.joined && net.last_use.elapsed() > WIFI_IDLE {
                            net.wifi.leave().await;
                            net.joined = false;
                            full_draw(&mut canvas, levels, &board, &ui, Some(net));
                            epd.set_speed(Speed::Turbo);
                            epd.present_levels(levels).await;
                        }
                    }
                }
            }
        };

        let ButtonEvent::Pressed(button) = event else { continue };

        // An engine failure note showing while Black is to move: any press
        // clears it and retries the request (transient network failures
        // should cost one button, not a hot-seat move).
        if !ui.note.is_empty() && board.side_to_move() == Color::Black && net.is_some() {
            ui.note.clear();
            continue;
        }

        // Hold B for a new game.
        if button == Button::B && held_long(Button::B).await {
            board = Board::default();
            save_fen(&mut settings, &board);
            ui = Ui { cursor: Square::E2, selected: None, eval: None, thinking: false, note: heapless::String::new() };
            moves_done = 0;
            epd.set_speed(Speed::Slow);
            full_draw(&mut canvas, levels, &board, &ui, net.as_ref());
            epd.present_levels(levels).await;
            continue;
        }

        if board.status() != GameStatus::Ongoing {
            continue; // game over: only new-game hold applies
        }

        let old_cursor = ui.cursor;
        let old_sel = ui.selected;
        let mut committed: Option<Move> = None;

        match (button, ui.selected) {
            (Button::B, None) => {
                if movable_squares(&board).contains(&ui.cursor) {
                    ui.selected = Some(ui.cursor);
                }
            }
            (Button::B, Some(sel)) => {
                if ui.cursor == sel {
                    ui.selected = None; // deselect
                } else if let Some(mv) = move_to(&board, sel, ui.cursor) {
                    committed = Some(mv);
                }
            }
            (dir, None) => {
                let list = movable_squares(&board);
                if let Some(next) = spatial_next(ui.cursor, &list, dir) {
                    ui.cursor = next;
                }
            }
            (dir, Some(sel)) => {
                let mut list = target_squares(&board, sel);
                let _ = list.push(sel); // the deselect spot
                if let Some(next) = spatial_next(ui.cursor, &list, dir) {
                    ui.cursor = next;
                }
            }
        }

        if let Some(mv) = committed {
            board.play(mv);
            save_fen(&mut settings, &board);
            ui.selected = None;
            ui.eval = None;
            moves_done += 1;
            sync_cursor(&board, &mut ui);
            epd.set_speed(Speed::Turbo);
            full_draw(&mut canvas, levels, &board, &ui, net.as_ref());
            epd.present_levels(levels).await;
        } else if ui.cursor != old_cursor || ui.selected != old_sel {
            // Cursor/selection change. Full TURBO refresh: partial band
            // refreshes were tried first and are faster, but every partial
            // scan slightly fades the undriven rest of the panel — with the
            // cursor moving constantly, the board washed out. The full
            // refresh redraws everything crisp each time.
            full_draw(&mut canvas, levels, &board, &ui, net.as_ref());
            epd.set_speed(Speed::Turbo);
            epd.present_levels(levels).await;
        }
    }
}

/// The network handle plus join/idle bookkeeping.
struct Net {
    wifi: Wifi,
    joined: bool,
    last_use: Instant,
}

/// UI state.
struct Ui {
    cursor: Square,
    selected: Option<Square>,
    /// Engine's view after its move: (eval in pawns, moves-to-mate if any).
    eval: Option<(f32, Option<i32>)>,
    thinking: bool,
    note: heapless::String<32>,
}

/// Splits the stored `ssid\0pass` credential blob.
fn split_cred(cred: &[u8]) -> (&str, &str) {
    let split = cred.iter().position(|&b| b == 0).unwrap_or(cred.len());
    (
        core::str::from_utf8(&cred[..split]).unwrap_or(""),
        core::str::from_utf8(cred.get(split + 1..).unwrap_or(&[])).unwrap_or(""),
    )
}

/// Rejoins if the idle timeout disconnected us.
async fn ensure_joined(net: &mut Net, cred: &[u8]) {
    if !net.joined {
        let (ssid, pass) = split_cred(cred);
        net.joined = net.wifi.rejoin(ssid, pass).await;
    }
    net.last_use = Instant::now();
}

/// True if `button` stays held for two seconds (consumes its Release).
async fn held_long(button: Button) -> bool {
    let deadline = Timer::after(Duration::from_secs(2));
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

/// Persists the position.
fn save_fen(settings: &mut Settings, board: &Board) {
    let mut fen = heapless::String::<96>::new();
    let _ = write!(fen, "{}", board);
    if !settings.set("chess", fen.as_bytes()) {
        log::warn!("could not save game");
    }
}

/// Squares of the side to move that have at least one legal move.
fn movable_squares(board: &Board) -> heapless::Vec<Square, 16> {
    let mut list = heapless::Vec::new();
    board.generate_moves(|moves| {
        let _ = list.push(moves.from);
        false
    });
    list
}

/// Legal destination squares for the piece on `from` (promotions collapsed).
fn target_squares(board: &Board, from: Square) -> heapless::Vec<Square, 32> {
    let mut list: heapless::Vec<Square, 32> = heapless::Vec::new();
    board.generate_moves_for(from.bitboard(), |moves| {
        for mv in moves {
            if !list.contains(&mv.to) {
                let _ = list.push(mv.to);
            }
        }
        false
    });
    list
}

/// The legal move `from -> to` (queening if it is a promotion).
fn move_to(board: &Board, from: Square, to: Square) -> Option<Move> {
    let mut found = None;
    board.generate_moves_for(from.bitboard(), |moves| {
        for mv in moves {
            if mv.to == to && (mv.promotion.is_none() || mv.promotion == Some(Piece::Queen)) {
                found = Some(mv);
                return true;
            }
        }
        false
    });
    found
}

/// After the position changes, park the cursor on a movable piece.
fn sync_cursor(board: &Board, ui: &mut Ui) {
    let list = movable_squares(board);
    if !list.contains(&ui.cursor) {
        if let Some(&sq) = list.first() {
            ui.cursor = sq;
        }
    }
}

/// The nearest square from `cur` in `dir` among `list`, wrapping around the
/// board edge (distance along the pressed axis dominates; the cross axis
/// breaks ties).
fn spatial_next(cur: Square, list: &[Square], dir: Button) -> Option<Square> {
    let (cf, cr) = (cur.file() as i32, cur.rank() as i32);
    let mut best: Option<(i32, Square)> = None;
    for &sq in list {
        if sq == cur {
            continue;
        }
        let (f, r) = (sq.file() as i32, sq.rank() as i32);
        let (main, cross) = match dir {
            Button::A => ((cf - f).rem_euclid(8), (cr - r).abs()),
            Button::C => ((f - cf).rem_euclid(8), (cr - r).abs()),
            Button::Up => ((r - cr).rem_euclid(8), (cf - f).abs()),
            Button::Down => ((cr - r).rem_euclid(8), (cf - f).abs()),
            _ => return None,
        };
        if main == 0 {
            continue; // not in that direction (same row/column handled by cross==...)
        }
        let score = main * 16 + cross;
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, sq));
        }
    }
    best.map(|(_, sq)| sq)
}

/// Asks chess-api.com for Black's move. Returns (move, eval, mate-in), or
/// a short failure reason for the panel.
async fn engine_move(
    net: &mut Net,
    tcp_state: &'static TcpClientState<1, 4096, 4096>,
    tls_rx: &mut [u8],
    tls_tx: &mut [u8],
    http_rx: &mut [u8],
    board: &Board,
) -> Result<(Move, f32, Option<i32>), &'static str> {
    if !net.joined {
        return Err("no wifi");
    }
    // chess-api.com's validator rejects ANY FEN carrying an en-passant
    // square (verified against known-legal positions), so blank that field.
    // cozy-chess only ever emits one when an ep capture is truly legal, so
    // the only cost is the engine occasionally not seeing that rare option.
    let mut fen = heapless::String::<96>::new();
    let _ = write!(fen, "{}", board);
    let mut body = heapless::String::<160>::new();
    let _ = body.push_str("{\"fen\":\"");
    for (i, field) in fen.split_ascii_whitespace().enumerate() {
        if i > 0 {
            let _ = body.push(' ');
        }
        let _ = body.push_str(if i == 3 { "-" } else { field });
    }
    let _ = body.push_str("\",\"depth\":12}");

    let tcp = TcpClient::new(net.wifi.stack, tcp_state);
    let dns = DnsSocket::new(net.wifi.stack);
    let tls = TlsConfig::new(RoscRng.next_u64(), tls_rx, tls_tx, TlsVerify::None);
    let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);

    let mut request = match client.request(Method::POST, "https://chess-api.com/v1").await {
        Ok(r) => r
            .headers(&[("Content-Type", "application/json")])
            .body(body.as_bytes()),
        Err(e) => {
            log::warn!("engine: connect failed: {:?}", e);
            return Err("connect fail");
        }
    };
    let response = match request.send(http_rx).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("engine: request failed: {:?}", e);
            return Err("send fail");
        }
    };
    let status = response.status;
    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(e) => {
            log::warn!("engine: read failed: {:?}", e);
            return Err("read fail");
        }
    };
    let text = core::str::from_utf8(body).map_err(|_| "bad utf8")?;
    log::info!("engine http {:?} ({} bytes)", status, text.len());
    let Some(uci) = json_string(text, "move") else {
        // Log enough of the body to see what the API actually said.
        log::warn!("engine: no move in reply: {}", &text[..text.len().min(300)]);
        return Err("no move");
    };
    let eval = json_number(text, "eval").unwrap_or(0.0);
    let mate = json_number(text, "mate").map(|m| m as i32);
    let mv = parse_engine_move(board, uci).ok_or("bad move")?;
    log::info!("engine: {} (eval {})", uci, eval);
    Ok((mv, eval, mate))
}

/// Parses a UCI move, translating standard castling (e8g8) into
/// cozy-chess's king-takes-rook form (e8h8), and verifies legality.
fn parse_engine_move(board: &Board, uci: &str) -> Option<Move> {
    let mut mv: Move = uci.parse().ok()?;
    if board.piece_on(mv.from) == Some(Piece::King) {
        let (ff, tf) = (mv.from.file() as i32, mv.to.file() as i32);
        if (ff - tf).abs() == 2 {
            let rook_file = if tf > ff { File::H } else { File::A };
            mv.to = Square::new(rook_file, mv.to.rank());
        }
    }
    let mut clone = board.clone();
    if clone.try_play(mv).is_err() {
        log::warn!("engine sent illegal move {}", uci);
        return None;
    }
    Some(mv)
}

/// A number following `"key":` (fails on `null`).
fn json_number(body: &str, key: &str) -> Option<f32> {
    let mut pat = heapless::String::<40>::new();
    let _ = write!(pat, "\"{}\":", key);
    let at = body.find(pat.as_str())? + pat.len();
    let rest = body[at..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// A string value for `key`, tolerating pretty-printed JSON (whitespace
/// after the colon — chess-api.com pretty-prints its responses).
fn json_string<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let mut pat = heapless::String::<40>::new();
    let _ = write!(pat, "\"{}\":", key);
    let at = body.find(pat.as_str())? + pat.len();
    let rest = body[at..].trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// Glyphs (queen first) for the `color` pieces no longer on the board.
fn captured_glyphs(board: &Board, color: Color) -> heapless::String<64> {
    const START: [(Piece, u32); 5] =
        [(Piece::Queen, 1), (Piece::Rook, 2), (Piece::Bishop, 2), (Piece::Knight, 2), (Piece::Pawn, 8)];
    let mut out = heapless::String::new();
    for (piece, start) in START {
        let alive = (board.pieces(piece) & board.colors(color)).len();
        for _ in alive..start {
            let _ = out.push(glyph(piece, color));
        }
    }
    out
}

/// Unifont chess glyph for a piece (white pieces outlined, black filled).
fn glyph(piece: Piece, color: Color) -> char {
    let base = match piece {
        Piece::King => 0x2654,
        Piece::Queen => 0x2655,
        Piece::Rook => 0x2656,
        Piece::Bishop => 0x2657,
        Piece::Knight => 0x2658,
        Piece::Pawn => 0x2659,
    };
    let code = if color == Color::White { base } else { base + 6 };
    char::from_u32(code).unwrap_or('?')
}

/// Screen rect of a square (White at the bottom).
fn square_rect(sq: Square) -> Rectangle {
    let x = BX + (sq.file() as i32) * SQ;
    let y = BY + (7 - sq.rank() as i32) * SQ;
    Rectangle::new(Point::new(x, y), Size::new(SQ as u32, SQ as u32))
}

/// Draws everything into `levels` (board, cursor, side panel).
fn full_draw(canvas: &mut Grey<'_>, levels: &mut [u8], board: &Board, ui: &Ui, net: Option<&Net>) {
    canvas.fill(255);
    let pieces_font = FontRenderer::new::<fonts::u8g2_font_unifont_t_76>();

    // Board squares + pieces.
    for &sq in &Square::ALL {
        let rect = square_rect(sq);
        let dark = (sq.file() as i32 + sq.rank() as i32) % 2 == 0;
        if dark {
            let _ = canvas.fill_solid(&rect, DARK_SQ);
        }
        if let (Some(piece), Some(color)) = (board.piece_on(sq), board.color_on(sq)) {
            let center = rect.top_left + Point::new(SQ / 2, SQ / 2);
            let _ = pieces_font.render_aligned(
                glyph(piece, color),
                center + Point::new(0, 7),
                VerticalPosition::Baseline,
                HorizontalAlignment::Center,
                FontColor::Transparent(BLACK),
                canvas,
            );
        }
    }
    // Frame.
    let _ = Rectangle::new(Point::new(BX - 2, BY - 2), Size::new((8 * SQ + 4) as u32, (8 * SQ + 4) as u32))
        .into_styled(PrimitiveStyle::with_stroke(BLACK, 2))
        .draw(canvas);

    // Selection marker: corner ticks; legal targets: a small centered dot.
    if let Some(sel) = ui.selected {
        let r = square_rect(sel);
        let _ = r.into_styled(PrimitiveStyle::with_stroke(BLACK, 1)).draw(canvas);
        for to in target_squares(board, sel) {
            let c = square_rect(to).top_left + Point::new(SQ / 2, SQ / 2);
            let _ = Rectangle::new(c - Point::new(1, 1), Size::new(3, 3))
                .into_styled(PrimitiveStyle::with_fill(BLACK))
                .draw(canvas);
        }
    }
    // Cursor: a bold border.
    let _ = square_rect(ui.cursor)
        .into_styled(PrimitiveStyle::with_stroke(BLACK, 2))
        .draw(canvas);

    // ---- Side panel.
    let panel_x = BX + 8 * SQ + 8;
    let title = FontRenderer::new::<fonts::u8g2_font_crox1h_tf>();
    let mut y = 20;
    let line = |canvas: &mut Grey<'_>, text: &str, y: &mut i32| {
        let _ = title.render_aligned(
            text,
            Point::new(panel_x, *y),
            VerticalPosition::Baseline,
            HorizontalAlignment::Left,
            FontColor::Transparent(BLACK),
            canvas,
        );
        *y += 16;
    };
    let status = board.status();
    let mut buf = heapless::String::<32>::new();
    match status {
        GameStatus::Won => {
            // The side to move is the one who got mated.
            let winner = if board.side_to_move() == Color::White { "Black" } else { "White" };
            let _ = write!(buf, "{} wins!", winner);
        }
        GameStatus::Drawn => {
            let _ = buf.push_str("Draw");
        }
        GameStatus::Ongoing => {
            if ui.thinking {
                let _ = buf.push_str("thinking...");
            } else if board.side_to_move() == Color::White {
                let _ = buf.push_str(if !board.checkers().is_empty() { "check!" } else { "your move" });
            } else {
                let _ = buf.push_str("black moves");
            }
        }
    }
    line(canvas, buf.as_str(), &mut y);

    if let Some((eval, mate)) = ui.eval {
        buf.clear();
        match mate {
            Some(m) => {
                let _ = write!(buf, "mate in {}", m.abs());
            }
            None => {
                let _ = write!(buf, "eval {:+.1}", eval);
            }
        }
        line(canvas, buf.as_str(), &mut y);
    }
    if !ui.note.is_empty() {
        line(canvas, ui.note.as_str(), &mut y);
    }

    // Captured pieces: your trophies (Black's losses, filled glyphs) above
    // Black's (your losses, outlined), wrapped five to a row.
    y = y.max(58);
    for color in [Color::Black, Color::White] {
        let taken = captured_glyphs(board, color);
        let mut chars = taken.chars().peekable();
        while chars.peek().is_some() {
            let mut row = heapless::String::<24>::new();
            for _ in 0..5 {
                if let Some(c) = chars.next() {
                    let _ = row.push(c);
                }
            }
            let _ = pieces_font.render_aligned(
                row.as_str(),
                Point::new(panel_x, y),
                VerticalPosition::Baseline,
                HorizontalAlignment::Left,
                FontColor::Transparent(BLACK),
                canvas,
            );
            y += 17;
        }
    }

    y = 130;
    match net {
        Some(n) if n.joined => line(canvas, "wifi: on", &mut y),
        Some(_) => line(canvas, "wifi: zzz", &mut y),
        None => line(canvas, "hot-seat", &mut y),
    }
    line(canvas, "B = select", &mut y);
    line(canvas, "hold B = new", &mut y);

    dither::quantize(
        canvas,
        Rectangle::new(Point::zero(), Size::new(WIDTH as u32, HEIGHT as u32)),
        Dither::Nearest,
        levels,
    );
}

// Rust guideline compliant 2026-08-30
