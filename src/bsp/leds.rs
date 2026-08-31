//! Four-zone rear case lighting (GPIO0-3) as simple feedback cues.
//!
//! App code fires a [`LedCue`] via [`cue`]; the LED task plays the matching
//! pattern. The zones ride hardware PWM (GPIO0/1 = slice 0 A/B, GPIO2/3 =
//! slice 1 A/B), so as well as on/off flashes there is [`LedCue::Breathe`]:
//! a slow gamma-corrected fade in and out that keeps playing until the next
//! cue — the "working on it" light for network fetches and other waits.

use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::{PIN_0, PIN_1, PIN_2, PIN_3, PWM_SLICE0, PWM_SLICE1};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
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
    /// Slow fade in and out, forever, until another cue replaces it.
    Breathe,
}

static CUE: Signal<CriticalSectionRawMutex, LedCue> = Signal::new();

/// Requests a lighting pattern (replaces any pattern still playing).
pub fn cue(cue: LedCue) {
    CUE.signal(cue);
}

/// PWM counter wrap: 10-bit duty at ~146 kHz from the 150 MHz system clock.
const TOP: u16 = 1023;

/// One breathe cycle (fade in + out), milliseconds.
const BREATHE_MS: u64 = 2600;

/// Milliseconds per breathe brightness step.
const BREATHE_STEP_MS: u64 = 20;

/// The four rear LED zones on PWM slices 0 and 1.
pub struct RearLeds {
    slices: [Pwm<'static>; 2],
}

impl RearLeds {
    /// Configures the four zone pins as PWM outputs, all off.
    pub fn new(
        slice0: Peri<'static, PWM_SLICE0>,
        slice1: Peri<'static, PWM_SLICE1>,
        cl0: Peri<'static, PIN_0>,
        cl1: Peri<'static, PIN_1>,
        cl2: Peri<'static, PIN_2>,
        cl3: Peri<'static, PIN_3>,
    ) -> Self {
        let cfg = config(0);
        Self {
            slices: [
                Pwm::new_output_ab(slice0, cl0, cl1, cfg.clone()),
                Pwm::new_output_ab(slice1, cl2, cl3, cfg),
            ],
        }
    }

    /// Sets every zone to `duty` out of [`TOP`].
    fn set_duty(&mut self, duty: u16) {
        let cfg = config(duty);
        for slice in &mut self.slices {
            slice.set_config(&cfg);
        }
    }

    fn set_all(&mut self, on: bool) {
        self.set_duty(if on { TOP } else { 0 });
    }
}

/// A slice config with both channels at `duty`.
fn config(duty: u16) -> PwmConfig {
    let mut cfg = PwmConfig::default();
    cfg.top = TOP;
    cfg.compare_a = duty;
    cfg.compare_b = duty;
    cfg
}

/// Perceived-linear brightness: triangle wave through a square-law gamma.
fn breathe_duty(elapsed_ms: u64) -> u16 {
    let phase = elapsed_ms % BREATHE_MS;
    let half = BREATHE_MS / 2;
    let tri = if phase < half { phase } else { BREATHE_MS - phase };
    let linear = (tri * u64::from(TOP)) / half;
    ((linear * linear) / u64::from(TOP)) as u16
}

/// Plays requested LED cues forever.
#[embassy_executor::task]
pub async fn led_task(mut leds: RearLeds) -> ! {
    let mut cue = LedCue::Off;
    loop {
        cue = match cue {
            LedCue::Off => {
                leds.set_all(false);
                CUE.wait().await
            }
            LedCue::Blink => {
                leds.set_all(true);
                Timer::after_millis(120).await;
                LedCue::Off
            }
            LedCue::Celebrate => {
                for _ in 0..3 {
                    leds.set_all(true);
                    Timer::after_millis(90).await;
                    leds.set_all(false);
                    Timer::after_millis(90).await;
                }
                LedCue::Off
            }
            LedCue::Error => {
                for _ in 0..2 {
                    leds.set_all(true);
                    Timer::after_millis(300).await;
                    leds.set_all(false);
                    Timer::after_millis(150).await;
                }
                LedCue::Off
            }
            LedCue::Breathe => {
                let mut elapsed: u64 = 0;
                loop {
                    leds.set_duty(breathe_duty(elapsed));
                    match select(CUE.wait(), Timer::after_millis(BREATHE_STEP_MS)).await {
                        Either::First(next) => break next,
                        Either::Second(()) => elapsed += BREATHE_STEP_MS,
                    }
                }
            }
        };
        // A cue fired mid-pattern replaces the scripted follow-up.
        if let Some(next) = CUE.try_take() {
            cue = next;
        }
    }
}

// Rust guideline compliant 2026-08-31
