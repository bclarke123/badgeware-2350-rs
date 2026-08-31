//! Board support for the Pimoroni Tufty 2350 and Badger 2350.
//!
//! Each submodule owns one hardware subsystem and exposes either a driver type,
//! an Embassy task, or both. Pin assignments come from Pimoroni's official
//! `board/pins.csv` files and are hardcoded per subsystem; these are fixed
//! boards, not devkits. Buttons, rear LEDs, I2C/RTC, USB and the POWMAN sleep
//! path are pin-identical on both; the screen (and, on the Tufty, backlight
//! and light sensor) differ and are selected by Cargo feature.

#[cfg(feature = "tufty")]
pub mod backlight;
#[cfg(feature = "badger")]
pub mod battery;
pub mod buttons;
#[cfg(feature = "tufty")]
pub mod display;
#[cfg(feature = "badger")]
pub mod epd;
pub mod leds;
pub mod power;
pub mod psram;
pub mod rtc;
pub mod settings;
pub mod screen;
pub mod usb;
pub mod wifi;

// Rust guideline compliant 2026-08-21
