//! Display backlight (PWM on GPIO26) with optional ambient-light auto-dimming.
//!
//! The backlight driver chain does not light below roughly 11% duty, so a
//! perceptual (cubic, approximating gamma 2.8) curve maps brightness 1-255 onto
//! `BACKLIGHT_MIN..=65535`; 0 is fully off. Constants follow Pimoroni's driver.
//!
//! [`auto_backlight_task`] samples the phototransistor on GPIO43 and publishes a
//! target brightness; [`backlight_task`] eases the PWM toward it.

use embassy_rp::adc::{Adc, Async, Channel as AdcChannel};
use embassy_rp::peripherals::{PIN_26, PWM_SLICE5};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Ticker};

/// Lowest PWM level at which the backlight lights on all boards (Pimoroni's value).
const BACKLIGHT_MIN: u32 = 8192;

/// Target brightness (0-255) published by the auto-backlight task.
static TARGET: Signal<CriticalSectionRawMutex, u8> = Signal::new();

/// PWM-driven backlight control.
pub struct Backlight {
    pwm: Pwm<'static>,
    config: PwmConfig,
}

impl Backlight {
    /// Configures GPIO26 as a PWM output, starting fully off.
    pub fn new(slice: Peri<'static, PWM_SLICE5>, pin: Peri<'static, PIN_26>) -> Self {
        let mut config = PwmConfig::default();
        config.top = 0xFFFF;
        config.compare_a = 0;
        let pwm = Pwm::new_output_a(slice, pin, config.clone());
        Self { pwm, config }
    }

    /// Sets brightness 0-255 through the perceptual curve (0 = off).
    pub fn set_brightness(&mut self, brightness: u8) {
        self.config.compare_a = duty_for(brightness);
        self.pwm.set_config(&self.config);
    }
}

/// Maps 0-255 brightness onto the PWM compare value.
fn duty_for(brightness: u8) -> u16 {
    if brightness == 0 {
        return 0;
    }
    // Integer cube approximates the 2.8 gamma of the reference driver closely
    // enough for a backlight: level = min + b^3 / 255^3 * (65535 - min).
    let b = u32::from(brightness);
    let span = 65535 - BACKLIGHT_MIN;
    // b^3 <= 2^24, span < 2^16; widen to u64 for the product.
    let scaled = (u64::from(b * b * b) * u64::from(span)) / (255 * 255 * 255);
    (BACKLIGHT_MIN + scaled as u32).min(65535) as u16
}

/// Eases the backlight toward the most recent target brightness.
#[embassy_executor::task]
pub async fn backlight_task(mut backlight: Backlight) -> ! {
    let mut current: u8 = 200;
    let mut target: u8 = current;
    // 20 ms steps give a ~1 s full-range fade: fast enough to track the room,
    // slow enough to be invisible in play.
    let mut ticker = Ticker::every(Duration::from_millis(20));
    loop {
        ticker.next().await;
        if let Some(t) = TARGET.try_take() {
            target = t;
        }
        if current != target {
            current = if current < target { current + 1 } else { current - 1 };
            backlight.set_brightness(current);
        }
    }
}

/// Samples ambient light and publishes a matching backlight target; also
/// samples `VBAT_SENSE` (GPIO40, 2:1 divider) each tick and publishes the
/// smoothed millivolts through [`crate::bsp::battery`], since this task
/// owns the board's only ADC.
#[embassy_executor::task]
pub async fn auto_backlight_task(
    mut adc: Adc<'static, Async>,
    mut light: AdcChannel<'static>,
    mut vbat: AdcChannel<'static>,
) -> ! {
    // The phototransistor saturates the 12-bit ADC in bright light and reads
    // near zero in the dark. These bounds map that usable range onto a dim but
    // readable minimum and full brightness.
    const DARK_COUNTS: u16 = 100;
    const BRIGHT_COUNTS: u16 = 3000;
    const MIN_BRIGHTNESS: u32 = 70;
    const MAX_BRIGHTNESS: u32 = 255;

    let mut ticker = Ticker::every(Duration::from_millis(500));
    let mut smoothed: u32 = 0;
    let mut vbat_smoothed: u32 = 0;
    loop {
        ticker.next().await;
        if let Ok(raw) = adc.read(&mut vbat).await {
            let mv = u32::from(raw) * 2 * 3300 / 4095;
            vbat_smoothed = if vbat_smoothed == 0 { mv } else { (vbat_smoothed * 3 + mv) / 4 };
            crate::bsp::battery::publish_vbat_mv(vbat_smoothed);
        }
        let raw = match adc.read(&mut light).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("light sensor read failed: {:?}", e);
                continue;
            }
        };
        // Exponential smoothing (1/4 new) so passing shadows don't pump the display.
        smoothed = (smoothed * 3 + u32::from(raw)) / 4;

        let clamped = (smoothed as u16).clamp(DARK_COUNTS, BRIGHT_COUNTS);
        let span = u32::from(BRIGHT_COUNTS - DARK_COUNTS);
        let level = MIN_BRIGHTNESS
            + (u32::from(clamped - DARK_COUNTS) * (MAX_BRIGHTNESS - MIN_BRIGHTNESS)) / span;
        TARGET.signal(level as u8);
    }
}

// Rust guideline compliant 2026-08-21
