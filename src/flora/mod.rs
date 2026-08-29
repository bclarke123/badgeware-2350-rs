//! Flora: a procedurally grown tree, rendered by the dual-core 3D engine.
//!
//! Each seed byte grows a different deterministic tree — recursive branches as
//! camera-facing ribbons, billboard leaves (pink blossoms on some seeds), wind
//! sway, staggered cubic-eased growth from a bare trunk to full bloom. Seeds
//! are random at boot and on every replant — an always-on gallery has no need
//! to remember which tree it was showing.
//!
//! Controls:
//! Left alone, the badge is a desk sculpture: every minute it plants a new
//! seed and grows a fresh tree (manual input postpones the cycle), while the
//! sky drifts through a ten-minute day: dusk, sunset, starry night, dawn.
//!
//! * **A** — plant a new random seed
//! * **B** — replay growth of the current tree
//! * **C** — pause/resume the auto-orbit (pick a favorite view, watch the sway)
//! * **UP / DOWN** — zoom

pub mod anim;
pub mod birds;
pub mod terrain;
pub mod tree;

use embassy_time::{Duration, Instant};
use embedded_graphics::pixelcolor::raw::{RawData, RawU16};
use embedded_graphics::pixelcolor::Rgb565;

use crate::bsp::buttons::{Button, ButtonEvent, EVENTS};
use crate::bsp::display::{Display, WIDTH};
use crate::gfx::FB_BYTES;
use crate::bsp::leds::{cue, LedCue};
use crate::gfx::FrameBuffer;
use crate::render3d::math::{fast_sin, v3, Camera};
use crate::render3d::texture::Textures;
use crate::render3d::{core1, ListBuilder, MeshTri, TriList};
use anim::{ease_in_cubic, lerp_color, Timeline};
use birds::Flock;
use terrain::Terrain;
use tree::Tree;

/// Runs the plant sim forever; owns the display, framebuffer, and the
/// statically allocated tree, terrain, and triangle lists.
pub async fn run(
    mut display: Display,
    mut frame: FrameBuffer,
    tri_list: &'static mut TriList,
    tri_list_b: &'static mut TriList,
    the_tree: &'static mut Tree,
    the_terrain: &'static mut Terrain,
    textures: &'static Textures,
) -> ! {
    let mut seed = embassy_rp::clocks::RoscRng.next_u32() & 0xff;
    log::info!("growing tree from seed {}", seed);
    the_tree.generate(seed);
    the_terrain.generate();
    let mut growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));

    let mut camera = Camera {
        yaw: 0.0,
        pitch: 0.25,
        dist: 3.2,
        target: v3(0.0, 0.1, 0.0),
    };
    // Fixed starfield (screen-space positions + twinkle phases), deterministic.
    let mut stars = [(0i32, 0i32, 0.0f32); STAR_COUNT];
    {
        let mut h: u32 = 0x1234_5677;
        for star in &mut stars {
            h ^= h << 13;
            h ^= h >> 17;
            h ^= h << 5;
            let sx = (h % 320) as i32;
            // Spread down toward the horizon (~y 175 at default pitch/zoom);
            // the tree and ground occlude any that land behind them.
            let sy = ((h >> 9) % 175) as i32;
            let phase = ((h >> 17) % 256) as f32 / 40.0;
            *star = (sx, sy, phase);
        }
    }

    let mut orbiting = true;
    let boot = Instant::now();
    let mut last_frame = Instant::now();
    let mut flock = Flock::new();
    // Desk-art mode: replant on a timer, postponed by any manual (re)growth.
    const REGROW_EVERY: Duration = Duration::from_secs(60);
    /// The old tree eases (in-cubic) into the ground over the first half of
    /// this window; the second half is a beat of empty ground before the new
    /// tree starts to grow.
    const DESPAWN: Duration = Duration::from_millis(600);
    let mut next_regrow = Instant::now() + REGROW_EVERY;
    // A pending replant: the seed to plant once the despawn shrink finishes.
    let mut despawn: Option<(Timeline, u32)> = None;

    // Raster split column: core 1 draws `0..split_x`, core 0 the rest.
    // Re-balanced every frame from the two cores' finish times (core 1
    // starts earlier but the tree's screen footprint moves with the orbit,
    // so no constant is right for long); see [`core1::RasterJob`].
    let mut split_x: usize = 176;
    const LOG_FRAMES: u32 = 120;
    let mut sum_geom = 0u64;
    let (mut sum_flat, mut sum_sprite, mut sum_clear) = (0u64, 0u64, 0u64);
    let (mut sum_wait, mut sum_dma, mut sum_join) = (0u64, 0u64, 0u64);
    let (mut sum_draw0, mut sum_c1wait, mut sum_c1draw) = (0u64, 0u64, 0u64);
    let mut clear_cache = crate::render3d::raster::ClearCache::new();
    let mut frames = 0u32;

    loop {
        // ---- Input ----
        while let Ok(event) = EVENTS.try_receive() {
            if let ButtonEvent::Pressed(button) = event {
                match button {
                    Button::A => {
                        let new_seed = embassy_rp::clocks::RoscRng.next_u32() & 0xff;
                        despawn = Some((Timeline::new(DESPAWN), new_seed));
                        cue(LedCue::Celebrate);
                    }
                    Button::B => {
                        // Replay: despawn and regrow the same seed.
                        despawn = Some((Timeline::new(DESPAWN), seed));
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
        if Instant::now() >= next_regrow && despawn.is_none() {
            let new_seed = embassy_rp::clocks::RoscRng.next_u32() & 0xff;
            despawn = Some((Timeline::new(DESPAWN), new_seed));
            log::info!("auto seed {}", new_seed);
        }

        // ---- Despawn: shrink the old tree, then plant the pending seed ----
        let mut tree_scale = 1.0;
        if let Some((timeline, pending)) = &despawn {
            if timeline.finished() {
                seed = *pending;
                the_tree.generate(seed);
                growth = Timeline::new(Duration::from_millis(tree::TOTAL_GROW_MS));
                next_regrow = Instant::now() + REGROW_EVERY;
                despawn = None;
            } else {
                // Shrink over the first half, hold at zero for the second.
                tree_scale = 1.0 - ease_in_cubic((timeline.progress() * 2.0).min(1.0));
            }
        }

        // ---- Present the previous frame: wait for vblank, then stream it to
        // the panel by DMA (~7 ms) while this frame's geometry runs on both
        // cores — geometry never touches the framebuffer, so it hides
        // entirely behind the transfer. ----
        let t0 = Instant::now();
        display.present_begin().await;
        let t_wait = t0.elapsed().as_micros();
        let fb_ptr = frame.as_ptr();
        let dma_read_addr = display.dma_read_addr_reg();
        // SAFETY: the framebuffer is a 'static allocation; core 1 only writes
        // columns the DMA has passed (it polls `READ_ADDR`), and core 0 only
        // writes after awaiting the transfer.
        let transfer = unsafe { display.present_dma_raw(core::ptr::slice_from_raw_parts(fb_ptr, FB_BYTES)) };

        // ---- Animate: slow auto-orbit (pausable) + wind clock ----
        // Sampled after the vsync wait so animation tracks the frame that
        // will actually show, not the wait's start.
        let now = Instant::now();
        let dt = (now - last_frame).as_micros() as f32 / 1_000_000.0;
        last_frame = now;
        if orbiting {
            camera.yaw += 0.15 * dt;
        }
        let time_s = boot.elapsed().as_micros() as f32 / 1_000_000.0;

        // ---- Sky cycle ----
        let phase = (time_s % DAY_CYCLE_S) / DAY_CYCLE_S;
        let (sky_top, sky_bottom, night) = sky_for(phase);
        flock.update(dt, time_s, night <= 0.0);

        // ---- Geometry (both cores: core 1 takes the odd half of the tree,
        // core 0 the even half plus all scenery, each into its own list) ----
        let t1 = Instant::now();
        let view = camera.view();
        let growth_elapsed = growth.elapsed();
        core1::dispatch(core1::Job::Geom(core1::GeomJob {
            tree: core::ptr::from_ref(the_tree),
            view,
            growth_elapsed,
            time_s,
            scale: tree_scale,
            out: core::ptr::from_mut(tri_list_b),
        }));
        let mut builder = ListBuilder::new_view_space(tri_list);
        push_stars(&mut builder, &stars, night, time_s, sky_top);
        flock.emit(time_s, &mut builder);
        the_terrain.emit(&view, 1.0 - 0.55 * night, &mut builder);
        the_tree.emit(&view, growth_elapsed, time_s, tree_scale, 0, &mut builder);
        builder.finish();
        join_or_panic("geom").await;
        let t_geom = t1.elapsed().as_micros();
        fn t2_dma_elapsed(since: Instant, minus_us: u64) -> u64 {
            since.elapsed().as_micros().saturating_sub(minus_us)
        }

        // ---- Rasterize: core 1 starts on the left part as soon as the DMA
        // has streamed it; core 0 takes the right part after the transfer. ----
        let t3 = Instant::now();
        let top = RawU16::from(sky_top).into_inner();
        let bottom = RawU16::from(sky_bottom).into_inner();
        let (left, right) = frame.split_at_column(split_x);
        core1::dispatch(core1::Job::Raster(core1::RasterJob {
            tris_a: core::ptr::from_ref(tri_list),
            tris_b: core::ptr::from_ref(tri_list_b),
            fb_part: left.as_mut_ptr(),
            x0: 0,
            x1: split_x as i32,
            dma_read_addr,
            wait_until: fb_ptr as u32 + left.len() as u32,
            clear_top: top,
            clear_bottom: bottom,
            textures: core::ptr::from_ref(textures),
        }));
        transfer.await;
        display.present_end().await;
        let t_dma = t2_dma_elapsed(t1, t_geom);
        let t_draw0 = Instant::now();
        let stats = crate::render3d::raster::draw_lists(
            tri_list,
            tri_list_b,
            right,
            split_x as i32,
            WIDTH as i32,
            top,
            bottom,
            textures,
            &mut clear_cache,
        );
        let t_draw0 = t_draw0.elapsed().as_micros();
        let t_join = Instant::now();
        join_or_panic("raster").await;
        let t_join = t_join.elapsed().as_micros();
        let _ = t3;
        let t_c1wait = u64::from(core1::RASTER_WAIT_US.load(portable_atomic::Ordering::Relaxed));
        let t_c1draw = u64::from(core1::RASTER_DRAW_US.load(portable_atomic::Ordering::Relaxed));

        // ---- Balance the split: nudge it toward whichever core finished
        // first (both finish times are relative to the raster dispatch). ----
        let finish1 = (t_c1wait + t_c1draw) as i64;
        let finish0 = (t_dma + t_draw0) as i64;
        if finish1 - finish0 > 250 {
            split_x = (split_x - 2).max(96);
        } else if finish0 - finish1 > 250 {
            split_x = (split_x + 2).min(256);
        }

        // ---- Stats ----
        sum_geom += t_geom;
        sum_draw0 += t_draw0;
        sum_c1wait += t_c1wait;
        sum_c1draw += t_c1draw;
        sum_flat += u64::from(stats.flat_us);
        sum_sprite += u64::from(stats.sprite_us);
        sum_clear += u64::from(stats.clear_us);
        sum_wait += t_wait;
        sum_dma += t_dma;
        sum_join += t_join;
        frames += 1;
        if frames == LOG_FRAMES {
            // vsync = idle wait for TE; dma = DMA time left after geometry;
            // c0 = core 0's draw of the right part; c1 = core 1's wait for
            // the DMA to pass the left part, then its draw; join = how long
            // core 0 waited for core 1 (0 => core 0 is the bottleneck: move
            // split_x right; large => move it left). The flat/sprite/clear
            // split is core 0's part only.
            log::info!(
                "frame: vsync {} geom {} dma {} c0 {} c1 {}+{} join {} split {} (c0: clear {} flat {} sprite {}) ({} prims)",
                sum_wait / u64::from(frames),
                sum_geom / u64::from(frames),
                sum_dma / u64::from(frames),
                sum_draw0 / u64::from(frames),
                sum_c1wait / u64::from(frames),
                sum_c1draw / u64::from(frames),
                sum_join / u64::from(frames),
                split_x,
                sum_clear / u64::from(frames),
                sum_flat / u64::from(frames),
                sum_sprite / u64::from(frames),
                tri_list.len + tri_list_b.len,
            );
            (sum_geom, sum_flat, sum_sprite, sum_clear, sum_wait, sum_dma, sum_join, frames) =
                (0, 0, 0, 0, 0, 0, 0, 0);
            (sum_draw0, sum_c1wait, sum_c1draw) = (0, 0, 0);
        }
    }
}

/// Waits for core 1's DONE; a silent core-1 wedge becomes a loud, reflashable
/// panic (BOOTSEL) with the failing stage in the log instead of a freeze.
async fn join_or_panic(stage: &str) {
    if embassy_time::with_timeout(Duration::from_millis(250), core1::join())
        .await
        .is_err()
    {
        log::error!("core1 join timeout during {}", stage);
        panic!("core1 wedged");
    }
}

/// One full sky day, in seconds (10 minutes).
const DAY_CYCLE_S: f32 = 600.0;

/// Number of stars in the night sky.
const STAR_COUNT: usize = 20;

/// Sky keyframes around the day cycle: (top, bottom) at phases 0, .25, .5, .75.
/// Dusk blue, sunset orange glow, deep night purple, pink dawn.
const SKY_KEYS: [(Rgb565, Rgb565); 4] = [
    (Rgb565::new(3, 10, 14), Rgb565::new(1, 5, 9)),   // dusk (the original)
    (Rgb565::new(5, 9, 15), Rgb565::new(22, 26, 3)),  // sunset: orange horizon
    (Rgb565::new(2, 2, 7), Rgb565::new(4, 3, 10)),    // night: dark purple
    (Rgb565::new(7, 11, 17), Rgb565::new(24, 22, 18)), // dawn: pink horizon
];

/// Sky gradient and star visibility (0..=1) for a cycle phase in 0..1.
fn sky_for(phase: f32) -> (Rgb565, Rgb565, f32) {
    let scaled = phase * 4.0;
    let idx = (scaled as usize) % 4;
    let next = (idx + 1) % 4;
    // Smoothstep the blend so the sky lingers on each mood.
    let f = scaled - scaled as i32 as f32;
    let f = f * f * (3.0 - 2.0 * f);
    let top = lerp_color(SKY_KEYS[idx].0, SKY_KEYS[next].0, f);
    let bottom = lerp_color(SKY_KEYS[idx].1, SKY_KEYS[next].1, f);
    // Stars ramp in around the night keyframe (phase 0.5).
    let d = (phase - 0.5).abs();
    let night = ((0.16 - d) / 0.06).clamp(0.0, 1.0);
    (top, bottom, night)
}

/// Emits the starfield as tiny far-plane triangles (fixed on screen, like a
/// skybox; drawn first by the painter's sort so the tree occludes them).
fn push_stars(
    out: &mut ListBuilder<'_>,
    stars: &[(i32, i32, f32); STAR_COUNT],
    night: f32,
    time_s: f32,
    sky_top: Rgb565,
) {
    if night <= 0.01 {
        return;
    }
    // Far plane: depth key 30 * 2048 stays inside u16 and behind everything.
    const Z: f32 = 30.0;
    const FOCAL: f32 = 228.0;
    let size = 1.6 * Z / FOCAL; // ~1.6 px on screen
    let white = Rgb565::new(26, 56, 31);
    for &(sx, sy, twinkle_phase) in stars {
        let twinkle = 0.55 + 0.45 * fast_sin(time_s * 1.9 + twinkle_phase);
        let color = lerp_color(sky_top, white, night * twinkle);
        let x = (sx as f32 - 160.0) * Z / FOCAL;
        let y = (120.0 - sy as f32) * Z / FOCAL;
        let p = v3(x, y, Z);
        out.push(MeshTri {
            v: [
                p.add(v3(0.0, size, 0.0)),
                p.add(v3(size, -size, 0.0)),
                p.add(v3(-size, -size, 0.0)),
            ],
            color,
        });
    }
}


// Rust guideline compliant 2026-08-21
