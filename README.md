# badgeware-2350-rs

Rust/[Embassy](https://embassy.dev) firmware for [Pimoroni's Badgeware](https://shop.pimoroni.com/collections/badgeware)
family of badges (and their RP2040 ancestor) — no MicroPython, no
interpreter, no heap; single static binaries straight on the metal.

| Board | Screen | Status |
|---|---|---|
| [Tufty 2350](https://github.com/pimoroni/tufty2350) | 2.8" 320×240 ST7789 LCD, 8-bit parallel | supported (`--features tufty`, default) |
| [Badger 2350](https://github.com/pimoroni/badger2350) | 2.7" 264×176 SSD1680 four-grey e-paper | supported (`--features badger`) |
| [Badger 2040 W](https://shop.pimoroni.com/products/badger-2040-w) | 2.9" 296×128 UC8151 mono e-paper | supported (`--features badger2040w`, RP2040) |
| Blinky 2350 | 39×26 white LED matrix | in the post |

One library crate (`bsp` drivers, `gfx` greyscale pipeline, `render3d`
software renderer, `flora` scene, `boot` plumbing); every app is a binary in
`examples/`. The RP2350 boards share their buttons, rear LEDs, RTC, RM2
radio and power path, so most examples are a feature flag away from each
other's hardware — and the first-generation Badger 2040 W (a whole different
chip: RP2040, Cortex-M0+, no FPU) rides the same crate through the same
seams, since everything network- and e-paper-shaped needs no floating point
worth mentioning.

## Examples

| Example | What it does |
|---|---|
| `simon` | The repo's original app: a Simon memory game on the five buttons — animated pads aligned with the physical controls, LED echoes, high score in the RTC's battery-backed byte. |
| `tree` | Procedurally grown 3D tree garden: dual-core renderer, anti-aliased edges, SIO-interpolator textured sprites, perspective grass, day cycle. 60 fps on the Tufty. |
| `epd_test` | E-paper test card: the four panel greys, dither comparisons (Bayer 4×4/8×8, Floyd–Steinberg), supersampled text, live calibration on the buttons. |
| `mono_test` | Its 1-bit cousin: a grey ramp under selectable dithers, hairline/checker/disc swatches, all four UC8151 waveform speeds with measured times over serial, invert-to-check-ghosting. |
| `badge` | Name badge driven by `examples/badge.vcf`: big name, social handles parsed from URLs (`GH: you`, `LI: you`), and a scannable vCard QR. |
| `totp` | Six-digit authenticator: battery-backed RTC, secret in flash, provisioning over USB serial, seven-segment digits with a draining 30 s bar. |
| `weather` | Ambient weather station: joins WiFi, geolocates by IP, fetches Open-Meteo, syncs the RTC from NTP, and refreshes every 15 min with condition icons and a battery gauge. Wake-render-sleep on battery: weeks per charge. |
| `chess` | Play White against Stockfish at chess-api.com over TLS 1.3. Title screen with three difficulties, `cozy-chess` legal moves on five buttons, eval + captured pieces in the panel, game persisted across power-off, battery gauge. |
| `marquee` | Concert beacon: hold A and the badge becomes a hotspot with a join-QR; your phone's captive-portal sheet pops a form, and the message renders as big as it fits. No app, no shared network, no typed URLs. |
| `reader` | E-reader: streams a Project Gutenberg book over TLS into the 8 MB PSRAM (progress bar included), then pages it on the e-paper — word wrap, typography cleanup, persistent bookmark, page-up/-down from any point. |

Which example runs where (✓ = works today, ? = should port cleanly — hover
for the caveat, blank = not a fit for the hardware):

| Example | Tufty 2350 | Badger 2350 | Badger 2040 W |
|---|:---:|:---:|:---:|
| `simon` | ✓ | | |
| `tree` | ✓ | ✓ | |
| `epd_test` | | ✓ | |
| `mono_test` | | | ✓ |
| `badge` | | ✓ | <abbr title="Needs the 1-bit present path swapped in; layout fits.">?</abbr> |
| `totp` | | ✓ | <abbr title="Straightforward port: same RTC, same flash store; needs the 1-bit present path.">?</abbr> |
| `weather` | <abbr title="All the plumbing exists (radio, RTC, battery); needs an LCD present path and a reason to burn a backlight on ambient info.">?</abbr> | ✓ | ✓ |
| `chess` | ✓ | ✓ | |
| `marquee` | | ✓ | <abbr title="Portable, but the Blinky's LED matrix is this app's real destination.">?</abbr> |
| `reader` | <abbr title="The Tufty has the same 8 MB PSRAM wired to QMI CS1, but it is untested there.">?</abbr> | ✓ | |

## Building & flashing (no debug probe needed)

Prerequisites: Rust stable with the `thumbv8m.main-none-eabihf` target
(`rust-toolchain.toml` installs it) and `picotool` (`brew install picotool`).

```sh
cargo run --release --example tree                                            # Tufty
cargo run --release --example weather --no-default-features --features badger # Badger 2350
cargo run --release --example mono_test --no-default-features \
    --features badger2040w --target thumbv6m-none-eabi                        # Badger 2040 W
```

(The RP2040 board needs the explicit `--target` — Cargo features can't
switch architectures.)

1. **First flash:** hold the BOOT button while plugging in USB — the badge
   mounts as the `RP2350` bootloader — then run the command above. (On a
   Badger still running BadgeOS, `import machine; machine.bootloader()` at
   its MicroPython REPL does the same; this firmware replaces BadgeOS, which
   is restored by flashing Pimoroni's release `.uf2`.)
2. **Every flash after that:** hold **HOME for 2 seconds** on the running
   firmware (**UP+DOWN together** on the Badger 2040 W, which has no HOME)
   to reboot into BOOTSEL, then run the command again. Panics and
   hard faults also end in BOOTSEL (after a ~5 s pause), so a badge is never
   bricked — if one seems dead, it may just be waiting in the bootloader.

Logs: `screen /dev/cu.usbmodem*` (or `tio`); plain-text `log` output over
USB CDC. Some examples also *read* that port for provisioning (`totp`,
`weather`). With a debug probe on the SWD header, the same log also streams
over RTT — `probe-rs attach --chip RP235x <elf>` — with no USB cable at
all, and `probe-rs download --chip RP235x <elf>` flashes without touching
BOOTSEL.

### Dependency gotcha

`embedded-tls` (behind `reqwless`, for the chess example) pins RustCrypto's
`der`/`der_derive` at pre-release versions that cargo's resolver will happily
"upgrade" into a broken build. The working versions are captured in
`Cargo.lock`; after any `cargo update`, restore them with:

```sh
cargo update -p der --precise 0.8.0-rc.10
cargo update -p der_derive --precise 0.8.0-rc.6
```

## Framework tour

- **`bsp`** — per-subsystem drivers: `display` (Tufty PIO+DMA LCD), `epd`
  (Badger SSD1680 incl. partial-window refresh), `buttons` (debounced
  events), `leds` (cue patterns), `backlight` (Tufty auto-brightness),
  `rtc` (PCF85063A timekeeping + lost-power flag), `battery` (calibrated
  voltage, LiPo percent, USB detect), `settings` (key-value store in the
  last flash sector — shared by all apps, survives reflashing), `usb`
  (logging + line input), `power` (POWMAN off, button/RTC wake), and
  `wifi` — station mode with bounded join/DHCP retries and an idle
  leave/rejoin lifecycle, plus access-point hosting for captive-portal
  provisioning (see `marquee`: one-lease DHCP server, wildcard DNS, and the
  HTTP form the phone auto-opens as its "sign in" page).
- **`gfx`** — `FrameBuffer` (RGB565, column-major, DMA-ready), `grey` (8-bit
  canvas with `embedded-graphics` support and 2× supersampling resolve),
  `dither` (linear-light quantization to the e-paper's *measured* grey
  levels; Bayer 4×4/8×8, Floyd–Steinberg, nearest — selectable per
  rectangle), `widgets` (battery gauge).
- **`render3d`** — the dual-core software renderer (below).
- **`boot`** — RP2350 image definition, picotool metadata, and the
  panic/HardFault→BOOTSEL strategy, linked into every binary.

Fonts come from [`u8g2-fonts`](https://crates.io/crates/u8g2-fonts) (crox,
logisoso, seven-segment, chess glyphs, Open Iconic weather icons); QR codes
from `qrcodegen-no-heap`. Radio firmware blobs are vendored in `firmware/`
under their permissive binary license.

## The Tufty renderer (`src/render3d`)

A dual-core painter's-algorithm renderer: flat-shaded triangles with
**anti-aliased silhouette edges**, plus **textured billboard sprites**
sampled by the RP2350's SIO interpolators. Every frame runs two fork-joins,
with the panel DMA hidden behind the first:

1. **Present + geometry**: core 0 waits for vblank (TE) and starts the ~8 ms
   DMA of the *previous* frame. While it streams, both cores emit and
   radix-sort their halves of the scene (packed `depth<<16|index` keys —
   sorting 4-byte keys, not 32-byte entries, keeps the sort out of the
   budget). Geometry never touches the framebuffer, so it is free on the
   critical path.
2. **Raster**: the DMA streams columns left to right, so core 1 takes the
   *left* part of the frame and starts the moment the DMA's `READ_ADDR` has
   passed it (~4 ms before the transfer ends); core 0 takes the right part
   after the transfer completes. The split column re-balances every frame
   from the cores' finish times, and each core pre-rejects primitives whose
   x-extent misses its part.

Details that matter:

- **Edge AA for almost nothing**: the fractional bits of each column run's
  16.16 end positions *are* the pixel coverage; those two pixels alpha-blend
  (5-bit alpha, one-multiply RGB565 lerp). A 3-bit per-triangle mask keeps
  shared edges on the exact pixel-center rule so seams never leak sky.
- **Sprites via `INTERP0`**: leaves and blossoms are single textured
  parallelograms; per-pixel u/v stepping is one `POP_FULL` read (the lane
  masks even wrap rounding slop into the map's transparent border). Textures
  are boot-baked *shade maps* tinted per sprite, so one 16×16 map (plus an
  8×8 mip) serves every green and every pink.
- **Perspective-correct grass**: `u/z, v/z, 1/z` are affine in screen space;
  columns walk in 8-pixel chunks with one reciprocal per chunk and the
  interpolator stepping inside. World-space UVs nail the texture to the
  ground under the orbiting camera; procedural noise tile, depth-picked
  mips, distance fog to the horizon colour.
- **Hot code lives in RAM** (`.data`): both cores share one XIP flash cache,
  and RAM placement of the raster + geometry paths bought milliseconds.
- Fast f32 trig/rsqrt everywhere — `libm`'s f64-internal `sinf` was a
  measured 20× geometry slowdown on the M33's single-precision FPU.

The scene (`src/flora`) grows three deterministic-per-seed species with wind
sway and staggered growth, over a bumpy meadow, a ten-minute day cycle,
stars, and birds. Tufty controls: A = new seed, B = replay growth,
C = pause orbit, UP/DOWN = zoom. Frame timings stream over serial.

### Tufty display notes

The ST7789 rides an 8-bit parallel bus (DB0–7 = GPIO32–39, WR = 30 strobed
by a two-instruction PIO program, DC = 28, CS = 27, TE/vsync = 21); DMA
feeds the PIO and a full 150 KiB frame streams in ~8 ms. The framebuffer is
stored in panel scan order (portrait, column-major — `src/gfx` pays the
transpose per pixel write), and presents wait for the TE pulse so the write
outruns the refresh beam: tear-free without double buffering.

### Badger 2040 W notes

The first-generation WiFi badge is a Pico W soldered to an e-paper shield,
and it ports surprisingly cleanly: the radio is the textbook `cyw43` target,
the RTC is the same PCF85063A, and the power story is even better than
POWMAN — an `EN_3V3` latch (GPIO10) the firmware holds high and drops to
power off *completely*, with any front button or the RTC alarm switching it
back on. Its UC8151 panel is strictly 1-bit, but its fast waveforms are the
real thing: the Turbo refresh (~250 ms measured) stays clean in a way the
2350 Badger's four-grey SSD1680 can't match — this is the board for
anything that updates often. `gfx/dither` quantizes to the panel through
the same Bayer/Floyd–Steinberg machinery at a 50 % linear-light threshold.
What stays behind: the 3D renderer (no FPU), PSRAM (none fitted), and the
four greys. Buttons are active high on this board, and first-time flashing
uses the BOOTSEL button on the Pico W module itself.

## The Badger pipeline

The SSD1680 driver ports Pimoroni's four-grey waveform verbatim; full
refreshes measure 1.14 s (TURBO) to 3.67 s (SLOW), with clean greys at every
tier and no visible ghosting. A partial-window mode can re-scan just a band
of columns, but every partial pass slightly fades the undriven rest of the
panel — after measuring twice, the house style is full TURBO refreshes, with
partials reserved for content that truly needs flicker-free updates.

Greyscale rendering is calibrated to the panel's *actual* reflectances
(level 1 is nearly black at ~10%, level 2 ~51% — assume even spacing and
everything drowns in black), quantized in linear light, with the dither
method selectable per region: ordered for stability, error-diffusion for
photographic content, nearest for text and QR codes. `epd_test` puts all of
it on the panel side by side, with live calibration on the buttons.

Networking runs the RM2 (CYW43439) over PIO SPI — the same wiring as a
Pico W — through `cyw43` + `embassy-net`: station mode with DHCP, DNS, TCP,
UDP, and TLS 1.3 via `reqwless`/`embedded-tls`; or access-point mode with
this repo's own tiny DHCP/DNS/HTTP servers for phone-based setup.

## Hardware covered

| Subsystem | Tufty | Badger |
|---|---|---|
| MCU | RP2350B @ 150 MHz, both cores + FPUs | RP2350A @ 150 MHz |
| Screen | ST7789 LCD, PIO+DMA, TE-synced (`bsp/display.rs`) | SSD1680 e-paper, 4 grey (`bsp/epd.rs`) |
| Buttons / rear LEDs | ✓ (`bsp/buttons.rs`, `bsp/leds.rs`) | ✓ (same pins) |
| Backlight + light sensor | ✓ (`bsp/backlight.rs`) | n/a |
| WiFi/BT (RM2 / CYW43439) | wired, untested here | ✓ STA + AP (`bsp/wifi.rs`) |
| RTC (PCF85063A) | ✓ (`bsp/rtc.rs`) | ✓ + NTP sync via `weather` |
| Battery gauge | pending (ADC shared with light sensor) | ✓ (`bsp/battery.rs`) |
| Flash settings store | ✓ (`bsp/settings.rs`) | ✓ (shared keys across apps) |
| Sleep / power-off | RESET-hold → POWMAN off, buttons wake (`bsp/power.rs`) | ✓ (same) |
| USB serial log + input | ✓ (`bsp/usb.rs`) | ✓ |
| PSRAM (8 MB, QMI CS1 = GPIO8) | wired, untested here | ✓ (`bsp/psram.rs`, memory-mapped) |

## Roadmap

- **Badger 2040 W apps**: the weather station and friends need only the
  mono present path swapped in — the port's plumbing is done.
- **Blinky 2350 bring-up**: matrix driver, scrolling marquee port (same
  captive-portal plumbing, open-AP short-SSID join QR, instant display
  toggle on B), greyscale-glow demos.
- Crate rename to match the repo (the library is still `tufty_2350` in
  code).
- 250 MHz overclock (Pimoroni ships the PLL + voltage config) and particles
  to spend the budget.
- POWMAN wake-render-sleep cycles for months-long battery life on e-paper.
