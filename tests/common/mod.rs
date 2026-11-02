#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub const PLUS_ROM_CHECKSUM: u32 = 0x4D1F_8172;
pub const II_ROM_CHECKSUM: u32 = 0x9779_D2C4;

/// A fake ROM carrying a real model's checksum header, but no Apple code.
pub fn write_rom(dir: &Path, name: &str, checksum: u32, size: usize) -> PathBuf {
    let path = dir.join(name);
    let mut bytes = vec![0u8; size];
    bytes[..4].copy_from_slice(&checksum.to_be_bytes());
    fs::write(&path, bytes).unwrap();
    path
}

/// A blank-but-valid-looking HFS volume.
pub fn write_disk(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let mut bytes = vec![0u8; 800 * 1024];
    bytes[1024..1026].copy_from_slice(b"BD");
    fs::write(&path, bytes).unwrap();
    path
}
