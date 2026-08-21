//! USB serial logging (the only log transport on a probe-less badge).
//!
//! Logs appear as plain text on the USB CDC serial port, e.g.
//! `screen /dev/tty.usbmodem* 115200` on macOS.

use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;

/// Runs the USB device and pumps `log` records out over CDC-ACM.
#[embassy_executor::task]
pub async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

// Rust guideline compliant 2026-08-21
