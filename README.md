# tufty-2350

Rust/[Embassy](https://embassy.dev) firmware for the [Pimoroni Tufty 2350](https://github.com/pimoroni/tufty2350)
badge: a procedurally grown 3D tree on a dual-core software renderer and a
custom PIO+DMA display driver. Each seed byte (persisted in the RTC's
battery-backed RAM) grows a different deterministic tree — recursive ribbon
branches, billboard leaves (blossoms on some seeds), wind sway, and staggered
cubic-eased growth from bare trunk to full bloom. A Simon-style memory game
lived here previously — see git history.

**Controls**: A = new seed (saved), B = replay growth, UP/DOWN = zoom,
C = dual/single-core raster toggle (timings on USB serial), HOME-hold =
BOOTSEL, RESET-hold = sleep.

## 3D renderer (`src/render3d`)

Flat-shaded triangles, painter's algorithm, per-triangle directional lighting,
near-plane clipping. Both cores rasterize ("tiled" split): core 0 builds,
lights, and depth-sorts the frame's triangle list, hands core 1 the right half
of the framebuffer via an `embassy-sync` Signal handshake (bare loop on core 1,
no executor), rasterizes the left half itself, joins, and presents. The
column-major framebuffer makes the two halves contiguous disjoint slices. The
rasterizer walks triangles column-by-column (f32 setup, 16.16 fixed per-column
increments) so the hot loop is a sequential byte fill.

## Hardware covered

| Subsystem | Support |
|---|---|
| RP2350B @ 150 MHz | `embassy-rp` (`rp235xb`) |
| 2.8" 320×240 ST7789, 8-bit 8080 parallel bus | custom PIO+DMA driver (`src/bsp/display.rs`) |
| 5 front buttons + HOME | debounced event channel (`src/bsp/buttons.rs`) |
| Backlight (PWM) + phototransistor | auto-brightness (`src/bsp/backlight.rs`) |
| 4-zone rear LEDs | cue patterns (`src/bsp/leds.rs`) |
| USB serial logging | `embassy-usb-logger` (`src/bsp/usb.rs`) |
| PCF85063A RTC | battery-backed RAM byte for persistence (`src/bsp/rtc.rs`) |
| Dual-core rendering | core 1 rasterization coprocessor (`src/render3d/core1.rs`) |
| Sleep / power-off | hold RESET ~1.5 s → POWMAN off, any front button wakes (`src/bsp/power.rs`) |
| WiFi/BT (RM2), battery gauge, PSRAM | not yet (see notes below) |

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
   A panic also drops the badge back into BOOTSEL after 5 seconds, so it can
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

Remaining first-hardware-test items:

- Battery bring-up: `POWER_EN` (GPIO41) is asserted first thing in `main`;
  verify the badge stays on when unplugged from USB.
- Light-sensor ADC range constants in `src/bsp/backlight.rs`.

## Roadmap

- WiFi/BT via `cyw43` (RM2 module: WL_ON=23, DATA=24, CS=25, CLK=29).
- RTC timekeeping + alarm sleep/wake (the chip is wired up in `src/bsp/rtc.rs`;
  only its RAM byte is used so far).
- Battery gauge (VBAT_SENSE=40, VBUS_DETECT=12).
