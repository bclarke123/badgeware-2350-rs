//! Core 1 as a rendering coprocessor.
//!
//! Core 1 runs a bare loop (no executor, no embassy-time — the time driver
//! lives on core 0) that waits for a [`Job`]: either emit half of the tree's
//! geometry into its own triangle list ([`GeomJob`], the frame's first
//! fork-join) or rasterize the right half of the framebuffer ([`RasterJob`],
//! the second), reporting done after each. The handshake uses `embassy-sync`
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
use portable_atomic::{AtomicU32, Ordering};
use static_cell::StaticCell;

use super::raster::ClearCache;
use super::texture::Textures;
use super::{raster, ListBuilder, TriList};
use crate::bsp::display::HEIGHT;
// Layering note: core1 knows about the tree so the geometry fork-join stays a
// plain function call on this side; acceptable coupling in a single-app binary.
use crate::flora::tree::Tree;
use embassy_time::Duration;
use crate::render3d::math::Mat34;


/// A frame work order for core 1.
#[derive(Clone, Copy)]
pub enum Job {
    Geom(GeomJob),
    Raster(RasterJob),
}

/// Geometry fork: emit the odd half of the tree into core 1's own list.
#[derive(Clone, Copy)]
pub struct GeomJob {
    /// The generated tree ('static; core 0 does not mutate during the window).
    pub tree: *const Tree,
    pub view: Mat34,
    /// Growth elapsed, sampled once on core 0 (core 1 never reads the clock).
    pub growth_elapsed: Duration,
    pub time_s: f32,
    pub scale: f32,
    /// Core 1's own triangle list ('static, exclusively core 1's until DONE).
    pub out: *mut TriList,
}

/// Raster fork: clear + draw both sorted lists into columns `x0..x1`.
///
/// Core 1 takes the *left* part of the frame because the presentation DMA
/// streams columns left to right: it spins on the DMA's `READ_ADDR` until
/// the transfer has passed its part, then starts — a few ms before core 0,
/// which must wait for the whole transfer before it can touch the right
/// part. The split column is chosen to balance that head start.
#[derive(Clone, Copy)]
pub struct RasterJob {
    /// The frame's two sorted triangle lists ('static, frozen until `DONE`).
    pub tris_a: *const TriList,
    pub tris_b: *const TriList,
    /// Start of core 1's framebuffer part (columns `x0..x1`, contiguous),
    /// exclusively core 1's between the DMA passing it and `DONE`.
    pub fb_part: *mut u8,
    pub x0: i32,
    pub x1: i32,
    /// The DMA channel's `READ_ADDR` register and the address it must reach
    /// before `fb_part` may be written.
    pub dma_read_addr: *const u32,
    pub wait_until: u32,
    /// Sky gradient for the clear pass (top and bottom RGB565).
    pub clear_top: u16,
    pub clear_bottom: u16,
    /// The boot-generated shade maps ('static, immutable after generation).
    pub textures: *const Textures,
}

// SAFETY: the raw pointers refer to 'static allocations whose exclusive use
// during the job window is guaranteed by the fork-join protocol documented in
// the module docs; sending them to core 1 is the entire point of this type.
unsafe impl Send for Job {}

/// Core 0 -> core 1: a frame's rasterization job.
pub static JOB: Signal<CriticalSectionRawMutex, Job> = Signal::new();
/// Core 1's raster timing for the last job, in microseconds from receiving
/// it: how long it waited for the DMA, then how long it drew. Written by
/// core 1 before `DONE`, read by core 0 after joining.
pub static RASTER_WAIT_US: AtomicU32 = AtomicU32::new(0);
pub static RASTER_DRAW_US: AtomicU32 = AtomicU32::new(0);
/// Core 1 -> core 0: the job is complete.
pub static DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// 16 KiB: raster + geometry (sort recursion) plus wide margin for IRQ frames.
static CORE1_STACK: StaticCell<Stack<16384>> = StaticCell::new();

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
pub fn dispatch(job: Job) {
    JOB.signal(job);
    cortex_m::asm::sev();
}

/// Blocks (async on core 0's executor) until core 1 finishes the frame.
pub async fn join() {
    DONE.wait().await;
}

fn worker() -> ! {
    let mut clear = ClearCache::new();
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

        match job {
            Job::Geom(g) => {
                // SAFETY: see the module-level soundness note — exclusive
                // during the job window, 'static allocations.
                let tree = unsafe { &*g.tree };
                // SAFETY: as above; `out` is core 1's own list until DONE.
                let out = unsafe { &mut *g.out };
                let mut builder = ListBuilder::new_view_space(out);
                tree.emit(&g.view, g.growth_elapsed, g.time_s, g.scale, 1, &mut builder);
                builder.finish();
            }
            Job::Raster(r) => {
                // SAFETY: see the module-level soundness note — exclusive
                // during the job window, 'static allocations.
                let tris_a = unsafe { &*r.tris_a };
                // SAFETY: as above.
                let tris_b = unsafe { &*r.tris_b };
                let t0 = embassy_rp::pac::TIMER0.timerawl().read();
                // Wait for the presentation DMA to move past our columns.
                // SAFETY: a live DMA register; reading it has no side effect.
                while unsafe { core::ptr::read_volatile(r.dma_read_addr) } < r.wait_until {}
                let t1 = embassy_rp::pac::TIMER0.timerawl().read();
                let len = (r.x1 - r.x0) as usize * HEIGHT * 2;
                // SAFETY: as above; fb_part is core 1's columns from
                // split_at_mut, exactly `len` bytes, now past the DMA.
                let part = unsafe { core::slice::from_raw_parts_mut(r.fb_part, len) };
                // SAFETY: as above; generated once at boot, read-only since.
                let textures = unsafe { &*r.textures };
                raster::draw_lists(
                    tris_a,
                    tris_b,
                    part,
                    r.x0,
                    r.x1,
                    r.clear_top,
                    r.clear_bottom,
                    textures,
                    &mut clear,
                );
                let t2 = embassy_rp::pac::TIMER0.timerawl().read();
                RASTER_WAIT_US.store(t1.wrapping_sub(t0), Ordering::Relaxed);
                RASTER_DRAW_US.store(t2.wrapping_sub(t1), Ordering::Relaxed);
            }
        }

        DONE.signal(());
        // Wake core 0's executor promptly (its thread-mode pender WFEs too).
        cortex_m::asm::sev();
    }
}

// Rust guideline compliant 2026-08-29
