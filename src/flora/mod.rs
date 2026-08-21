//! Flora: a procedurally grown tree, rendered by the dual-core 3D engine.
//!
//! Each seed byte grows a different deterministic tree — recursive branches as
//! camera-facing ribbons, billboard leaves (pink blossoms on some seeds), wind
//! sway, staggered cubic-eased growth from a bare trunk to full bloom. The
//! seed lives in the RTC's battery-backed RAM byte, so the badge regrows *its*
//! tree after power-off.
//!
//! Controls:
//! Left alone, the badge is a desk sculpture: every minute it plants a new
//! seed and grows a fresh tree (manual input postpones the cycle).
//!
//! * **A** — new random seed (saved to the RTC), regrow
//! * **B** — replay growth of the current tree
//! * **C** — pause/resume the auto-orbit (pick a favorite view, watch the sway)
//! * **UP / DOWN** — zoom

pub mod anim;
pub mod tree;

use embassy_time::{Duration, Instant};
use embedded_graphics::pixelcolor::raw::{RawData, RawU16};
use embedded_graphics::pixelcolor::Rgb565;

use crate::bsp::buttons::{Button, ButtonEvent, EVENTS};
use crate::bsp::display::{Display, WIDTH};
use crate::bsp::leds::{cue, LedCue};
use crate::bsp::rtc::RtcRam;
use crate::gfx::FrameBuffer;
use crate::render3d::math::{v3, Camera, Mat34, Vec3};
use crate::render3d::{core1, ListBuilder, MeshTri, TriList};
use anim::Timeline;
use tree::Tree;

/// Runs the plant sim forever; owns the display, framebuffer, RTC, and the
/// statically allocated tree + triangle list.
pub async fn run(
    mut display: Display,
    mut frame: FrameBuffer,
    mut rtc: RtcRam,
    tri_list: &'static mut TriList,
    the_tree: &'static mut Tree,
) -> ! {
    // The badge's tree is whatever seed the RTC remembers (any byte is valid).
    let mut seed = u32::from(rtc.read().unwrap_or(42));
    log::info!("growing tree from seed {}", seed);
    the_tree.generate(seed);
    let mut growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));

    let mut camera = Camera {
        yaw: 0.0,
        pitch: 0.25,
        dist: 3.2,
        target: v3(0.0, 0.1, 0.0),
    };
    let mut orbiting = true;
    let boot = Instant::now();
    let mut last_frame = Instant::now();
    // Desk-art mode: replant on a timer, postponed by any manual (re)growth.
    const REGROW_EVERY: Duration = Duration::from_secs(60);
    let mut next_regrow = Instant::now() + REGROW_EVERY;

    const LOG_FRAMES: u32 = 120;
    let (mut sum_geom, mut sum_raster) = (0u64, 0u64);
    let mut frames = 0u32;

    loop {
        // ---- Input ----
        while let Ok(event) = EVENTS.try_receive() {
            if let ButtonEvent::Pressed(button) = event {
                match button {
                    Button::A => {
                        seed = embassy_rp::clocks::RoscRng.next_u32() & 0xff;
                        rtc.write(seed as u8);
                        the_tree.generate(seed);
                        growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));
                        next_regrow = Instant::now() + REGROW_EVERY;
                        cue(LedCue::Celebrate);
                        log::info!("new seed {}", seed);
                    }
                    Button::B => {
                        growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));
                        next_regrow = Instant::now() + REGROW_EVERY;
                        cue(LedCue::Blink);
                    }
                    Button::C => {
                        orbiting = !orbiting;
                        cue(LedCue::Blink);
                    }
                    Button::Up => camera.dist = (camera.dist - 0.25).max(1.6),
                    Button::Down => camera.dist = (camera.dist + 0.25).min(6.5),
                    Button::Home => {}
                }
            }
        }

        // ---- Desk-art auto-replant (silent: no LED flash on the timer) ----
        if Instant::now() >= next_regrow {
            seed = embassy_rp::clocks::RoscRng.next_u32() & 0xff;
            rtc.write(seed as u8);
            the_tree.generate(seed);
            growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));
            next_regrow = Instant::now() + REGROW_EVERY;
            log::info!("auto seed {}", seed);
        }

        // ---- Animate: slow auto-orbit (pausable) + wind clock ----
        let now = Instant::now();
        let dt = (now - last_frame).as_micros() as f32 / 1_000_000.0;
        last_frame = now;
        if orbiting {
            camera.yaw += 0.15 * dt;
        }
        let time_s = boot.elapsed().as_micros() as f32 / 1_000_000.0;

        // ---- Geometry (core 0) ----
        let t0 = Instant::now();
        let view = camera.view();
        let mut builder = ListBuilder::new_view_space(tri_list);
        push_ground(&view, &mut builder);
        the_tree.emit(&view, &growth, time_s, &mut builder);
        builder.finish();
        let t_geom = t0.elapsed().as_micros();

        // ---- Rasterize ----
        let t1 = Instant::now();
        let top = RawU16::from(SKY_TOP).into_inner();
        let bottom = RawU16::from(SKY_BOTTOM).into_inner();
        let (left, right) = frame.split_halves();
        core1::dispatch(core1::RasterJob {
            tris: core::ptr::from_ref(tri_list),
            fb_half: right.as_mut_ptr(),
            clear_top: top,
            clear_bottom: bottom,
        });
        crate::render3d::raster::draw_list(tri_list, left, 0, (WIDTH / 2) as i32, top, bottom);
        core1::join().await;
        let t_raster = t1.elapsed().as_micros();

        // ---- Present (vsync'd) ----
        display.present(frame.bytes()).await;

        // ---- Stats ----
        sum_geom += t_geom;
        sum_raster += t_raster;
        frames += 1;
        if frames == LOG_FRAMES {
            log::info!(
                "frame: geom {}us raster {}us ({} tris)",
                sum_geom / u64::from(frames),
                sum_raster / u64::from(frames),
                tri_list.len,
            );
            (sum_geom, sum_raster, frames) = (0, 0, 0);
        }
    }
}

/// Dusk sky gradient: lighter overhead, deepening toward the horizon.
const SKY_TOP: Rgb565 = Rgb565::new(3, 10, 14);
const SKY_BOTTOM: Rgb565 = Rgb565::new(1, 5, 9);

/// A simple ground plane under the tree (world space, wound +y outward).
fn push_ground(view: &Mat34, out: &mut ListBuilder<'_>) {
    const GROUND: Rgb565 = Rgb565::new(4, 13, 4); // pre-baked: old sun lighting lit up-facing ground at ~0.55
    let y = -0.95;
    let e = 2.2;
    let corners = [
        v3(-e, y, -e),
        v3(-e, y, e),
        v3(e, y, e),
        v3(e, y, -e),
    ];
    let p: [Vec3; 4] = corners.map(|c| view.transform_point(c));
    out.push(MeshTri { v: [p[0], p[1], p[2]], color: GROUND });
    out.push(MeshTri { v: [p[0], p[2], p[3]], color: GROUND });
}

// Rust guideline compliant 2026-08-21
