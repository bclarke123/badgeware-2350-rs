//! PCF85063A real-time clock on I2C0 — used (for now) purely as one byte of
//! battery-backed storage.
//!
//! The RTC has a single general-purpose `RAM_byte` register that persists as
//! long as the chip is powered, i.e. across power-off and reflashing, but not
//! across battery disconnection or full drain. The high score lives there
//! until level 255 becomes achievable.
//!
//! `POWER_EN` (GPIO41) must be high for the I2C bus/RTC to respond; `main`
//! asserts it at boot. Timekeeping and alarm-wake support can grow here later.

use embassy_rp::i2c::{Blocking, Config, I2c};
use embassy_rp::peripherals::{I2C0, PIN_4, PIN_5};
use embassy_rp::Peri;
use embedded_hal::i2c::I2c as _;

/// 7-bit I2C address of the PCF85063A.
const RTC_ADDR: u8 = 0x51;

/// Register address of the free `RAM_byte`.
const REG_RAM_BYTE: u8 = 0x03;

/// One byte of battery-backed storage in the RTC.
pub struct RtcRam {
    i2c: I2c<'static, I2C0, Blocking>,
}

impl RtcRam {
    /// Sets up blocking I2C0 (SDA=GPIO4, SCL=GPIO5) at the default 100 kHz.
    ///
    /// Blocking is fine here: transfers are two bytes long and happen at most
    /// once per game.
    pub fn new(
        i2c: Peri<'static, I2C0>,
        scl: Peri<'static, PIN_5>,
        sda: Peri<'static, PIN_4>,
    ) -> Self {
        Self {
            i2c: I2c::new_blocking(i2c, scl, sda, Config::default()),
        }
    }

    /// Reads the stored byte, or `None` if the RTC does not respond.
    pub fn read(&mut self) -> Option<u8> {
        let mut buf = [0u8; 1];
        match self.i2c.write_read(RTC_ADDR, &[REG_RAM_BYTE], &mut buf) {
            Ok(()) => Some(buf[0]),
            Err(e) => {
                log::warn!("RTC RAM read failed: {:?}", e);
                None
            }
        }
    }

    /// Stores one byte; failures are logged and otherwise ignored (the game
    /// keeps its in-memory copy either way).
    pub fn write(&mut self, value: u8) {
        if let Err(e) = self.i2c.write(RTC_ADDR, &[REG_RAM_BYTE, value]) {
            log::warn!("RTC RAM write failed: {:?}", e);
        }
    }
}

// Rust guideline compliant 2026-08-21
