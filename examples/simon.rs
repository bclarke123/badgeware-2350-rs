//! Simon-style memory game on the Tufty's five front buttons — the app this
//! repo was born with, resurrected from git history as an example.
//!
//! `cargo run --release --example simon`
//!
//! Watch the growing sequence flash on screen, then repeat it with the physical
//! buttons. The on-screen pads are positioned to line up with the physical
//! controls (verified against a photo of the badge): A / B / C along the bottom
//! edge directly above their buttons, UP and DN on the right edge beside the
//! side buttons. Rear LEDs echo every step.
//!
//! All transitions are animated with cubic easing (see [`anim`]): pads pop in
//! on the title screen, flashes inflate slightly, and the game-over screen
//! slides in and counts the score up. Presents are vsync-locked (~60 Hz), so
//! animation loops simply draw-present until their timeline finishes.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO1, USB};
use embassy_rp::{adc, bind_interrupts, dma, pio, usb};
use static_cell::ConstStaticCell;
use tufty_2350::bsp::backlight::Backlight;
use tufty_2350::bsp::leds::RearLeds;
use embassy_rp::clocks::RoscRng;
use embassy_time::{with_timeout, Duration, Ticker, Timer};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X13};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};
use heapless::Vec;

use tufty_2350::bsp::buttons::{Button, ButtonEvent, ButtonPins, EVENTS};
use tufty_2350::bsp::display::Display;
use tufty_2350::bsp::leds::{cue, LedCue};
use tufty_2350::bsp::rtc::RtcRam;
use tufty_2350::bsp;
use tufty_2350::gfx::{FrameBuffer, FB_BYTES};

use tufty_2350::flora::anim::{
    self, ease_in_out_cubic, ease_out_cubic, lerp_color, scale_rect, segment_progress, Timeline,
};

/// Shim for the retired `Timeline::segment` method: progress (0..=1) of a
/// sub-window `delay..delay+length` of the timeline.
fn seg(tl: &Timeline, delay: Duration, length: Duration) -> f32 {
    segment_progress(tl.elapsed(), delay, length)
}

/// Longest sequence the game can hold; reaching it is a win by exhaustion.
const MAX_SEQUENCE: usize = 64;

/// How long the player has for each press before the round is failed.
const INPUT_TIMEOUT: Duration = Duration::from_secs(5);

/// Background color shared by every screen.
const BACKGROUND: Rgb565 = Rgb565::new(2, 4, 4);

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

    /// Screen rectangle, aligned with the *physical* button positions as
    /// measured from a photo of the badge: the A/B/C buttons sit left of
    /// screen-center thirds, and the side DOWN button is well below the
    /// screen's vertical middle.
    fn rect(self) -> Rectangle {
        match self {
            Self::A => Rectangle::new(Point::new(12, 172), Size::new(72, 62)),
            Self::B => Rectangle::new(Point::new(92, 172), Size::new(72, 62)),
            Self::C => Rectangle::new(Point::new(172, 172), Size::new(72, 62)),
            Self::Up => Rectangle::new(Point::new(252, 30), Size::new(60, 52)),
            Self::Down => Rectangle::new(Point::new(252, 146), Size::new(60, 52)),
        }
    }

    fn color(self) -> Rgb565 {
        match self {
            Self::A => Rgb565::new(31, 12, 6),    // red
            Self::B => Rgb565::new(31, 48, 0),    // amber
            Self::C => Rgb565::new(6, 50, 10),    // green
            Self::Up => Rgb565::new(4, 30, 31),   // blue
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

    fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::Up => 3,
            Self::Down => 4,
        }
    }
}

/// Per-pad visual state for one frame of animation.
#[derive(Clone, Copy)]
struct PadVis {
    /// Size about the pad center: 0.0 collapsed, 1.0 resting, >1.0 inflated.
    scale: f32,
    /// 0.0 idle/dim through 1.0 fully lit (adds the white ring near 1.0).
    glow: f32,
}

const PADS_IDLE: [PadVis; 5] = [PadVis { scale: 1.0, glow: 0.0 }; 5];

/// Static text elements for one frame.
struct Chrome<'a> {
    headline: &'a str,
    /// Baseline y of the headline (animated on transitions).
    headline_y: i32,
    subline: &'a str,
    subline_color: Rgb565,
    score: usize,
    best: usize,
}

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

static FRAMEBUFFER: ConstStaticCell<[u8; FB_BYTES]> = ConstStaticCell::new([0; FB_BYTES]);

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut p = embassy_rp::init(Default::default());
    let power_en = Output::new(p.PIN_41, Level::High);
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
    log::info!("simon booting");

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

    let mut backlight = Backlight::new(p.PWM_SLICE5, p.PIN_26);
    let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
    let mut display = Display::new(
        p.PIO1, Irqs, dma_ch, p.PIN_21, p.PIN_27, p.PIN_28, p.PIN_30, p.PIN_31, p.PIN_32,
        p.PIN_33, p.PIN_34, p.PIN_35, p.PIN_36, p.PIN_37, p.PIN_38, p.PIN_39,
    );
    display.init().await;

    let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let light = adc::Channel::new_pin(p.PIN_43, Pull::None);
    spawner.spawn(bsp::backlight::auto_backlight_task(adc, light).unwrap());

    let frame = FrameBuffer::new(FRAMEBUFFER.take());
    display.present(frame.bytes()).await;
    backlight.set_brightness(200);
    spawner.spawn(bsp::backlight::backlight_task(backlight).unwrap());

    let rtc = RtcRam::new(p.I2C0, p.PIN_5, p.PIN_4);
    run(display, frame, rtc).await
}

/// Runs the game forever; owns the display, framebuffer, and RTC storage.
async fn run(mut display: Display, mut frame: FrameBuffer, mut rtc: RtcRam) -> ! {
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
        attract(&mut display, &mut frame, best).await;

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
            render_game(&mut display, &mut frame, round - 1, best, &PADS_IDLE).await;
            Timer::after_millis(600).await;
            for &step in &sequence {
                let pad = PADS[step as usize];
                cue(LedCue::Blink);
                flash_pad(&mut display, &mut frame, round - 1, best, pad, on_ms).await;
                Timer::after_millis(140).await;
            }

            // Input phase: the player repeats the sequence.
            let mut failed = false;
            drain_events();
            for &step in &sequence {
                let expected = PADS[step as usize];
                match next_pad_press().await {
                    Some(pad) => {
                        cue(LedCue::Blink);
                        flash_pad(&mut display, &mut frame, round - 1, best, pad, 160).await;
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
        game_over(&mut display, &mut frame, score, best).await;
    }
}

/// Title screen: pads pop in staggered, headline slides down, then the prompt
/// pulses until A is pressed.
async fn attract(display: &mut Display, frame: &mut FrameBuffer, best: usize) {
    const INTRO: Duration = Duration::from_millis(1100);
    const TITLE_SLIDE: Duration = Duration::from_millis(450);
    const POP_START: Duration = Duration::from_millis(350);
    const POP_LEN: Duration = Duration::from_millis(300);
    const POP_STAGGER: Duration = Duration::from_millis(90);

    let intro = Timeline::new(INTRO);
    while !intro.finished() {
        let title_t = ease_out_cubic(seg(&intro, Duration::from_millis(0), TITLE_SLIDE));
        let mut vis = PADS_IDLE;
        for (i, v) in vis.iter_mut().enumerate() {
            let delay = POP_START + POP_STAGGER * i as u32;
            v.scale = ease_out_cubic(seg(&intro, delay, POP_LEN));
        }
        let chrome = Chrome {
            headline: "SIMON",
            headline_y: anim::lerp(-8.0, 34.0, title_t) as i32,
            subline: "",
            subline_color: BACKGROUND,
            score: 0,
            best,
        };
        draw_frame(display, frame, &chrome, &vis).await;
    }

    // Pulse the prompt until A starts a game.
    const PULSE_PERIOD_MS: u64 = 1400;
    let dim = Rgb565::new(8, 20, 12);
    let bright = Rgb565::new(24, 58, 28);
    let mut ticker = Ticker::every(Duration::from_millis(33));
    let mut elapsed_ms: u64 = 0;
    drain_events();
    loop {
        // Triangle wave over the period, smoothed by in-out cubic easing.
        let phase = (elapsed_ms % PULSE_PERIOD_MS) as f32 / PULSE_PERIOD_MS as f32;
        let tri = 1.0 - (2.0 * phase - 1.0).abs();
        let chrome = Chrome {
            headline: "SIMON",
            headline_y: 34,
            subline: "press A to start",
            subline_color: lerp_color(dim, bright, ease_in_out_cubic(tri)),
            score: 0,
            best,
        };
        draw_frame(display, frame, &chrome, &PADS_IDLE).await;

        match select(EVENTS.receive(), ticker.next()).await {
            Either::First(ButtonEvent::Pressed(Button::A)) => return,
            Either::First(_) => {}
            Either::Second(()) => elapsed_ms += 33,
        }
    }
}

/// Game-over screen: headline slides in, the score counts up with easing, then
/// waits for A.
async fn game_over(display: &mut Display, frame: &mut FrameBuffer, score: usize, best: usize) {
    const OUTRO: Duration = Duration::from_millis(1400);
    const TITLE_SLIDE: Duration = Duration::from_millis(400);
    const COUNT_DELAY: Duration = Duration::from_millis(300);
    const COUNT_LEN: Duration = Duration::from_millis(900);

    let outro = Timeline::new(OUTRO);
    while !outro.finished() {
        let title_t = ease_out_cubic(seg(&outro, Duration::from_millis(0), TITLE_SLIDE));
        let count_t = ease_out_cubic(seg(&outro, COUNT_DELAY, COUNT_LEN));
        let shown = (count_t * score as f32 + 0.5) as usize;
        // Pads deflate slightly and dim during the outro.
        let shrink = 1.0 - 0.12 * ease_in_out_cubic(outro.progress());
        let vis = [PadVis { scale: shrink, glow: 0.0 }; 5];
        let chrome = Chrome {
            headline: "GAME OVER",
            headline_y: anim::lerp(-8.0, 34.0, title_t) as i32,
            subline: "",
            subline_color: BACKGROUND,
            score: shown,
            best,
        };
        draw_frame(display, frame, &chrome, &vis).await;
    }

    drain_events();
    let chrome = Chrome {
        headline: "GAME OVER",
        headline_y: 34,
        subline: "press A to retry",
        subline_color: Rgb565::new(20, 40, 20),
        score,
        best,
    };
    draw_frame(display, frame, &chrome, &PADS_IDLE).await;
    wait_for_press(Button::A).await;
}

/// Lights one pad for `on_ms` with a quick eased inflate, then redraws idle.
async fn flash_pad(
    display: &mut Display,
    frame: &mut FrameBuffer,
    score: usize,
    best: usize,
    pad: Pad,
    on_ms: u64,
) {
    const INFLATE: f32 = 0.09;
    let flash = Timeline::new(Duration::from_millis(on_ms));
    while !flash.finished() {
        // Inflate over the first third of the flash, then hold.
        let t = ease_out_cubic((flash.progress() * 3.0).min(1.0));
        let mut vis = PADS_IDLE;
        vis[pad.index()] = PadVis {
            scale: 1.0 + INFLATE * t,
            glow: 1.0,
        };
        render_game(display, frame, score, best, &vis).await;
    }
    render_game(display, frame, score, best, &PADS_IDLE).await;
}

/// Renders the in-game scene with the given pad visuals.
async fn render_game(
    display: &mut Display,
    frame: &mut FrameBuffer,
    score: usize,
    best: usize,
    vis: &[PadVis; 5],
) {
    let chrome = Chrome {
        headline: "WATCH & REPEAT",
        headline_y: 34,
        subline: "",
        subline_color: BACKGROUND,
        score,
        best,
    };
    draw_frame(display, frame, &chrome, vis).await;
}

/// Draws one complete frame (chrome + pads) and presents it.
async fn draw_frame(
    display: &mut Display,
    frame: &mut FrameBuffer,
    chrome: &Chrome<'_>,
    vis: &[PadVis; 5],
) {
    // Infallible: FrameBuffer's error type is Infallible, so unwraps cannot fire.
    frame.clear(BACKGROUND).unwrap();

    let title_style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let small_style = MonoTextStyle::new(&FONT_6X13, Rgb565::new(20, 40, 20));
    Text::new(chrome.headline, Point::new(12, chrome.headline_y), title_style)
        .draw(frame)
        .unwrap();
    if !chrome.subline.is_empty() {
        let subline_style = MonoTextStyle::new(&FONT_6X13, chrome.subline_color);
        Text::new(chrome.subline, Point::new(12, chrome.headline_y + 25), subline_style)
            .draw(frame)
            .unwrap();
    }

    let mut score_buf = heapless::String::<32>::new();
    let _ = core::fmt::write(
        &mut score_buf,
        format_args!("score {}   best {}", chrome.score, chrome.best),
    );
    Text::new(&score_buf, Point::new(12, chrome.headline_y + 50), small_style)
        .draw(frame)
        .unwrap();

    for pad in PADS {
        let v = vis[pad.index()];
        if v.scale <= 0.05 {
            continue;
        }
        let rect = scale_rect(pad.rect(), v.scale);
        let color = lerp_color(pad.dim_color(), pad.color(), v.glow);
        let rounded = RoundedRectangle::with_equal_corners(rect, Size::new(8, 8));
        rounded
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(frame)
            .unwrap();
        if v.glow > 0.7 {
            rounded
                .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 3))
                .draw(frame)
                .unwrap();
        }
        if v.scale > 0.5 {
            let label_style = MonoTextStyle::new(&FONT_6X13, Rgb565::WHITE);
            Text::with_alignment(
                pad.label(),
                rect.center() + Point::new(0, 4),
                label_style,
                Alignment::Center,
            )
            .draw(frame)
            .unwrap();
        }
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

// Rust guideline compliant 2026-08-31
