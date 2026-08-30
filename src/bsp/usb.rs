//! USB serial logging (the only log transport on a probe-less badge), with
//! optional line input for provisioning flows.
//!
//! Logs appear as plain text on the USB CDC serial port, e.g.
//! `screen /dev/cu.usbmodem* 115200` on macOS. With
//! [`logger_task_with_input`], bytes typed into that same port are queued on
//! [`INPUT`]; [`read_line`] assembles them into lines (with echo) so an app
//! can accept simple `KEY value` commands over the wire.

use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb_logger::ReceiverHandler;

/// Bytes received on the CDC port (dropped if nobody is reading).
pub static INPUT: Channel<CriticalSectionRawMutex, u8, 64> = Channel::new();

/// Queues every received byte on [`INPUT`].
pub struct InputHandler;

impl ReceiverHandler for InputHandler {
    async fn handle_data(&self, data: &[u8]) {
        for &b in data {
            // try_send: never let a chatty host block the USB task.
            let _ = INPUT.try_send(b);
        }
    }

    fn new() -> Self {
        Self
    }
}

/// Runs the USB device and pumps `log` records out over CDC-ACM.
#[embassy_executor::task]
pub async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

/// [`logger_task`] plus received bytes queued on [`INPUT`].
#[embassy_executor::task]
pub async fn logger_task_with_input(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver, InputHandler);
}

/// Reads one line from [`INPUT`] into `buf` (up to its capacity), echoing
/// through the log so the user sees what they type. Returns the trimmed
/// line. Handles CR, LF and backspace.
pub async fn read_line(buf: &mut heapless::String<128>) -> &str {
    buf.clear();
    loop {
        let b = INPUT.receive().await;
        match b {
            b'\r' | b'\n' => {
                if !buf.is_empty() {
                    return buf.trim();
                }
            }
            0x08 | 0x7f => {
                buf.pop();
            }
            0x20..=0x7e => {
                let _ = buf.push(b as char);
            }
            _ => {}
        }
    }
}

// Rust guideline compliant 2026-08-30
