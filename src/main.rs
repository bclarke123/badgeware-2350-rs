//! Tufty 2350 badge firmware: Embassy framework plus a Simon-style memory game.
//!
//! Boot order matters here: `POWER_EN` (GPIO41) is driven high immediately so the
//! badge stays latched on when running from battery, before anything slow happens.
//!
//! No debug probe is required. Flashing goes over USB via `picotool` (see
//! `.cargo/config.toml`), logs are plain text on the USB serial port, and holding
//! HOME for two seconds reboots into BOOTSEL for reflashing.

#![no_std]
#![no_main]

mod bsp;
mod flora;
mod gfx;
mod render3d;

use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIO1, USB};
use embassy_rp::{adc, bind_interrupts, dma, pio, usb};
use static_cell::ConstStaticCell;

use crate::bsp::backlight::Backlight;
use crate::bsp::buttons::ButtonPins;
use crate::bsp::display::Display;
use crate::bsp::leds::RearLeds;
use crate::gfx::{FrameBuffer, FB_BYTES};

/// Boot ROM image definition; without this block in `.start_block` the RP2350
/// boot ROM refuses to run the image (the board would appear dead after flashing).
#[link_section = ".start_block"]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

/// Metadata shown by `picotool info`.
#[link_section = ".bi_entries"]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 3] = [
    embassy_rp::binary_info::rp_program_name!(c"tufty-2350"),
    embassy_rp::binary_info::rp_program_description!(c"Dual-core 3D procedural tree garden"),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

/// The 150 KiB RGB565 framebuffer lives in `.bss` (const-initialized in place,
/// never on the stack).
static FRAMEBUFFER: ConstStaticCell<[u8; FB_BYTES]> = ConstStaticCell::new([0; FB_BYTES]);

/// The two 3D triangle lists (32 KiB each, `.bss`): during the parallel
/// geometry phase each core fills and sorts its own.
static TRI_LIST: ConstStaticCell<render3d::TriList> =
    ConstStaticCell::new(render3d::TriList::EMPTY);
static TRI_LIST_B: ConstStaticCell<render3d::TriList> =
    ConstStaticCell::new(render3d::TriList::EMPTY);

/// The generated tree (a few KiB — too large to live inside a task future).
static TREE: ConstStaticCell<flora::tree::Tree> = ConstStaticCell::new(flora::tree::Tree::EMPTY);

/// The generated ground patch (also static, regenerated with each seed).
static TERRAIN: ConstStaticCell<flora::terrain::Terrain> =
    ConstStaticCell::new(flora::terrain::Terrain::EMPTY);

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut p = embassy_rp::init(Default::default());

    // Latch board power before anything else: on battery, the badge browns out
    // if this pin is not asserted shortly after boot. It also powers the light
    // sensor, RTC, and Qwiic connector. Forget the pin so it stays high forever.
    let power_en = Output::new(p.PIN_41, Level::High);
    core::mem::forget(power_en);

    // Stock-firmware sleep emulation: if the RESET button (readable on GPIO14
    // after the reset it caused) is still held, ramp the rear LEDs and power
    // off with any-front-button wake. Returns immediately on a normal boot.
    bsp::power::sleep_if_reset_held(
        p.PIN_14.reborrow(),
        p.PIN_0.reborrow(),
        p.PIN_1.reborrow(),
        p.PIN_2.reborrow(),
        p.PIN_3.reborrow(),
    )
    .await;

    // Start core 1 as the rasterization coprocessor. Deliberately after the
    // sleep check: the power-off path never runs with core 1 alive.
    render3d::core1::spawn(p.CORE1);

    // USB logging first so later init steps can report progress.
    let usb_driver = usb::Driver::new(p.USB, Irqs);
    spawner.spawn(bsp::usb::logger_task(usb_driver).unwrap());

    log::info!("tufty-2350 booting");

    // Rear LED zones (GPIO0-3).
    let leds = RearLeds::new(p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3);
    spawner.spawn(bsp::leds::led_task(leds).unwrap());

    // Front buttons (active low, external pull-ups; internal pulls are harmless).
    let buttons = ButtonPins {
        a: Input::new(p.PIN_7, Pull::Up),
        b: Input::new(p.PIN_9, Pull::Up),
        c: Input::new(p.PIN_10, Pull::Up),
        up: Input::new(p.PIN_11, Pull::Up),
        down: Input::new(p.PIN_6, Pull::Up),
        home: Input::new(p.PIN_22, Pull::Up),
    };
    spawner.spawn(bsp::buttons::button_task(buttons).unwrap());

    // Backlight off until the first frame has been presented.
    let mut backlight = Backlight::new(p.PWM_SLICE5, p.PIN_26);

    // Display: ST7789 over the 8-bit 8080 parallel bus, PIO1 + DMA.
    let dma_ch = dma::Channel::new(p.DMA_CH0, Irqs);
    let mut display = Display::new(
        p.PIO1, Irqs, dma_ch, p.PIN_21, p.PIN_27, p.PIN_28, p.PIN_30, p.PIN_31, p.PIN_32,
        p.PIN_33, p.PIN_34, p.PIN_35, p.PIN_36, p.PIN_37, p.PIN_38, p.PIN_39,
    );
    display.init().await;
    log::info!("display initialised");

    let frame = FrameBuffer::new(FRAMEBUFFER.take());

    // Light sensor for automatic backlight (GPIO43 / ADC).
    let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let light = adc::Channel::new_pin(p.PIN_43, Pull::None);
    spawner.spawn(bsp::backlight::auto_backlight_task(adc, light).unwrap());

    // First frame is black; light the backlight only after it is on screen so
    // power-on never shows random panel RAM.
    display.present(frame.bytes()).await;
    backlight.set_brightness(200);
    spawner.spawn(bsp::backlight::backlight_task(backlight).unwrap());

    log::info!("entering flora");
    flora::run(display, frame, TRI_LIST.take(), TRI_LIST_B.take(), TREE.take(), TERRAIN.take()).await;
}

/// Fault strategy matches the panic strategy: any hard fault on either core
/// (both share this vector table) reboots into BOOTSEL so the badge is always
/// reflashable — a faulted core 1 must not leave core 0 frozen at a join.
#[cortex_m_rt::exception]
unsafe fn HardFault(_frame: &cortex_m_rt::ExceptionFrame) -> ! {
    embassy_rp::rom_data::reboot(0x0002, 100, 0, 0);
    loop {
        cortex_m::asm::wfe();
    }
}

/// Panic strategy for a probe-less board: give a human five seconds to notice
/// (and a USB host time to read any last log), then reboot into BOOTSEL so the
/// badge can always be reflashed and never feels bricked.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log::error!("panic: {}", info);
    // ~5 s at the default 150 MHz system clock.
    cortex_m::asm::delay(750_000_000);
    // 0x0002 = REBOOT_TYPE_BOOTSEL, 100 ms delay, no PC/SP.
    embassy_rp::rom_data::reboot(0x0002, 100, 0, 0);
    loop {
        cortex_m::asm::wfe();
    }
}

// Rust guideline compliant 2026-08-21
