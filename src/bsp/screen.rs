//! Screen geometry for the selected board (Cargo feature `tufty` or
//! `badger`). Everything above the drivers — framebuffer, projection,
//! rasterizer split — is written against these two constants.

/// Landscape width in pixels.
#[cfg(feature = "tufty")]
pub const WIDTH: usize = 320;
/// Landscape height in pixels.
#[cfg(feature = "tufty")]
pub const HEIGHT: usize = 240;

/// Landscape width in pixels.
#[cfg(feature = "badger")]
pub const WIDTH: usize = 264;
/// Landscape height in pixels.
#[cfg(feature = "badger")]
pub const HEIGHT: usize = 176;

/// Landscape width in pixels.
#[cfg(feature = "badger2040w")]
pub const WIDTH: usize = 296;
/// Landscape height in pixels.
#[cfg(feature = "badger2040w")]
pub const HEIGHT: usize = 128;

#[cfg(not(any(feature = "tufty", feature = "badger", feature = "badger2040w")))]
compile_error!("enable exactly one board feature: `tufty` (default), `badger` or `badger2040w`");
#[cfg(any(
    all(feature = "tufty", feature = "badger"),
    all(feature = "tufty", feature = "badger2040w"),
    all(feature = "badger", feature = "badger2040w"),
))]
compile_error!("board features are mutually exclusive (use --no-default-features)");

// Rust guideline compliant 2026-08-30
