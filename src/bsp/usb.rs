//! Logging over USB serial and RTT, with optional line input for
//! provisioning flows.
//!
//! One global `log` sink tees every record to two transports:
//!
//! - **USB CDC serial** (the probe-less default): plain text on
//!   `screen /dev/cu.usbmodem* 115200`.
//! - **RTT** (SEGGER Real-Time Transfer): a ring buffer in RAM that a debug
//!   probe reads over SWD behind the running target's back — no USB cable,
//!   no UART. Stream it with `probe-rs attach --chip RP235x <elf>` while
//!   the badge runs on battery. The channel never blocks: with no probe
//!   draining it, full-buffer writes are simply skipped.
//!
//! With [`logger_task_with_input`], bytes typed into the USB port are queued
//! on [`INPUT`]; [`read_line`] assembles them into lines (with echo) so an
//! app can accept simple `KEY value` commands over the wire.

use core::cell::RefCell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb_logger::{LoggerState, ReceiverHandler, UsbLogger};
use log::{Metadata, Record};
use rtt_target::UpChannel;
use static_cell::ConstStaticCell;

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

type Usb = UsbLogger<1024, InputHandler>;

static USB_LOGGER: ConstStaticCell<Usb> = ConstStaticCell::new(UsbLogger::new());
/// Set once the USB logger exists; the tee forwards records through it.
static USB_SINK: AtomicPtr<Usb> = AtomicPtr::new(null_mut());
/// The RTT up channel, once initialized (fmt writes need `&mut`).
static RTT: CsMutex<RefCell<Option<UpChannel>>> = CsMutex::new(RefCell::new(None));

/// The global logger: every record goes to RTT and to the USB pipe.
struct Tee;

impl log::Log for Tee {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        critical_section::with(|cs| {
            if let Some(ch) = RTT.borrow_ref_mut(cs).as_mut() {
                use core::fmt::Write;
                let _ = write!(ch, "{}\r\n", record.args());
            }
        });
        let usb = USB_SINK.load(Ordering::Acquire);
        // SAFETY: only ever set to a &'static UsbLogger, never unset.
        if let Some(usb) = unsafe { usb.as_ref() } {
            log::Log::log(usb, record);
        }
    }

    fn flush(&self) {}
}

static TEE: Tee = Tee;

/// One-shot: brings up RTT, registers the tee as the global logger, and
/// returns the USB half for the task to run.
fn init(with_input: bool) -> &'static Usb {
    let channels = rtt_target::rtt_init! {
        up: {
            0: {
                size: 2048,
                name: "log"
            }
        }
    };
    critical_section::with(|cs| *RTT.borrow_ref_mut(cs) = Some(channels.up.0));

    let logger = USB_LOGGER.take();
    if with_input {
        logger.with_handler(InputHandler);
    }
    let logger: &'static Usb = logger;
    USB_SINK.store(core::ptr::from_ref(logger).cast_mut(), Ordering::Release);
    // SAFETY: called once, before interrupts could race the logger.
    unsafe {
        let _ = log::set_logger_racy(&TEE).map(|()| log::set_max_level_racy(log::LevelFilter::Info));
    }
    logger
}

/// Runs the USB device and pumps `log` records out over CDC-ACM (and RTT).
#[embassy_executor::task]
pub async fn logger_task(driver: Driver<'static, USB>) {
    let logger = init(false);
    logger.run(&mut LoggerState::new(), driver).await;
}

/// [`logger_task`] plus received bytes queued on [`INPUT`].
#[embassy_executor::task]
pub async fn logger_task_with_input(driver: Driver<'static, USB>) {
    let logger = init(true);
    logger.run(&mut LoggerState::new(), driver).await;
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
                    // Echo the completed line so typing into a quiet port
                    // (screen's default) is verifiable.
                    log::info!("> {}", buf.as_str());
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

// Rust guideline compliant 2026-08-31
