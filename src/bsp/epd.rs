//! SSD1680 e-paper driver for the Badger 2350's 2.7" 264x176 four-grey panel.
//!
//! Wiring (Pimoroni `board/pins.csv`): SPI0 with SCLK on GPIO18 and MOSI on
//! GPIO19, CS GPIO17, DC GPIO20, RESET GPIO21, BUSY GPIO16 (high while the
//! panel is refreshing). 12 MHz, SPI mode 0, write-only.
//!
//! The command sequences, waveform LUT and RAM layout are ported verbatim
//! from Pimoroni's `modules/c/ssd1680/ssd1680.cpp` (the driver BadgeOS uses),
//! measured on hardware at 1.14 s (TURBO) to 3.67 s (SLOW) per full refresh
//! with four clean greys at every tier and no visible ghosting.
//!
//! # Greys
//!
//! The controller has two RAM planes, "black/white" (`0x24`) and "red"
//! (`0x26`); with Pimoroni's LUT the pair of bits per pixel selects one of
//! four greys. [`Epd::present`] takes our RGB565 column-major framebuffer
//! (luma, calibrated Bayer 8x8 via [`crate::gfx::dither`]);
//! [`Epd::present_levels`] takes already-quantized levels from the grey
//! canvas pipeline. The panel RAM is column-major with y fastest — the same
//! orientation as the framebuffer.

use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_rp::Peri;
use embassy_time::{Duration, Timer};

use super::screen::{HEIGHT, WIDTH};

/// Bytes per RAM plane: 176 rows / 8 per column, 264 columns.
pub const PLANE_BYTES: usize = WIDTH * HEIGHT / 8;

/// Refresh waveform speed; slower tiers repeat the LUT groups more often.
/// Measured full-update times on hardware: TURBO 1.14 s, FAST 1.98 s,
/// NORMAL 2.82 s, SLOW 3.67 s. All four give four distinct greys, but the
/// mid greys land slightly differently between TURBO and SLOW, so use one
/// tier consistently where tone matters (SLOW for "hero" frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "the full tier set is the driver's API; the scene uses Slow and Turbo")]
pub enum Speed {
    Slow = 0,
    Normal = 1,
    Fast = 2,
    Turbo = 3,
}

// Command bytes (SSD1680 datasheet names as in Pimoroni's driver).
const DOC: u8 = 0x01; // gate driver output
const GDVC: u8 = 0x03; // gate driving voltage
const SDVC: u8 = 0x04; // source driving voltage
const BTST: u8 = 0x0C; // booster soft start
const DEM: u8 = 0x11; // data entry mode
const SWR: u8 = 0x12; // software reset
const ADUS: u8 = 0x20; // activate display update
const DUC2: u8 = 0x22; // display update control 2
const WRAM_BW: u8 = 0x24; // write RAM (black/white)
const WRAM_R: u8 = 0x26; // write RAM (red / grey plane)
const WVCOM: u8 = 0x2C; // VCOM
const WLR: u8 = 0x32; // write LUT
const EOPT: u8 = 0x3F; // LUT end option
const SRX: u8 = 0x44; // RAM x start/end
const SRY: u8 = 0x45; // RAM y start/end
const SRXC: u8 = 0x4E; // RAM x counter
const SRYC: u8 = 0x4F; // RAM y counter

// RAM window: the controller's "x" is our y (176 px = 22 bytes), its "y" our
// x (264 lines), written x+ y- from line 263 down.
const X_START: u8 = 0x00;
const X_END: u8 = 0x15;
const Y_START_L: u8 = 0x07;
const Y_START_H: u8 = 0x01;
const Y_END_L: u8 = 0x00;
const Y_END_H: u8 = 0x00;

/// The four-grey waveform LUT: five voltage-sequence rows, twelve phase
/// groups, then the frame-rate/XON config. Bytes 66, 73 and 80 are the
/// repeat counts of groups 0..2, patched per [`Speed`].
const LUT: [u8; 153] = [
    // VS L0..L4, 12 groups each
    0x40, 0x68, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0xA0, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0xA8, 0x65, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0xAA, 0x65, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    // Group 0..2: L0 L1 L2 L3 L4 SR repeat
    0x02, 0x00, 0x00, 0x05, 0x0A, 0x00, 0x00, //
    0x19, 0x19, 0x00, 0x02, 0x00, 0x00, 0x00, //
    0x05, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, //
    // Groups 3..11 unused
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    // FR, XON config
    0x44, 0x42, 0x22, 0x22, 0x23, 0x32, 0x00, 0x00, 0x00,
];
const LUT_REPEAT_OFFSETS: [usize; 3] = [66, 73, 80];

/// The e-paper panel plus its two packed planes.
pub struct Epd {
    spi: Spi<'static, SPI0, spi::Blocking>,
    cs: Output<'static>,
    dc: Output<'static>,
    reset: Output<'static>,
    busy: Input<'static>,
    speed: Speed,
    planes: [[u8; PLANE_BYTES]; 2],
}

impl Epd {
    /// Claims SPI0 and the control pins. The panel is untouched until
    /// [`Epd::init`].
    pub fn new(
        spi: Peri<'static, SPI0>,
        sclk: Peri<'static, PIN_18>,
        mosi: Peri<'static, PIN_19>,
        cs: Peri<'static, PIN_17>,
        dc: Peri<'static, PIN_20>,
        reset: Peri<'static, PIN_21>,
        busy: Peri<'static, PIN_16>,
    ) -> Self {
        let mut config = spi::Config::default();
        config.frequency = 12_000_000;
        Self {
            spi: Spi::new_blocking_txonly(spi, sclk, mosi, config),
            cs: Output::new(cs, Level::High),
            dc: Output::new(dc, Level::Low),
            reset: Output::new(reset, Level::High),
            busy: Input::new(busy, Pull::Down),
            speed: Speed::Turbo,
            planes: [[0; PLANE_BYTES]; 2],
        }
    }

    /// Hardware + software reset and RAM window setup.
    pub async fn init(&mut self) {
        self.reset.set_low();
        Timer::after(Duration::from_millis(10)).await;
        self.reset.set_high();
        Timer::after(Duration::from_millis(10)).await;
        self.busy_wait().await;

        self.command(SWR, &[]);
        self.busy_wait().await;

        self.command(DOC, &[Y_START_L, Y_START_H, 0x00]);
        // 0b001: x increments, y decrements.
        self.command(DEM, &[0b001]);
        self.command(SRX, &[X_START, X_END]);
        self.command(SRY, &[Y_START_L, Y_START_H, Y_END_L, Y_END_H]);
        self.busy_wait().await;
    }

    /// Selects the waveform speed for subsequent presents.
    pub fn set_speed(&mut self, speed: Speed) {
        self.speed = speed;
    }

    /// Quantizes the RGB565 column-major framebuffer to four greys (luma,
    /// linear light, Bayer 8x8 against the calibrated panel levels) and
    /// refreshes the panel.
    pub async fn present(&mut self, frame: &[u8]) {
        pack_rgb565(frame, &mut self.planes);
        self.refresh().await;
    }

    /// Refreshes the panel from a row-major buffer of levels 0..=3 (black to
    /// white), one byte per pixel, as produced by [`crate::gfx::dither`].
    pub async fn present_levels(&mut self, levels: &[u8]) {
        pack_levels(levels, &mut self.planes);
        self.refresh().await;
    }

    /// Streams both planes and runs the waveform, returning once the refresh
    /// has finished (1–4 s depending on [`Speed`]; the wait is async, so
    /// buttons and logging keep running).
    async fn refresh(&mut self) {
        self.busy_wait().await;
        self.write_luts().await;
        for (cmd, plane) in [(WRAM_R, 0usize), (WRAM_BW, 1)] {
            self.command(SRXC, &[X_START]);
            self.command(SRYC, &[Y_START_L, Y_START_H]);
            self.command(cmd, &[]);
            let Self { spi, cs, dc, planes, .. } = self;
            cs.set_low();
            dc.set_high();
            let _ = spi.blocking_write(&planes[plane]);
            cs.set_high();
        }
        self.command(BTST, &[]);
        self.command(DUC2, &[0xC7]);
        self.busy_wait().await;
        self.command(ADUS, &[]);
        self.busy_wait().await;
    }

    async fn write_luts(&mut self) {
        let mut lut = LUT;
        let repeat = 3 - self.speed as u8;
        for &i in &LUT_REPEAT_OFFSETS {
            lut[i] = repeat;
        }
        self.command(WLR, &lut);
        self.command(EOPT, &[0x22]);
        self.command(GDVC, &[0x17]);
        self.command(SDVC, &[0x41, 0xAE, 0x32]);
        self.command(WVCOM, &[0x28]);
        self.busy_wait().await;
    }

    async fn busy_wait(&mut self) {
        if self.busy.is_high() {
            self.busy.wait_for_low().await;
        }
    }

    fn command(&mut self, reg: u8, data: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[reg]);
        if !data.is_empty() {
            self.dc.set_high();
            let _ = self.spi.blocking_write(data);
        }
        self.cs.set_high();
    }

}

/// Sets one pixel's level (0..=3) in both planes: plane 0 ("red") holds the
/// inverted high bit, plane 1 (black/white) the inverted low bit, one bit
/// per pixel, MSB = top row, column-major.
#[inline(always)]
fn put_level(planes: &mut [[u8; PLANE_BYTES]; 2], x: usize, y: usize, level: u8) {
    let bit = 7 - (y & 7);
    let i = (y + x * HEIGHT) / 8;
    planes[0][i] |= (!(level >> 1) & 1) << bit;
    planes[1][i] |= (!level & 1) << bit;
}

/// Packs an RGB565 (big-endian, column-major) framebuffer: luma, then the
/// calibrated Bayer 8x8 quantizer from [`crate::gfx::dither`].
fn pack_rgb565(frame: &[u8], planes: &mut [[u8; PLANE_BYTES]; 2]) {
    use crate::gfx::dither::{bayer8_threshold, ordered_level, Panel};
    let panel = Panel::current();
    planes[0].fill(0);
    planes[1].fill(0);
    for x in 0..WIDTH {
        let col = &frame[x * HEIGHT * 2..(x + 1) * HEIGHT * 2];
        for (y, px) in col.as_chunks::<2>().0.iter().enumerate() {
            let c = u16::from(px[0]) << 8 | u16::from(px[1]);
            let r = u32::from(c >> 11) & 31;
            let g = u32::from(c >> 5) & 63;
            let b = u32::from(c) & 31;
            let lum = ((r * 8 * 77 + g * 4 * 151 + b * 8 * 28) >> 8).min(255) as u8;
            put_level(planes, x, y, ordered_level(&panel, lum, bayer8_threshold(x, y)));
        }
    }
}

/// Packs a row-major buffer of levels 0..=3.
fn pack_levels(levels: &[u8], planes: &mut [[u8; PLANE_BYTES]; 2]) {
    planes[0].fill(0);
    planes[1].fill(0);
    for y in 0..HEIGHT {
        let row = &levels[y * WIDTH..(y + 1) * WIDTH];
        for (x, &level) in row.iter().enumerate() {
            put_level(planes, x, y, level & 3);
        }
    }
}

// Rust guideline compliant 2026-08-30
