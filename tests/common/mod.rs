#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub const PLUS_ROM_CHECKSUM: u32 = 0x4D1F_8172;
pub const II_ROM_CHECKSUM: u32 = 0x9779_D2C4;
pub const IIX_ROM_CHECKSUM: u32 = 0x9722_1136;

/// A synthetic ROM that passes the real checksum test, with no Apple code.
pub fn write_rom(dir: &Path, name: &str, checksum: u32, size: usize) -> PathBuf {
    let path = dir.join(name);
    let mut bytes = vec![0u8; size];
    bytes[..4].copy_from_slice(&checksum.to_be_bytes());
    let mut remaining = checksum;
    for word in bytes[4..].as_chunks_mut::<2>().0 {
        let value = remaining.min(0xFFFF) as u16;
        *word = value.to_be_bytes();
        remaining -= u32::from(value);
    }
    assert_eq!(remaining, 0, "image too small to reach the checksum");
    fs::write(&path, bytes).unwrap();
    path
}

/// A blank-but-valid-looking HFS volume.
pub fn write_disk(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let mut bytes = vec![0u8; 800 * 1024];
    bytes[1024..1026].copy_from_slice(b"BD");
    bytes[1034] = 0x01; // Cleanly unmounted.
    fs::write(&path, bytes).unwrap();
    path
}

/// A disk with a partition map, a driver, and an HFS volume at block 96.
/// Returns the path and the volume's bytes.
pub fn write_partitioned_disk(dir: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let (path, mut volumes) = write_disk_with_volumes(dir, name, &[96]);
    (path, volumes.remove(0))
}

/// As [`write_partitioned_disk`], with a 1600-block HFS volume at each block in
/// `starts`, as partitions 3 onwards. Each volume's contents are distinct.
pub fn write_disk_with_volumes(dir: &Path, name: &str, starts: &[u32]) -> (PathBuf, Vec<Vec<u8>>) {
    let blocks = starts.iter().max().unwrap_or(&96) + 2048;
    let mut bytes = vec![0u8; blocks as usize * 512];
    bytes[..2].copy_from_slice(b"ER");
    bytes[2..4].copy_from_slice(&512u16.to_be_bytes());
    let mut entries = vec![
        (1u32, 63u32, "Apple_partition_map"),
        (64, 32, "Apple_Driver"),
    ];
    entries.extend(starts.iter().map(|start| (*start, 1600, "Apple_HFS")));
    for (index, (start, length, kind)) in entries.iter().enumerate() {
        let at = (index + 1) * 512;
        bytes[at..at + 2].copy_from_slice(b"PM");
        bytes[at + 4..at + 8].copy_from_slice(&(entries.len() as u32).to_be_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&start.to_be_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&length.to_be_bytes());
        bytes[at + 48..at + 48 + kind.len()].copy_from_slice(kind.as_bytes());
    }
    bytes[64 * 512] = 0xDD; // the driver
    let mut volumes = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let volume = *start as usize * 512;
        bytes[volume..volume + 2].copy_from_slice(b"LK");
        bytes[volume + 1024..volume + 1026].copy_from_slice(b"BD");
        bytes[volume + 4096] = 0xA0 + index as u8;
        volumes.push(bytes[volume..volume + 1600 * 512].to_vec());
    }
    let path = dir.join(name);
    fs::write(&path, &bytes).unwrap();
    (path, volumes)
}
