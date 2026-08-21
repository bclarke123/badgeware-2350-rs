//! Board support for the Pimoroni Tufty 2350.
//!
//! Each submodule owns one hardware subsystem and exposes either a driver type,
//! an Embassy task, or both. Pin assignments come from Pimoroni's official
//! `board/pins.csv` and are hardcoded per subsystem; the Tufty is a fixed board,
//! not a devkit.

pub mod backlight;
pub mod buttons;
pub mod display;
pub mod leds;
pub mod power;
pub mod rtc;
pub mod usb;

// Rust guideline compliant 2026-08-21
