//! Simon-style memory game built on the five front buttons.
//!
//! Watch the growing sequence flash on screen, then repeat it with the physical
//! buttons. The on-screen pads mirror the button layout: A / B / C along the
//! bottom edge (matching the buttons below the screen) and up / down on the
//! right edge (matching the side buttons). Rear LEDs echo every step.
//!
//! Rendering is state-driven: the scene is redrawn and presented on every
//! change rather than on a fixed tick — a memory game has no continuous motion,
//! and this keeps the main loop trivially simple.

use embassy_rp::clocks::RoscRng;
use embassy_time::{with_timeout, Duration, Timer};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X13};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::Text;
use heapless::Vec;

use crate::bsp::buttons::{Button, ButtonEvent, EVENTS};
use crate::bsp::display::Display;
use crate::bsp::leds::{cue, LedCue};
use crate::bsp::rtc::RtcRam;
use crate::gfx::FrameBuffer;

/// Longest sequence the game can hold; reaching it is a win by exhaustion.
const MAX_SEQUENCE: usize = 64;

/// How long the player has for each press before the round is failed.
const INPUT_TIMEOUT: Duration = Duration::from_secs(5);

/// The five playable pads, one per front button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pad {
    A,
    B,
    C,
    Up,
    Down,
}

const PADS: [Pad; 5] = [Pad::A, Pad::B, Pad::C, Pad::Up, Pad::Down];

impl Pad {
    fn from_button(button: Button) -> Option<Self> {
        match button {
            Button::A => Some(Self::A),
            Button::B => Some(Self::B),
            Button::C => Some(Self::C),
            Button::Up => Some(Self::Up),
            Button::Down => Some(Self::Down),
            Button::Home => None,
        }
    }

    /// Screen rectangle, mirroring the physical button positions.
    fn rect(self) -> Rectangle {
        match self {
            Self::A => Rectangle::new(Point::new(10, 165), Size::new(90, 65)),
            Self::B => Rectangle::new(Point::new(105, 165), Size::new(90, 65)),
            Self::C => Rectangle::new(Point::new(200, 165), Size::new(90, 65)),
            Self::Up => Rectangle::new(Point::new(245, 15), Size::new(65, 60)),
            Self::Down => Rectangle::new(Point::new(245, 85), Size::new(65, 60)),
        }
    }

    fn color(self) -> Rgb565 {
        match self {
            Self::A => Rgb565::new(31, 12, 6),   // red
            Self::B => Rgb565::new(31, 48, 0),   // amber
            Self::C => Rgb565::new(6, 50, 10),   // green
            Self::Up => Rgb565::new(4, 30, 31),  // blue
            Self::Down => Rgb565::new(27, 8, 29), // magenta
        }
    }

    /// Dimmed (idle) variant of the pad color.
    fn dim_color(self) -> Rgb565 {
        let c = self.color();
        Rgb565::new(c.r() / 4, c.g() / 4, c.b() / 4)
    }

    fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::Up => "UP",
            Self::Down => "DN",
        }
    }
}

/// Everything the scene renderer needs for one redraw.
struct Scene<'a> {
    lit: Option<Pad>,
    headline: &'a str,
    subline: &'a str,
    score: usize,
    best: usize,
}

/// Runs the game forever; owns the display, framebuffer, and RTC storage.
pub async fn run(mut display: Display, mut frame: FrameBuffer, mut rtc: RtcRam) -> ! {
    let mut rng = RoscRng;
    // High score lives in the RTC's single battery-backed RAM byte. Values
    // above MAX_SEQUENCE cannot have been written by this game (fresh chip or
    // another firmware's leftovers), so treat them as no score.
    let mut best: usize = match rtc.read() {
        Some(b) if usize::from(b) <= MAX_SEQUENCE => usize::from(b),
        _ => 0,
    };
    log::info!("high score from RTC: {}", best);

    loop {
        // ---- Attract screen ----
        cue(LedCue::Off);
        draw_and_present(
            &mut display,
            &mut frame,
            &Scene {
                lit: None,
                headline: "SIMON",
                subline: "press A to start",
                score: 0,
                best,
            },
        )
        .await;
        wait_for_press(Button::A).await;

        // ---- One game ----
        let mut sequence: Vec<u8, MAX_SEQUENCE> = Vec::new();
        let score = loop {
            // Push is infallible until MAX_SEQUENCE, which ends the game as a win.
            if sequence.push((rng.next_u32() % 5) as u8).is_err() {
                break sequence.len();
            }
            let round = sequence.len();
            log::info!("round {}", round);

            // Show phase: replay the sequence, speeding up as it grows.
            let on_ms = 500u64.saturating_sub(round as u64 * 20).max(160);
            show_scene(&mut display, &mut frame, round - 1, best, None).await;
            Timer::after_millis(600).await;
            for &step in &sequence {
                let pad = PADS[step as usize];
                cue(LedCue::Blink);
                show_scene(&mut display, &mut frame, round - 1, best, Some(pad)).await;
                Timer::after_millis(on_ms).await;
                show_scene(&mut display, &mut frame, round - 1, best, None).await;
                Timer::after_millis(140).await;
            }

            // Input phase: the player repeats the sequence.
            let mut failed = false;
            drain_events();
            for &step in &sequence {
                let expected = PADS[step as usize];
                match next_pad_press().await {
                    Some(pad) => {
                        show_scene(&mut display, &mut frame, round - 1, best, Some(pad)).await;
                        cue(LedCue::Blink);
                        Timer::after_millis(160).await;
                        show_scene(&mut display, &mut frame, round - 1, best, None).await;
                        if pad != expected {
                            failed = true;
                            break;
                        }
                    }
                    None => {
                        // Timed out.
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                break round - 1;
            }

            cue(LedCue::Celebrate);
            Timer::after_millis(400).await;
        };

        // ---- Game over ----
        if score > best {
            best = score;
            // MAX_SEQUENCE is 64, so the cast cannot truncate.
            rtc.write(best as u8);
            log::info!("new high score {} saved to RTC", best);
        }
        log::info!("game over, score {} best {}", score, best);
        cue(LedCue::Error);
        draw_and_present(
            &mut display,
            &mut frame,
            &Scene {
                lit: None,
                headline: "GAME OVER",
                subline: "press A to retry",
                score,
                best,
            },
        )
        .await;
        Timer::after_millis(800).await;
        drain_events();
        wait_for_press(Button::A).await;
    }
}

/// Renders the in-game scene (score line plus pads, one optionally lit).
async fn show_scene(
    display: &mut Display,
    frame: &mut FrameBuffer,
    score: usize,
    best: usize,
    lit: Option<Pad>,
) {
    draw_and_present(
        display,
        frame,
        &Scene {
            lit,
            headline: "WATCH & REPEAT",
            subline: "",
            score,
            best,
        },
    )
    .await;
}

/// Draws a full scene into the framebuffer and pushes it to the panel.
async fn draw_and_present(display: &mut Display, frame: &mut FrameBuffer, scene: &Scene<'_>) {
    // Infallible: FrameBuffer's error type is Infallible, so unwraps cannot fire.
    let bg = Rgb565::new(2, 4, 4);
    frame.clear(bg).unwrap();

    let title_style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let small_style = MonoTextStyle::new(&FONT_6X13, Rgb565::new(20, 40, 20));
    Text::new(scene.headline, Point::new(12, 30), title_style)
        .draw(frame)
        .unwrap();
    if !scene.subline.is_empty() {
        Text::new(scene.subline, Point::new(12, 55), small_style)
            .draw(frame)
            .unwrap();
    }

    let mut score_buf = heapless::String::<32>::new();
    let _ = core::fmt::write(
        &mut score_buf,
        format_args!("score {}   best {}", scene.score, scene.best),
    );
    Text::new(&score_buf, Point::new(12, 80), small_style)
        .draw(frame)
        .unwrap();

    for pad in PADS {
        let lit = scene.lit == Some(pad);
        let color = if lit { pad.color() } else { pad.dim_color() };
        let rounded = RoundedRectangle::with_equal_corners(pad.rect(), Size::new(8, 8));
        rounded
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(frame)
            .unwrap();
        if lit {
            rounded
                .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 3))
                .draw(frame)
                .unwrap();
        }
        let center = pad.rect().center();
        let label_style = MonoTextStyle::new(&FONT_6X13, Rgb565::WHITE);
        Text::new(
            pad.label(),
            Point::new(center.x - 6, center.y + 4),
            label_style,
        )
        .draw(frame)
        .unwrap();
    }

    display.present(frame.bytes()).await;
}

/// Discards any queued button events (stale presses from a previous phase).
fn drain_events() {
    while EVENTS.try_receive().is_ok() {}
}

/// Waits for a specific button to be pressed, ignoring everything else.
async fn wait_for_press(wanted: Button) {
    loop {
        if let ButtonEvent::Pressed(b) = EVENTS.receive().await {
            if b == wanted {
                return;
            }
        }
    }
}

/// Waits for the next pad press, or `None` on timeout.
async fn next_pad_press() -> Option<Pad> {
    let deadline = INPUT_TIMEOUT;
    loop {
        match with_timeout(deadline, EVENTS.receive()).await {
            Ok(ButtonEvent::Pressed(button)) => {
                if let Some(pad) = Pad::from_button(button) {
                    return Some(pad);
                }
            }
            Ok(ButtonEvent::Released(_)) => {}
            Err(_) => return None,
        }
    }
}

// Rust guideline compliant 2026-08-21
