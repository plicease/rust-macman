//! Unique physical storage: the host blocks a set of files occupies, with a
//! block that several of them share, by a hard link, a reflink, or the file
//! system's deduplication, counted once. `macman du --physical` reports it
//! beside the allocated-file total of [`crate::usage`], which counts each
//! distinct file's allocation in full however its blocks are shared.
//!
//! It's measured only where the file system says where each file's data is
//! and those places identify the blocks: on Linux, through `FIEMAP`, on ext4,
//! XFS, and Btrfs. Anything else, or anything those can't account for, makes
//! the whole figure unavailable, with the reason, never a guess:
//!
//! - other file systems, including ZFS, whose block cloning and deduplication
//!   aren't visible per file, and stacked ones such as overlayfs, whose
//!   extents belong to the file systems underneath;
//! - compressed (encoded) extents, as on Btrfs, whose length is the data's
//!   and not what it takes on disk, and extents whose place isn't known yet,
//!   or is shared with metadata (inline or tail-packed data);
//! - files on an XFS realtime device, whose addresses aren't the data
//!   device's;
//! - a file that's missing or unreadable, or changes while it's mapped;
//! - macOS and Windows, for now.
//!
//! What's counted is the blocks the files reference. Two caveats remain, and
//! are documented rather than measured: Btrfs keeps a whole extent while any
//! part of it is referenced, so a partly overwritten extent can hold more
//! than its referenced part, and redundancy below the file system, such as
//! Btrfs RAID1 or DUP, isn't multiplied in. Nor is this what removing the
//! files would free, which depends on references macman doesn't know about,
//! such as snapshots or files outside its directories.
//!
//! Mapping a file first flushes its pending writes, so its extents are final,
//! which costs time while an emulator is writing to it; that, and reading
//! every extent of every file, is why it's asked for rather than shown by
//! default.

use std::collections::BTreeMap;
use std::path::Path;

/// What makes a physical address unique: the file system it's on.
// Only Linux measures yet.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Volume {
    /// A file system on one device, by its device number.
    Device(u64),
    /// Btrfs, whose subvolumes have device numbers of their own but share
    /// one address space, by the file system's UUID.
    Btrfs([u8; 16]),
}

/// One extent of a file, as the file system reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    physical: u64,
    length: u64,
    flags: u32,
}

// FIEMAP's extent flags, from linux/fiemap.h.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LAST: u32 = 0x0001;
const UNKNOWN: u32 = 0x0002;
const DELALLOC: u32 = 0x0004;
const ENCODED: u32 = 0x0008;
const NOT_ALIGNED: u32 = 0x0100;
const DATA_INLINE: u32 = 0x0200;
const DATA_TAIL: u32 = 0x0400;

/// Why `extent` can't be counted by its place and length, if it can't.
fn unaccountable(extent: &Extent) -> Option<&'static str> {
    if extent.flags & (UNKNOWN | DELALLOC) != 0 {
        Some("has data whose place on disk isn't known yet")
    } else if extent.flags & ENCODED != 0 {
        Some("has compressed or otherwise encoded data, whose size on disk isn't reported")
    } else if extent.flags & (NOT_ALIGNED | DATA_INLINE | DATA_TAIL) != 0 {
        Some("has data stored alongside metadata rather than in blocks of its own")
    } else {
        None
    }
}

/// The bytes the ranges `(start, length)` cover, counting overlaps once.
fn union_length(mut ranges: Vec<(u64, u64)>) -> u64 {
    ranges.sort_unstable();
    let mut total = 0u64;
    let mut current: Option<(u64, u64)> = None;
    for (start, length) in ranges {
        let end = start.saturating_add(length);
        current = match current {
            Some((from, to)) if start <= to => Some((from, to.max(end))),
            Some((from, to)) => {
                total = total.saturating_add(to - from);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((from, to)) = current {
        total = total.saturating_add(to - from);
    }
    total
}

/// What the extents, by volume, occupy together.
fn unique_bytes(extents: BTreeMap<Volume, Vec<(u64, u64)>>) -> u64 {
    extents
        .into_values()
        .map(union_length)
        .fold(0, u64::saturating_add)
}

/// The bytes `files` occupy on the host, counting each physical block once
/// however many of them use it, or why that can't be told reliably.
pub fn unique_physical<'a>(files: impl IntoIterator<Item = &'a Path>) -> Result<u64, String> {
    let mut extents: BTreeMap<Volume, Vec<(u64, u64)>> = BTreeMap::new();
    for file in files {
        let (volume, found) = imp::map(file)?;
        let ranges = extents.entry(volume).or_default();
        for extent in found {
            if let Some(why) = unaccountable(&extent) {
                return Err(format!("{} {why}", file.display()));
            }
            ranges.push((extent.physical, extent.length));
        }
    }
    Ok(unique_bytes(extents))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Extent, LAST, Volume};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    const EXT4: u32 = 0xEF53;
    const XFS: u32 = 0x5846_5342;
    const BTRFS: u32 = 0x9123_683E;

    /// Names for file systems that are often met and can't be measured.
    fn name_of(magic: u32) -> Option<&'static str> {
        Some(match magic {
            0x2FC1_2FC1 => "ZFS",
            0x0102_1994 => "tmpfs",
            0x794C_7630 => "overlayfs",
            0x6969 => "NFS",
            0xFF53_4D42 | 0xFE53_4D42 => "SMB",
            0x6573_5546 => "FUSE",
            0xF2F5_2010 => "F2FS",
            0xCA45_1A4E => "bcachefs",
            0x5346_544E => "NTFS",
            0x4D44 => "FAT",
            0x2011_BAB0 => "exFAT",
            _ => return None,
        })
    }

    // From linux/fs.h and linux/fiemap.h.
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    const FIEMAP_FLAG_SYNC: u32 = 0x0001;
    // _IOR('X', 31, struct fsxattr)
    const FS_IOC_FSGETXATTR: libc::c_ulong = 0x801C_581F;
    const FS_XFLAG_REALTIME: u32 = 0x0001;
    // _IOR(0x94, 31, struct btrfs_ioctl_fs_info_args)
    const BTRFS_IOC_FS_INFO: libc::c_ulong = 0x8400_941F;

    /// Extents fetched per call.
    const BATCH: usize = 256;
    /// Times a file that changes while it's mapped is tried.
    const ATTEMPTS: usize = 3;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FiemapExtent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [FiemapExtent; BATCH],
    }

    #[repr(C)]
    #[derive(Default)]
    struct FsXattr {
        xflags: u32,
        extsize: u32,
        nextents: u32,
        projid: u32,
        cowextsize: u32,
        pad: [u8; 8],
    }

    #[repr(C)]
    struct BtrfsFsInfo {
        max_id: u64,
        num_devices: u64,
        fsid: [u8; 16],
        rest: [u8; 1024 - 32],
    }

    fn ioctl<T>(file: &File, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
        // SAFETY: `arg` is the struct the request reads and writes, and lives
        // for the call.
        let result =
            unsafe { libc::ioctl(file.as_raw_fd(), request as _, std::ptr::from_mut(arg)) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn fstat(file: &File) -> io::Result<libc::stat> {
        // SAFETY: zero is a valid stat, and it's only read after success.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open, and `stat` is writable.
        if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    /// What changes when a file's data or allocation does.
    fn version(stat: &libc::stat) -> (i64, i64, i64, i64, i64, i64) {
        // The fields' widths vary between architectures.
        #[allow(clippy::unnecessary_cast)]
        (
            stat.st_size as i64,
            stat.st_blocks as i64,
            stat.st_mtime as i64,
            stat.st_mtime_nsec as i64,
            stat.st_ctime as i64,
            stat.st_ctime_nsec as i64,
        )
    }

    /// Where `file`'s data is, and on which volume, or why that can't be
    /// trusted to identify its blocks.
    pub fn map(path: &Path) -> Result<(Volume, Vec<Extent>), String> {
        let shown = path.display();
        let file = File::open(path).map_err(|error| format!("can't read {shown}: {error}"))?;
        let stat = fstat(&file).map_err(|error| format!("can't read {shown}: {error}"))?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(format!("{shown} isn't a regular file"));
        }
        // SAFETY: zero is a valid statfs, and it's only read after success.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open, and `fs` is writable.
        if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } < 0 {
            let error = io::Error::last_os_error();
            return Err(format!(
                "can't tell what file system {shown} is on: {error}"
            ));
        }
        // The magic is 32 bits, in a field whose width varies.
        let magic = fs.f_type as u32;
        let volume = match magic {
            #[allow(clippy::unnecessary_cast)]
            EXT4 => Volume::Device(stat.st_dev as u64),
            XFS => {
                let mut attributes = FsXattr::default();
                ioctl(&file, FS_IOC_FSGETXATTR, &mut attributes)
                    .map_err(|error| format!("can't read {shown}'s attributes: {error}"))?;
                if attributes.xflags & FS_XFLAG_REALTIME != 0 {
                    return Err(format!(
                        "{shown} is on an XFS realtime device, whose block addresses \
                         macman can't tell from the data device's"
                    ));
                }
                #[allow(clippy::unnecessary_cast)]
                Volume::Device(stat.st_dev as u64)
            }
            BTRFS => {
                let mut info = BtrfsFsInfo {
                    max_id: 0,
                    num_devices: 0,
                    fsid: [0; 16],
                    rest: [0; 1024 - 32],
                };
                ioctl(&file, BTRFS_IOC_FS_INFO, &mut info).map_err(|error| {
                    format!("can't identify the Btrfs file system {shown} is on: {error}")
                })?;
                Volume::Btrfs(info.fsid)
            }
            other => {
                let what = name_of(other)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("a file system of type {other:#x}"));
                return Err(format!(
                    "{shown} is on {what}, where macman can't tell which blocks files \
                     share; it can on ext4, XFS, and Btrfs"
                ));
            }
        };
        for _ in 0..ATTEMPTS {
            let before = version(&fstat(&file).map_err(|error| format!("{shown}: {error}"))?);
            let extents = extents(&file)
                .map_err(|error| format!("can't map where {shown}'s data is on disk: {error}"))?;
            let after = version(&fstat(&file).map_err(|error| format!("{shown}: {error}"))?);
            if before == after {
                return Ok((volume, extents));
            }
        }
        Err(format!("{shown} kept changing while it was being measured"))
    }

    /// Every extent of `file`, fetched a batch at a time, after its pending
    /// writes are flushed so none is still to be placed.
    fn extents(file: &File) -> io::Result<Vec<Extent>> {
        let mut found = Vec::new();
        let mut start = 0u64;
        let mut flags = FIEMAP_FLAG_SYNC;
        loop {
            let mut request = Box::new(Fiemap {
                start,
                length: u64::MAX - start,
                flags,
                mapped_extents: 0,
                extent_count: BATCH as u32,
                reserved: 0,
                extents: [FiemapExtent::default(); BATCH],
            });
            ioctl(file, FS_IOC_FIEMAP, &mut *request)?;
            // Flushed once is enough.
            flags = 0;
            let mapped = (request.mapped_extents as usize).min(BATCH);
            let batch = &request.extents[..mapped];
            for extent in batch {
                found.push(Extent {
                    physical: extent.physical,
                    length: extent.length,
                    flags: extent.flags,
                });
            }
            match batch.last() {
                Some(last) if last.flags & LAST == 0 => {
                    let next = last.logical.saturating_add(last.length);
                    if next <= start {
                        return Err(io::Error::other("the extents didn't advance"));
                    }
                    start = next;
                }
                _ => return Ok(found),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{Extent, Volume};
    use std::path::Path;

    pub fn map(_path: &Path) -> Result<(Volume, Vec<Extent>), String> {
        Err(format!(
            "unique physical usage isn't implemented on {} yet",
            match std::env::consts::OS {
                "macos" => "macOS",
                "windows" => "Windows",
                other => other,
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(physical: u64, length: u64, flags: u32) -> Extent {
        Extent {
            physical,
            length,
            flags,
        }
    }

    #[test]
    fn overlapping_ranges_count_once() {
        assert_eq!(union_length(vec![]), 0);
        assert_eq!(union_length(vec![(0, 4096)]), 4096);
        // The same range twice, as two links to one file or a reflink.
        assert_eq!(union_length(vec![(8192, 4096), (8192, 4096)]), 4096);
        // Partly shared, as a clone that was then partly rewritten.
        assert_eq!(union_length(vec![(0, 8192), (4096, 8192)]), 12288);
        // One inside another, as a file that shares part of a larger extent.
        assert_eq!(union_length(vec![(0, 65536), (4096, 4096)]), 65536);
        // Adjacent but not overlapping, and apart.
        assert_eq!(union_length(vec![(4096, 4096), (0, 4096)]), 8192);
        assert_eq!(union_length(vec![(0, 4096), (1 << 20, 4096)]), 8192);
        assert_eq!(union_length(vec![(u64::MAX - 10, 100)]), 10);
    }

    #[test]
    fn the_same_address_on_different_volumes_is_different_storage() {
        let mut extents = BTreeMap::new();
        extents.insert(Volume::Device(1), vec![(0, 4096), (0, 4096)]);
        extents.insert(Volume::Device(2), vec![(0, 4096)]);
        extents.insert(Volume::Btrfs([7; 16]), vec![(0, 4096)]);
        assert_eq!(unique_bytes(extents), 12288);
    }

    #[test]
    fn only_extents_with_known_blocks_of_their_own_are_counted() {
        assert_eq!(unaccountable(&extent(0, 4096, 0)), None);
        // Last, preallocated, merged, shared, and encrypted in place are fine.
        for flags in [LAST, 0x0800, 0x1000, 0x2000, 0x0080, LAST | 0x2000] {
            assert_eq!(unaccountable(&extent(0, 4096, flags)), None, "{flags:#x}");
        }
        for flags in [
            UNKNOWN,
            DELALLOC,
            ENCODED,
            NOT_ALIGNED,
            DATA_INLINE,
            DATA_TAIL,
        ] {
            assert!(
                unaccountable(&extent(0, 4096, flags | LAST)).is_some(),
                "{flags:#x}"
            );
        }
    }

    #[test]
    fn a_missing_file_makes_it_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.dsk");
        let error = unique_physical([missing.as_path()]).unwrap_err();
        assert!(error.contains("missing.dsk"), "{error}");
        assert_eq!(unique_physical([]), Ok(0));
    }
}
