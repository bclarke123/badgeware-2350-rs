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
by day, over a perspective-textured meadow that fogs into the horizon.
Dense scenes run ~1,700–1,900 triangles at a vsync-locked 60 fps on
the stock 150 MHz clock (re-measure after the sprite/AA renderer: see the
`frame:` lines on the serial log: `vsync` is idle wait, `dma` the transfer
time left after geometry, and the `core0:` split is the left half's raster).

**Controls**: A = plant a new random seed, B = replay the current tree's
growth, C = pause/resume the orbit, UP/DOWN = zoom, HOME-hold ~2 s = reboot to
BOOTSEL, RESET-hold ~1.5 s = sleep (any front button wakes). Frame timings and
seeds are logged over USB serial.

## 3D renderer (`src/render3d`)

Painter's algorithm, near-plane clipping, colors baked at generation time.
Two primitive kinds: flat-shaded triangles with **anti-aliased silhouette
edges**, and **textured billboard sprites** (every leaf and blossom) sampled
by the RP2350's SIO interpolators. Every frame runs **two fork-joins across
both cores**, with the panel DMA hidden behind the first:

1. **Present + geometry**: core 0 waits for vblank (TE) and starts the ~7 ms
   DMA of the *previous* frame (22 Mbyte/s parallel bus). While it streams,
   core 1 emits the odd half of the tree into its own primitive list and
   core 0 does the even half plus scenery (terrain, stars, birds) into a
   second list; each list is radix-sorted by depth (packed `depth<<16|index`
   keys — sorting 4-byte keys, not 32-byte entries, is what keeps the sort
   out of the budget). Geometry never touches the framebuffer, so it costs
   nothing on the critical path.
2. **Raster**: each core clears and draws one part of the framebuffer,
   merge-walking the two sorted lists (one compare per primitive — no copy,
   no re-sort). The column-major framebuffer makes the parts contiguous
   disjoint slices. The DMA streams columns left to right, so core 1 takes
   the *left* part and starts the moment the DMA's `READ_ADDR` has passed it
   (~4 ms before the transfer ends), while core 0 takes the right part after
   the transfer completes. The split column is re-balanced every frame from
   the two cores' finish times (screen columns are not work: the tree is
   centred, and its footprint moves with the orbit), and each core skips
   primitives whose x extent misses its part before any setup.

The DMA is ~8 ms of the 16.7 ms frame (150 KB at 37.5 MHz / 2 cycles per
byte), so this staggering is what keeps a dense bush at 60 fps. The
rasterizer is linked into RAM: both cores execute it concurrently and the
XIP flash cache is shared.

Core 1 runs a bare loop (no executor, no clock access) fed jobs through an
`embassy-sync` Signal handshake; a HardFault handler and a join watchdog turn
any wedge into a reflashable BOOTSEL reboot instead of a freeze. The
rasterizer walks triangles column-by-column (f32 setup, 16.16 fixed per-column
increments) so the hot loop is a sequential byte fill. Trig and square roots
are fast f32 approximations — `libm`'s f64-internal `sinf` costs microseconds
per call on the M33's single-precision FPU (measured as a 20x geometry
slowdown before replacement).

**Edge anti-aliasing** costs almost nothing because the coverage is already
there: the fractional bits of each column run's 16.16 end positions are the
exact vertical coverage of its first and last pixel, so those two pixels are
alpha-blended (5-bit alpha, one-multiply RGB565 lerp) over whatever the
painter's order already put there. Only silhouette edges blend — each
triangle carries a 3-bit mask, and an edge shared with a neighbour (a
ribbon's diagonal, every terrain grid line) uses the exact pixel-center
ownership rule instead, so seams stay seamless rather than leaking a
hairline of sky. Coverage is resolved along y only: shallow edges (the ones
that shimmer under wind sway) go smooth, steep edges keep their stairs.

**Ground.** The meadow's 98 triangles are perspective-correct textured:
`u/z`, `v/z`, `1/z` are affine in screen space, so each column is walked in
8-pixel chunks with one reciprocal per chunk and the interpolator stepping
u/v inside it. Texture coordinates are world x/z, so the grass is nailed to
the ground as the camera orbits. The tile is a boot-time procedural shade
map (3-octave tiling value noise quantized to 8 levels plus blade flecks,
64 texels per unit, box-averaged mips picked per triangle by depth), tinted
per triangle from the cell mottle and fogged toward the sky's horizon colour
with distance.

**Sprites and the SIO interpolator.** Leaves, tufts and blossoms are one
screen-aligned textured quad each (2 triangles; blossoms used to be 4 flat
ones). Textures are *shade maps*, not images — 0 transparent, 1 half-covered
edge, 2–4 dark/mid/light — baked at boot by supersampling analytic shapes
into 16×16 maps plus 8×8 mips (`src/render3d/texture.rs`); the rasterizer
turns levels into RGB565 from each triangle's own base tint, so one map
serves every green and every pink. Affine u/v stepping per pixel is
"add, shift, mask, combine into an address", which is exactly what the
per-core `INTERP0` does in hardware: lane 0 accumulates u, lane 1 v, and one
`POP_FULL` read returns the texel address while auto-advancing both. The
lane masks also wrap out-of-range coordinates into the map's transparent
border, so quad-edge rounding slop can never show a stray texel.

## Badger 2350 (e-paper)

The same library targets the Badger 2350 (`--no-default-features --features
badger`): RP2350A, 2.7" 264×176 SSD1680 four-grey e-paper on SPI0
(`src/bsp/epd.rs`, Pimoroni's waveform LUT ported verbatim; full refresh
measured at 1.14 s TURBO to 3.67 s SLOW, four clean greys, no visible
ghosting). Buttons, LEDs, RTC and the POWMAN sleep path are pin-identical to
the Tufty.

Greyscale rendering goes through `gfx::grey` (an 8-bit `embedded-graphics`
canvas, with a 2× supersampling resolve) and `gfx::dither` (linear-light
quantization against the panel's calibrated reflectances — its level 1 is
nearly black, ~10%, and level 2 ~51% — with Bayer 4×4/8×8, Floyd–Steinberg
and nearest, selectable per rectangle). `examples/epd_test.rs` is the test
card used to judge all of this on the panel: flat levels and ramps, the three
dithers, 1:1 vs supersampled text, and a shaded sphere; A/B refresh at
TURBO/SLOW and C/UP/DOWN adjust the calibration live. The tree garden also
runs on it (`--example tree`), though it was designed for a backlit screen.

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

The crate is a library plus binaries in `examples/`; the board is a Cargo
feature (`tufty`, default, or `badger`):

```sh
cargo run --release --example tree                                        # Tufty 2350
cargo run --release --example tree --no-default-features --features badger # Badger 2350
```

1. **First flash:** hold the BOOT button while plugging in USB — the badge
   mounts as `RP2350` bootloader — then run the command above. (On a Badger
   still running BadgeOS, `import machine; machine.bootloader()` at its
   MicroPython REPL does the same; our firmware replaces BadgeOS, which is
   restored by flashing Pimoroni's release `.uf2`.)

2. **Every flash after that:** hold **HOME for 2 seconds** on the running
   firmware to reboot into BOOTSEL, then run the command again.
   Panics and hard faults also drop the badge back into BOOTSEL, so it can
   always be reflashed.

Logs: `screen /dev/cu.usbmodem*` (or `tio`); plain-text `log` output over USB CDC.

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
