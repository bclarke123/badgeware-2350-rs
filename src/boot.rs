//! Boot plumbing every firmware image needs, in one place so each binary
//! (`examples/*.rs`) is just its `main`: the RP2350 image definition and
//! picotool metadata, and the fault/panic strategy for a probe-less board.

#[cfg(not(feature = "badger2040w"))]
use embassy_rp::block::ImageDef;

/// Boot ROM image definition; without this block in `.start_block` the RP2350
/// boot ROM refuses to run the image (the board would appear dead after
/// flashing). The RP2040 boards boot via the boot2 loader instead, which the
/// `boot2-*` Cargo feature links in.
#[cfg(not(feature = "badger2040w"))]
#[link_section = ".start_block"]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

/// Metadata shown by `picotool info`.
#[cfg(not(feature = "badger2040w"))]
#[link_section = ".bi_entries"]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 3] = [
    embassy_rp::binary_info::rp_program_name!(c"tufty-2350"),
    embassy_rp::binary_info::rp_program_description!(c"Dual-core 3D procedural tree garden"),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

/// Reboots into the USB bootloader, per chip (the ROM APIs differ).
pub fn reboot_to_bootsel() -> ! {
    #[cfg(feature = "badger2040w")]
    embassy_rp::rom_data::reset_to_usb_boot(0, 0);
    #[cfg(not(feature = "badger2040w"))]
    embassy_rp::rom_data::reboot(0x0002, 100, 0, 0);
    loop {
        cortex_m::asm::wfe();
    }
}

/// Fault strategy matches the panic strategy: any hard fault on either core
/// (both share this vector table) reboots into BOOTSEL so the badge is always
/// reflashable — a faulted core 1 must not leave core 0 frozen at a join.
#[cortex_m_rt::exception]
unsafe fn HardFault(_frame: &cortex_m_rt::ExceptionFrame) -> ! {
    reboot_to_bootsel();
}

/// Panic strategy for a probe-less board: give a human five seconds to notice
/// (and a USB host time to read any last log), then reboot into BOOTSEL so the
/// badge can always be reflashed and never feels bricked.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log::error!("panic: {}", info);
    // ~5 s at the default system clock (150 MHz RP2350 / 125 MHz RP2040).
    cortex_m::asm::delay(700_000_000);
    reboot_to_bootsel();
}

// Rust guideline compliant 2026-08-30
