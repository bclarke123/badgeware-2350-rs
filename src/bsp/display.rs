//! ST7789 display driver over the Tufty's 8-bit 8080 parallel bus.
//!
//! There is no off-the-shelf async driver for this wiring, so this is the one
//! piece of genuinely custom hardware support in the project. The design follows
//! Pimoroni's official C driver (`modules/c/st7789` in the tufty2350 repo):
//!
//! * A two-instruction PIO program clocks bytes onto DB0-7 (GPIO32-39) with the
//!   panel's WR strobe (GPIO30) as a side-set pin: data is set up while WR is
//!   low, and the rising edge latches it.
//! * Everything — init commands and pixel data — flows through that one state
//!   machine; the DC (data/command) line is toggled in software between bursts.
//! * Frames are fed by DMA as single bytes: with `ShiftDirection::Left` and an
//!   8-bit autopull threshold, each FIFO word yields exactly one bus byte
//!   (byte-size bus writes are lane-replicated, so the top byte is the pushed
//!   byte), and bytes stream in memory order.
//!
//! Pixel format on the wire is RGB565 big-endian (high byte first), so the
//! framebuffer stores pre-swapped bytes; see [`crate::gfx`].
//!
//! GPIO32-39 sit in the RP2350's second GPIO bank; `embassy-rp` configures the
//! PIO `GPIOBASE=16` window automatically because every pin used by the state
//! machine (WR=30, DB=32-39) is >= 16.

use embassy_rp::dma;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::peripherals::{
    PIN_21, PIN_27, PIN_28, PIN_30, PIN_31, PIN_32, PIN_33, PIN_34, PIN_35, PIN_36, PIN_37,
    PIN_38, PIN_39, PIO1,
};
use embassy_rp::pio::program::pio_asm;
use embassy_rp::pio::{
    Config, Direction, FifoJoin, Instance, InterruptHandler, Pio, ShiftConfig, ShiftDirection,
    StateMachine,
};
use embassy_rp::Peri;
use embassy_time::Timer;
use fixed::types::extra::U8;
use fixed::FixedU32;

/// Maximum PIO clock for the parallel bus, from Pimoroni's driver (`max_pio_clk`).
///
/// One byte takes two PIO cycles, so this yields ~22 Mbyte/s — faster than the
/// ST7789 datasheet's 66 ns write cycle, but proven on this exact hardware by
/// the stock firmware. The divider below rounds *up* (integer only) so we never
/// exceed it.
const MAX_PIO_CLK_HZ: u32 = 44_000_000;

// ST7789 command bytes (subset used here).
const SWRESET: u8 = 0x01;
const SLPOUT: u8 = 0x11;
const INVON: u8 = 0x21;
const CASET: u8 = 0x2A;
const RASET: u8 = 0x2B;
const RAMWR: u8 = 0x2C;
const MADCTL: u8 = 0x36;
const COLMOD: u8 = 0x3A;
const RAMCTRL: u8 = 0xB0;
const PORCTRL: u8 = 0xB2;
const GCTRL: u8 = 0xB7;
const VCOMS: u8 = 0xBB;
const LCMCTRL: u8 = 0xC0;
const VDVVRHEN: u8 = 0xC2;
const VRHS: u8 = 0xC3;
const VDVS: u8 = 0xC4;
const FRCTRL2: u8 = 0xC6;
const PWCTRL1: u8 = 0xD0;
const TEON: u8 = 0x35;
const STE: u8 = 0x44;
const DISPON: u8 = 0x29;
const GMCTRP1: u8 = 0xE0;
const GMCTRN1: u8 = 0xE1;

/// MADCTL matching the panel's native portrait scan: ROW_ORDER | SCAN_ORDER.
///
/// This is exactly what Pimoroni's shipped firmware uses. Frames are streamed
/// in scan order (the framebuffer is stored column-major, see [`crate::gfx`]),
/// which combined with the TE/vsync wait in [`Display::present`] means the
/// write never crosses the refresh beam — the fix for the diagonal tearing
/// visible with landscape (SWAP_XY) addressing.
const MADCTL_PORTRAIT_SCAN: u8 = 0x90;

/// TEON parameter 0: TE pulses on vblank only (mode 1 would add hblank pulses).
const TE_VBLANK_ONLY: u8 = 0x00;

/// ST7789 on the Tufty 2350's parallel bus (PIO1 SM0 + one DMA channel).
pub struct Display {
    dc: Output<'static>,
    cs: Output<'static>,
    /// RD is never used but must idle high or the panel drives the bus.
    _rd: Output<'static>,
    /// Tear-effect (vsync) signal from the panel; pulses high at each vblank.
    te: Input<'static>,
    sm: StateMachine<'static, PIO1, 0>,
    dma: dma::Channel<'static>,
}

impl Display {
    /// Claims PIO1 SM0, loads the parallel-bus program, and configures all pins.
    ///
    /// The panel itself is untouched until [`Display::init`] is called.
    #[expect(clippy::too_many_arguments, reason = "fixed board wiring, called once from main")]
    pub fn new(
        pio: Peri<'static, PIO1>,
        irqs: impl Binding<<PIO1 as Instance>::Interrupt, InterruptHandler<PIO1>>,
        dma: dma::Channel<'static>,
        te: Peri<'static, PIN_21>,
        cs: Peri<'static, PIN_27>,
        dc: Peri<'static, PIN_28>,
        wr: Peri<'static, PIN_30>,
        rd: Peri<'static, PIN_31>,
        db0: Peri<'static, PIN_32>,
        db1: Peri<'static, PIN_33>,
        db2: Peri<'static, PIN_34>,
        db3: Peri<'static, PIN_35>,
        db4: Peri<'static, PIN_36>,
        db5: Peri<'static, PIN_37>,
        db6: Peri<'static, PIN_38>,
        db7: Peri<'static, PIN_39>,
    ) -> Self {
        let Pio {
            mut common,
            mut sm0,
            ..
        } = Pio::new(pio, irqs);

        // Data is set up with WR low; the rising edge on the second instruction
        // latches it into the panel. Two PIO cycles per byte.
        let program = pio_asm!(
            ".side_set 1",
            ".wrap_target",
            "out pins, 8  side 0",
            "nop          side 1",
            ".wrap",
        );
        let loaded = common.load_program(&program.program);

        let wr = common.make_pio_pin(wr);
        let db = [
            common.make_pio_pin(db0),
            common.make_pio_pin(db1),
            common.make_pio_pin(db2),
            common.make_pio_pin(db3),
            common.make_pio_pin(db4),
            common.make_pio_pin(db5),
            common.make_pio_pin(db6),
            common.make_pio_pin(db7),
        ];

        let mut cfg = Config::default();
        cfg.use_program(&loaded, &[&wr]);
        cfg.set_out_pins(&[&db[0], &db[1], &db[2], &db[3], &db[4], &db[5], &db[6], &db[7]]);
        cfg.fifo_join = FifoJoin::TxOnly;
        cfg.shift_out = ShiftConfig {
            threshold: 8,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        // Integer divider, rounded up, so the bus never runs above MAX_PIO_CLK_HZ.
        let sys_hz = embassy_rp::clocks::clk_sys_freq();
        let divider = sys_hz.div_ceil(MAX_PIO_CLK_HZ).max(1);
        cfg.clock_divider = FixedU32::<U8>::from_num(divider);
        sm0.set_config(&cfg);

        sm0.set_pin_dirs(Direction::Out, &[&wr]);
        sm0.set_pin_dirs(
            Direction::Out,
            &[&db[0], &db[1], &db[2], &db[3], &db[4], &db[5], &db[6], &db[7]],
        );
        sm0.set_enable(true);

        Self {
            dc: Output::new(dc, Level::High),
            cs: Output::new(cs, Level::High),
            _rd: Output::new(rd, Level::High),
            te: Input::new(te, Pull::None),
            sm: sm0,
            dma,
        }
    }

    /// Runs the panel's power-on sequence (Pimoroni's, verbatim) and clears it.
    ///
    /// Takes ~300 ms due to mandatory post-reset and sleep-out delays. The
    /// backlight should stay off until the first real frame is presented.
    pub async fn init(&mut self) {
        self.command(SWRESET, &[]).await;
        Timer::after_millis(150).await;

        self.command(COLMOD, &[0x05]).await; // 16 bits per pixel
        self.command(PORCTRL, &[0x0c, 0x0c, 0x00, 0x33, 0x33]).await;
        self.command(LCMCTRL, &[0x2c]).await;
        self.command(VDVVRHEN, &[0x01]).await;
        self.command(VRHS, &[0x0f]).await;
        self.command(VDVS, &[0x20]).await;
        self.command(PWCTRL1, &[0xa4, 0xa1]).await;
        self.command(FRCTRL2, &[0x0f]).await;
        // Required to avoid grey banding in low-brightness greens; see
        // pimoroni-pico issue #1040.
        self.command(RAMCTRL, &[0x00, 0xc0]).await;
        self.command(GCTRL, &[0x35]).await;
        self.command(VCOMS, &[0x1b]).await;
        self.command(
            GMCTRP1,
            &[
                0xF0, 0x00, 0x06, 0x04, 0x05, 0x05, 0x31, 0x44, 0x48, 0x36, 0x12, 0x12, 0x2B,
                0x34,
            ],
        )
        .await;
        self.command(
            GMCTRN1,
            &[
                0xF0, 0x0B, 0x0F, 0x0F, 0x0D, 0x26, 0x31, 0x43, 0x47, 0x38, 0x14, 0x14, 0x2C,
                0x32,
            ],
        )
        .await;

        self.command(INVON, &[]).await;
        self.command(SLPOUT, &[]).await;
        Timer::after_millis(100).await;

        self.command(MADCTL, &[MADCTL_PORTRAIT_SCAN]).await;
        // Full-screen address window in the panel's native portrait orientation
        // (columns 0-239, rows 0-319), big-endian u16 pairs.
        self.command(CASET, &[0x00, 0x00, 0x00, 0xEF]).await;
        self.command(RASET, &[0x00, 0x00, 0x01, 0x3F]).await;

        // Tear-effect line on (vblank pulses only, scanline 0).
        self.command(TEON, &[TE_VBLANK_ONLY]).await;
        self.command(STE, &[0x00, 0x00]).await;

        self.command(DISPON, &[]).await;
    }

    /// Streams a full framebuffer (RGB565 big-endian bytes, panel scan order)
    /// to the panel, synchronized to vblank so the write never tears.
    ///
    /// Waits for the next TE pulse (up to one panel refresh, ~17 ms), then
    /// streams the frame in ~8 ms — faster than the refresh beam it starts
    /// ahead of, so the beam never catches the write. The stream itself is
    /// entirely DMA-driven; the CPU is free while awaiting.
    pub async fn present(&mut self, frame: &[u8]) {
        self.present_begin().await;
        self.present_dma(frame).await;
        self.present_end().await;
    }

    /// First third of a presentation: waits for vblank (TE) and opens the RAM
    /// write. Split from [`Display::present_dma`] / [`Display::present_end`]
    /// so the caller can do CPU work that does not touch the framebuffer
    /// (the next frame's geometry) while the ~7 ms DMA streams the panel.
    pub async fn present_begin(&mut self) {
        // Robustness on unexpected panels/states: if TE never pulses (e.g. TE
        // wiring assumption wrong), present unsynchronized rather than hanging.
        let te_wait = embassy_time::with_timeout(
            embassy_time::Duration::from_millis(25),
            self.te.wait_for_rising_edge(),
        )
        .await;
        if te_wait.is_err() {
            log::warn!("TE pulse not seen; presenting without vsync");
        }
        self.command(RAMWR, &[]).await;
        self.dc.set_high();
        self.cs.set_low();
    }

    /// Second third: the frame's DMA transfer. Await the returned future
    /// (it borrows `frame` until then) before [`Display::present_end`].
    pub fn present_dma<'a>(&'a mut self, frame: &'a [u8]) -> dma::Transfer<'a> {
        self.sm.tx().dma_push(&mut self.dma, frame, false)
    }

    /// [`Display::present_dma`] from a raw pointer, so the caller keeps no
    /// borrow of the framebuffer during the transfer and may start writing
    /// the next frame into the part the DMA has already read.
    ///
    /// # Safety
    /// `frame` must point at [`crate::gfx::FB_BYTES`] readable bytes that
    /// stay valid until the returned transfer completes or is dropped. Bytes
    /// at or above the address reported by [`Display::dma_read_addr_reg`]
    /// must not be written until then; bytes below it may be.
    pub unsafe fn present_dma_raw(&mut self, frame: *const [u8]) -> dma::Transfer<'_> {
        // Same configuration `dma_push` makes: byte transfers into PIO1 SM0's
        // TX FIFO, paced by its DREQ.
        // SAFETY: the caller's contract above.
        unsafe {
            self.dma.write(
                frame,
                embassy_rp::pac::PIO1.txf(0).as_ptr() as *mut u8,
                embassy_rp::pac::dma::vals::TreqSel::PIO1_TX0,
                false,
            )
        }
    }

    /// The DMA channel's `READ_ADDR` register: during a transfer it holds
    /// the address of the next byte to fetch, so everything below it has
    /// been consumed. Readable from either core without the driver.
    pub fn dma_read_addr_reg(&self) -> *const u32 {
        self.dma.regs().read_addr().as_ptr().cast_const()
    }

    /// Last third: lets the final bytes clock out and deselects the panel.
    pub async fn present_end(&mut self) {
        self.drain().await;
        self.cs.set_high();
    }

    /// Sends one command byte plus optional parameter bytes.
    async fn command(&mut self, command: u8, data: &[u8]) {
        self.drain().await;
        self.dc.set_low();
        self.cs.set_low();
        self.push_byte(command).await;
        self.drain().await;
        if !data.is_empty() {
            self.dc.set_high();
            for &byte in data {
                self.push_byte(byte).await;
            }
            self.drain().await;
        }
        self.cs.set_high();
    }

    /// Pushes one byte through the state machine from the CPU.
    ///
    /// Shift direction is left, so the byte must sit in the top bits of the word.
    async fn push_byte(&mut self, byte: u8) {
        self.sm.tx().wait_push(u32::from(byte) << 24).await;
    }

    /// Waits until the FIFO is empty *and* the state machine has stalled, i.e.
    /// the final byte is fully clocked out. Without the stall check, DC or CS
    /// could change mid-byte and corrupt the last write of a burst.
    async fn drain(&mut self) {
        while !self.sm.tx().empty() {
            embassy_futures::yield_now().await;
        }
        // Bounded busy-wait: at most one byte time (~90 ns) after the FIFO
        // empties. Uses the raw FDEBUG TXSTALL bit, as embassy-rp does not
        // expose it.
        let fdebug = embassy_rp::pac::PIO1.fdebug();
        fdebug.write(|w| w.set_txstall(1 << 0));
        while fdebug.read().txstall() & (1 << 0) == 0 {}
    }
}

// Rust guideline compliant 2026-08-29
