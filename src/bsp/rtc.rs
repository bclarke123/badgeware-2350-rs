//! PCF85063A real-time clock on I2C0: timekeeping, the oscillator-stop
//! ("lost power") flag, and one byte of battery-backed storage.
//!
//! The chip stays powered from the badge battery, so time survives power-off
//! and reflashing; a full battery drain (or first boot) sets the OS flag in
//! the seconds register, which apps use to trigger a set-the-clock flow.
//!
//! `POWER_EN` must be high for the I2C bus/RTC to respond; `main` asserts it
//! at boot.

#![allow(dead_code, reason = "framework driver: each app uses a subset")]

use embassy_rp::i2c::{Blocking, Config, I2c};
use embassy_rp::peripherals::{I2C0, PIN_4, PIN_5};
use embassy_rp::Peri;
use embedded_hal::i2c::I2c as _;

/// 7-bit I2C address of the PCF85063A.
const RTC_ADDR: u8 = 0x51;

/// Register address of the free `RAM_byte`.
const REG_RAM_BYTE: u8 = 0x03;

/// First time register (seconds; bit 7 is the oscillator-stop "OS" flag).
const REG_SECONDS: u8 = 0x04;

/// A calendar timestamp as the RTC stores it (no timezone; store UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    /// Seconds since the Unix epoch (valid for 2000..=2099, the RTC's range).
    pub fn unix(&self) -> u64 {
        // Days since 1970-01-01 via the civil-from-days inverse (Howard
        // Hinnant's algorithm, simplified for 2000..2099: every year
        // divisible by 4 is a leap year in this window).
        const CUM_DAYS: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let y = u64::from(self.year);
        let leap_days = (y - 1969) / 4; // leaps in 1970..year (exclusive)
        let mut days = (y - 1970) * 365 + leap_days + u64::from(CUM_DAYS[usize::from(self.month - 1)])
            + u64::from(self.day - 1);
        if self.month <= 2 && y % 4 == 0 {
            days -= 1; // this year's leap day has not happened yet
        }
        days * 86400 + u64::from(self.hour) * 3600 + u64::from(self.minute) * 60 + u64::from(self.second)
    }
}

fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0f)
}

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

    /// Stores one byte; failures are logged and otherwise ignored (the caller
    /// keeps its in-memory copy either way).
    pub fn write(&mut self, value: u8) {
        if let Err(e) = self.i2c.write(RTC_ADDR, &[REG_RAM_BYTE, value]) {
            log::warn!("RTC RAM write failed: {:?}", e);
        }
    }

    /// True if the oscillator has stopped since the clock was last set
    /// (first boot or battery drain): the time is not trustworthy.
    pub fn lost_power(&mut self) -> bool {
        let mut buf = [0u8; 1];
        match self.i2c.write_read(RTC_ADDR, &[REG_SECONDS], &mut buf) {
            Ok(()) => buf[0] & 0x80 != 0,
            Err(e) => {
                log::warn!("RTC read failed: {:?}", e);
                true
            }
        }
    }

    /// Reads the current time, or `None` on bus error.
    pub fn read_datetime(&mut self) -> Option<DateTime> {
        let mut buf = [0u8; 7];
        if let Err(e) = self.i2c.write_read(RTC_ADDR, &[REG_SECONDS], &mut buf) {
            log::warn!("RTC read failed: {:?}", e);
            return None;
        }
        Some(DateTime {
            second: from_bcd(buf[0] & 0x7f),
            minute: from_bcd(buf[1] & 0x7f),
            hour: from_bcd(buf[2] & 0x3f),
            day: from_bcd(buf[3] & 0x3f),
            // buf[4] is the weekday, unused.
            month: from_bcd(buf[5] & 0x1f),
            year: 2000 + u16::from(from_bcd(buf[6])),
        })
    }

    /// Sets the clock (also clears the OS "lost power" flag, which lives in
    /// the seconds register and is written as 0 here).
    pub fn set_datetime(&mut self, dt: &DateTime) -> bool {
        let regs = [
            REG_SECONDS,
            to_bcd(dt.second),
            to_bcd(dt.minute),
            to_bcd(dt.hour),
            to_bcd(dt.day),
            0, // weekday, unused
            to_bcd(dt.month),
            to_bcd((dt.year - 2000) as u8),
        ];
        match self.i2c.write(RTC_ADDR, &regs) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("RTC set failed: {:?}", e);
                false
            }
        }
    }
}

// Rust guideline compliant 2026-08-30
