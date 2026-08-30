//! A tiny persistent settings record in the last 4 KiB sector of flash.
//!
//! For things that must survive reflashing and power loss but have no other
//! home (a TOTP secret, WiFi credentials): one record of up to
//! [`MAX_LEN`] bytes, stored as `magic | len | bytes` and rewritten
//! whole. The firmware image lives at the bottom of the 16 MiB flash and is
//! nowhere near this sector.
//!
//! Writes stall XIP briefly (the ROM routines run from RAM); on these apps
//! core 1 is parked, so blocking calls are safe.

use embassy_rp::flash::{Blocking, Flash, ERASE_SIZE};
use embassy_rp::peripherals::FLASH;
use embassy_rp::Peri;

/// Total flash size (both boards ship 16 MiB).
const FLASH_SIZE: usize = 16 * 1024 * 1024;

/// Offset of the settings sector: the last erase block.
const OFFSET: u32 = (FLASH_SIZE - ERASE_SIZE) as u32;

/// Identifies a valid record (bump when the layout changes).
const MAGIC: u32 = 0x0BAD_9E01;

/// Maximum record payload.
pub const MAX_LEN: usize = 256;

/// The settings sector.
pub struct Settings {
    flash: Flash<'static, FLASH, Blocking, FLASH_SIZE>,
}

impl Settings {
    pub fn new(flash: Peri<'static, FLASH>) -> Self {
        Self { flash: Flash::new_blocking(flash) }
    }

    /// Reads the record into `buf`, returning the stored slice, or `None`
    /// if the sector holds no valid record.
    pub fn read<'a>(&mut self, buf: &'a mut [u8; MAX_LEN]) -> Option<&'a [u8]> {
        let mut header = [0u8; 8];
        self.flash.blocking_read(OFFSET, &mut header).ok()?;
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if magic != MAGIC || len > MAX_LEN {
            return None;
        }
        self.flash.blocking_read(OFFSET + 8, &mut buf[..len.next_multiple_of(4)]).ok()?;
        Some(&buf[..len])
    }

    /// Erases the sector and stores `data` (up to [`MAX_LEN`] bytes).
    pub fn write(&mut self, data: &[u8]) -> bool {
        if data.len() > MAX_LEN {
            return false;
        }
        let mut record = [0xffu8; 8 + MAX_LEN];
        record[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        record[4..8].copy_from_slice(&(data.len() as u32).to_le_bytes());
        record[8..8 + data.len()].copy_from_slice(data);
        let end = 8 + data.len().next_multiple_of(4);
        let ok = self
            .flash
            .blocking_erase(OFFSET, OFFSET + ERASE_SIZE as u32)
            .and_then(|()| self.flash.blocking_write(OFFSET, &record[..end]));
        if let Err(e) = ok {
            log::warn!("settings write failed: {:?}", e);
            return false;
        }
        true
    }
}

// Rust guideline compliant 2026-08-30
