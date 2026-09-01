//! The Badger 2040 W's UC8151 e-paper panel: 296x128, strictly 1-bit.
//!
//! A port of Pimoroni's `uc8151.cpp` driver (init sequence, waveform LUTs
//! and refresh flow taken verbatim). Four update speeds trade refresh
//! quality for time, from the flash-free OTP waveform (~4.5 s) down to
//! Turbo (~0.25 s) — this panel's party trick is how usable its fast
//! waveforms are, where the 2350's four-grey SSD1680 has no equivalent.
//!
//! Framebuffer format on the wire: column-major, 16 bytes (128 pixels) per
//! column, MSB = top pixel of the byte, bit set = white. [`Epd::present_levels`]
//! packs from the row-major level buffer that [`crate::gfx::dither`] fills.

use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, PIN_26, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_rp::Peri;
use embassy_time::{Duration, Timer};

use super::screen::{HEIGHT, WIDTH};

/// Packed plane size: one bit per pixel.
const PLANE_BYTES: usize = WIDTH * HEIGHT / 8;

// UC8151 command bytes (the subset this driver uses).
const PSR: u8 = 0x00;
const PWR: u8 = 0x01;
const POF: u8 = 0x02;
const PFS: u8 = 0x03;
const PON: u8 = 0x04;
const DSLP: u8 = 0x07;
const BTST: u8 = 0x06;
const DSP: u8 = 0x11;
const DRF: u8 = 0x12;
const DTM2: u8 = 0x13;
const LUT_VCOM: u8 = 0x20;
const LUT_WW: u8 = 0x21;
const LUT_BW: u8 = 0x22;
const LUT_WB: u8 = 0x23;
const LUT_BB: u8 = 0x24;
const PLL: u8 = 0x30;
const TSE: u8 = 0x41;
const CDI: u8 = 0x50;
const TCON: u8 = 0x60;
const PTOU: u8 = 0x92;

/// Refresh waveform speed, slowest (cleanest) to fastest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// The panel's factory OTP waveform: ~4.5 s, deepest blacks.
    Default,
    /// ~2.0 s.
    Medium,
    /// ~0.8 s.
    Fast,
    /// ~0.25 s: visibly grey blacks, but instant by e-paper standards.
    Turbo,
}

impl Speed {
    /// Rough refresh duration, for pacing UI decisions.
    pub fn millis(self) -> u32 {
        match self {
            Speed::Default => 4500,
            Speed::Medium => 2000,
            Speed::Fast => 800,
            Speed::Turbo => 250,
        }
    }
}

/// One waveform row template: the three active 6-byte phases of a LUT (the
/// rest of each LUT is zero padding). Rows are (vcom, ww, bw, wb, bb) level
/// bytes plus shared timing.
struct Luts {
    timing: [[u8; 5]; 3],
    pll: u8,
}

/// Timings per speed, from Pimoroni's medium/fast/turbo LUT tables — each
/// phase is [t0, t1, t2, t3, repeat].
const MEDIUM: Luts = Luts {
    timing: [[0x16, 0x16, 0x0d, 0x00, 0x01], [0x23, 0x23, 0x00, 0x00, 0x02], [0x16, 0x16, 0x0d, 0x00, 0x01]],
    pll: 0x3A, // 100 Hz
};
const FAST: Luts = Luts {
    timing: [[0x04, 0x04, 0x07, 0x00, 0x01], [0x0c, 0x0c, 0x00, 0x00, 0x02], [0x04, 0x04, 0x07, 0x00, 0x02]],
    pll: 0x39, // 200 Hz
};
const TURBO: Luts = Luts {
    timing: [[0x01, 0x01, 0x02, 0x00, 0x01], [0x02, 0x02, 0x00, 0x00, 0x02], [0x02, 0x02, 0x03, 0x00, 0x02]],
    pll: 0x39, // 200 Hz
};

/// Per-phase voltage-pattern level bytes (Pimoroni's tables): VCOM is always
/// 0x00; WW/BW share one pattern sequence and WB/BB the reverse.
const PHASE_WW_BW: [u8; 3] = [0x54, 0x60, 0xa8];
const PHASE_WB_BB: [u8; 3] = [0xa8, 0x60, 0x54];

/// The e-paper panel plus its packed plane.
pub struct Epd {
    spi: Spi<'static, SPI0, spi::Blocking>,
    cs: Output<'static>,
    dc: Output<'static>,
    reset: Output<'static>,
    busy: Input<'static>,
    speed: Speed,
    plane: [u8; PLANE_BYTES],
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
        busy: Peri<'static, PIN_26>,
    ) -> Self {
        let mut config = spi::Config::default();
        config.frequency = 12_000_000;
        Self {
            spi: Spi::new_blocking_txonly(spi, sclk, mosi, config),
            cs: Output::new(cs, Level::High),
            dc: Output::new(dc, Level::Low),
            reset: Output::new(reset, Level::High),
            busy: Input::new(busy, Pull::Up),
            speed: Speed::Default,
            plane: [0; PLANE_BYTES],
        }
    }

    /// Hardware reset and full panel setup for the current speed.
    ///
    /// Every present ends by parking the panel in deep sleep (see
    /// [`Epd::refresh`]), so every present also begins with this — calling
    /// it manually is only needed to warm the panel before a first draw.
    pub async fn init(&mut self) {
        self.reset.set_low();
        Timer::after(Duration::from_millis(10)).await;
        self.reset.set_high();
        Timer::after(Duration::from_millis(10)).await;
        self.busy_wait().await;
        self.setup().await;
    }

    /// Selects the waveform speed for subsequent presents (applied by the
    /// re-init at the start of the next refresh).
    pub fn set_speed(&mut self, speed: Speed) {
        self.speed = speed;
    }

    /// The current refresh speed.
    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// Panel configuration, mirroring `UC8151::setup()`.
    async fn setup(&mut self) {
        // PSR: 128x296, B/W, booster on, no reset; LUTs from OTP at the
        // default speed, from registers otherwise; shift right, scan down.
        let lut_src = if self.speed == Speed::Default { 0x00 } else { 0x20 };
        self.command(PSR, &[0x80 | 0x10 | 0x02 | 0x01 | lut_src | 0x04]);

        match self.speed {
            Speed::Default => {}
            Speed::Medium => self.write_luts(&MEDIUM).await,
            Speed::Fast => self.write_luts(&FAST).await,
            Speed::Turbo => self.write_luts(&TURBO).await,
        }

        // Power: internal DC/DC, VCOM_VD, VGHL 16 V, +-11 V source.
        self.command(PWR, &[0x03, 0x00, 0x2b, 0x2b, 0x2b]);
        self.command(PON, &[]);
        self.busy_wait().await;

        // Booster soft-start, power-off frames, internal temp sensor.
        self.command(BTST, &[0x17, 0x17, 0x17]);
        self.command(PFS, &[0x00]);
        self.command(TSE, &[0x00]);
        self.command(TCON, &[0x22]);
        // VCOM/data interval, non-inverted.
        self.command(CDI, &[0b0100_1100]);
        // Pimoroni's setup ends with an unconditional 100 Hz PLL (even
        // after the fast/turbo LUTs set 200 Hz) — ported verbatim, since
        // the known-good timings were measured with this in place.
        self.command(PLL, &[0x3A]);
        self.command(POF, &[]);
        self.busy_wait().await;
    }

    /// Uploads the three-phase register LUT set for `luts`.
    async fn write_luts(&mut self, luts: &Luts) {
        let mut buf = [0u8; 44];
        // VCOM: level byte 0x00 for every phase.
        for (i, t) in luts.timing.iter().enumerate() {
            buf[i * 6] = 0x00;
            buf[i * 6 + 1..i * 6 + 6].copy_from_slice(t);
        }
        self.command(LUT_VCOM, &buf);
        for (cmd, phases) in [(LUT_WW, &PHASE_WW_BW), (LUT_BW, &PHASE_WW_BW), (LUT_WB, &PHASE_WB_BB), (LUT_BB, &PHASE_WB_BB)] {
            let mut lut = [0u8; 42];
            for (i, t) in luts.timing.iter().enumerate() {
                lut[i * 6] = phases[i];
                lut[i * 6 + 1..i * 6 + 6].copy_from_slice(t);
            }
            self.command(cmd, &lut);
        }
        self.command(PLL, &[luts.pll]);
        self.busy_wait().await;
    }

    /// Refreshes the panel from a row-major buffer of levels (0 = black,
    /// nonzero = white), one byte per pixel, as produced by
    /// [`crate::gfx::dither::quantize_mono`].
    pub async fn present_levels(&mut self, levels: &[u8]) {
        // Pack row-major levels into the panel's column-major bit plane.
        self.plane.fill(0);
        for y in 0..HEIGHT {
            let row = &levels[y * WIDTH..(y + 1) * WIDTH];
            let byte_in_col = y / 8;
            let bit = 0x80u8 >> (y % 8);
            for (x, &level) in row.iter().enumerate() {
                // Panel polarity (hardware-verified): bit set = black.
                if level == 0 {
                    self.plane[x * (HEIGHT / 8) + byte_in_col] |= bit;
                }
            }
        }
        self.refresh().await;
    }

    /// Full refresh cycle, ending in deep sleep.
    ///
    /// Unlike the SSD1680, a UC8151 left merely powered-off scribbles
    /// random lines on the panel as its supply rail collapses (unplugging
    /// USB, dropping the EN_3V3 latch). Deep sleep deafens the controller
    /// until the next hardware reset, so every refresh re-inits on the way
    /// in and parks the panel comatose on the way out — a power cut can
    /// then never reach a listening controller.
    async fn refresh(&mut self) {
        self.init().await;
        self.command(PON, &[]);
        self.command(PTOU, &[]);
        self.command_long(DTM2);
        self.data(&{ self.plane });
        self.command(DSP, &[]);
        self.command(DRF, &[]);
        self.busy_wait().await;
        self.command(POF, &[]);
        self.busy_wait().await;
        self.command(DSLP, &[0xA5]);
    }

    /// BUSY is active low; yield until the panel is idle.
    async fn busy_wait(&mut self) {
        while self.busy.is_low() {
            Timer::after(Duration::from_millis(1)).await;
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

    /// Starts a command whose data follows via [`Epd::data`].
    fn command_long(&mut self, reg: u8) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[reg]);
        self.dc.set_high();
    }

    fn data(&mut self, data: &[u8]) {
        let _ = self.spi.blocking_write(data);
        self.cs.set_high();
    }
}

// Rust guideline compliant 2026-08-31
