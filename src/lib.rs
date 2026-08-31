//! Procedural tree garden firmware library for Pimoroni's RP2350 badges:
//! the Tufty 2350 (320x240 LCD, feature `tufty`, default) and the Badger 2350
//! (264x176 four-grey e-paper, feature `badger`).
//!
//! Binaries live in `examples/` and compose these modules: `bsp` (board
//! drivers), `gfx` (framebuffer), `render3d` (dual-core software renderer),
//! `flora` (the tree garden scene) and `boot` (image definition, fault and
//! panic handlers — linked into every binary).

#![no_std]

pub mod boot;
pub mod bsp;
#[cfg(not(feature = "badger2040w"))]
pub mod flora;
pub mod gfx;
#[cfg(not(feature = "badger2040w"))]
pub mod render3d;

// Rust guideline compliant 2026-08-30
