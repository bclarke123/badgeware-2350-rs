//! The tree garden: a procedurally grown 3D tree on a dual-core software
//! renderer. `cargo run --release --example tree` for the Tufty 2350 (LCD);
//! add `--no-default-features --features badger` for the Badger 2350's
//! four-grey e-paper (it runs, but was designed for a backlit screen).
//!
//! Boot order matters here: `POWER_EN` (GPIO41 on the Tufty, GPIO27 on the
//! Badger) is driven high immediately so the badge stays latched on when
//! running from battery, before anything slow happens.
//!
//! No debug probe is required. Flashing goes over USB via `picotool` (see
//! `.cargo/config.toml`), logs are plain text on the USB serial port, and holding
//! HOME for two seconds reboots into BOOTSEL for reflashing.

#![no_std]
#![no_main]

use tufty_2350::{bsp, flora, gfx, render3d};

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
#[cfg(feature = "tufty")]
use embassy_rp::peripherals::PIO1;
#[cfg(feature = "tufty")]
use embassy_rp::{adc, dma, pio};
use embassy_rp::{bind_interrupts, usb};
use static_cell::ConstStaticCell;

#[cfg(feature = "tufty")]
use bsp::backlight::Backlight;
use bsp::buttons::ButtonPins;
#[cfg(feature = "tufty")]
use bsp::display::Display;
#[cfg(feature = "badger")]
use bsp::epd::Epd;
use bsp::leds::RearLeds;
use gfx::{FrameBuffer, FB_BYTES};

#[cfg(feature = "tufty")]
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});
#[cfg(feature = "badger")]
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// The 150 KiB RGB565 framebuffer lives in `.bss` (const-initialized in place,
/// never on the stack).
static FRAMEBUFFER: ConstStaticCell<[u8; FB_BYTES]> = ConstStaticCell::new([0; FB_BYTES]);

/// The two 3D triangle lists (64 KiB each, `.bss`): during the parallel
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

/// Leaf and blossom shade maps, baked once at boot and then read by both
/// cores' rasterizers.
static TEXTURES: ConstStaticCell<render3d::texture::Textures> =
    ConstStaticCell::new(render3d::texture::Textures::EMPTY);

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut p = embassy_rp::init(Default::default());

    // Latch board power before anything else: on battery, the badge browns out
    // if this pin is not asserted shortly after boot. It also powers the light
    // sensor, RTC, and Qwiic connector. Forget the pin so it stays high forever.
    #[cfg(feature = "tufty")]
    let power_en = Output::new(p.PIN_41, Level::High);
    #[cfg(feature = "badger")]
    let power_en = Output::new(p.PIN_27, Level::High);
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
    let leds = RearLeds::new(p.PWM_SLICE0, p.PWM_SLICE1, p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3);
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

    let frame = FrameBuffer::new(FRAMEBUFFER.take());

    #[cfg(feature = "tufty")]
    let display = {
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

        // Light sensor for automatic backlight (GPIO43 / ADC).
        let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
        let light = adc::Channel::new_pin(p.PIN_43, Pull::None);
        let vbat = adc::Channel::new_pin(p.PIN_40, Pull::None);
        spawner.spawn(bsp::backlight::auto_backlight_task(adc, light, vbat).unwrap());

        // First frame is black; light the backlight only after it is on
        // screen so power-on never shows random panel RAM.
        display.present(frame.bytes()).await;
        backlight.set_brightness(200);
        spawner.spawn(bsp::backlight::backlight_task(backlight).unwrap());
        display
    };

    #[cfg(feature = "badger")]
    let display = {
        // E-paper: SSD1680 on SPI0. No backlight, no light sensor.
        let mut display = Epd::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_17, p.PIN_20, p.PIN_21, p.PIN_16);
        display.init().await;
        log::info!("e-paper initialised");
        display
    };

    let textures = TEXTURES.take();
    textures.generate();

    log::info!("entering flora");
    flora::run(
        display,
        frame,
        TRI_LIST.take(),
        TRI_LIST_B.take(),
        TREE.take(),
        TERRAIN.take(),
        textures,
    )
    .await;
}

// Rust guideline compliant 2026-08-30
