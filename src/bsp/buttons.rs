//! Debounced button input as a stream of events.
//!
//! One task polls all six buttons every 10 ms (cheaper than six edge-wait tasks,
//! and debounce falls out of the sampling naturally). Consumers receive
//! [`ButtonEvent`]s from the global [`EVENTS`] channel.
//!
//! Holding HOME for two seconds reboots into BOOTSEL so the badge can be
//! reflashed over USB without touching the physical BOOT button.

use embassy_rp::gpio::Input;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Ticker};

/// The five front buttons plus HOME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    A,
    B,
    C,
    Up,
    Down,
    Home,
}

/// A debounced state change on one button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonEvent {
    Pressed(Button),
    Released(Button),
}

/// Global stream of debounced button events.
///
/// Depth 16 absorbs mashing while the consumer is busy presenting a frame;
/// events are dropped (not blocked on) if it ever fills.
pub static EVENTS: Channel<CriticalSectionRawMutex, ButtonEvent, 16> = Channel::new();

/// The button GPIOs (active low on the RP2350 boards; the Badger 2040 W's
/// are active high and it has no HOME button — reflash via its physical
/// BOOTSEL button instead).
pub struct ButtonPins {
    pub a: Input<'static>,
    pub b: Input<'static>,
    pub c: Input<'static>,
    pub up: Input<'static>,
    pub down: Input<'static>,
    #[cfg(not(feature = "badger2040w"))]
    pub home: Input<'static>,
}

/// How often the buttons are sampled. Two consecutive equal samples are needed
/// to accept a change, so worst-case latency is two periods (20 ms) — well under
/// human perception, and enough to swallow contact bounce.
const POLL_PERIOD: Duration = Duration::from_millis(10);

/// HOME held for this many consecutive polls (2 s) triggers a BOOTSEL reboot.
#[cfg(not(feature = "badger2040w"))]
const HOME_HOLD_POLLS: u32 = 200;

/// Polls, debounces, and publishes button events forever.
#[embassy_executor::task]
pub async fn button_task(pins: ButtonPins) -> ! {
    let buttons = [
        (Button::A, &pins.a),
        (Button::B, &pins.b),
        (Button::C, &pins.c),
        (Button::Up, &pins.up),
        (Button::Down, &pins.down),
        #[cfg(not(feature = "badger2040w"))]
        (Button::Home, &pins.home),
    ];

    let mut ticker = Ticker::every(POLL_PERIOD);
    // Debounce state per button: last raw sample and the accepted stable state.
    let mut last_raw = [false; 6];
    let mut stable = [false; 6];
    #[cfg(not(feature = "badger2040w"))]
    let mut home_held_polls: u32 = 0;
    #[cfg(feature = "badger2040w")]
    let mut updown_held_polls: u32 = 0;

    loop {
        ticker.next().await;

        for (i, (button, pin)) in buttons.iter().enumerate() {
            #[cfg(not(feature = "badger2040w"))]
            let raw = pin.is_low(); // active low: low = pressed
            #[cfg(feature = "badger2040w")]
            let raw = pin.is_high(); // active high: high = pressed
            if raw == last_raw[i] && raw != stable[i] {
                stable[i] = raw;
                let event = if raw {
                    ButtonEvent::Pressed(*button)
                } else {
                    ButtonEvent::Released(*button)
                };
                if EVENTS.try_send(event).is_err() {
                    log::warn!("button event dropped: {:?}", event);
                }
            }
            last_raw[i] = raw;
        }

        // Long-press HOME: reboot to BOOTSEL for cable-only reflashing.
        #[cfg(not(feature = "badger2040w"))]
        if stable[5] {
            home_held_polls += 1;
            if home_held_polls == HOME_HOLD_POLLS {
                log::info!("HOME held; rebooting to BOOTSEL");
                // Give the log a moment to flush over USB.
                embassy_time::Timer::after_millis(50).await;
                // 0x0002 = REBOOT_TYPE_BOOTSEL.
                embassy_rp::rom_data::reboot(0x0002, 100, 0, 0);
            }
        } else {
            home_held_polls = 0;
        }

        // The Badger 2040 W has no HOME: holding UP and DOWN together for
        // two seconds reboots to BOOTSEL instead.
        #[cfg(feature = "badger2040w")]
        if stable[3] && stable[4] {
            updown_held_polls += 1;
            if updown_held_polls == 200 {
                log::info!("UP+DOWN held; rebooting to BOOTSEL");
                embassy_time::Timer::after_millis(50).await;
                crate::boot::reboot_to_bootsel();
            }
        } else {
            updown_held_polls = 0;
        }
    }
}

// Rust guideline compliant 2026-08-21
