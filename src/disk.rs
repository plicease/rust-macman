//! Disk-image safety (design §8): format checks, advisory locks, and the
//! session records used to spot abnormal exits.

use crate::fsutil::{atomic_copy, list_dir, read_toml, sha256_bytes, write_toml};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    RawMfs,
    RawHfs,
    DiskCopy42,
}

/// One entry of an Apple partition map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// 1-based position in the map.
    pub index: u32,
    pub kind: String,
    pub offset: u64,
    pub length: u64,
    /// The classic volume inside, if any.
    pub volume: Option<DiskFormat>,
}

/// What an image turned out to be. See `docs/DISK_FORMATS.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Mini vMac can attach this as it stands.
    Supported(DiskFormat),
    /// A whole-disk image, which Mini vMac cannot mount as it stands.
    Partitioned(Vec<Partition>),
    /// Block-aligned with nothing in it; the guest can initialize it.
    Blank,
    /// Known, and known not to work. The string says what it is.
    Unsupported(String),
    /// Not something macman can identify; it may or may not work.
    Unrecognized,
}

/// The bytes of an image to store as a system volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub offset: u64,
    pub length: u64,
    /// How the volume was found, when it isn't the whole file.
    pub extracted: Option<String>,
}

const BLOCK: u64 = 512;
const MDB_OFFSET: u64 = 1024;
const DC42_HEADER: u64 = 84;
const MACBINARY_HEADER: usize = 128;
/// Far beyond any real map; a guard against corrupt counts.
const MAX_PARTITIONS: u32 = 1024;

fn read_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)?;
    Ok(())
}

fn be16(bytes: &[u8]) -> u64 {
    u64::from(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The classic volume starting at `offset`, if there is one.
fn volume_at(file: &mut File, offset: u64, size: u64) -> Result<Option<DiskFormat>> {
    if offset + MDB_OFFSET + 2 > size {
        return Ok(None);
    }
    let mut signature = [0u8; 2];
    read_at(file, offset + MDB_OFFSET, &mut signature)?;
    Ok(match &signature {
        [0xD2, 0xD7] => Some(DiskFormat::RawMfs),
        b"BD" => Some(DiskFormat::RawHfs),
        _ => None,
    })
}

/// The data range of a Disk Copy 4.2 image, by Mini vMac's own rules
/// (`SONYEMDV.c`), so that macman and the emulator agree on what counts.
fn disk_copy_data(header: &[u8], size: u64) -> Option<(u64, u64)> {
    if header.len() < DC42_HEADER as usize || header[82..84] != [0x01, 0x00] {
        return None;
    }
    let data_size = be32(&header[64..]);
    let tag_size = be32(&header[68..]);
    let blocks = data_size / BLOCK;
    let valid = size >= DC42_HEADER + data_size + tag_size
        && data_size.is_multiple_of(BLOCK)
        && blocks >= 4
        && header[0] < 64
        && (tag_size == 0 || tag_size == blocks * 12);
    valid.then_some((DC42_HEADER, data_size))
}

fn partition_map(file: &mut File, block_size: u64, size: u64) -> Result<Option<Vec<Partition>>> {
    let mut partitions = Vec::new();
    let mut count = 1;
    let mut index = 0;
    while index < count {
        let mut entry = [0u8; 512];
        let at = block_size * (u64::from(index) + 1);
        if at + BLOCK > size {
            return Ok(None);
        }
        read_at(file, at, &mut entry)?;
        if &entry[..2] != b"PM" {
            return Ok(None);
        }
        count = u32::try_from(be32(&entry[4..])).unwrap_or(u32::MAX);
        if count == 0 || count > MAX_PARTITIONS {
            return Ok(None);
        }
        let kind = &entry[48..80];
        let kind = &kind[..kind.iter().position(|b| *b == 0).unwrap_or(kind.len())];
        let offset = be32(&entry[8..]) * block_size;
        let length = be32(&entry[12..]) * block_size;
        let volume = if offset + length <= size {
            volume_at(file, offset, size)?
        } else {
            None
        };
        index += 1;
        partitions.push(Partition {
            index,
            kind: String::from_utf8_lossy(kind).into_owned(),
            offset,
            length,
            volume,
        });
    }
    Ok(Some(partitions))
}

/// Identify an image by content, because names like `.dsk`, `.img`, and
/// `.image` promise nothing about what's inside.
pub fn classify(path: &Path) -> Result<Classification> {
    use Classification::{Blank, Partitioned, Supported, Unrecognized, Unsupported};

    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    let mut head = vec![0u8; size.min(4096) as usize];
    read_at(&mut file, 0, &mut head)?;

    // Mini vMac tests for Disk Copy 4.2 first, so this has to as well.
    if disk_copy_data(&head, size).is_some() {
        return Ok(if head[DC42_HEADER as usize..].starts_with(b"ER") {
            Unsupported("a Disk Copy image holding a partitioned disk".into())
        } else {
            Supported(DiskFormat::DiskCopy42)
        });
    }

    let foreign = [
        (&b"\x1f\x8b"[..], "a gzip archive"),
        (b"BZh", "a bzip2 archive"),
        (b"PK\x03\x04", "a zip archive"),
        (b"SIT!", "a StuffIt archive"),
        (b"StuffIt", "a StuffIt archive"),
        (b"(This file must be converted", "a BinHex file"),
    ];
    if let Some((_, what)) = foreign.iter().find(|(magic, _)| head.starts_with(magic)) {
        return Ok(Unsupported((*what).to_string()));
    }
    if size >= BLOCK {
        let mut trailer = [0u8; 4];
        read_at(&mut file, size - BLOCK, &mut trailer)?;
        if &trailer == b"koly" {
            return Ok(Unsupported("a modern disk image (UDIF .dmg)".into()));
        }
    }
    if head.len() >= MACBINARY_HEADER
        && head[0] == 0
        && (1..64).contains(&head[1])
        && head[74] == 0
        && head[82] == 0
    {
        return Ok(Unsupported("a MacBinary-wrapped file".into()));
    }

    if !size.is_multiple_of(BLOCK) || size < MDB_OFFSET + 2 {
        return Ok(Unrecognized);
    }
    if head.starts_with(b"ER") {
        let block_size = be16(&head[2..]);
        if block_size >= BLOCK
            && block_size.is_multiple_of(BLOCK)
            && let Some(partitions) = partition_map(&mut file, block_size, size)?
        {
            return Ok(Partitioned(partitions));
        }
    }
    if head.get(512..514) == Some(b"TS") {
        return Ok(Unsupported("a disk with an old-style partition map".into()));
    }
    if let Some(format) = volume_at(&mut file, 0, size)? {
        return Ok(Supported(format));
    }
    match head.get(MDB_OFFSET as usize..MDB_OFFSET as usize + 2) {
        Some(b"H+" | b"HX") => Ok(Unsupported(
            "an HFS+ volume, which needs Mac OS 8.1 or later".into(),
        )),
        _ if head.iter().all(|byte| *byte == 0) => Ok(Blank),
        _ => Ok(Unrecognized),
    }
}

const ACCEPTED: &str = "macman accepts raw MFS/HFS volumes and Disk Copy 4.2 images";

fn partitioned_hint(path: &Path, partitions: &[Partition]) -> String {
    format!(
        "{} is a partitioned disk, which Mini vMac cannot mount; use `macman system add` to \
         extract its volume ({} partitions)",
        path.display(),
        partitions.len()
    )
}

/// The format of a usable volume image; everything else is an error.
pub fn detect_format(path: &Path) -> Result<DiskFormat> {
    match classify(path)? {
        Classification::Supported(format) => Ok(format),
        Classification::Partitioned(partitions) => bail!(partitioned_hint(path, &partitions)),
        Classification::Blank => bail!(
            "{} is blank, so it has no volume to use here; {ACCEPTED}",
            path.display()
        ),
        Classification::Unsupported(what) => {
            bail!(
                "{} is {what}, not a supported disk image; {ACCEPTED}",
                path.display()
            )
        }
        Classification::Unrecognized => bail!(
            "macman does not recognize the format of {}; {ACCEPTED}, and does not convert anything else",
            path.display()
        ),
    }
}

/// Check a disk attached for one run, which needn't be a bootable volume.
pub fn check_attachable(path: &Path, any_format: bool, writable: bool) -> Result<()> {
    match classify(path)? {
        Classification::Blank => {
            eprintln!(
                "note: {} is blank; the guest will offer to initialize it",
                path.display()
            );
        }
        Classification::Unrecognized if any_format => {
            eprintln!(
                "warning: attaching {} in an unrecognized format; the guest may offer to \
                 erase what it cannot read",
                path.display()
            );
        }
        Classification::Unrecognized => bail!(
            "macman does not recognize the format of {}; {ACCEPTED}. If you know Mini vMac can \
             read it, pass --any-format, ideally with --readonly",
            path.display()
        ),
        _ => {
            if detect_format(path)? == DiskFormat::DiskCopy42 && writable {
                warn_disk_copy_read_only(path);
            }
        }
    }
    Ok(())
}

/// Mini vMac's default build revokes write access to Disk Copy images, as it
/// won't keep their checksums current.
pub fn warn_disk_copy_read_only(path: &Path) {
    eprintln!(
        "note: {} is a Disk Copy 4.2 image, which Mini vMac attaches read-only; the guest \
         cannot change it",
        path.display()
    );
}

/// Work out which bytes of `path` to store as a system volume.
pub fn plan_import(path: &Path, partition: Option<u32>) -> Result<Import> {
    let whole = || -> Result<Import> {
        Ok(Import {
            offset: 0,
            length: fs::metadata(path)?.len(),
            extracted: None,
        })
    };
    match classify(path)? {
        Classification::Partitioned(partitions) => choose_partition(path, &partitions, partition),
        _ if partition.is_some() => {
            detect_format(path)?;
            bail!(
                "{} has no partition map, so --partition does not apply",
                path.display()
            )
        }
        Classification::Supported(DiskFormat::DiskCopy42) => {
            let mut head = [0u8; DC42_HEADER as usize];
            read_at(&mut File::open(path)?, 0, &mut head)?;
            let size = fs::metadata(path)?.len();
            let (offset, length) =
                disk_copy_data(&head, size).context("Disk Copy image changed while reading")?;
            Ok(Import {
                offset,
                length,
                extracted: Some("Disk Copy 4.2 image".into()),
            })
        }
        _ => {
            detect_format(path)?;
            whole()
        }
    }
}

fn choose_partition(path: &Path, partitions: &[Partition], chosen: Option<u32>) -> Result<Import> {
    let volumes: Vec<_> = partitions.iter().filter(|p| p.volume.is_some()).collect();
    let listing = || {
        partitions
            .iter()
            .map(|p| format!("{}: {} ({}K)", p.index, p.kind, p.length / 1024))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let partition = match (chosen, volumes.as_slice()) {
        (Some(index), _) => {
            let Some(partition) = volumes.iter().find(|p| p.index == index) else {
                bail!(
                    "partition {index} of {} holds no MFS or HFS volume; partitions are {}",
                    path.display(),
                    listing()
                );
            };
            partition
        }
        (None, [only]) => only,
        (None, []) => bail!(
            "{} has no MFS or HFS volume to extract; partitions are {}",
            path.display(),
            listing()
        ),
        (None, _) => bail!(
            "{} has several volumes; pick one with --partition N. Partitions are {}",
            path.display(),
            listing()
        ),
    };
    Ok(Import {
        offset: partition.offset,
        length: partition.length,
        extracted: Some(format!(
            "partition {} ({})",
            partition.index, partition.kind
        )),
    })
}

/// Whether the volume has boot blocks, without which it cannot start up.
pub fn has_boot_blocks(path: &Path) -> Result<bool> {
    let mut signature = [0u8; 2];
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    read_at(&mut file, 0, &mut signature)?;
    Ok(&signature == b"LK")
}

/// An exclusive or shared advisory lock on a disk image, held until dropped.
///
/// Locks live in a sidecar directory, keyed by file identity rather than path,
/// so symlinks and hard links to one image contend for the same lock.
#[derive(Debug)]
pub struct ImageLock {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// The image may be written; no one else may touch it.
    Exclusive,
    /// The image is only being read (say, copied); writers are kept out.
    Shared,
}

pub fn lock_image(paths: &Paths, image: &Path, mode: LockMode) -> Result<ImageLock> {
    let key = file_identity(image)?;
    fs::create_dir_all(paths.locks()).context("cannot create lock directory")?;
    let lock_path = paths.locks().join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open lock {}", lock_path.display()))?;
    let result = match mode {
        LockMode::Exclusive => file.try_lock(),
        LockMode::Shared => file.try_lock_shared(),
    };
    match result {
        Ok(()) => Ok(ImageLock { _file: file }),
        Err(TryLockError::WouldBlock) => bail!(
            "{} is in use by another macman process or machine; close that emulator first, \
             or work on an explicit copy",
            image.display()
        ),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", image.display()))
        }
    }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(format!("{:x}-{:x}", meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(sha256_bytes(path.to_string_lossy().as_bytes()))
}

/// Metadata for one `run`. A clean shutdown deletes it, so any record whose
/// lock is free describes a run that ended abnormally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub machine: String,
    pub pid: u32,
    /// How the run ended, when macman lived to see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Writable disks the session had attached.
    pub writable_disks: Vec<PathBuf>,
}

impl SessionRecord {
    /// Why this run counts as abnormal.
    pub fn describe(&self) -> &str {
        self.outcome
            .as_deref()
            .unwrap_or("macman stopped before the emulator did")
    }
}

/// A running session. Dropping it without [`Session::finish`] leaves the
/// record behind, as a killed macman would.
#[derive(Debug)]
pub struct Session {
    pub dir: PathBuf,
    _lock: File,
}

const SESSION_LOCK: &str = "session.lock";
const SESSION_RECORD: &str = "session.toml";

impl Session {
    pub fn start(paths: &Paths, machine: &str) -> Result<Self> {
        let unique =
            sha256_bytes(format!("{machine}-{:?}", std::time::SystemTime::now()).as_bytes());
        let dir =
            paths
                .sessions()
                .join(format!("{machine}-{}-{}", std::process::id(), &unique[..8]));
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create session directory {}", dir.display()))?;
        // Lock before any record exists: a sweeper skips recordless sessions,
        // and treats a recorded session as dead only if this lock is free.
        let lock = File::create(dir.join(SESSION_LOCK)).context("cannot create session lock")?;
        lock.try_lock().context("cannot lock new session")?;
        Ok(Self { dir, _lock: lock })
    }

    pub fn record(&self, record: &SessionRecord) -> Result<()> {
        write_toml(&self.dir.join(SESSION_RECORD), record)
    }

    /// Copy `source` into this session and make the copy read-only, so the
    /// emulator can only open it read-only and never holds the source at all.
    pub fn throwaway_copy(&self, paths: &Paths, source: &Path, index: usize) -> Result<PathBuf> {
        let _guard = lock_image(paths, source, LockMode::Shared)?;
        let name = source
            .file_name()
            .context("disk path has no file name")?
            .to_string_lossy();
        let copy = self.dir.join(format!("{index}-{name}"));
        atomic_copy(source, &copy)?;
        let mut perms = fs::metadata(&copy)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&copy, perms)?;
        Ok(copy)
    }

    /// Record a clean shutdown and discard any throwaway copies.
    pub fn finish(self) -> Result<()> {
        fs::remove_dir_all(&self.dir)
            .with_context(|| format!("cannot remove session {}", self.dir.display()))
    }

    /// End the session abnormally, leaving `record` for `doctor` and the next
    /// `run`. If it can't be saved, the record from startup still marks the
    /// session as abnormal, so this only warns.
    pub fn abandon(self, record: &SessionRecord) {
        if let Err(error) = self.record(record) {
            eprintln!("warning: cannot update session record: {error:#}");
        }
    }
}

/// A recorded session whose owner is gone.
#[derive(Debug)]
pub struct StaleSession {
    pub dir: PathBuf,
    pub record: SessionRecord,
}

impl StaleSession {
    /// Drop the record and its throwaway copies. User disks live elsewhere
    /// and are untouched.
    pub fn clear(self) -> Result<()> {
        fs::remove_dir_all(&self.dir)
            .with_context(|| format!("cannot remove session {}", self.dir.display()))
    }
}

pub fn stale_sessions(paths: &Paths) -> Result<Vec<StaleSession>> {
    let mut stale = Vec::new();
    for name in list_dir(&paths.sessions())? {
        let dir = paths.sessions().join(name);
        let record_path = dir.join(SESSION_RECORD);
        if !record_path.exists() {
            continue;
        }
        let Ok(lock) = File::open(dir.join(SESSION_LOCK)) else {
            continue;
        };
        if lock.try_lock().is_ok() {
            stale.push(StaleSession {
                record: read_toml(&record_path)?,
                dir,
            });
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_image(signature: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 800 * 1024];
        bytes[1024..1026].copy_from_slice(signature);
        bytes
    }

    /// An Iomega-style disk: map, driver, then one HFS partition.
    fn partitioned_image(volumes: &[(u32, &[u8])]) -> Vec<u8> {
        let blocks = 4096u32;
        let mut bytes = vec![0u8; blocks as usize * 512];
        bytes[..2].copy_from_slice(b"ER");
        bytes[2..4].copy_from_slice(&512u16.to_be_bytes());
        let mut entry = |index: usize, count: usize, start: u32, length: u32, kind: &str| {
            let at = (index + 1) * 512;
            bytes[at..at + 2].copy_from_slice(b"PM");
            bytes[at + 4..at + 8].copy_from_slice(&(count as u32).to_be_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&start.to_be_bytes());
            bytes[at + 12..at + 16].copy_from_slice(&length.to_be_bytes());
            bytes[at + 48..at + 48 + kind.len()].copy_from_slice(kind.as_bytes());
        };
        let count = volumes.len() + 2;
        entry(0, count, 1, 63, "Apple_partition_map");
        entry(1, count, 64, 32, "Apple_Driver");
        for (i, (start, _)) in volumes.iter().enumerate() {
            entry(i + 2, count, *start, 512, "Apple_HFS");
        }
        for (start, signature) in volumes {
            let at = *start as usize * 512;
            bytes[at..at + 2].copy_from_slice(b"LK");
            bytes[at + 1024..at + 1026].copy_from_slice(signature);
        }
        bytes
    }

    fn dc42(data_blocks: u32, tags: u32) -> Vec<u8> {
        let data = data_blocks * 512;
        let mut bytes = vec![0u8; 84 + data as usize + tags as usize];
        bytes[64..68].copy_from_slice(&data.to_be_bytes());
        bytes[68..72].copy_from_slice(&tags.to_be_bytes());
        bytes[82] = 1;
        if data_blocks >= 4 {
            bytes[84 + 1024..84 + 1026].copy_from_slice(b"BD");
        }
        bytes
    }

    fn classified(bytes: &[u8]) -> Classification {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image");
        fs::write(&file, bytes).unwrap();
        classify(&file).unwrap()
    }

    #[test]
    fn detects_supported_formats() {
        use Classification::Supported;
        assert_eq!(classified(&raw_image(b"BD")), Supported(DiskFormat::RawHfs));
        assert_eq!(
            classified(&raw_image(&[0xD2, 0xD7])),
            Supported(DiskFormat::RawMfs)
        );
        assert_eq!(
            classified(&dc42(1600, 0)),
            Supported(DiskFormat::DiskCopy42)
        );
        assert_eq!(
            classified(&dc42(1600, 1600 * 12)),
            Supported(DiskFormat::DiskCopy42)
        );
    }

    #[test]
    fn disk_copy_rules_match_mini_vmac() {
        // Each of these falls through to raw handling in the emulator.
        let bad_tags = dc42(1600, 5);
        let mut unaligned = dc42(1600, 0);
        unaligned[64..68].copy_from_slice(&1000u32.to_be_bytes());
        let mut long_name = dc42(1600, 0);
        long_name[0] = 64;
        let too_small = dc42(3, 0);
        for image in [bad_tags, unaligned, long_name, too_small] {
            assert!(!matches!(
                classified(&image),
                Classification::Supported(DiskFormat::DiskCopy42)
            ));
        }
        // Trailing bytes are tolerated, as they are by the emulator.
        let mut trailing = dc42(1600, 0);
        trailing.extend_from_slice(&[0u8; 100]);
        assert_eq!(
            classified(&trailing),
            Classification::Supported(DiskFormat::DiskCopy42)
        );
    }

    #[test]
    fn sorts_unusable_images_by_what_is_known_about_them() {
        let unsupported = |bytes: &[u8], expected: &str| match classified(bytes) {
            Classification::Unsupported(what) => assert!(what.contains(expected), "{what}"),
            other => panic!("{expected}: {other:?}"),
        };
        let with_prefix = |magic: &[u8]| {
            let mut bytes = vec![0u8; 8192];
            bytes[..magic.len()].copy_from_slice(magic);
            bytes
        };
        unsupported(&raw_image(b"H+"), "HFS+");
        unsupported(&raw_image(b"HX"), "HFS+");
        unsupported(&with_prefix(b"\x1f\x8b\x08"), "gzip");
        unsupported(&with_prefix(b"PK\x03\x04"), "zip");
        unsupported(&with_prefix(b"StuffIt (c)"), "StuffIt");
        unsupported(
            &with_prefix(b"(This file must be converted with BinHex"),
            "BinHex",
        );
        let mut macbinary = vec![0u8; 8192];
        macbinary[1] = 8;
        macbinary[2..10].copy_from_slice(b"Disk.img");
        unsupported(&macbinary, "MacBinary");
        let mut udif = raw_image(b"\0\0");
        let at = udif.len() - 512;
        udif[0] = 1;
        udif[at..at + 4].copy_from_slice(b"koly");
        unsupported(&udif, "UDIF");
        let mut old_map = raw_image(b"\0\0");
        old_map[0] = 1;
        old_map[512..514].copy_from_slice(b"TS");
        unsupported(&old_map, "old-style");
        unsupported(
            &{
                let mut wrapped = dc42(1600, 0);
                wrapped[84..86].copy_from_slice(b"ER");
                wrapped
            },
            "partitioned",
        );

        assert_eq!(classified(&vec![0u8; 4096]), Classification::Blank);
        assert_eq!(classified(&raw_image(b"\0\0")), Classification::Blank);
        assert_eq!(
            classified(&raw_image(b"BD")[..2000]),
            Classification::Unrecognized
        );
        let mut noise = raw_image(b"??");
        noise[0] = 1;
        assert_eq!(classified(&noise), Classification::Unrecognized);
        assert_eq!(classified(b"tiny"), Classification::Unrecognized);
    }

    #[test]
    fn extension_does_not_matter() {
        let dir = tempfile::tempdir().unwrap();
        let image = partitioned_image(&[(96, b"BD")]);
        for name in ["a.image", "a.dsk", "a.img", "a.hda"] {
            let file = dir.path().join(name);
            fs::write(&file, &image).unwrap();
            assert!(matches!(
                classify(&file).unwrap(),
                Classification::Partitioned(_)
            ));
        }
    }

    #[test]
    fn reads_partition_maps() {
        let Classification::Partitioned(partitions) =
            classified(&partitioned_image(&[(96, b"BD")]))
        else {
            panic!("expected a partitioned image");
        };
        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions[1].volume, None);
        assert_eq!(partitions[2].kind, "Apple_HFS");
        assert_eq!(partitions[2].index, 3);
        assert_eq!(partitions[2].offset, 96 * 512);
        assert_eq!(partitions[2].length, 512 * 512);
        assert_eq!(partitions[2].volume, Some(DiskFormat::RawHfs));

        // A corrupt map isn't believed.
        let mut corrupt = partitioned_image(&[(96, b"BD")]);
        corrupt[512 + 4..512 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(classified(&corrupt), Classification::Unrecognized);
    }

    #[test]
    fn plans_imports() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image");
        let plan = |bytes: &[u8], partition| {
            fs::write(&file, bytes).unwrap();
            plan_import(&file, partition)
        };

        let raw = plan(&raw_image(b"BD"), None).unwrap();
        assert_eq!((raw.offset, raw.extracted), (0, None));

        let unwrapped = plan(&dc42(1600, 0), None).unwrap();
        assert_eq!((unwrapped.offset, unwrapped.length), (84, 1600 * 512));

        let single = plan(&partitioned_image(&[(96, b"BD")]), None).unwrap();
        assert_eq!((single.offset, single.length), (96 * 512, 512 * 512));
        assert_eq!(single.extracted.unwrap(), "partition 3 (Apple_HFS)");

        let two = partitioned_image(&[(96, b"BD"), (1024, b"BD")]);
        let err = plan(&two, None).unwrap_err().to_string();
        assert!(err.contains("--partition"), "{err}");
        assert_eq!(plan(&two, Some(4)).unwrap().offset, 1024 * 512);
        let err = plan(&two, Some(2)).unwrap_err().to_string();
        assert!(err.contains("holds no MFS or HFS"), "{err}");

        let err = plan(&partitioned_image(&[]), None).unwrap_err().to_string();
        assert!(err.contains("no MFS or HFS volume"), "{err}");
        let err = plan(&raw_image(b"BD"), Some(1)).unwrap_err().to_string();
        assert!(err.contains("no partition map"), "{err}");
        assert!(plan(&vec![0u8; 4096], None).is_err());
    }

    #[test]
    fn partitioned_images_cannot_be_attached_directly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("zip.image");
        fs::write(&file, partitioned_image(&[(96, b"BD")])).unwrap();
        let err = detect_format(&file).unwrap_err().to_string();
        assert!(err.contains("system add"), "{err}");
        assert!(check_attachable(&file, true, true).is_err());
    }

    #[test]
    fn only_unrecognized_images_can_be_forced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image");

        let mut noise = raw_image(b"??");
        noise[0] = 1;
        fs::write(&file, noise).unwrap();
        assert!(check_attachable(&file, false, true).is_err());
        check_attachable(&file, true, true).unwrap();

        fs::write(&file, b"PK\x03\x04-------------------------").unwrap();
        assert!(check_attachable(&file, true, true).is_err());

        fs::write(&file, vec![0u8; 8192]).unwrap();
        check_attachable(&file, false, true).unwrap();
        assert!(detect_format(&file).is_err());
    }

    #[test]
    fn locks_follow_the_file_through_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let image = dir.path().join("a.dsk");
        fs::write(&image, b"x").unwrap();
        let symlink = dir.path().join("sym.dsk");
        let hardlink = dir.path().join("hard.dsk");
        std::os::unix::fs::symlink(&image, &symlink).unwrap();
        fs::hard_link(&image, &hardlink).unwrap();

        let held = lock_image(&paths, &image, LockMode::Exclusive).unwrap();
        for alias in [&image, &symlink, &hardlink] {
            assert!(lock_image(&paths, alias, LockMode::Exclusive).is_err());
            assert!(lock_image(&paths, alias, LockMode::Shared).is_err());
        }
        drop(held);

        let reader = lock_image(&paths, &image, LockMode::Shared).unwrap();
        let _second_reader = lock_image(&paths, &symlink, LockMode::Shared).unwrap();
        assert!(lock_image(&paths, &image, LockMode::Exclusive).is_err());
        drop(reader);
    }

    #[test]
    fn sessions_only_go_stale_when_their_owner_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let record = SessionRecord {
            machine: "m".into(),
            pid: 1,
            outcome: None,
            writable_disks: vec![],
        };

        let live = Session::start(&paths, "m").unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty()); // no record yet
        live.record(&record).unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty()); // record, but locked

        drop(live); // an abnormal exit: no finish()
        let stale = stale_sessions(&paths).unwrap();
        assert_eq!(stale.len(), 1);
        stale.into_iter().next().unwrap().clear().unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty());

        let clean = Session::start(&paths, "m").unwrap();
        clean.record(&record).unwrap();
        clean.finish().unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty());
    }

    #[test]
    fn throwaway_copy_is_read_only_and_separate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = dir.path().join("app.dsk");
        fs::write(&source, b"data").unwrap();

        let session = Session::start(&paths, "m").unwrap();
        let copy = session.throwaway_copy(&paths, &source, 0).unwrap();
        assert_ne!(copy, source);
        assert!(fs::metadata(&copy).unwrap().permissions().readonly());
        assert!(!fs::metadata(&source).unwrap().permissions().readonly());
        session.finish().unwrap();
        assert!(!copy.exists() && source.exists());
    }
}
