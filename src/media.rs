//! Registered media: installation CDs, floppies, and other images a machine
//! boots or reads from for a while, and doesn't keep.
//!
//! Unlike a system image, which `system add` takes the volume out of and a
//! machine clones, a medium is stored whole, partition map and all, exactly as
//! it was given, and is never written: `run --media` attaches a CD read-only,
//! and anything else as a throwaway copy. Each is the image, `NAME.img`, plus a
//! TOML sidecar of what was found in it.
//!
//! The one exception is an HFS+ volume with nothing around it, as some Mac OS X
//! disc images are. Open Firmware starts only from a disc with a partition map,
//! so the stored copy has one in front of the volume, whose bytes are as given.

use crate::disk::{self, Classification, DiskFormat, LockMode};
use crate::fsutil::{
    atomic_copy_range_checked, hex_digest, list_dir, read_toml, sha256_file, validate_name,
    write_toml_new,
};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How a medium is attached, and so what the guest sees it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// A CD-ROM, attached read-only in the emulator's CD drive.
    Cdrom,
    /// A floppy, attached as a disk from a throwaway copy.
    Floppy,
    /// Any other disk image, attached the same way as a floppy.
    Disk,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cdrom => "cdrom",
            Self::Floppy => "floppy",
            Self::Disk => "disk",
        }
    }
}

/// What a medium's bytes were found to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaFormat {
    /// An HFS volume with nothing around it.
    Hfs,
    Mfs,
    DiskCopy42,
    /// An Apple partition map, such as most Mac CDs have.
    Partitioned,
    /// ISO 9660 alone, which a Mac reads but can't start from.
    Iso9660,
    /// Nothing macman knows, registered with `--any-format`.
    Unrecognized,
}

impl MediaFormat {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Hfs => "HFS volume",
            Self::Mfs => "MFS volume",
            Self::DiskCopy42 => "Disk Copy 4.2",
            Self::Partitioned => "partitioned",
            Self::Iso9660 => "ISO 9660",
            Self::Unrecognized => "unrecognized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub kind: MediaKind,
    /// Whether `kind` was given to `media add`, rather than found.
    #[serde(default, skip_serializing_if = "is_false")]
    pub kind_given: bool,
    pub format: MediaFormat,
    /// Whether some volume on it has boot blocks. That's what a Mac needs to
    /// start from it, but it doesn't promise any machine will.
    pub bootable: bool,
    /// The name of the file it was registered from, for reference only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What it holds, as given to `media add`; never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The digest of the file it was registered from, when that isn't the
    /// stored image: a bare HFS+ volume, stored with a partition map in front.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The largest floppy a Mac drive takes: a 1440K high-density disk.
const FLOPPY_MAX: u64 = 1440 * 1024;
/// A Disk Copy 4.2 image's header, before its data.
const DC42_HEADER: u64 = 84;

fn media_path(paths: &Paths, name: &str) -> PathBuf {
    paths.media().join(format!("{name}.img"))
}

fn sidecar(file: &Path) -> PathBuf {
    file.with_extension("toml")
}

/// Where a registered medium is stored.
pub fn media_file(paths: &Paths, name: &str) -> PathBuf {
    media_path(paths, name)
}

/// Whether anything at all occupies this media name.
pub fn media_exists(paths: &Paths, name: &str) -> bool {
    let file = media_path(paths, name);
    file.symlink_metadata().is_ok() || sidecar(&file).symlink_metadata().is_ok()
}

/// What `path` is, as a medium: its kind, its format, and whether it has boot
/// blocks. `kind`, if given, overrides the kind found, but a CD must still
/// hold something a CD drive can present.
pub fn inspect(
    path: &Path,
    kind: Option<MediaKind>,
    any_format: bool,
) -> Result<(MediaKind, MediaFormat, bool)> {
    let size = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    // Registered with a partition map in front, which is what's described.
    if let Some(blessed) = disk::bare_hfs_plus(path)? {
        if kind.is_some_and(|kind| kind != MediaKind::Cdrom) {
            bail!(
                "{} is a bare HFS+ volume, which macman registers only as a CD, with a partition \
                 map in front for Open Firmware; leave out --kind",
                path.display()
            );
        }
        return Ok((MediaKind::Cdrom, MediaFormat::Partitioned, blessed));
    }
    let (found, format, bootable) = match disk::classify(path)? {
        Classification::Supported(DiskFormat::DiskCopy42) => (
            MediaKind::Floppy,
            MediaFormat::DiskCopy42,
            boot_blocks_at(path, DC42_HEADER)?,
        ),
        Classification::Supported(format) => {
            let kind = if size <= FLOPPY_MAX {
                MediaKind::Floppy
            } else {
                MediaKind::Cdrom
            };
            let format = match format {
                DiskFormat::RawMfs => MediaFormat::Mfs,
                _ => MediaFormat::Hfs,
            };
            (kind, format, boot_blocks_at(path, 0)?)
        }
        Classification::Partitioned(partitions) => {
            let mut bootable = false;
            for partition in &partitions {
                if partition.volume == Some(DiskFormat::RawHfs) {
                    bootable |= boot_blocks_at(path, partition.offset)?;
                }
            }
            // HFS+ has no boot blocks: Open Firmware starts from its blessed folder.
            bootable |= disk::hfs_plus_in(path, &partitions)?.unwrap_or(false);
            // Media with a partition map are nearly always CDs; a hard disk
            // image among them says so with `--kind disk`. The driver map's
            // block size doesn't tell them apart: CDs say 512 as well as 2048.
            (MediaKind::Cdrom, MediaFormat::Partitioned, bootable)
        }
        // ISO 9660 leaves its first 32K empty, so it can look blank.
        _ if disk::is_iso9660(path)? => (MediaKind::Cdrom, MediaFormat::Iso9660, false),
        Classification::Blank => bail!(
            "{} is blank, so there's nothing on it to register; a machine starts with a blank \
             disk from `macman create --disk-size`",
            path.display()
        ),
        Classification::Unsupported(what) => bail!(
            "{} is {what}, which no emulator here can attach; unpack or convert it first",
            path.display()
        ),
        Classification::Unrecognized if any_format => (
            kind.unwrap_or(MediaKind::Disk),
            MediaFormat::Unrecognized,
            false,
        ),
        Classification::Unrecognized => bail!(
            "macman does not recognize the format of {}; if you know an emulator can use it, \
             register it with --any-format, and pick how it's attached with --kind",
            path.display()
        ),
    };
    let kind = kind.unwrap_or(found);
    if kind == MediaKind::Cdrom && !matches!(format, MediaFormat::Hfs | MediaFormat::Iso9660) {
        let holds_hfs = format == MediaFormat::Partitioned
            && matches!(
                disk::check_cd_image(path),
                Ok(disk::CdVolume::Hfs | disk::CdVolume::HfsPlus)
            );
        if !holds_hfs {
            bail!(
                "{} is {}, which can't be attached as a CD; a CD holds HFS, alone or in a \
                 partition map, HFS+, or ISO 9660",
                path.display(),
                format.describe()
            );
        }
    }
    Ok((kind, format, bootable))
}

fn boot_blocks_at(path: &Path, offset: u64) -> Result<bool> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut signature = [0u8; 2];
    file.seek(SeekFrom::Start(offset))?;
    Ok(file.read_exact(&mut signature).is_ok() && &signature == b"LK")
}

/// Options for [`add_media`].
#[derive(Debug, Default, Clone)]
pub struct AddOptions<'a> {
    pub kind: Option<MediaKind>,
    pub version: Option<&'a str>,
    pub any_format: bool,
}

/// Register a copy of the whole of `source`, which is left as it is.
pub fn add_media(paths: &Paths, source: &Path, name: &str, options: &AddOptions) -> Result<Media> {
    validate_name("media", name)?;
    if media_exists(paths, name) {
        bail!("media named {name:?} is already registered; pick another --name");
    }
    if let Some(version) = options.version
        && version.chars().any(char::is_control)
    {
        bail!("--version can't hold control characters");
    }
    // Held throughout, so what's inspected, copied, and hashed is one state of
    // the source, for cooperating processes.
    let _guard = disk::lock_image(paths, source, LockMode::Shared)?;
    let (kind, format, bootable) = inspect(source, options.kind, options.any_format)?;
    let wrapped = disk::bare_hfs_plus(source)?.is_some();
    let mut media = Media {
        name: name.to_string(),
        sha256: String::new(),
        size: fs::metadata(source)?.len(),
        kind,
        kind_given: options.kind.is_some(),
        format,
        bootable,
        source: source
            .file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .filter(|file| !file.chars().any(char::is_control)),
        version: options.version.map(str::to_string),
        source_sha256: None,
    };
    if !wrapped {
        media.sha256 = sha256_file(source)?;
        add_media_blob(paths, source, &media)?;
        return Ok(media);
    }
    // Built beside where it goes, under a name `media list` doesn't read.
    fs::create_dir_all(paths.media())?;
    let image = paths
        .media()
        .join(format!(".{name}.{}.wrapping", uuid::Uuid::new_v4()));
    let result = write_wrapped(source, &image).and_then(|(source_sha256, sha256)| {
        media.sha256 = sha256;
        media.size = fs::metadata(&image)?.len();
        media.source_sha256 = Some(source_sha256);
        add_media_blob(paths, &image, &media)
    });
    let _ = fs::remove_file(&image);
    result.map(|()| media)
}

/// 512-byte blocks before a wrapped volume: a driver descriptor, then a
/// partition map of 63 blocks, as on Apple's own CDs.
const WRAP_BLOCKS: u32 = 64;

fn partition_entry(
    count: u32,
    start: u32,
    blocks: u32,
    name: &[u8],
    kind: &[u8],
    status: u32,
) -> [u8; 512] {
    let mut entry = [0u8; 512];
    entry[..2].copy_from_slice(b"PM");
    entry[4..8].copy_from_slice(&count.to_be_bytes());
    entry[8..12].copy_from_slice(&start.to_be_bytes());
    entry[12..16].copy_from_slice(&blocks.to_be_bytes());
    entry[16..16 + name.len()].copy_from_slice(name);
    entry[48..48 + kind.len()].copy_from_slice(kind);
    // The data area: the whole partition.
    entry[84..88].copy_from_slice(&blocks.to_be_bytes());
    entry[88..92].copy_from_slice(&status.to_be_bytes());
    entry
}

/// Write `source`, a bare HFS+ volume, to `dest`, behind an Apple partition map
/// of the map itself and the volume, as `Apple_HFS`, which is what HFS+ is
/// called in a map. Returns the digests of `source` and of `dest`, from one
/// read of `source`.
fn write_wrapped(source: &Path, dest: &Path) -> Result<(String, String)> {
    let size = fs::metadata(source)?.len();
    let blocks = u32::try_from(size / 512)
        .ok()
        .filter(|blocks| blocks.checked_add(WRAP_BLOCKS).is_some())
        .with_context(|| format!("{} is too big for a partition map", source.display()))?;
    let mut head = vec![0u8; WRAP_BLOCKS as usize * 512];
    head[..2].copy_from_slice(b"ER");
    head[2..4].copy_from_slice(&512u16.to_be_bytes());
    head[4..8].copy_from_slice(&(WRAP_BLOCKS + blocks).to_be_bytes());
    // Valid, allocated, in use, readable: as Apple's tools mark a map.
    let map = partition_entry(2, 1, WRAP_BLOCKS - 1, b"Apple", b"Apple_partition_map", 0x3);
    // Also writable, and bootable, which Open Firmware looks for.
    let volume = partition_entry(2, WRAP_BLOCKS, blocks, b"Mac OS", b"Apple_HFS", 0x4000_0033);
    head[512..1024].copy_from_slice(&map);
    head[1024..1536].copy_from_slice(&volume);

    let mut input =
        File::open(source).with_context(|| format!("cannot read {}", source.display()))?;
    let file =
        File::create_new(dest).with_context(|| format!("cannot write {}", dest.display()))?;
    let mut output = BufWriter::new(file);
    let (mut source_hash, mut dest_hash) = (Sha256::new(), Sha256::new());
    dest_hash.update(&head);
    output.write_all(&head)?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", source.display()))?;
        if count == 0 {
            break;
        }
        source_hash.update(&buffer[..count]);
        dest_hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    output
        .into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()
        .with_context(|| format!("cannot write {}", dest.display()))?;
    Ok((hex_digest(source_hash), hex_digest(dest_hash)))
}

/// Register `media` from the bytes at `blob`, which must match its digest. The
/// image goes in, read-only, before its metadata, and is removed if the
/// metadata can't be written, so a stop in between leaves what
/// [`recover_media`] can finish.
pub fn add_media_blob(paths: &Paths, blob: &Path, media: &Media) -> Result<()> {
    fs::create_dir_all(paths.media())?;
    let dest = media_path(paths, &media.name);
    if sidecar(&dest).symlink_metadata().is_ok() {
        bail!("{} already exists", sidecar(&dest).display());
    }
    atomic_copy_range_checked(blob, 0, media.size, &dest, |copy| {
        if sha256_file(copy)? != media.sha256 {
            bail!("media {} failed its digest after copying", media.name);
        }
        set_readonly(copy, true)
    })?;
    if let Err(error) = write_toml_new(&sidecar(&dest), media) {
        let _ = remove_image(&dest);
        return Err(error);
    }
    Ok(())
}

fn set_readonly(path: &Path, readonly: bool) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms).with_context(|| format!("cannot protect {}", path.display()))
}

/// Windows won't delete a read-only file, so it's made writable first.
fn remove_image(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        let _ = set_readonly(path, false);
    }
    fs::remove_file(path)
}

pub fn list_media(paths: &Paths) -> Result<Vec<Media>> {
    let dir = paths.media();
    list_dir(&dir)?
        .iter()
        .filter(|name| name.ends_with(".toml"))
        .map(|name| read_toml(&dir.join(name)))
        .collect()
}

pub fn load_media(paths: &Paths, name: &str) -> Result<(Media, PathBuf)> {
    validate_name("media", name)?;
    let file = media_path(paths, name);
    if !file.exists() {
        bail!("no media named {name:?}; register it with `macman media add FILE --name {name}`");
    }
    Ok((read_toml(&sidecar(&file))?, file))
}

/// Delete a registered medium, refusing while a run has it. This takes either
/// file alone too, so it can finish a removal that was interrupted.
pub fn remove_media(paths: &Paths, name: &str) -> Result<()> {
    validate_name("media", name)?;
    if !media_exists(paths, name) {
        bail!("no media named {name:?}; see `macman media list`");
    }
    let file = media_path(paths, name);
    let _lock = file
        .exists()
        .then(|| disk::lock_image(paths, &file, LockMode::Exclusive))
        .transpose()?;
    // The sidecar goes first, as `media list` reads only sidecars.
    let metadata = sidecar(&file);
    match fs::remove_file(&metadata) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("cannot remove {}", metadata.display()));
        }
        _ => {}
    }
    match remove_image(&file) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("cannot remove {}", file.display()))
        }
        _ => Ok(()),
    }
}

/// How a media name here compares with `media`, from an archive. The local
/// image is read in full, so a damaged copy doesn't pass for the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMedia {
    Absent,
    /// Registered, with the same contents.
    Same,
    /// Only the image, with the same contents: a registration that stopped
    /// before its metadata, which [`recover_media`] can finish.
    Unfinished,
    /// Anything else: other contents, or metadata alone.
    Different,
}

pub fn local_media(paths: &Paths, media: &Media) -> Result<LocalMedia> {
    let file = media_path(paths, &media.name);
    if !media_exists(paths, &media.name) {
        return Ok(LocalMedia::Absent);
    }
    let is_plain_file = file
        .symlink_metadata()
        .is_ok_and(|found| found.file_type().is_file());
    if !is_plain_file {
        return Ok(LocalMedia::Different);
    }
    let same_bytes = sha256_file(&file)? == media.sha256;
    let metadata = sidecar(&file);
    if metadata.symlink_metadata().is_err() {
        return Ok(if same_bytes {
            LocalMedia::Unfinished
        } else {
            LocalMedia::Different
        });
    }
    let registered: Option<Media> = read_toml(&metadata).ok();
    Ok(match registered {
        Some(local) if same_bytes && local.sha256 == media.sha256 => LocalMedia::Same,
        _ => LocalMedia::Different,
    })
}

/// Finish a registration [`local_media`] found [`LocalMedia::Unfinished`].
/// Returns false if something else finished first.
pub fn recover_media(paths: &Paths, media: &Media) -> Result<bool> {
    let metadata = sidecar(&media_path(paths, &media.name));
    match write_toml_new(&metadata, media) {
        Ok(()) => Ok(true),
        Err(_) if metadata.symlink_metadata().is_ok() => Ok(false),
        Err(error) => Err(error),
    }
}

/// Check, cheaply, that the stored image is still the size registered. A
/// full digest is `media info`'s job, as a CD takes a while to read.
pub fn check_size(media: &Media, file: &Path) -> Result<()> {
    let size = fs::metadata(file)
        .with_context(|| format!("cannot read {}", file.display()))?
        .len();
    if size != media.size {
        bail!(
            "media {:?} changed since it was registered; remove it and register it again",
            media.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hfs(size: usize, boot: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; size];
        if boot {
            bytes[..2].copy_from_slice(b"LK");
        }
        bytes[1024..1026].copy_from_slice(b"BD");
        bytes
    }

    /// A CD-style image: a driver map saying 2048-byte blocks, and a 512-byte
    /// partition map with one bootable HFS partition at block 64.
    pub fn cd(blocks: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; blocks as usize * 512];
        bytes[..2].copy_from_slice(b"ER");
        bytes[2..4].copy_from_slice(&2048u16.to_be_bytes());
        let entries = [
            (1u32, 63u32, "Apple_partition_map"),
            (64, blocks - 64, "Apple_HFS"),
        ];
        for (index, (start, length, kind)) in entries.iter().enumerate() {
            let at = (index + 1) * 512;
            bytes[at..at + 2].copy_from_slice(b"PM");
            bytes[at + 4..at + 8].copy_from_slice(&(entries.len() as u32).to_be_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&start.to_be_bytes());
            bytes[at + 12..at + 16].copy_from_slice(&length.to_be_bytes());
            bytes[at + 48..at + 48 + kind.len()].copy_from_slice(kind.as_bytes());
        }
        let volume = 64 * 512;
        bytes[volume..volume + 2].copy_from_slice(b"LK");
        bytes[volume + 1024..volume + 1026].copy_from_slice(b"BD");
        bytes
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn kinds_and_formats_come_from_contents() {
        let dir = tempfile::tempdir().unwrap();
        let floppy = write(dir.path(), "a.image", &hfs(800 * 1024, true));
        assert_eq!(
            inspect(&floppy, None, false).unwrap(),
            (MediaKind::Floppy, MediaFormat::Hfs, true)
        );
        let toast = write(dir.path(), "b.toast", &hfs(4 * 1024 * 1024, false));
        assert_eq!(
            inspect(&toast, None, false).unwrap(),
            (MediaKind::Cdrom, MediaFormat::Hfs, false)
        );
        let cd = write(dir.path(), "c.dsk", &cd(4096));
        assert_eq!(
            inspect(&cd, None, false).unwrap(),
            (MediaKind::Cdrom, MediaFormat::Partitioned, true)
        );
        let mut iso = vec![0u8; 64 * 1024];
        iso[0x8001..0x8006].copy_from_slice(b"CD001");
        let iso = write(dir.path(), "d.iso", &iso);
        assert_eq!(
            inspect(&iso, None, false).unwrap(),
            (MediaKind::Cdrom, MediaFormat::Iso9660, false)
        );

        // The kind can be given, but a CD must still be one.
        assert_eq!(
            inspect(&toast, Some(MediaKind::Disk), false).unwrap().0,
            MediaKind::Disk
        );
        let mut odd = vec![0x5Au8; 4096];
        odd[0] = 1;
        let odd = write(dir.path(), "e.bin", &odd);
        assert!(inspect(&odd, None, false).is_err());
        assert_eq!(
            inspect(&odd, None, true).unwrap(),
            (MediaKind::Disk, MediaFormat::Unrecognized, false)
        );
        assert!(inspect(&odd, Some(MediaKind::Cdrom), true).is_err());
        assert!(inspect(&floppy, Some(MediaKind::Cdrom), false).is_ok());

        let blank = write(dir.path(), "f.img", &vec![0u8; 8192]);
        assert!(inspect(&blank, None, false).is_err());
        let gzip = write(dir.path(), "g.gz", b"\x1f\x8b\x08\0\0\0\0\0\0\0rest");
        assert!(inspect(&gzip, None, true).is_err());
    }

    #[test]
    fn a_registered_cd_is_a_read_only_copy_of_every_byte() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let bytes = cd(4096);
        let source = write(dir.path(), "install.toast", &bytes);
        let media = add_media(&paths, &source, "install", &AddOptions::default()).unwrap();
        assert_eq!(media.kind, MediaKind::Cdrom);
        assert_eq!(media.source.as_deref(), Some("install.toast"));
        assert_eq!(media.version, None, "never guessed");

        let (loaded, file) = load_media(&paths, "install").unwrap();
        assert_eq!(loaded, media);
        assert_eq!(fs::read(&file).unwrap(), bytes, "partition map included");
        assert_eq!(sha256_file(&file).unwrap(), sha256_file(&source).unwrap());
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        assert!(!fs::metadata(&source).unwrap().permissions().readonly());
        assert_eq!(fs::read(&source).unwrap(), bytes, "source untouched");

        let again = add_media(&paths, &source, "install", &AddOptions::default());
        assert!(
            again
                .unwrap_err()
                .to_string()
                .contains("already registered")
        );
        assert_eq!(list_media(&paths).unwrap(), vec![media.clone()]);
        assert_eq!(local_media(&paths, &media).unwrap(), LocalMedia::Same);

        remove_media(&paths, "install").unwrap();
        assert!(!media_exists(&paths, "install"));
        assert!(source.exists());
        assert!(remove_media(&paths, "install").is_err());
    }

    /// A bare HFS+ volume, as some Mac OS X disc images are, blessed or not.
    fn hfs_plus(size: usize, blessed: bool) -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        bytes[..1024].fill(0);
        bytes[1024..1026].copy_from_slice(b"H+");
        bytes[1024 + 80..1024 + 84].copy_from_slice(&u32::from(blessed).to_be_bytes());
        bytes
    }

    #[test]
    fn a_bare_hfs_plus_volume_is_registered_behind_a_partition_map() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let bytes = hfs_plus(4 * 1024 * 1024, true);
        let source = write(dir.path(), "Tiger.iso", &bytes);
        assert_eq!(
            inspect(&source, None, false).unwrap(),
            (MediaKind::Cdrom, MediaFormat::Partitioned, true)
        );
        let error = inspect(&source, Some(MediaKind::Disk), false).unwrap_err();
        assert!(error.to_string().contains("only as a CD"), "{error}");
        let error = disk::check_cd_image(&source).unwrap_err().to_string();
        assert!(error.contains("macman media add"), "{error}");

        let media = add_media(&paths, &source, "tiger", &AddOptions::default()).unwrap();
        assert_eq!(media.source_sha256, Some(sha256_file(&source).unwrap()));
        assert_eq!(fs::read(&source).unwrap(), bytes, "source untouched");
        let (loaded, file) = load_media(&paths, "tiger").unwrap();
        assert_eq!(loaded, media);
        assert_eq!(sha256_file(&file).unwrap(), media.sha256);
        assert_eq!(media.size, bytes.len() as u64 + 64 * 512);
        assert!(fs::metadata(&file).unwrap().permissions().readonly());

        // The volume, byte for byte, as the map's second partition.
        let stored = fs::read(&file).unwrap();
        assert_eq!(&stored[64 * 512..], &bytes[..]);
        match disk::classify(&file).unwrap() {
            Classification::Partitioned(partitions) => {
                let kinds: Vec<_> = partitions.iter().map(|p| p.kind.as_str()).collect();
                assert_eq!(kinds, ["Apple_partition_map", "Apple_HFS"]);
                assert_eq!(partitions[1].offset, 64 * 512);
                assert_eq!(partitions[1].length, bytes.len() as u64);
                assert!(partitions.iter().all(|p| p.in_bounds));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            disk::check_cd_image(&file).unwrap(),
            disk::CdVolume::HfsPlus
        );
        // Nothing's left behind but the image and its sidecar.
        assert_eq!(list_dir(&paths.media()).unwrap().len(), 2);
        assert_eq!(local_media(&paths, &media).unwrap(), LocalMedia::Same);

        // Registered again, the map is found and nothing is added.
        let again = add_media(&paths, &file, "again", &AddOptions::default()).unwrap();
        assert_eq!(again.source_sha256, None);
        assert_eq!(again.sha256, media.sha256);
        assert!(again.bootable);

        let unblessed = write(dir.path(), "data.iso", &hfs_plus(4 * 1024 * 1024, false));
        assert!(
            !add_media(&paths, &unblessed, "data", &AddOptions::default())
                .unwrap()
                .bootable
        );
    }

    #[test]
    fn a_medium_in_use_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = write(dir.path(), "disk1.image", &hfs(400 * 1024, true));
        add_media(&paths, &source, "disk1", &AddOptions::default()).unwrap();
        let file = media_file(&paths, "disk1");
        let held = disk::lock_image(&paths, &file, LockMode::Shared).unwrap();
        assert!(remove_media(&paths, "disk1").is_err());
        assert!(media_exists(&paths, "disk1"));
        drop(held);
        remove_media(&paths, "disk1").unwrap();
    }
}
