//! Unique physical storage: the host blocks a set of files occupies, with a
//! block that several of them share, by a hard link, a reflink, or the file
//! system's deduplication, counted once. `macman du --physical` reports it
//! beside the allocated-file total of [`crate::usage`], which counts each
//! distinct file's allocation in full however its blocks are shared.
//!
//! It's measured only where the file system says where each file's data is
//! and those places identify the blocks: on Linux, through `FIEMAP`, on ext4,
//! XFS, and Btrfs, and on Windows, through `FSCTL_GET_RETRIEVAL_POINTERS`, on
//! local NTFS and ReFS volumes, whose cluster numbers are addresses in the
//! volume's one cluster space, so a block-cloned cluster on ReFS has the same
//! number in every file that shares it. Anything else, or anything those
//! can't account for, makes the whole figure unavailable, with the reason,
//! never a guess:
//!
//! - other file systems, including ZFS, whose block cloning and deduplication
//!   aren't visible per file, stacked ones such as overlayfs, whose extents
//!   belong to the file systems underneath, and network shares;
//! - compressed data, as on Btrfs or NTFS, whose length is the data's and not
//!   what it takes on disk, and extents whose place isn't known yet, or is
//!   shared with metadata (inline or tail-packed data, or a small file kept
//!   in its NTFS file record);
//! - Windows files whose data isn't in the clusters they report: reparse
//!   points, among them files deduplicated by Windows Server's Data
//!   Deduplication or held in the cloud, and files compacted by `compact
//!   /exe` or WIMBoot, whose reparse point is hidden; and, as a check on all
//!   of them, any file whose clusters don't cover all of its data;
//! - files on an XFS realtime device, whose addresses aren't the data
//!   device's;
//! - a file that's missing or unreadable, or changes while it's mapped;
//! - macOS, for now.
//!
//! What's counted is the blocks the files reference. Two caveats remain, and
//! are documented rather than measured: Btrfs keeps a whole extent while any
//! part of it is referenced, so a partly overwritten extent can hold more
//! than its referenced part, and redundancy below the file system, such as
//! Btrfs RAID1 or DUP, isn't multiplied in. Nor is this what removing the
//! files would free, which depends on references macman doesn't know about,
//! such as snapshots or files outside its directories.
//!
//! On Linux, mapping a file first flushes its pending writes, so its extents
//! are final, which costs time while an emulator is writing to it; that, and
//! reading every extent of every file, is why it's asked for rather than
//! shown by default. On Windows, every writable file on ReFS is flushed,
//! as ReFS moves rewritten data to new clusters only then (macman flushes a
//! file before making it read-only, which keeps it from being written), but on NTFS only a file
//! whose recent writes have no clusters yet, as flushing takes opening it to
//! write, which an emulator holding it may not allow. Only a file's main data is counted: on
//! Windows, its alternate data streams aren't, as they aren't in its
//! allocation either.

use std::collections::BTreeMap;
use std::path::Path;

/// What makes a physical address unique: the file system it's on.
// Each platform makes only its own kinds.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Volume {
    /// A file system on one device, by its device number.
    Device(u64),
    /// Btrfs, whose subvolumes have device numbers of their own but share
    /// one address space, by the file system's UUID.
    Btrfs([u8; 16]),
    /// A Windows volume, by its GUID path, `\\?\Volume{...}\`, which unlike
    /// its serial number can't be the same as another's.
    Windows(String),
}

/// One extent of a file, as the file system reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    physical: u64,
    length: u64,
    flags: u32,
}

// FIEMAP's extent flags, from linux/fiemap.h; Windows's extents have none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LAST: u32 = 0x0001;
const UNKNOWN: u32 = 0x0002;
const DELALLOC: u32 = 0x0004;
const ENCODED: u32 = 0x0008;
const NOT_ALIGNED: u32 = 0x0100;
const DATA_INLINE: u32 = 0x0200;
const DATA_TAIL: u32 = 0x0400;

const ALONGSIDE_METADATA: &str =
    "has data stored alongside metadata rather than in blocks of its own";
const PLACE_UNKNOWN: &str = "has data whose place on disk isn't known yet";

/// Why `extent` can't be counted by its place and length, if it can't.
fn unaccountable(extent: &Extent) -> Option<&'static str> {
    if extent.flags & (UNKNOWN | DELALLOC) != 0 {
        Some(PLACE_UNKNOWN)
    } else if extent.flags & ENCODED != 0 {
        Some("has compressed or otherwise encoded data, whose size on disk isn't reported")
    } else if extent.flags & (NOT_ALIGNED | DATA_INLINE | DATA_TAIL) != 0 {
        Some(ALONGSIDE_METADATA)
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

#[cfg(windows)]
mod imp {
    use super::{ALONGSIDE_METADATA, Extent, PLACE_UNKNOWN, Volume};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_FUNCTION, ERROR_MORE_DATA,
        ERROR_NOT_SUPPORTED, ERROR_OBJECT_NOT_EXTERNALLY_BACKED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
        FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
        FileAttributeTagInfo, FileBasicInfo, FileStandardInfo, GetDiskFreeSpaceW,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
        VOLUME_NAME_GUID,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_EXTERNAL_BACKING, FSCTL_GET_RETRIEVAL_POINTERS, FSCTL_QUERY_ALLOCATED_RANGES,
    };

    /// Runs fetched per call.
    const BATCH: usize = 256;
    /// Times a file that changes while it's mapped is tried.
    const ATTEMPTS: usize = 3;

    /// One run of `RETRIEVAL_POINTERS_BUFFER`: the clusters up to `next_vcn`
    /// are at `lcn` onwards, or are a hole if it's -1.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Run {
        next_vcn: i64,
        lcn: i64,
    }

    /// `RETRIEVAL_POINTERS_BUFFER`, with room for a batch of runs.
    #[repr(C)]
    struct Pointers {
        extent_count: u32,
        starting_vcn: i64,
        runs: [Run; BATCH],
    }

    /// The information of class `class` about `file`.
    fn info<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
        let mut value = T::default();
        // SAFETY: the handle is open, and `value` is the class's struct, of
        // the size given.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&raw mut value).cast(),
                size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    fn wide_to_string(wide: &[u16]) -> String {
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..end])
    }

    /// The name of the file system `file` is on, such as `NTFS`.
    fn file_system(file: &File) -> io::Result<String> {
        let mut name = [0u16; 261];
        // SAFETY: the handle is open, the buffer is the length given, and
        // what isn't asked for is null.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wide_to_string(&name))
    }

    /// The GUID path of the volume `file` is on, `\\?\Volume{...}\`, which
    /// only a local volume has.
    fn volume_root(file: &File) -> io::Result<String> {
        let mut buffer = vec![0u16; 512];
        loop {
            // SAFETY: the handle is open, and the buffer is the length given.
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    VOLUME_NAME_GUID,
                )
            } as usize;
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            // Too small, it says the length needed, terminator included.
            if length >= buffer.len() {
                buffer.resize(length + 1, 0);
                continue;
            }
            let path = String::from_utf16_lossy(&buffer[..length]);
            return match path.find("}\\") {
                Some(end) if path.starts_with(r"\\?\Volume{") => Ok(path[..end + 2].to_string()),
                _ => Err(io::Error::other(format!(
                    "{path} isn't a volume's GUID path"
                ))),
            };
        }
    }

    /// The bytes in a cluster of the volume at `root`.
    fn cluster_size(root: &str) -> io::Result<u64> {
        let root: Vec<u16> = std::ffi::OsStr::new(root)
            .encode_wide()
            .chain([0])
            .collect();
        let (mut sectors, mut bytes) = (0u32, 0u32);
        // SAFETY: `root` is terminated, and the counts are writable; the free
        // and total counts aren't asked for.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                root.as_ptr(),
                &mut sectors,
                &mut bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        match u64::from(sectors) * u64::from(bytes) {
            0 => Err(io::Error::other("the volume reports no cluster size")),
            size => Ok(size),
        }
    }

    /// What changes when a file's data or allocation does, and its size.
    fn version(file: &File) -> io::Result<(i64, i64, i64, i64)> {
        let standard: FILE_STANDARD_INFO = info(file, FileStandardInfo)?;
        let basic: FILE_BASIC_INFO = info(file, FileBasicInfo)?;
        Ok((
            standard.EndOfFile,
            standard.AllocationSize,
            basic.LastWriteTime,
            basic.ChangeTime,
        ))
    }

    /// Where `file`'s data is, and on which volume, or why that can't be
    /// trusted to identify its clusters.
    pub fn map(path: &Path) -> Result<(Volume, Vec<Extent>), String> {
        let shown = path.display();
        // Attributes only, which is all that mapping needs, and which an
        // emulator holding the image with sharing denied still allows.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .map_err(|error| format!("can't read {shown}: {error}"))?;
        let tag: FILE_ATTRIBUTE_TAG_INFO = info(&file, FileAttributeTagInfo)
            .map_err(|error| format!("can't read {shown}'s attributes: {error}"))?;
        let attributes = tag.FileAttributes;
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(format!("{shown} isn't a regular file"));
        }
        // A link was followed to what it names, so this is the file itself.
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{shown} is a reparse point (tag {:#x}), such as a deduplicated, compacted, \
                 or cloud file, whose data isn't in the clusters it reports",
                tag.ReparseTag
            ));
        }
        if attributes
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
        {
            return Err(format!("{shown} isn't all stored on this host"));
        }
        if attributes & FILE_ATTRIBUTE_COMPRESSED != 0 {
            return Err(format!(
                "{shown} is compressed, and its clusters aren't counted as its size on disk"
            ));
        }
        let system = file_system(&file)
            .map_err(|error| format!("can't tell what file system {shown} is on: {error}"))?;
        if system != "NTFS" && system != "ReFS" {
            return Err(format!(
                "{shown} is on {system}, where macman can't tell which blocks files share; \
                 it can on NTFS and ReFS"
            ));
        }
        // A network share can report NTFS, but has no GUID path here.
        let root = volume_root(&file).map_err(|error| {
            format!("{shown} isn't on a local volume macman can identify: {error}")
        })?;
        let cluster = cluster_size(&root)
            .map_err(|error| format!("can't read the cluster size of {root}: {error}"))?;
        // Its reparse point hidden, a file `compact /exe` or WIMBoot left
        // keeps its data elsewhere, and is decompressed if opened to write.
        if externally_backed(&file)
            .map_err(|error| format!("can't tell where {shown} keeps its data: {error}"))?
        {
            return Err(format!(
                "{shown} is compressed or backed by another file, as `compact /exe` and \
                 WIMBoot leave files, and its clusters aren't counted as its size on disk"
            ));
        }
        // A sparse file's data is wherever it isn't a hole, which takes
        // reading to ask; anything else's is all of it.
        let sparse = attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0;
        let reader = sparse
            .then(|| File::open(path))
            .transpose()
            .map_err(|error| format!("can't read {shown}: {error}"))?;
        // ReFS writes even a rewrite to new clusters, given only as it's
        // flushed, so until then the map can show data where it no longer
        // is, such as still in clusters a clone shares, and cover the file
        // all the same. NTFS rewrites in place, so needs a flush only for
        // what it hasn't placed at all, which falling short shows below.
        // A read-only file takes no writes, and can't be opened to flush;
        // macman flushes its files before making them read-only.
        if system == "ReFS" && attributes & FILE_ATTRIBUTE_READONLY == 0 {
            flush(path).map_err(|error| {
                format!("can't flush {shown}'s writes so its clusters are final: {error}")
            })?;
        }
        let mut flushed = false;
        // One more for after a flush.
        for _ in 0..=ATTEMPTS {
            let before = version(&file).map_err(|error| format!("{shown}: {error}"))?;
            let size = u64::try_from(before.0).unwrap_or(0);
            let mapped = extents(&file, cluster)
                .map_err(|error| format!("can't map where {shown}'s data is on disk: {error}"))?;
            let data = match &reader {
                Some(reader) => allocated_ranges(reader, size)
                    .map_err(|error| format!("can't read where {shown} has data: {error}"))?,
                None => vec![(0, size)],
            };
            let after = version(&file).map_err(|error| format!("{shown}: {error}"))?;
            if before != after {
                continue;
            }
            let (extents, covered, reason) = match mapped {
                Some((extents, covered)) => (extents, covered, PLACE_UNKNOWN),
                // No clusters at all: data that isn't sparse is kept in the
                // file's record, as a small file's is on NTFS, unless it's
                // still to be placed.
                None if !sparse && size > 0 => (Vec::new(), Vec::new(), ALONGSIDE_METADATA),
                None => (Vec::new(), Vec::new(), PLACE_UNKNOWN),
            };
            if covers(&covered, &data, cluster) {
                return Ok((Volume::Windows(root), extents));
            }
            // Data just written is given clusters as it's flushed, so the map
            // can be missing it: ReFS places all new data that way, and NTFS
            // what's written to a sparse file's holes. Files that opening to
            // write would change, compressed or compacted, were refused above.
            if flushed {
                return Err(format!("{shown} {reason}"));
            }
            flushed = true;
            flush(path).map_err(|error| {
                format!("{shown} {reason}, and its writes can't be flushed: {error}")
            })?;
        }
        Err(format!("{shown} kept changing while it was being measured"))
    }

    /// Whether `file`'s data is kept by the Windows Overlay Filter, as
    /// `compact /exe` and WIMBoot keep it, rather than in its own clusters.
    fn externally_backed(file: &File) -> io::Result<bool> {
        let mut info = [0u8; 64];
        let mut returned = 0u32;
        // SAFETY: there's no input, the output is the size given, and it
        // isn't overlapped.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_EXTERNAL_BACKING,
                std::ptr::null(),
                0,
                info.as_mut_ptr().cast(),
                info.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            // There's backing, with more to say than fits.
            Some(ERROR_INSUFFICIENT_BUFFER | ERROR_MORE_DATA) => Ok(true),
            // None, or no overlay filter on the volume to have any.
            Some(
                ERROR_OBJECT_NOT_EXTERNALLY_BACKED | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED,
            ) => Ok(false),
            _ => Err(error),
        }
    }

    /// Write `path`'s cached data to disk, which needs a handle that can
    /// write, so its clusters are all assigned.
    fn flush(path: &Path) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.sync_all()
    }

    /// Whether the byte ranges `covered`, `(start, end)` in whole clusters,
    /// sorted, take in every cluster of the ranges `data`.
    fn covers(covered: &[(u64, u64)], data: &[(u64, u64)], cluster: u64) -> bool {
        data.iter().all(|&(start, end)| {
            if start >= end {
                return true;
            }
            let start = start / cluster * cluster;
            let end = end.div_ceil(cluster).saturating_mul(cluster);
            let mut reached = start;
            for &(from, to) in covered {
                if from > reached {
                    break;
                }
                reached = reached.max(to);
                if reached >= end {
                    return true;
                }
            }
            false
        })
    }

    /// `FILE_ALLOCATED_RANGE_BUFFER`.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    struct Range {
        offset: i64,
        length: i64,
    }

    /// The byte ranges of `file`, `size` long, that hold data rather than
    /// being holes, as `(start, end)`; it needs a handle that can read.
    fn allocated_ranges(file: &File, size: u64) -> io::Result<Vec<(u64, u64)>> {
        let mut found = Vec::new();
        let mut offset = 0u64;
        while offset < size {
            let query = Range {
                offset: offset as i64,
                length: (size - offset) as i64,
            };
            let mut ranges = Box::new([Range::default(); BATCH]);
            let mut returned = 0u32;
            // SAFETY: the input is one range, the output room for a batch,
            // both live for the call, and it isn't overlapped.
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    FSCTL_QUERY_ALLOCATED_RANGES,
                    (&raw const query).cast(),
                    size_of::<Range>() as u32,
                    ranges.as_mut_ptr().cast(),
                    size_of::<[Range; BATCH]>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            let more = if ok != 0 {
                false
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error().map(|code| code as u32) != Some(ERROR_MORE_DATA) {
                    return Err(error);
                }
                true
            };
            let count = (returned as usize / size_of::<Range>()).min(BATCH);
            for range in &ranges[..count] {
                let (start, length) = (range.offset as u64, range.length as u64);
                found.push((start, start.saturating_add(length)));
            }
            match found.last() {
                Some(&(_, end)) if more && end > offset => offset = end,
                _ if more => return Err(io::Error::other("the ranges didn't advance")),
                _ => break,
            }
        }
        Ok(found)
    }

    /// Every cluster run of `file` that has clusters, fetched a batch at a
    /// time, in bytes, and the byte ranges of the file they hold, or `None`
    /// if it has no clusters at all.
    #[allow(clippy::type_complexity)]
    fn extents(file: &File, cluster: u64) -> io::Result<Option<(Vec<Extent>, Vec<(u64, u64)>)>> {
        let too_far = || io::Error::other("an extent is beyond what can be counted");
        let mut found = Vec::new();
        let mut covered: Vec<(u64, u64)> = Vec::new();
        let mut vcn = 0i64;
        loop {
            let mut pointers = Box::new(Pointers {
                extent_count: 0,
                starting_vcn: 0,
                runs: [Run::default(); BATCH],
            });
            let mut returned = 0u32;
            // SAFETY: the input is a STARTING_VCN_INPUT_BUFFER, the output a
            // RETRIEVAL_POINTERS_BUFFER of the size given, both live for the
            // call, and it isn't overlapped.
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    (&raw const vcn).cast(),
                    size_of::<i64>() as u32,
                    (&raw mut *pointers).cast(),
                    size_of::<Pointers>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            let more = if ok != 0 {
                false
            } else {
                let error = io::Error::last_os_error();
                match error.raw_os_error().map(|code| code as u32) {
                    Some(ERROR_MORE_DATA) => true,
                    Some(ERROR_HANDLE_EOF) if vcn == 0 => return Ok(None),
                    Some(ERROR_HANDLE_EOF) => return Ok(Some((found, covered))),
                    _ => return Err(error),
                }
            };
            let count = (pointers.extent_count as usize).min(BATCH);
            let mut from = pointers.starting_vcn;
            for run in &pointers.runs[..count] {
                let clusters = run.next_vcn.checked_sub(from).filter(|&n| n > 0);
                let clusters = clusters.ok_or_else(|| io::Error::other("a run is empty"))?;
                let bytes = |vcn: i64| (vcn as u64).checked_mul(cluster).ok_or_else(too_far);
                // -1 is a hole in a sparse file, which takes nothing.
                if run.lcn >= 0 {
                    found.push(Extent {
                        physical: bytes(run.lcn)?,
                        length: bytes(clusters)?,
                        flags: 0,
                    });
                    let (start, end) = (bytes(from)?, bytes(run.next_vcn)?);
                    match covered.last_mut() {
                        Some(last) if last.1 == start => last.1 = end,
                        _ => covered.push((start, end)),
                    }
                }
                from = run.next_vcn;
            }
            if !more {
                return Ok(Some((found, covered)));
            }
            if from <= vcn {
                return Err(io::Error::other("the runs didn't advance"));
            }
            vcn = from;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::covers;

        #[test]
        fn data_must_be_in_clusters_the_map_has() {
            let map = [(0, 8192), (16384, 20480)];
            assert!(covers(&map, &[], 4096));
            assert!(covers(&map, &[(0, 100), (16384, 16385)], 4096));
            // Partial clusters count as theirs.
            assert!(covers(&map, &[(1, 8191)], 4096));
            assert!(covers(&map, &[(5, 5)], 4096));
            assert!(!covers(&map, &[(0, 8193)], 4096));
            assert!(!covers(&map, &[(12288, 12289)], 4096));
            assert!(!covers(&map, &[(16384, 24576)], 4096));
            assert!(!covers(&[], &[(0, 1)], 4096));
            assert!(covers(&[(0, 4096), (4096, 8192)], &[(0, 8192)], 4096));
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::{Extent, Volume};
    use std::path::Path;

    pub fn map(_path: &Path) -> Result<(Volume, Vec<Extent>), String> {
        Err(format!(
            "unique physical usage isn't implemented on {} yet",
            match std::env::consts::OS {
                "macos" => "macOS",
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
        extents.insert(Volume::Windows(r"\\?\Volume{a}\".into()), vec![(0, 4096)]);
        extents.insert(Volume::Windows(r"\\?\Volume{b}\".into()), vec![(0, 4096)]);
        assert_eq!(unique_bytes(extents), 20480);
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
        // Elsewhere nothing is mapped yet, so the reason is the platform.
        if cfg!(any(target_os = "linux", windows)) {
            assert!(error.contains("missing.dsk"), "{error}");
        } else {
            assert!(error.contains("isn't implemented"), "{error}");
        }
        assert_eq!(unique_physical([]), Ok(0));
    }
}
