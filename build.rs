//! Copies `memory.x` where the linker can find it and sets link args.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // Per-chip memory map: the RP2040 boards boot via boot2 and have a
    // different flash/RAM layout from the RP2350 ones.
    // NOTE: neither source file may be named `memory.x` — the linker
    // resolves link.x's INCLUDE from the project root ahead of OUT_DIR, so
    // a root-level memory.x would silently shadow this selection.
    let memory: &[u8] = if env::var_os("CARGO_FEATURE_BADGER2040W").is_some() {
        include_bytes!("memory-rp2040.x")
    } else {
        include_bytes!("memory-rp2350.x")
    };
    File::create(out.join("memory.x")).unwrap().write_all(memory).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory-rp2350.x");
    println!("cargo:rerun-if-changed=memory-rp2040.x");

    println!("cargo:rustc-link-arg-examples=--nmagic");
    println!("cargo:rustc-link-arg-examples=-Tlink.x");
}
