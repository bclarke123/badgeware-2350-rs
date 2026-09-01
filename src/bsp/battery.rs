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

#[cfg(not(feature = "tufty"))]
use embassy_rp::adc::{Adc, Blocking, Channel, Config};
#[cfg(any(feature = "badger", feature = "tufty"))]
use embassy_rp::gpio::Input;
#[cfg(not(feature = "tufty"))]
use embassy_rp::gpio::Pull;
#[cfg(feature = "tufty")]
use embassy_rp::gpio::Pull;
#[cfg(feature = "tufty")]
use embassy_rp::peripherals::PIN_12 as VBUS_PIN;
#[cfg(feature = "badger")]
use embassy_rp::peripherals::{PIN_12, PIN_26, PIN_28};
#[cfg(not(feature = "tufty"))]
use embassy_rp::peripherals::ADC;
#[cfg(feature = "tufty")]
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "badger2040w")]
use embassy_rp::peripherals::{PIN_25, PIN_29};
use embassy_rp::Peri;

/// Estimated charge 0..=100 from a piecewise-linear single-cell LiPo
/// discharge curve, from a millivolt reading.
#[cfg(any(feature = "tufty", feature = "badger2040w"))]
fn lipo_percent(mv: u32) -> u8 {
    const CURVE: [(u32, u8); 10] = [
        (4150, 100),
        (4050, 92),
        (3950, 82),
        (3850, 70),
        (3750, 55),
        (3650, 38),
        (3550, 20),
        (3450, 10),
        (3300, 4),
        (3000, 0),
    ];
    if mv >= CURVE[0].0 {
        return 100;
    }
    for pair in CURVE.windows(2) {
        let ((hi_v, hi_p), (lo_v, lo_p)) = (pair[0], pair[1]);
        if mv >= lo_v {
            let f = (mv - lo_v) * 100 / (hi_v - lo_v);
            return (u32::from(lo_p) + (u32::from(hi_p) - u32::from(lo_p)) * f / 100) as u8;
        }
    }
    0
}

/// Battery monitor (Tufty 2350): consumes VBAT readings published by the
/// auto-backlight task (which owns the ADC; `VBAT_SENSE` is GPIO40 through
/// a 2:1 divider, uncalibrated — the Tufty has no spare reference pin).
/// `VBUS_DETECT` (GPIO12) is high on USB power. Readings are `None` until
/// the first sample lands (~0.5 s after boot).
#[cfg(feature = "tufty")]
static VBAT_MV: AtomicU32 = AtomicU32::new(0);

/// Publishes a smoothed VBAT millivolt reading (called by the
/// auto-backlight task, which owns the ADC).
#[cfg(feature = "tufty")]
pub fn publish_vbat_mv(mv: u32) {
    VBAT_MV.store(mv, Ordering::Relaxed);
}

#[cfg(feature = "tufty")]
pub struct Battery {
    vbus: Input<'static>,
}

#[cfg(feature = "tufty")]
impl Battery {
    pub fn new(vbus: Peri<'static, VBUS_PIN>) -> Self {
        Self { vbus: Input::new(vbus, Pull::None) }
    }

    /// Estimated charge, once the first VBAT sample has been published.
    pub fn percent(&mut self) -> Option<u8> {
        let mv = VBAT_MV.load(Ordering::Relaxed);
        (mv > 0).then(|| lipo_percent(mv))
    }

    /// USB power present.
    pub fn on_usb(&self) -> bool {
        self.vbus.is_high()
    }
}

/// Battery monitor (Badger 2040 W): a single VSYS sample taken at boot.
///
/// The Pico W reads its supply on ADC3/GPIO29 — a pin the radio then owns
/// as its SPI clock — so [`Battery::sample`] MUST run before
/// `wifi::connect`, and the value is cached for the life of the boot. On
/// battery that is at most one wake cycle old (each wake is a cold boot);
/// on USB it goes stale, but on USB the gauge shows the bolt anyway.
/// USB detection is the reading itself: VSYS above ~4.45 V can only be
/// VBUS through the input diode (a full LiPo sits at 4.2 V).
#[cfg(feature = "badger2040w")]
pub struct Battery {
    vsys_mv: u32,
}

#[cfg(feature = "badger2040w")]
impl Battery {
    /// Samples VSYS once; the peripherals are only borrowed and are free
    /// again after this returns (the radio needs both pins).
    ///
    /// Pico W quirk: the VSYS/3 divider on GPIO29 is behind a FET switched
    /// by WL_CS (GPIO25) — with CS low the ADC reads a floating pin, so CS
    /// is held high for the duration of the sample.
    pub fn sample(adc: Peri<'_, ADC>, vsys: Peri<'_, PIN_29>, wl_cs: Peri<'_, PIN_25>) -> Self {
        use embassy_rp::gpio::{Level, Output};
        let cs = Output::new(wl_cs, Level::High);
        let mut adc = Adc::new_blocking(adc, Config::default());
        let mut ch = Channel::new_pin(vsys, Pull::None);
        cortex_m::asm::delay(10_000); // let the divider settle
        let mut sum = 0u32;
        for _ in 0..10 {
            sum += u32::from(adc.blocking_read(&mut ch).unwrap_or(0));
        }
        drop(cs);
        // VSYS through the onboard 3:1 divider, 3.3 V reference, 12 bits.
        let vsys_mv = (sum / 10) * 3 * 3300 / 4095;
        log::info!("vsys: {} mV", vsys_mv);
        Self { vsys_mv }
    }

    /// The sampled VSYS in volts.
    pub fn voltage(&mut self) -> f32 {
        self.vsys_mv as f32 / 1000.0
    }

    /// Estimated charge 0..=100 (single-cell LiPo on the JST connector).
    pub fn percent(&mut self) -> u8 {
        if self.on_usb() {
            return 100;
        }
        lipo_percent(self.vsys_mv)
    }

    /// USB power present (VSYS is VBUS-shaped, not battery-shaped).
    pub fn on_usb(&self) -> bool {
        self.vsys_mv > 4450
    }
}

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
