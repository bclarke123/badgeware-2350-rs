//! Battery state: voltage, an estimated charge percentage, and whether USB
//! power is present.
//!
//! On the Badger, `VBAT_SENSE` (GPIO26) reads the cell through a 2:1
//! divider and is calibrated against the board's 1.1 V reference on
//! `SENSE_1V1` (GPIO28), exactly as Pimoroni's firmware does — this cancels
//! the ADC supply error, which matters when the 3V3 rail sags on battery.
//! `VBUS_DETECT` (GPIO12) is high on USB power. (On the Tufty, VBAT is
//! GPIO40 and there is no spare reference pin — GPIO28 drives the display —
//! so this module is Badger-only for now; the Tufty's ADC is also owned by
//! its light-sensor task.)
//!
//! Uses the blocking ADC (a battery sample is 20 conversions, microseconds).

use embassy_rp::adc::{Adc, Blocking, Channel, Config};
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::peripherals::{ADC, PIN_12, PIN_26, PIN_28};
use embassy_rp::Peri;

/// Battery monitor (Badger 2350).
#[cfg(feature = "badger")]
pub struct Battery {
    adc: Adc<'static, Blocking>,
    vbat: Channel<'static>,
    vref: Channel<'static>,
    vbus: Input<'static>,
}

#[cfg(feature = "badger")]
impl Battery {
    pub fn new(
        adc: Peri<'static, ADC>,
        vbat: Peri<'static, PIN_26>,
        vref: Peri<'static, PIN_28>,
        vbus: Peri<'static, PIN_12>,
    ) -> Self {
        Self {
            adc: Adc::new_blocking(adc, Config::default()),
            vbat: Channel::new_pin(vbat, Pull::None),
            vref: Channel::new_pin(vref, Pull::None),
            vbus: Input::new(vbus, Pull::None),
        }
    }

    /// Cell voltage, reference-calibrated, averaged over 10 samples.
    pub fn voltage(&mut self) -> f32 {
        let mut bat = 0u32;
        let mut refv = 0u32;
        for _ in 0..10 {
            bat += u32::from(self.adc.blocking_read(&mut self.vbat).unwrap_or(0));
            refv += u32::from(self.adc.blocking_read(&mut self.vref).unwrap_or(0));
        }
        if refv == 0 {
            return 0.0;
        }
        // (vbat / vref) * 1.1 V reference * 2:1 divider.
        bat as f32 / refv as f32 * 1.1 * 2.0
    }

    /// Estimated charge 0..=100, from a piecewise-linear LiPo discharge
    /// curve over 3.0–4.1 V.
    pub fn percent(&mut self) -> u8 {
        const CURVE: [(f32, u8); 10] = [
            (4.10, 100),
            (4.00, 90),
            (3.90, 78),
            (3.80, 62),
            (3.70, 45),
            (3.60, 26),
            (3.50, 13),
            (3.40, 6),
            (3.30, 3),
            (3.00, 0),
        ];
        let v = self.voltage();
        if v >= CURVE[0].0 {
            return 100;
        }
        for pair in CURVE.windows(2) {
            let (hi_v, hi_p) = pair[0];
            let (lo_v, lo_p) = pair[1];
            if v >= lo_v {
                let t = (v - lo_v) / (hi_v - lo_v);
                return (f32::from(lo_p) + t * f32::from(hi_p - lo_p)) as u8;
            }
        }
        0
    }

    /// True when USB power is present (the cell is charging or full).
    pub fn on_usb(&self) -> bool {
        self.vbus.is_high()
    }
}

// Rust guideline compliant 2026-08-30
