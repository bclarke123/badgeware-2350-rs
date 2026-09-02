//! Sleep / power-off support, emulating the stock firmware's RESET-button
//! behavior via the RP2350 POWMAN block.
//!
//! How the stock firmware does it (ported from `modules/c/powman` in the
//! tufty2350 repo): the RESET button is wired to the chip's RUN line *and*
//! readable on GPIO14 after reboot. A short press is therefore just a reset;
//! if the firmware finds GPIO14 still held at boot, the user is long-pressing,
//! and after an LED ramp it powers the chip fully off (POWMAN state P1.7) with
//! wake-up armed on GPIO15 (`SWITCH_INT` — the five front buttons are
//! diode-OR'd onto it, so any button wakes) and GPIO13 (RTC alarm). Waking is
//! a cold boot.
//!
//! POWMAN register sequences are ported from pico-sdk's `hardware_powman`;
//! every POWMAN write requires the 0x5afe password in the top 16 bits.
//! Experimenting here is safe: the RESET button acts on the RUN line in
//! hardware, so a badge that sleeps wrong can always be reset or BOOTSEL'd.

use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::pac;
use embassy_rp::pac::powman::regs::{Pwrup, SeqCfg, State};
use embassy_rp::peripherals::{PIN_0, PIN_1, PIN_14, PIN_2, PIN_3};
use embassy_rp::Peri;
use embassy_time::{Duration, Instant, Timer};

/// How long RESET must stay held after reboot to trigger sleep (the stock LED
/// swoosh runs ~1.4 s; this matches its feel).
const RESET_HOLD: Duration = Duration::from_millis(1500);

/// Every POWMAN register write must carry this password in bits 31:16.
const PASSWORD: u32 = 0x5afe << 16;

/// Front-button interrupt line (diode-OR of all five buttons, active low).
const GPIO_SWITCH_INT: u8 = 15;
/// RTC alarm output (active low).
const GPIO_RTC_ALARM: u8 = 13;

/// If RESET (GPIO14) is held at boot, ramp the rear LEDs while it stays held
/// and power off once [`RESET_HOLD`] elapses; returns immediately (and quietly)
/// on a normal boot, or after an early release. Call before spawning tasks.
pub async fn sleep_if_reset_held(
    reset: Peri<'_, PIN_14>,
    led0: Peri<'_, PIN_0>,
    led1: Peri<'_, PIN_1>,
    led2: Peri<'_, PIN_2>,
    led3: Peri<'_, PIN_3>,
) {
    let reset = Input::new(reset, Pull::Up);
    if reset.is_high() {
        return;
    }

    let mut leds = [
        Output::new(led0, Level::Low),
        Output::new(led1, Level::Low),
        Output::new(led2, Level::Low),
        Output::new(led3, Level::Low),
    ];
    let start = Instant::now();
    let step = RESET_HOLD / 4;
    while reset.is_low() {
        let held = start.elapsed();
        if held >= RESET_HOLD {
            power_off();
        }
        // Light one more LED per quarter of the hold as a "keep holding" cue.
        let lit = (held.as_ticks() / step.as_ticks()) as usize + 1;
        for (i, led) in leds.iter_mut().enumerate() {
            led.set_level(if i < lit { Level::High } else { Level::Low });
        }
        Timer::after_millis(20).await;
    }
    // Released early: LEDs drop on scope exit, boot continues normally.
}

/// Powers the chip fully off; any front button (or the RTC alarm) causes a
/// cold boot. Never returns.
pub fn power_off() -> ! {
    cortex_m::interrupt::disable();
    quiesce_gpio();

    // Arm the wake sources (same channels as the stock firmware). Falling
    // edge, matching the active-low lines. Wait for each line to be idle
    // (high) first so a still-latched press does not wake us instantly.
    // A line stuck low (e.g. an uncleared RTC alarm holding INT down)
    // would make the armed edge unreachable — the badge would sleep
    // unwakeable — so bail out to a clean reboot instead: the app's boot
    // path clears the alarm and gets a fresh shot at sleeping.
    if !wait_gpio_high(GPIO_RTC_ALARM) || !wait_gpio_high(GPIO_SWITCH_INT) {
        log::warn!("power: wake line stuck low; rebooting instead of sleeping");
        reboot_normal();
    }
    arm_gpio_wakeup(1, GPIO_RTC_ALARM);
    arm_gpio_wakeup(3, GPIO_SWITCH_INT);

    // Sleep state P1.7 (all domains off) waking into P0.3 (switched core +
    // XIP cache on, both SRAM banks staying off, as the stock firmware does).
    // SRAM transitions are off->off in both, which the sequencer encodes as
    // HW_PWRUP bits set (see pico-sdk powman_configure_wakeup_state).
    let sram_bits = {
        let mut m = SeqCfg(0);
        m.set_hw_pwrup_sram0(true);
        m.set_hw_pwrup_sram1(true);
        m.0
    };
    pm_set(pac::POWMAN.seq_cfg().as_ptr() as *mut u32, sram_bits);

    // Wake into the normal boot path.
    for i in 0..4 {
        pm_write(pac::POWMAN.boot(i).as_ptr(), 0);
    }

    enter_off();
}

/// Reconfigures every GPIO for minimum sleep current, mirroring the stock
/// firmware's table: buttons and wake lines pulled up, PSRAM CS pulled up,
/// analog/power pins floating, everything else pulled down; all pads set to
/// SIO inputs with input buffers disabled except the wake lines.
fn quiesce_gpio() {
    for i in 0..48usize {
        let is_wake = i == usize::from(GPIO_SWITCH_INT) || i == usize::from(GPIO_RTC_ALARM);
        // Function select 5 = SIO, output disabled.
        pac::IO_BANK0.gpio(i).ctrl().write(|w| w.set_funcsel(5));
        pac::SIO.gpio_oe(i / 32).value_clr().write_value(1 << (i % 32));
        pac::PADS_BANK0.gpio(i).modify(|w| {
            w.set_ie(is_wake);
            w.set_iso(false);
            match i {
                // PSRAM CS and the front buttons (their pull-ups feed the
                // diode-OR onto SWITCH_INT) plus the wake lines: pull up.
                8 | 6 | 7 | 9 | 10 | 11 | 13 | 15 => {
                    w.set_pue(true);
                    w.set_pde(false);
                }
                // RESET_SW, HOME, POWER_EN, VBUS detect, and the analog pins:
                // floating.
                12 | 14 | 22 | 40 | 41 | 42 => {
                    w.set_pue(false);
                    w.set_pde(false);
                }
                _ => {
                    w.set_pue(false);
                    w.set_pde(true);
                }
            }
        });
    }
}

/// Busy-waits (bounded) for a wake line to sit in its idle high state.
fn wait_gpio_high(gpio: u8) -> bool {
    let bank = usize::from(gpio) / 32;
    let mask = 1u32 << (usize::from(gpio) % 32);
    // ~1 s at 150 MHz; matches the stock firmware's 1 s timeout.
    for _ in 0..1_500_000u32 {
        if pac::SIO.gpio_in(bank).read() & mask != 0 {
            return true;
        }
    }
    false
}

/// Arms one POWMAN GPIO wake channel for a falling edge (pico-sdk
/// `powman_enable_gpio_wakeup` sequence: configure, clear latched status,
/// then enable separately).
fn arm_gpio_wakeup(channel: usize, gpio: u8) {
    let reg = pac::POWMAN.pwrup(channel).as_ptr() as *mut u32;
    let mut cfg = Pwrup(0);
    cfg.set_source(gpio);
    cfg.set_direction(pac::powman::vals::Direction::LOW_FALLING);
    cfg.set_mode(pac::powman::vals::Mode::EDGE);
    pm_write(reg, cfg.0);

    let status = {
        let mut m = Pwrup(0);
        m.set_status(true);
        m.0
    };
    pm_clear(reg, status);

    let enable = {
        let mut m = Pwrup(0);
        m.set_enable(true);
        m.0
    };
    pm_set(reg, enable);
}

/// Requests the off state and parks the core. Runs from RAM because the XIP
/// cache domain powers down during the transition while these instructions
/// are still executing.
#[inline(never)]
#[link_section = ".data.power_off"]
fn enter_off() -> ! {
    let state = pac::POWMAN.state().as_ptr() as *mut u32;

    // POWMAN can ABANDON an off request: a wake edge landing while the
    // sequencer is waiting sets PWRUP_WHILE_WAITING and leaves the chip
    // running (observed in the field: one abandoned request stranded the
    // badge awake, radio on, for half an hour — STATE read 0x200 over
    // SWD). Retry a few times, clearing the abandonment flags and any
    // freshly latched wake status; if it still will not go down, a clean
    // reboot costs one extra wake cycle instead of the whole battery.
    for attempt in 0..5u32 {
        let flags = {
            let mut m = State(0);
            m.set_req_ignored(true);
            m.set_pwrup_while_waiting(true);
            m.0
        };
        pm_clear(state, flags);

        // Request all four domains off (REQ field is active-low domain mask).
        let req = {
            let mut m = State(0);
            m.set_req(0xf);
            m.0
        };
        pm_write(state, req);

        // POWMAN completes the power-down only while the processor sleeps
        // in WFI. Arm SysTick (~100 ms) so an ABANDONED request wakes us
        // to retry — a pended-but-masked interrupt wakes WFI without
        // running its handler. A successful power-off ends execution here.
        // SAFETY: raw SysTick register writes (CSR/RVR/CVR); the values
        // enable the counter with interrupt from the core clock.
        unsafe {
            core::ptr::write_volatile(0xE000_E014 as *mut u32, 15_000_000); // RVR ~100 ms
            core::ptr::write_volatile(0xE000_E018 as *mut u32, 0); // CVR reset
            core::ptr::write_volatile(0xE000_E010 as *mut u32, 0b111) // enable + tickint + core clock
        };
        cortex_m::asm::wfi();
        log::warn!("power: off request abandoned (attempt {})", attempt + 1);

        // A glitch re-latched a wake edge: clear channel status and retry.
        for ch in [1usize, 3] {
            let reg = pac::POWMAN.pwrup(ch).as_ptr() as *mut u32;
            let status = {
                let mut m = Pwrup(0);
                m.set_status(true);
                m.0
            };
            pm_clear(reg, status);
        }
    }
    log::warn!("power: could not power off; rebooting");
    reboot_normal();
}

/// Clean reboot into the normal boot path — the fallback whenever the
/// power-off path cannot proceed safely.
fn reboot_normal() -> ! {
    // 0x0000 = REBOOT_TYPE_NORMAL, 100 ms delay.
    embassy_rp::rom_data::reboot(0x0000, 100, 0, 0);
    loop {
        cortex_m::asm::wfe();
    }
}

/// Password-carrying full register write (value must fit in 16 bits).
fn pm_write(reg: *mut u32, value: u32) {
    debug_assert!(value >> 16 == 0);
    // SAFETY: reg is a valid POWMAN register address from the PAC; POWMAN
    // requires the password in the top halfword of every write.
    unsafe { reg.write_volatile(PASSWORD | value) }
}

/// Password-carrying atomic bit set via the +0x2000 alias.
fn pm_set(reg: *mut u32, bits: u32) {
    // SAFETY: the RP2350 maps an atomic bit-set alias of every register at
    // +0x2000 from its base address.
    unsafe { (reg.byte_add(0x2000)).write_volatile(PASSWORD | bits) }
}

/// Password-carrying atomic bit clear via the +0x3000 alias.
fn pm_clear(reg: *mut u32, bits: u32) {
    // SAFETY: the RP2350 maps an atomic bit-clear alias of every register at
    // +0x3000 from its base address.
    unsafe { (reg.byte_add(0x3000)).write_volatile(PASSWORD | bits) }
}

// Rust guideline compliant 2026-08-21
