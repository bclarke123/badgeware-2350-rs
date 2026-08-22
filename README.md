# tufty-2350

Rust/[Embassy](https://embassy.dev) firmware for the [Pimoroni Tufty 2350](https://github.com/pimoroni/tufty2350)
badge: a procedurally grown 3D tree garden on a dual-core software renderer
and a custom PIO+DMA display driver. No MicroPython, no interpreter, no heap —
a single static binary straight on the metal. (A Simon-style memory game lived
here previously — see git history.)

Left alone it is a desk sculpture: every minute the current tree eases back
into the ground and a new random seed grows in its place — three species
(pink-blossom sakura, classic green, dense low bush), each deterministic per
seed, with recursive ribbon branches, billboard leaves, wind sway, and
staggered cubic-eased growth from bare trunk to full bloom. Around it: a
gently bumpy meadow, a ten-minute day cycle (dusk → orange sunset → starry
purple night → pink dawn, Bayer-dithered gradients), twinkling stars that only
come out after dark, and distant flapping bird silhouettes that cross the sky
by day. Dense scenes run ~1,700–1,900 triangles at a vsync-locked 60 fps on
the stock 150 MHz clock.

**Controls**: A = plant a new random seed, B = replay the current tree's
growth, C = pause/resume the orbit, UP/DOWN = zoom, HOME-hold ~2 s = reboot to
BOOTSEL, RESET-hold ~1.5 s = sleep (any front button wakes). Frame timings and
seeds are logged over USB serial.

## 3D renderer (`src/render3d`)

Flat-shaded triangles, painter's algorithm, near-plane clipping, colors baked
at generation time. Every frame runs **two fork-joins across both cores**:

1. **Geometry**: core 1 emits the odd half of the tree into its own triangle
   list and depth-sorts it, while core 0 does the even half plus scenery
   (terrain, stars, birds) into a second list.
2. **Raster**: each core clears and draws one half of the framebuffer,
   merge-walking the two sorted lists (one compare per triangle — no copy, no
   re-sort). The column-major framebuffer makes the halves contiguous disjoint
   slices, split at x=160.

Core 1 runs a bare loop (no executor, no clock access) fed jobs through an
`embassy-sync` Signal handshake; a HardFault handler and a join watchdog turn
any wedge into a reflashable BOOTSEL reboot instead of a freeze. The
rasterizer walks triangles column-by-column (f32 setup, 16.16 fixed per-column
increments) so the hot loop is a sequential byte fill. Trig and square roots
are fast f32 approximations — `libm`'s f64-internal `sinf` costs microseconds
per call on the M33's single-precision FPU (measured as a 20x geometry
slowdown before replacement).

## Hardware covered

| Subsystem | Support |
|---|---|
| RP2350B @ 150 MHz, both cores + FPUs | `embassy-rp` (`rp235xb`) |
| 2.8" 320×240 ST7789, 8-bit 8080 parallel bus | custom PIO+DMA driver (`src/bsp/display.rs`) |
| 5 front buttons + HOME | debounced event channel (`src/bsp/buttons.rs`) |
| Backlight (PWM) + phototransistor | auto-brightness (`src/bsp/backlight.rs`) |
| 4-zone rear LEDs | cue patterns (`src/bsp/leds.rs`) |
| USB serial logging | `embassy-usb-logger` (`src/bsp/usb.rs`) |
| Dual-core rendering | core 1 geometry + raster coprocessor (`src/render3d/core1.rs`) |
| Sleep / power-off | hold RESET ~1.5 s → POWMAN off, any front button wakes (`src/bsp/power.rs`) |
| PCF85063A RTC | dormant driver (`src/bsp/rtc.rs`; held the Simon high score in its one battery-backed RAM byte, now awaiting timekeeping/alarm duty) |
| WiFi/BT (RM2), battery gauge, PSRAM | not yet (see roadmap) |

## Building & flashing (no debug probe needed)

Prerequisites: Rust stable with the `thumbv8m.main-none-eabihf` target
(`rust-toolchain.toml` installs it) and `picotool` (`brew install picotool`).

1. **First flash:** hold the BOOT button while plugging in USB — the badge
   mounts as `RP2350` bootloader — then:

   ```sh
   cargo run --release
   ```

2. **Every flash after that:** hold **HOME for 2 seconds** on the running
   firmware to reboot into BOOTSEL, then `cargo run --release` again.
   Panics and hard faults also drop the badge back into BOOTSEL, so it can
   always be reflashed.

Logs: `screen /dev/tty.usbmodem*` (or `tio`); plain-text `log` output over USB CDC.

## Display driver notes

The ST7789 is wired over an 8-bit parallel bus (DB0–7 = GPIO32–39, WR = 30,
DC = 28, CS = 27, RD = 31 held high, backlight = 26, TE/vsync = 21). A
two-instruction PIO program (`out pins,8 side 0` / `nop side 1`) strobes WR
while DMA feeds bytes; a full 150 KiB frame presents in ~8 ms. Init sequence
and bus timing follow Pimoroni's official C driver (`modules/c/st7789` in the
tufty2350 repo).

Tear-free presents: the framebuffer is stored in panel scan order (the panel
refreshes as 240×320 portrait; `src/gfx` pays the transpose in per-pixel index
math, keeping fills contiguous), and `present` waits for the TE vblank pulse
before streaming. The ~8 ms write starts ahead of the ~17 ms refresh beam and
outruns it, so the beam never crosses the write.

## Roadmap

- 250 MHz overclock (Pimoroni ships the PLL + voltage config for this board)
  and falling leaves/petals to spend the new budget.
- WiFi/BT via `cyw43` (RM2 module: WL_ON=23, DATA=24, CS=25, CLK=29).
- RTC timekeeping + alarm sleep/wake (`src/bsp/rtc.rs` is wired and waiting).
- Battery gauge (VBAT_SENSE=40, VBUS_DETECT=12).
