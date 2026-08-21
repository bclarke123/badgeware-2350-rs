//! Core 1 as a rasterization coprocessor.
//!
//! Core 1 runs a bare loop (no executor, no embassy-time — the time driver
//! lives on core 0) that waits for a [`RasterJob`], rasterizes the right half
//! of the framebuffer, and reports done. The handshake uses `embassy-sync`
//! `Signal`s, which are cross-core sound here because the crate's
//! `critical-section-impl` takes hardware spinlock 31. The SIO FIFO is *not*
//! used: embassy's core-1 startup installs a FIFO interrupt handler for its
//! pause/resume protocol, which would eat application FIFO words.
//!
//! # Soundness (the only unsafe crossing in the renderer)
//!
//! [`RasterJob`] smuggles raw pointers because the borrow checker cannot see
//! across cores. The invariant that makes it sound is the strict fork-join
//! frame structure in [`crate::flora`]:
//! * `tris` points at the `'static` triangle list; core 0 finishes writing and
//!   sorting it *before* signaling, and does not touch it again until `DONE`.
//! * `fb_half` + [`HALF_BYTES`] is the right half from `split_at_mut` — core 0
//!   provably holds only the disjoint left half during the job window.
//! * `Signal`'s critical section provides acquire/release ordering across
//!   cores, and SRAM is uncached on the RP2350.

use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::peripherals::CORE1;
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use static_cell::StaticCell;

use super::{raster, TriList};
use crate::bsp::display::{HEIGHT, WIDTH};

/// Bytes in one framebuffer half (the split is at column WIDTH/2).
pub const HALF_BYTES: usize = (WIDTH / 2) * HEIGHT * 2;

/// One frame's work order for core 1.
#[derive(Clone, Copy)]
pub struct RasterJob {
    /// The frame's sorted triangle list ('static, frozen until `DONE`).
    pub tris: *const TriList,
    /// Start of the right framebuffer half, [`HALF_BYTES`] long, exclusively
    /// core 1's between `JOB` and `DONE`.
    pub fb_half: *mut u8,
    /// Sky gradient for the clear pass (top and bottom RGB565).
    pub clear_top: u16,
    pub clear_bottom: u16,
}

// SAFETY: the raw pointers refer to 'static allocations whose exclusive use
// during the job window is guaranteed by the fork-join protocol documented in
// the module docs; sending them to core 1 is the entire point of this type.
unsafe impl Send for RasterJob {}

/// Core 0 -> core 1: a frame's rasterization job.
pub static JOB: Signal<CriticalSectionRawMutex, RasterJob> = Signal::new();
/// Core 1 -> core 0: the job is complete.
pub static DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// 8 KiB is generous: the rasterizer uses a few dozen stack bytes.
static CORE1_STACK: StaticCell<Stack<8192>> = StaticCell::new();

/// Starts core 1 and parks it waiting for jobs. Call once at boot.
pub fn spawn(core1: Peri<'static, CORE1>) {
    let stack = CORE1_STACK.init(Stack::new());
    spawn_core1(core1, stack, || worker());
}

/// Submits a job to core 1.
///
/// The explicit SEV matters: `Signal::signal` releases a spinlock without an
/// event, so a core 1 parked in WFE would otherwise sleep until some unrelated
/// event arrived.
pub fn dispatch(job: RasterJob) {
    JOB.signal(job);
    cortex_m::asm::sev();
}

/// Blocks (async on core 0's executor) until core 1 finishes the frame.
pub async fn join() {
    DONE.wait().await;
}

fn worker() -> ! {
    loop {
        // No executor on this core: poll with WFE naps. Spurious wakeups
        // (any interrupt or event) just re-poll; the event register is sticky
        // so a SEV arriving between the check and the WFE is not lost.
        let job = loop {
            if let Some(job) = JOB.try_take() {
                break job;
            }
            cortex_m::asm::wfe();
        };

        // SAFETY: see the module-level soundness note — exclusive during the
        // job window, 'static allocations, correct length by construction.
        let tris = unsafe { &*job.tris };
        // SAFETY: as above; fb_half is the right half from split_at_mut with
        // exactly HALF_BYTES bytes, untouched by core 0 until DONE.
        let half = unsafe { core::slice::from_raw_parts_mut(job.fb_half, HALF_BYTES) };

        raster::draw_list(
            tris,
            half,
            (WIDTH / 2) as i32,
            WIDTH as i32,
            job.clear_top,
            job.clear_bottom,
        );

        DONE.signal(());
        // Wake core 0's executor promptly (its thread-mode pender WFEs too).
        cortex_m::asm::sev();
    }
}

// Rust guideline compliant 2026-08-21
