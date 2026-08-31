//! The 8 MB QSPI PSRAM (APS6404-class, QMI chip-select 1, CS = GPIO8 on both
//! boards): after [`init`] it is plain memory-mapped RAM at
//! [`PSRAM_BASE`] — slower than SRAM (quad-SPI: ~30 MB/s sequential,
//! hundreds of ns per random access) but thirty times bigger, and writable
//! at runtime unlike flash. Contents are lost on power-off.
//!
//! The bring-up sequence is ported from SparkFun's `sfe_psram.c` (MIT),
//! itself derived from the CircuitPython RP2350 support: probe the chip in
//! QMI direct mode (reset, read ID), switch it to quad mode, program the
//! M1 window timing from the system clock and the quad read/write formats,
//! and mark the XIP window writable.
//!
//! Direct mode takes over the QMI that flash XIP also runs on, so the core
//! of the init executes from RAM with interrupts disabled and touches
//! nothing flash-resident. Call [`init`] once, early, before core 1 runs.

use embassy_rp::pac;

/// The M1 (chip-select 1) XIP window: PSRAM appears here after [`init`].
pub const PSRAM_BASE: usize = 0x1100_0000;

/// PSRAM max clock at 3.3 V (APS6404 datasheet).
const MAX_SCK_HZ: u32 = 109_000_000;

/// Expected "known good die" ID byte.
const PSRAM_KGD: u32 = 0x5D;

// Command bytes.
const CMD_QUAD_END: u32 = 0xF5;
const CMD_QUAD_ENABLE: u32 = 0x35;
const CMD_READ_ID: u32 = 0x9F;
const CMD_RSTEN: u32 = 0x66;
const CMD_RST: u32 = 0x99;
const CMD_QUAD_READ: u8 = 0xEB;
const CMD_QUAD_WRITE: u8 = 0x38;
const CMD_NOOP: u32 = 0xFF;

/// Probes and configures the PSRAM. Returns the whole chip as a `'static`
/// byte slice (or `None` if no PSRAM answers), after a write/readback
/// self-test on a few addresses.
pub fn init() -> Option<&'static mut [u8]> {
    // CS1 pin: function XIP_SS_N_1 (funcsel 9), pad driven, not isolated.
    pac::PADS_BANK0.gpio(8).modify(|w| {
        w.set_iso(false);
        w.set_ie(true);
        w.set_od(false);
    });
    pac::IO_BANK0.gpio(8).ctrl().modify(|w| w.set_funcsel(9));

    // Read the system clock while flash access is still allowed.
    let sys_hz = embassy_rp::clocks::clk_sys_freq();

    let size = cortex_m::interrupt::free(|_| setup(sys_hz));
    if size == 0 {
        log::warn!("psram: no chip detected");
        return None;
    }

    // SAFETY: the QMI now maps `size` bytes of PSRAM at PSRAM_BASE, and this
    // function's one-shot use hands out the only reference.
    let ram = unsafe { core::slice::from_raw_parts_mut(PSRAM_BASE as *mut u8, size) };

    // Self-test a few spots (start, middle, end) before trusting it.
    for &offset in &[0usize, size / 2 + 1, size - 4] {
        let marker = (offset as u32) ^ 0xA5A5_5A5A;
        // SAFETY: offset < size - 3; unaligned u32 access is fine on M33.
        unsafe {
            let p = ram.as_mut_ptr().add(offset).cast::<u32>();
            p.write_unaligned(marker);
            if p.read_unaligned() != marker {
                log::warn!("psram: readback failed at {:#x}", offset);
                return None;
            }
        }
    }
    log::info!("psram: {} MB at {:#010x}", size / (1024 * 1024), PSRAM_BASE);
    Some(ram)
}

/// Snapshot of the QMI M1 (PSRAM) window configuration.
///
/// The RP2350 ROM flash helpers (behind every flash erase/program, e.g.
/// [`super::settings`]) reset BOTH QMI chip selects, silently killing the
/// PSRAM window — the pico-sdk saves and restores these registers around
/// flash ops for exactly this reason. Capture with [`m1_save`] before any
/// flash write and put it back with [`m1_restore`].
pub struct M1Snapshot {
    timing: pac::qmi::regs::Timing,
    rfmt: pac::qmi::regs::Rfmt,
    rcmd: pac::qmi::regs::Rcmd,
    wfmt: pac::qmi::regs::Wfmt,
    wcmd: pac::qmi::regs::Wcmd,
    writable: bool,
}

/// Captures the current M1 window configuration.
pub fn m1_save() -> M1Snapshot {
    let m = pac::QMI.mem(1);
    M1Snapshot {
        timing: m.timing().read(),
        rfmt: m.rfmt().read(),
        rcmd: m.rcmd().read(),
        wfmt: m.wfmt().read(),
        wcmd: m.wcmd().read(),
        writable: pac::XIP_CTRL.ctrl().read().writable_m1(),
    }
}

/// Restores an M1 window configuration saved by [`m1_save`].
pub fn m1_restore(s: &M1Snapshot) {
    let m = pac::QMI.mem(1);
    m.timing().write_value(s.timing);
    m.rfmt().write_value(s.rfmt);
    m.rcmd().write_value(s.rcmd);
    m.wfmt().write_value(s.wfmt);
    m.wcmd().write_value(s.wcmd);
    pac::XIP_CTRL.ctrl().modify(|w| w.set_writable_m1(s.writable));
}

/// The direct-mode probe + window configuration. Runs from RAM with
/// interrupts off (the caller); returns the detected size in bytes.
#[link_section = ".data.psram"]
#[inline(never)]
fn setup(sys_hz: u32) -> usize {
    let qmi = pac::QMI;

    // ---- Probe in direct mode (slow clock: divide by 30).
    qmi.direct_csr().write(|w| {
        w.set_clkdiv(30);
        w.set_en(true);
    });
    while qmi.direct_csr().read().busy() {}

    // In case the chip is already in quad mode from a previous boot: exit it.
    qmi.direct_csr().modify(|w| w.set_assert_cs1n(true));
    qmi.direct_tx().write(|w| {
        w.set_oe(true);
        w.set_iwidth(pac::qmi::vals::Iwidth::Q);
        w.set_data(CMD_QUAD_END as u16);
    });
    while qmi.direct_csr().read().busy() {}
    let _ = qmi.direct_rx().read();
    qmi.direct_csr().modify(|w| w.set_assert_cs1n(false));

    // Read the ID: command then six clocked-out bytes.
    qmi.direct_csr().modify(|w| w.set_assert_cs1n(true));
    let mut kgd = 0u32;
    let mut eid = 0u32;
    for i in 0..7u32 {
        qmi.direct_tx()
            .write(|w| w.set_data(if i == 0 { CMD_READ_ID } else { CMD_NOOP } as u16));
        while !qmi.direct_csr().read().txempty() {}
        while qmi.direct_csr().read().busy() {}
        let rx = qmi.direct_rx().read().0;
        match i {
            5 => kgd = rx & 0xff,
            6 => eid = rx & 0xff,
            _ => {}
        }
    }
    qmi.direct_csr().modify(|w| w.set_assert_cs1n(false));

    if kgd != PSRAM_KGD {
        qmi.direct_csr().modify(|w| w.set_en(false));
        return 0;
    }
    let mut size: usize = 1024 * 1024;
    let size_id = eid >> 5;
    if eid == 0x26 || size_id == 2 {
        size *= 8;
    } else if size_id == 0 {
        size *= 2;
    } else if size_id == 1 {
        size *= 4;
    }

    // ---- Reset and switch the chip to quad mode.
    for cmd in [CMD_RSTEN, CMD_RST, CMD_QUAD_ENABLE] {
        qmi.direct_csr().modify(|w| w.set_assert_cs1n(true));
        qmi.direct_tx().write(|w| w.set_data(cmd as u16));
        while qmi.direct_csr().read().busy() {}
        qmi.direct_csr().modify(|w| w.set_assert_cs1n(false));
        cortex_m::asm::delay(50);
        let _ = qmi.direct_rx().read();
    }
    qmi.direct_csr().modify(|w| w.set_en(false));

    // ---- M1 timing from the system clock (values per the APS6404 sheet:
    // max 8 us select in units of 64 cycles, min 50 ns deselect).
    let clkdiv = sys_hz.div_ceil(MAX_SCK_HZ) as u8;
    let fs_per_cycle = 1_000_000_000_000_000u64 / u64::from(sys_hz);
    let max_select = (125_000_000u64 / fs_per_cycle) as u8;
    let min_deselect = (50_000_000u64.div_ceil(fs_per_cycle)) as u8;
    qmi.mem(1).timing().write(|w| {
        w.set_pagebreak(pac::qmi::vals::Pagebreak::_1024);
        w.set_select_hold(3);
        w.set_cooldown(1);
        w.set_rxdelay(1);
        w.set_max_select(max_select);
        w.set_min_deselect(min_deselect);
        w.set_clkdiv(clkdiv);
    });

    // ---- Quad read (0xEB, 24 dummy bits) and write (0x38) formats.
    use pac::qmi::vals::*;
    qmi.mem(1).rfmt().write(|w| {
        w.set_prefix_width(PrefixWidth::Q);
        w.set_addr_width(AddrWidth::Q);
        w.set_suffix_width(SuffixWidth::Q);
        w.set_dummy_width(DummyWidth::Q);
        w.set_dummy_len(DummyLen::_24);
        w.set_data_width(DataWidth::Q);
        w.set_prefix_len(PrefixLen::_8);
        w.set_suffix_len(SuffixLen::NONE);
    });
    qmi.mem(1).rcmd().write(|w| {
        w.set_prefix(CMD_QUAD_READ);
        w.set_suffix(0);
    });
    qmi.mem(1).wfmt().write(|w| {
        w.set_prefix_width(PrefixWidth::Q);
        w.set_addr_width(AddrWidth::Q);
        w.set_suffix_width(SuffixWidth::Q);
        w.set_dummy_width(DummyWidth::Q);
        w.set_dummy_len(DummyLen::NONE);
        w.set_data_width(DataWidth::Q);
        w.set_prefix_len(PrefixLen::_8);
        w.set_suffix_len(SuffixLen::NONE);
    });
    qmi.mem(1).wcmd().write(|w| {
        w.set_prefix(CMD_QUAD_WRITE);
        w.set_suffix(0);
    });

    // Allow writes through the M1 XIP window.
    pac::XIP_CTRL.ctrl().modify(|w| w.set_writable_m1(true));

    size
}

// Rust guideline compliant 2026-08-31
