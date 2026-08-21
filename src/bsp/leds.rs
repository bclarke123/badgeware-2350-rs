//! Four-zone rear case lighting (GPIO0-3) as simple game-feedback cues.
//!
//! Game code fires a [`LedCue`] via [`cue`]; the LED task plays the matching
//! pattern and returns to off. Plain on/off outputs are plenty for feedback
//! flashes; PWM breathing can come later if wanted.

use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIN_0, PIN_1, PIN_2, PIN_3};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;

/// A lighting pattern request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "framework cue set; each app uses a subset")]
pub enum LedCue {
    /// Everything off.
    Off,
    /// One short blink of all zones (sequence step / button feedback).
    Blink,
    /// Three quick flashes (level cleared).
    Celebrate,
    /// Two long flashes (wrong answer).
    Error,
}

static CUE: Signal<CriticalSectionRawMutex, LedCue> = Signal::new();

/// Requests a lighting pattern (replaces any pattern still playing).
pub fn cue(cue: LedCue) {
    CUE.signal(cue);
}

/// The four rear LED zone outputs.
pub struct RearLeds {
    zones: [Output<'static>; 4],
}

impl RearLeds {
    /// Configures the four zone GPIOs, all off.
    pub fn new(
        cl0: Peri<'static, PIN_0>,
        cl1: Peri<'static, PIN_1>,
        cl2: Peri<'static, PIN_2>,
        cl3: Peri<'static, PIN_3>,
    ) -> Self {
        Self {
            zones: [
                Output::new(cl0, Level::Low),
                Output::new(cl1, Level::Low),
                Output::new(cl2, Level::Low),
                Output::new(cl3, Level::Low),
            ],
        }
    }

    fn set_all(&mut self, on: bool) {
        for zone in &mut self.zones {
            if on {
                zone.set_high();
            } else {
                zone.set_low();
            }
        }
    }
}

/// Plays requested LED cues forever.
#[embassy_executor::task]
pub async fn led_task(mut leds: RearLeds) -> ! {
    loop {
        let cue = CUE.wait().await;
        match cue {
            LedCue::Off => leds.set_all(false),
            LedCue::Blink => {
                leds.set_all(true);
                Timer::after_millis(120).await;
                leds.set_all(false);
            }
            LedCue::Celebrate => {
                for _ in 0..3 {
                    leds.set_all(true);
                    Timer::after_millis(90).await;
                    leds.set_all(false);
                    Timer::after_millis(90).await;
                }
            }
            LedCue::Error => {
                for _ in 0..2 {
                    leds.set_all(true);
                    Timer::after_millis(300).await;
                    leds.set_all(false);
                    Timer::after_millis(150).await;
                }
            }
        }
    }
}

// Rust guideline compliant 2026-08-21
