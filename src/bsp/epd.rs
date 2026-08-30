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
const GSS: u8 = 0x0F; // gate scan start position
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

/// Phase frame counts (groups 0..2, six phases each) for a full refresh —
/// the values baked into [`LUT`].
const FULL_COUNTS: [[u8; 6]; 3] = [
    [0x02, 0x00, 0x00, 0x05, 0x0A, 0x00],
    [0x19, 0x19, 0x00, 0x02, 0x00, 0x00],
    [0x05, 0x0A, 0x00, 0x00, 0x00, 0x00],
];

/// Roughly halved frame counts for partial refreshes: pixels outside the
/// scanned gate window still see source/VCOM activity and drift a little
/// with every pass, so partial ticks use the shortest waveform that settles
/// black and white (grey accuracy matters less there — keep frequently
/// ticking content two-tone).
const PARTIAL_COUNTS: [[u8; 6]; 3] = [
    [0x01, 0x00, 0x00, 0x03, 0x05, 0x00],
    [0x0c, 0x0c, 0x00, 0x01, 0x00, 0x00],
    [0x03, 0x05, 0x00, 0x00, 0x00, 0x00],
];

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

        // 0b001: x increments, y decrements.
        self.command(DEM, &[0b001]);
        self.set_full_window();
        self.busy_wait().await;
    }

    /// Gate range, RAM window and counters for a whole-panel refresh.
    fn set_full_window(&mut self) {
        self.command(DOC, &[Y_START_L, Y_START_H, 0x00]);
        self.command(GSS, &[0x00, 0x00]);
        self.command(SRX, &[X_START, X_END]);
        self.command(SRY, &[Y_START_L, Y_START_H, Y_END_L, Y_END_H]);
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

    /// Refreshes only the columns `x0..x1` from `levels`, leaving the rest
    /// of the panel untouched (no full-screen blink).
    ///
    /// The SSD1680's gate axis is our horizontal axis, so a partial window
    /// is a vertical strip; the refresh scans just those gates, taking
    /// roughly `TURBO * columns / WIDTH` plus fixed overhead. Content that
    /// updates often (a countdown, a spinner) should live in a narrow
    /// column band and use this.
    pub async fn present_partial(&mut self, levels: &[u8], x0: usize, x1: usize) {
        let (x0, x1) = (x0.min(WIDTH), x1.min(WIDTH));
        if x0 >= x1 {
            return;
        }
        pack_levels_columns(levels, &mut self.planes, x0, x1);
        self.busy_wait().await;
        self.write_luts_with(&PARTIAL_COUNTS, 0).await;

        // Our column x maps to controller gate/row (WIDTH-1 - x) (the data
        // entry mode counts Y down). Scan only those gates, and window the
        // RAM to the same rows.
        let count = (x1 - x0) as u16;
        let start = (WIDTH - x1) as u16;
        let row_hi = (WIDTH - 1 - x0) as u16;
        self.command(DOC, &[(count - 1) as u8, (count >> 8) as u8, 0x00]);
        self.command(GSS, &[start as u8, (start >> 8) as u8]);
        self.command(SRX, &[X_START, X_END]);
        self.command(SRY, &[row_hi as u8, (row_hi >> 8) as u8, start as u8, (start >> 8) as u8]);
        for (cmd, plane) in [(WRAM_R, 0usize), (WRAM_BW, 1)] {
            self.command(SRXC, &[X_START]);
            self.command(SRYC, &[row_hi as u8, (row_hi >> 8) as u8]);
            self.command(cmd, &[]);
            let Self { spi, cs, dc, planes, .. } = self;
            cs.set_low();
            dc.set_high();
            let bytes_per_col = HEIGHT / 8;
            let _ = spi.blocking_write(&planes[plane][x0 * bytes_per_col..x1 * bytes_per_col]);
            cs.set_high();
        }
        self.command(DUC2, &[0xC7]);
        self.busy_wait().await;
        self.command(ADUS, &[]);
        self.busy_wait().await;
        // Restore whole-panel scanning for the next full refresh.
        self.set_full_window();
    }

    /// Streams both planes and runs the waveform, returning once the refresh
    /// has finished (1–4 s depending on [`Speed`]; the wait is async, so
    /// buttons and logging keep running).
    async fn refresh(&mut self) {
        self.busy_wait().await;
        self.write_luts().await;
        self.set_full_window();
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
        self.write_luts_with(&FULL_COUNTS, 3 - self.speed as u8).await;
    }

    /// Writes the waveform with the given phase frame counts and repeat.
    async fn write_luts_with(&mut self, counts: &[[u8; 6]; 3], repeat: u8) {
        let mut lut = LUT;
        for (g, group) in counts.iter().enumerate() {
            lut[60 + g * 7..60 + g * 7 + 6].copy_from_slice(group);
        }
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
    pack_levels_columns(levels, planes, 0, WIDTH);
}

/// Packs only columns `x0..x1` (their plane bytes are contiguous).
fn pack_levels_columns(levels: &[u8], planes: &mut [[u8; PLANE_BYTES]; 2], x0: usize, x1: usize) {
    let bytes_per_col = HEIGHT / 8;
    for p in planes.iter_mut() {
        p[x0 * bytes_per_col..x1 * bytes_per_col].fill(0);
    }
    for y in 0..HEIGHT {
        let row = &levels[y * WIDTH..(y + 1) * WIDTH];
        for (x, &level) in row.iter().enumerate().take(x1).skip(x0) {
            put_level(planes, x, y, level & 3);
        }
    }
}

// Rust guideline compliant 2026-08-30
