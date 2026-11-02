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

/// A disk with a partition map, a driver, and one HFS volume at block 96.
/// Returns the path and the volume's bytes.
pub fn write_partitioned_disk(dir: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let mut bytes = vec![0u8; 2048 * 512];
    bytes[..2].copy_from_slice(b"ER");
    bytes[2..4].copy_from_slice(&512u16.to_be_bytes());
    for (index, (start, length, kind)) in [
        (1u32, 63u32, "Apple_partition_map"),
        (64, 32, "Apple_Driver"),
        (96, 1600, "Apple_HFS"),
    ]
    .into_iter()
    .enumerate()
    {
        let at = (index + 1) * 512;
        bytes[at..at + 2].copy_from_slice(b"PM");
        bytes[at + 4..at + 8].copy_from_slice(&3u32.to_be_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&start.to_be_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&length.to_be_bytes());
        bytes[at + 48..at + 48 + kind.len()].copy_from_slice(kind.as_bytes());
    }
    let volume = 96 * 512;
    bytes[volume..volume + 2].copy_from_slice(b"LK");
    bytes[volume + 1024..volume + 1026].copy_from_slice(b"BD");
    bytes[volume + 4096] = 0xAB;
    let path = dir.join(name);
    fs::write(&path, &bytes).unwrap();
    let extracted = bytes[volume..volume + 1600 * 512].to_vec();
    (path, extracted)
}
