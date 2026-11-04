//! Host disk usage: the space the file system has allocated to a file, as
//! opposed to its length. A sparse disk image may be far smaller on the host
//! than the disk the guest sees. Every listing that reports usage, and
//! `macman du`, measures through here, so they agree.
//!
//! What's measured is allocation as the platform reports it: `st_blocks` on
//! Unix, and the allocation size Windows keeps for the file. Neither knows
//! about extents shared with other files, by reflinks or deduplication, nor
//! about compression the file system does underneath, so usage can overstate
//! what removing a file would free. Hard links to one file are counted once
//! within a total, be it a machine's or `du`'s of everything.

use crate::checkpoint;
use crate::disk::{self, Classification, DiskFormat};
use crate::machine::Machine;
use crate::paths::Paths;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What identifies a file on its volume, so hard links are counted once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Identity(u64, u64);

/// The bytes allocated to `path`, and its identity, or `None` where the
/// platform can't say or the file can't be read.
fn measure(path: &Path) -> Option<(Identity, u64)> {
    imp::measure(path)
}

#[cfg(unix)]
mod imp {
    use super::Identity;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn measure(path: &Path) -> Option<(Identity, u64)> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        // `st_blocks` counts 512-byte units on Linux and macOS alike.
        Some((
            Identity(metadata.dev(), metadata.ino()),
            metadata.blocks().saturating_mul(512),
        ))
    }
}

#[cfg(windows)]
mod imp {
    use super::Identity;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    pub fn measure(path: &Path) -> Option<(Identity, u64)> {
        // Attributes only, which an emulator holding the image with sharing
        // denied still allows.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        // SAFETY: all-zero structs are valid, and are only read after success.
        let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for the call, and the buffer is the
        // size given.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&raw mut standard).cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: as above.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for the call, and `info` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return None;
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Some((
            Identity(u64::from(info.dwVolumeSerialNumber), index),
            u64::try_from(standard.AllocationSize).ok()?,
        ))
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::Identity;
    use std::path::Path;

    /// No portable way to tell, so it's reported as unavailable, not as the
    /// file's length.
    pub fn measure(_path: &Path) -> Option<(Identity, u64)> {
        None
    }
}

/// The bytes the host has allocated to the file at `path`, or `None` if it's
/// missing, unreadable, or this platform can't tell.
pub fn allocated(path: &Path) -> Option<u64> {
    measure(path).map(|(_, bytes)| bytes)
}

/// The allocation of every file in `paths`, counting a file reached by more
/// than one hard link once. `None` if any can't be measured, as a partial total
/// would understate the whole.
pub fn allocated_total<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<u64> {
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for path in paths {
        let (identity, bytes) = measure(path)?;
        if seen.insert(identity) {
            total = total.saturating_add(bytes);
        }
    }
    Some(total)
}

/// `bytes` in IEC units, as `512 B`, `1.5 KiB`, or `63 MiB`: one decimal
/// below 10 of a unit, and whole units above.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // Rounding up to the next unit reads better as that unit.
    if value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 9.95 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// [`format_bytes`], or `-` where usage is unavailable.
pub fn format_usage(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "-".to_string(), format_bytes)
}

/// What a machine keeps on the host: every disk its definition lists, and
/// every published checkpoint, in one total, so a file reached both ways is
/// counted once. Not what a run makes for itself, such as an ephemeral copy,
/// which lives in the session and goes with it, nor a checkpoint still being
/// written, which has no metadata yet. `None` if any of it can't be measured,
/// including a checkpoint whose image is missing.
pub fn machine_used(paths: &Paths, machine: &Machine) -> Option<u64> {
    allocated_total(machine_files(paths, machine)?.iter().map(PathBuf::as_path))
}

/// The files [`machine_used`] measures, or `None` if they can't all be
/// found out, so a wider total can count each file once.
pub fn machine_files(paths: &Paths, machine: &Machine) -> Option<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = machine
        .disk_paths(paths)
        .ok()?
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    for checkpoint in checkpoint::list(paths, &machine.name).ok()? {
        files.push(checkpoint::image_file(
            paths,
            &machine.name,
            &checkpoint.name,
        ));
    }
    Some(files)
}

/// The capacity of a machine's boot disk as the guest sees it: the data of a
/// Disk Copy image, and otherwise the whole file, sparse or not.
pub fn boot_disk_capacity(paths: &Paths, machine: &Machine) -> Option<u64> {
    let disks = machine.disk_paths(paths).ok()?;
    let (path, _) = disks.iter().find(|(_, disk)| disk.boot)?;
    match disk::classify(path).ok()? {
        Classification::Supported(DiskFormat::DiskCopy42) => disk::disk_copy_contents(path)
            .ok()
            .map(|(_, length, _)| length),
        _ => std::fs::metadata(path).ok().map(|metadata| metadata.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sizes_read_in_iec_units() {
        let cases = [
            (0, "0 B"),
            (512, "512 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (128 * 1024, "128 KiB"),
            (800 * 1024, "800 KiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (63 * 1024 * 1024, "63 MiB"),
            (1 << 30, "1.0 GiB"),
            (412 * (1 << 20), "412 MiB"),
            (u64::MAX, "16 EiB"),
        ];
        for (bytes, text) in cases {
            assert_eq!(format_bytes(bytes), text, "{bytes}");
        }
        assert_eq!(format_usage(None), "-");
        assert_eq!(format_usage(Some(0)), "0 B", "zero isn't unavailable");
    }

    #[test]
    fn written_files_are_allocated_and_missing_ones_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("full.dsk");
        fs::write(&file, vec![0x5Au8; 256 * 1024]).unwrap();
        let used = allocated(&file).unwrap();
        assert!(used >= 256 * 1024, "{used}");
        assert_eq!(allocated(&dir.path().join("missing")), None);
        assert_eq!(allocated(dir.path()), None, "a directory isn't an image");
        let both = [file.as_path(), &dir.path().join("missing")];
        assert_eq!(allocated_total(both), None, "never a partial total");
        assert_eq!(allocated_total([]), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn a_sparse_file_uses_less_than_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sparse.dsk");
        let handle = fs::File::create(&file).unwrap();
        handle.set_len(1 << 30).unwrap();
        drop(handle);
        assert_eq!(fs::metadata(&file).unwrap().len(), 1 << 30);
        let used = allocated(&file).unwrap();
        assert!(used < 1 << 20, "a 1G hole allocated {used}");
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_count_once_in_a_total() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.dsk");
        fs::write(&file, vec![1u8; 64 * 1024]).unwrap();
        let link = dir.path().join("b.dsk");
        fs::hard_link(&file, &link).unwrap();
        let one = allocated(&file).unwrap();
        assert_eq!(allocated_total([file.as_path(), &link]), Some(one));
    }
}
