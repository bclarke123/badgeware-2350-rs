//! A tiny persistent key-value store in the last 4 KiB sector of flash.
//!
//! For things that must survive reflashing and power loss but have no other
//! home — a TOTP secret, WiFi credentials — shared by every app so flashing
//! a different example never clobbers another's data. Keys are short strings,
//! values up to [`MAX_VAL`] bytes; the whole record (magic, count, packed
//! `key_len,val_len,key,val` entries, [`RECORD_CAP`] bytes max) is rewritten
//! on every [`Settings::set`].
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

/// Identifies a valid record (bumped when the layout changed to key-value).
const MAGIC: u32 = 0x0BAD_9E02;

/// Maximum value size.
pub const MAX_VAL: usize = 128;

/// Maximum total record size (header + entries).
const RECORD_CAP: usize = 1024;

/// The settings sector.
pub struct Settings {
    flash: Flash<'static, FLASH, Blocking, FLASH_SIZE>,
}

impl Settings {
    pub fn new(flash: Peri<'static, FLASH>) -> Self {
        Self { flash: Flash::new_blocking(flash) }
    }

    /// Loads the record area (valid or not, the caller checks via parsing).
    fn load(&mut self, image: &mut [u8; RECORD_CAP]) -> bool {
        if self.flash.blocking_read(OFFSET, image).is_err() {
            return false;
        }
        u32::from_le_bytes(image[0..4].try_into().unwrap()) == MAGIC
    }

    /// Looks `key` up, copying its value into `buf`.
    pub fn get<'a>(&mut self, key: &str, buf: &'a mut [u8; MAX_VAL]) -> Option<&'a [u8]> {
        let mut image = [0u8; RECORD_CAP];
        if !self.load(&mut image) {
            return None;
        }
        let (k, v) = find(&image, key)?;
        let val = &image[v.0..v.0 + v.1];
        buf[..val.len()].copy_from_slice(val);
        let _ = k;
        Some(&buf[..val.len()])
    }

    /// Inserts or replaces `key` and rewrites the sector. Other keys are
    /// preserved.
    pub fn set(&mut self, key: &str, value: &[u8]) -> bool {
        if key.is_empty() || key.len() > 255 || value.len() > MAX_VAL {
            return false;
        }
        let mut image = [0u8; RECORD_CAP];
        let valid = self.load(&mut image);
        let mut out = [0xffu8; RECORD_CAP];
        let mut n = 8usize;
        let mut count = 0u32;
        if valid {
            // Copy every other key's entry.
            let mut pos = 8usize;
            let stored = u32::from_le_bytes(image[4..8].try_into().unwrap());
            for _ in 0..stored.min(64) {
                let Some((kl, vl)) = entry_lens(&image, pos) else { break };
                let k = &image[pos + 2..pos + 2 + kl];
                let len = 2 + kl + vl;
                if k != key.as_bytes() {
                    out[n..n + len].copy_from_slice(&image[pos..pos + len]);
                    n += len;
                    count += 1;
                }
                pos += len;
            }
        }
        // Append the new entry.
        if n + 2 + key.len() + value.len() > RECORD_CAP {
            return false;
        }
        out[n] = key.len() as u8;
        out[n + 1] = value.len() as u8;
        out[n + 2..n + 2 + key.len()].copy_from_slice(key.as_bytes());
        n += 2 + key.len();
        out[n..n + value.len()].copy_from_slice(value);
        n += value.len();
        count += 1;
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&count.to_le_bytes());
        let end = n.next_multiple_of(4);
        // The ROM flash helpers reset the QMI's PSRAM window; put it back.
        let m1 = super::psram::m1_save();
        let ok = self
            .flash
            .blocking_erase(OFFSET, OFFSET + ERASE_SIZE as u32)
            .and_then(|()| self.flash.blocking_write(OFFSET, &out[..end]));
        super::psram::m1_restore(&m1);
        if let Err(e) = ok {
            log::warn!("settings write failed: {:?}", e);
            return false;
        }
        true
    }
}

/// Entry lengths at `pos`, if they fit the image.
fn entry_lens(image: &[u8; RECORD_CAP], pos: usize) -> Option<(usize, usize)> {
    if pos + 2 > RECORD_CAP {
        return None;
    }
    let (kl, vl) = (usize::from(image[pos]), usize::from(image[pos + 1]));
    (kl > 0 && pos + 2 + kl + vl <= RECORD_CAP).then_some((kl, vl))
}

/// Finds `key`, returning (key span, value span) as (start, len).
fn find(image: &[u8; RECORD_CAP], key: &str) -> Option<((usize, usize), (usize, usize))> {
    let count = u32::from_le_bytes(image[4..8].try_into().unwrap());
    let mut pos = 8usize;
    for _ in 0..count.min(64) {
        let (kl, vl) = entry_lens(image, pos)?;
        if &image[pos + 2..pos + 2 + kl] == key.as_bytes() {
            return Some(((pos + 2, kl), (pos + 2 + kl, vl)));
        }
        pos += 2 + kl + vl;
    }
    None
}

// Rust guideline compliant 2026-08-30
