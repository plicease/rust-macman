//! Portable machine archives: `macman export` and `macman import`.
//!
//! An archive is a gzipped tar holding `manifest.toml`, first, followed by
//! `blobs/<sha256>` for each distinct ROM, system image, medium, or disk. Addressing blobs by content
//! dedupes them, and leaves exactly two valid forms of entry name. No name,
//! path, or permission from an archive is ever used to reach the filesystem
//! without validation. See `docs/EXPORT_IMPORT.md`.

use crate::backend::Backends;
use crate::disk::{self, ImageLock, LockMode};
use crate::fsutil::{
    HeldLock, atomic_copy_range_checked, hex_digest, list_dir, publish_new, sha256_file,
    temp_sibling, validate_name, windows_can_name,
};
use crate::machine::{
    Basilisk2Settings, Disk, Display, Machine, SCHEMA_VERSION, SheepShaverSettings, SystemInfo,
    new_id, publish_dir,
};
use crate::media::{self, LocalMedia, Media};
use crate::model::Model;
use crate::paths::Paths;
use crate::registry;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// The newest format this reads, and writes when an archive needs it: one
/// with registered media. Format 2 has no media, and is written without them.
pub const FORMAT_VERSION: u32 = 3;
const WITHOUT_MEDIA_FORMAT: u32 = 2;
/// What an archive of machines alone is written as, so that older versions of
/// macman, which read only this, can still import it. Format 2 adds system
/// images and ROMs registered for their own sake, which those would ignore.
const MACHINES_ONLY_FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.toml";
const BLOB_DIR: &str = "blobs/";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
/// Limits that bound what a hostile archive can make macman write.
pub const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    #[serde(default)]
    roms: Vec<ArchivedRom>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    systems: Vec<ArchivedSystem>,
    /// Registered media, with what `media add` found in them (format 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    media: Vec<Media>,
    #[serde(default)]
    machines: Vec<ArchivedMachine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedRom {
    name: String,
    sha256: String,
    size: u64,
    /// Register it even if no machine that uses it is imported, as `--all`
    /// does. Otherwise a ROM comes in only with a machine.
    #[serde(default, skip_serializing_if = "is_false")]
    register: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A registered system image, with the metadata `system add` recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedSystem {
    name: String,
    sha256: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extracted_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_sha256: Option<String>,
}

impl ArchivedSystem {
    fn image(&self) -> registry::SystemImage {
        registry::SystemImage {
            name: self.name.clone(),
            sha256: self.sha256.clone(),
            size: self.size,
            version: self.version.clone(),
            extracted_from: self.extracted_from.clone(),
            source_sha256: self.source_sha256.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedDisk {
    path: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedMachine {
    rom_sha256: String,
    contents: Vec<ArchivedDisk>,
    machine: Machine,
}

/// Hashes everything read through it.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..count]);
        Ok(count)
    }
}

/// What to put in an archive.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub machines: Vec<String>,
    /// Registered system images, by name.
    pub systems: Vec<String>,
    /// Registered media, by name.
    pub media: Vec<String>,
    /// Registered ROMs to register on import by these names, whether or not a
    /// machine here uses them.
    pub roms: Vec<String>,
}

/// How many of each an archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exported {
    pub machines: usize,
    pub roms: usize,
    pub systems: usize,
    pub media: usize,
}

/// Write `selection` to a new archive at `output`.
pub fn export(paths: &Paths, selection: &Selection, output: &Path) -> Result<Exported> {
    if output.symlink_metadata().is_ok() {
        bail!("{} already exists", output.display());
    }
    let names: BTreeSet<_> = selection.machines.iter().collect();
    let system_names: BTreeSet<_> = selection.systems.iter().collect();

    // Every disk and system image is locked before any is read, so the archive
    // is one coherent snapshot. The locks are held until the archive is finished.
    let mut locks: Vec<ImageLock> = Vec::new();
    let mut machines = Vec::new();
    for name in names {
        validate_name("machine", name)?;
        let machine = Machine::load(paths, name)?;
        for (path, _) in machine.disk_paths(paths)? {
            let lock = disk::lock_image(paths, &path, LockMode::Shared)
                .with_context(|| format!("cannot export machine {name:?}"))?;
            locks.push(lock);
        }
        machines.push(machine);
    }
    let mut systems = Vec::new();
    for name in system_names {
        let (image, file) = registry::load_system(paths, name)?;
        let lock = disk::lock_image(paths, &file, LockMode::Shared)
            .with_context(|| format!("cannot export system image {name:?}"))?;
        locks.push(lock);
        systems.push((image, file));
    }
    let mut media = Vec::new();
    for name in selection.media.iter().collect::<BTreeSet<_>>() {
        let (medium, file) = media::load_media(paths, name)?;
        let lock = disk::lock_image(paths, &file, LockMode::Shared)
            .with_context(|| format!("cannot export media {name:?}"))?;
        locks.push(lock);
        media.push((medium, file));
    }

    let mut manifest = Manifest {
        format_version: FORMAT_VERSION,
        roms: Vec::new(),
        systems: Vec::new(),
        media: Vec::new(),
        machines: Vec::new(),
    };
    let mut blobs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let add_rom = |manifest: &mut Manifest,
                   blobs: &mut BTreeMap<String, PathBuf>,
                   name: &str,
                   register: bool|
     -> Result<String> {
        let (rom, rom_path) = registry::load_rom(paths, name)?;
        registry::verify(&rom_path, "ROM", &rom.name, &rom.sha256)?;
        // A ROM registered for its own sake keeps its name, so each registered
        // name gets an entry, though identical ones share a blob. One that only
        // comes with machines has one entry for its contents, as in format 1.
        let found = manifest
            .roms
            .iter()
            .position(|r| r.name == rom.name)
            .or_else(|| {
                manifest
                    .roms
                    .iter()
                    .position(|r| !register && !r.register && r.sha256 == rom.sha256)
            });
        match found {
            Some(index) => manifest.roms[index].register |= register,
            None => manifest.roms.push(ArchivedRom {
                name: rom.name.clone(),
                sha256: rom.sha256.clone(),
                size: rom.size,
                register,
            }),
        }
        blobs.insert(rom.sha256.clone(), rom_path);
        Ok(rom.sha256)
    };
    for mut machine in machines {
        if machine.id.is_none() {
            machine.id = Some(new_id());
            machine.save_to(&paths.machine(&machine.name))?;
        }
        if let Some(settings) = &mut machine.basilisk2 {
            // It's a path on this host, and may say more than the owner meant to.
            settings.shared_folder = None;
        }
        if let Some(settings) = &mut machine.sheepshaver {
            settings.shared_folder = None;
        }
        let rom_sha256 = add_rom(&mut manifest, &mut blobs, &machine.hardware.rom, false)?;

        let mut contents = Vec::new();
        for (path, disk) in machine.disk_paths(paths)? {
            let sha256 = sha256_file(&path)?;
            let size = fs::metadata(&path)?.len();
            contents.push(ArchivedDisk {
                path: disk.path.clone(),
                sha256: sha256.clone(),
                size,
            });
            blobs.insert(sha256, path);
        }
        manifest.machines.push(ArchivedMachine {
            rom_sha256,
            contents,
            machine,
        });
    }
    // After the machines', so a machine's ROM named here too is registered.
    for name in selection.roms.iter().collect::<BTreeSet<_>>() {
        validate_name("ROM", name)?;
        add_rom(&mut manifest, &mut blobs, name, true)?;
    }
    for (image, file) in systems {
        registry::verify(&file, "system image", &image.name, &image.sha256)?;
        disk::detect_format(&file)
            .with_context(|| format!("cannot export system image {:?}", image.name))?;
        blobs.insert(image.sha256.clone(), file);
        manifest.systems.push(ArchivedSystem {
            name: image.name,
            sha256: image.sha256,
            size: image.size,
            version: image.version,
            extracted_from: image.extracted_from,
            source_sha256: image.source_sha256,
        });
    }
    for (medium, file) in media {
        // The archive's own digest covers what's written, so this only stops
        // a medium damaged here from being passed on as good.
        registry::verify(&file, "media", &medium.name, &medium.sha256)?;
        blobs.insert(medium.sha256.clone(), file);
        manifest.media.push(medium);
    }
    if manifest.media.is_empty() {
        manifest.format_version = WITHOUT_MEDIA_FORMAT;
        if manifest.systems.is_empty() && !manifest.roms.iter().any(|rom| rom.register) {
            manifest.format_version = MACHINES_ONLY_FORMAT;
        }
    }

    // Don't produce something that a later import is bound to refuse.
    validate(&manifest).context("cannot export these")?;

    let temp = temp_sibling(output)?;
    let result = write_archive(&temp, &manifest, &blobs).and_then(|()| publish_new(&temp, output));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("cannot write {}", output.display()))?;
    drop(locks);
    Ok(Exported {
        machines: manifest.machines.len(),
        roms: manifest.roms.len(),
        systems: manifest.systems.len(),
        media: manifest.media.len(),
    })
}

fn entry_header(path: &str, size: u64) -> Result<tar::Header> {
    let mut header = tar::Header::new_gnu();
    header.set_path(path)?;
    header.set_size(size);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    Ok(header)
}

fn write_archive(
    path: &Path,
    manifest: &Manifest,
    blobs: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let file = File::create(path)?;
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));

    let text = toml::to_string_pretty(manifest).context("cannot serialize manifest")?;
    let header = entry_header(MANIFEST, text.len() as u64)?;
    builder.append(&header, text.as_bytes())?;

    for (sha256, source) in blobs {
        let size = fs::metadata(source)?.len();
        let mut reader = HashingReader {
            inner: File::open(source)?.take(size),
            hasher: Sha256::new(),
        };
        let header = entry_header(&format!("{BLOB_DIR}{sha256}"), size)?;
        builder.append(&header, &mut reader)?;
        // This hash covers the bytes actually archived, so it also catches
        // writers that ignore macman's locks.
        if hex_digest(reader.hasher) != *sha256 {
            bail!("{} changed during the export", source.display());
        }
    }

    let gz = builder.into_inner()?;
    let file = gz.finish()?;
    file.sync_all()?;
    Ok(())
}

/// What became of one item in an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Imported,
    Unchanged,
    /// Local data is in the way; nothing was changed.
    Conflict(String),
    /// This machine can't be used here.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineResult {
    pub name: String,
    pub outcome: Outcome,
}

/// What became of one ROM in an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomOutcome {
    Imported,
    /// Its file was registered, but not its metadata, which was finished.
    Recovered,
    /// Already registered under the same name, which the machine uses.
    Unchanged {
        local: String,
    },
    /// Already registered under another name, which the machine uses.
    Reused {
        local: String,
    },
    /// Registered for its own sake, but its name is taken; nothing was changed.
    Conflict(String),
    /// Registered for its own sake, but that went wrong.
    Failed(String),
}

/// What became of one system image, or one medium, in an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemOutcome {
    Imported,
    /// Its image was registered, but not its metadata, which was finished.
    Recovered,
    Unchanged,
    /// Its name is taken by something else; nothing was changed.
    Conflict(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemResult {
    pub name: String,
    pub outcome: SystemOutcome,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Whether this is what an import would do, rather than what it did.
    pub dry_run: bool,
    /// What became of each ROM, by its name in the archive.
    pub roms: BTreeMap<String, RomOutcome>,
    /// In the archive's order.
    pub systems: Vec<SystemResult>,
    /// In the archive's order.
    pub media: Vec<SystemResult>,
    pub machines: Vec<MachineResult>,
}

impl ImportReport {
    /// Machines, system images, and media, counted by outcome.
    fn count(
        &self,
        machine: impl Fn(&Outcome) -> bool,
        system: impl Fn(&SystemOutcome) -> bool,
    ) -> usize {
        let machines = self.machines.iter().filter(|m| machine(&m.outcome)).count();
        let systems = self
            .systems
            .iter()
            .chain(&self.media)
            .filter(|s| system(&s.outcome))
            .count();
        machines + systems
    }

    pub fn imported(&self) -> usize {
        self.count(
            |o| *o == Outcome::Imported,
            |o| matches!(o, SystemOutcome::Imported | SystemOutcome::Recovered),
        )
    }

    pub fn unchanged(&self) -> usize {
        self.count(
            |o| *o == Outcome::Unchanged,
            |o| *o == SystemOutcome::Unchanged,
        )
    }

    /// Conflicts and failures of machines, system images, and media, and of ROMs
    /// registered for their own sake.
    pub fn unresolved(&self) -> usize {
        let roms = self
            .roms
            .values()
            .filter(|o| matches!(o, RomOutcome::Conflict(_) | RomOutcome::Failed(_)))
            .count();
        roms + self.count(
            |o| matches!(o, Outcome::Conflict(_) | Outcome::Failed(_)),
            |o| matches!(o, SystemOutcome::Conflict(_) | SystemOutcome::Failed(_)),
        )
    }
}

/// Removes its directory when dropped, on success and failure alike.
struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Import everything possible from `archive`. An `Err` means the archive was
/// rejected as a whole, and nothing changed.
///
/// With `dry_run`, decide everything the same way, but change nothing: report
/// what an import would do now. Only the ROMs are unpacked, to a private
/// temporary directory, because deciding needs their contents; disks are
/// verified as they're read, and discarded.
pub fn import(
    paths: &Paths,
    backends: &Backends,
    archive: &Path,
    rename: Option<&str>,
    dry_run: bool,
) -> Result<ImportReport> {
    if let Some(name) = rename {
        validate_name("machine", name)?;
    }
    if dry_run {
        return preview(paths, backends, archive, rename);
    }

    // One import at a time, so anything already in staging is abandoned.
    fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("cannot create {}", paths.data_dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.data_dir.join("import.lock"))
        .context("cannot open the import lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another macman import is running"),
        Err(TryLockError::Error(error)) => {
            return Err(error).context("cannot take the import lock");
        }
    }
    let _lock = HeldLock::new(lock);
    if paths.staging().exists() {
        fs::remove_dir_all(paths.staging()).context("cannot clear an abandoned import")?;
    }

    let staging = StagingDir(paths.staging());
    fs::create_dir_all(staging.0.join("blobs")).context("cannot create the staging directory")?;
    decide_all(paths, backends, archive, rename, &staging.0, false)
}

/// [`import`] with `dry_run`.
fn preview(
    paths: &Paths,
    backends: &Backends,
    archive: &Path,
    rename: Option<&str>,
) -> Result<ImportReport> {
    // A shared hold on the import lock keeps a real import from starting
    // meanwhile, if one ever has; if none has, there's no lock to hold.
    let _lock = match File::open(paths.data_dir.join("import.lock")) {
        Ok(lock) => {
            match lock.try_lock_shared() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => bail!("another macman import is running"),
                Err(TryLockError::Error(error)) => {
                    return Err(error).context("cannot take the import lock");
                }
            }
            Some(HeldLock::new(lock))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("cannot open the import lock"),
    };

    let staging = StagingDir(std::env::temp_dir().join(format!("macman-preview-{}", new_id())));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(staging.0.join("blobs"))
        .context("cannot create a temporary directory")?;
    decide_all(paths, backends, archive, rename, &staging.0, true)
}

/// Unpack `archive` into `staging`, and decide each machine, importing it
/// unless this is a `dry_run`.
fn decide_all(
    paths: &Paths,
    backends: &Backends,
    archive: &Path,
    rename: Option<&str>,
    staging: &Path,
    dry_run: bool,
) -> Result<ImportReport> {
    let manifest = unpack(archive, &staging.join("blobs"), dry_run)
        .with_context(|| format!("cannot import {}", archive.display()))?;

    if rename.is_some() && manifest.machines.len() != 1 {
        bail!(
            "--rename needs an archive with one machine, but this one has {}",
            manifest.machines.len()
        );
    }

    let mut report = ImportReport {
        dry_run,
        ..ImportReport::default()
    };
    for entry in &manifest.machines {
        let context = Importing {
            paths,
            backends,
            staging,
            manifest: &manifest,
            entry,
            rename,
            dry_run,
        };
        let (name, outcome) = context.run(&mut report.roms);
        report.machines.push(MachineResult { name, outcome });
    }
    let blobs = staging.join("blobs");
    for rom in manifest.roms.iter().filter(|rom| rom.register) {
        // Settled if a machine brought it in under its name, or found it here so;
        // not if a machine reused an identical ROM under another name.
        let settled = match report.roms.get(&rom.name) {
            Some(RomOutcome::Imported | RomOutcome::Recovered) => true,
            Some(RomOutcome::Unchanged { local }) => *local == rom.name,
            _ => false,
        };
        if !settled {
            let outcome = register_rom(paths, &blobs, rom, dry_run)
                .unwrap_or_else(|error| RomOutcome::Failed(format!("{error:#}")));
            report.roms.insert(rom.name.clone(), outcome);
        }
    }
    for system in &manifest.systems {
        let outcome = register_system(paths, &blobs, system, dry_run)
            .unwrap_or_else(|error| SystemOutcome::Failed(format!("{error:#}")));
        report.systems.push(SystemResult {
            name: system.name.clone(),
            outcome,
        });
    }
    for medium in &manifest.media {
        let outcome = register_media(paths, &blobs, medium, dry_run)
            .unwrap_or_else(|error| SystemOutcome::Failed(format!("{error:#}")));
        report.media.push(SystemResult {
            name: medium.name.clone(),
            outcome,
        });
    }
    Ok(report)
}

/// Bring in a ROM the archive asks to have registered whether or not a machine
/// uses it. It's a registration in its own right, so like a system image, it
/// keeps its name: it isn't folded into an identical ROM under another name.
fn register_rom(
    paths: &Paths,
    blobs: &Path,
    rom: &ArchivedRom,
    dry_run: bool,
) -> Result<RomOutcome> {
    let blob = blobs.join(&rom.sha256);
    let incoming = registry::rom_from_blob(&blob, &rom.name, &rom.sha256)?;
    if registry::rom_exists(paths, &rom.name) {
        let same = registry::load_rom(paths, &rom.name).is_ok_and(|(local, file)| {
            local.sha256 == rom.sha256 && sha256_file(&file).is_ok_and(|sha| sha == rom.sha256)
        });
        if same {
            return Ok(RomOutcome::Unchanged {
                local: rom.name.clone(),
            });
        }
        let recovered = if dry_run {
            registry::can_recover_rom(paths, &incoming)?
        } else {
            registry::recover_rom(paths, &incoming)?
        };
        return Ok(if recovered {
            RomOutcome::Recovered
        } else {
            RomOutcome::Conflict(format!(
                "a different ROM is already named {:?}; nothing was changed",
                rom.name
            ))
        });
    }
    if dry_run {
        return Ok(RomOutcome::Imported);
    }
    fs::create_dir_all(paths.roms())?;
    match registry::add_rom_blob(paths, &blob, &incoming) {
        Ok(()) => Ok(RomOutcome::Imported),
        Err(_) if registry::rom_exists(paths, &rom.name) => Ok(RomOutcome::Conflict(format!(
            "ROM {:?} appeared during the import; nothing was changed",
            rom.name
        ))),
        Err(error) => Err(error),
    }
}

/// Bring in a system image under its own name. Its name is what `create
/// --system` uses, so unlike a ROM, it isn't folded into an identical image
/// that goes by another name.
fn register_system(
    paths: &Paths,
    blobs: &Path,
    system: &ArchivedSystem,
    dry_run: bool,
) -> Result<SystemOutcome> {
    let image = system.image();
    let file = registry::system_file(paths, &system.name);
    // As `system remove` takes an exclusive lock, this keeps it from being
    // removed while it's compared. A preview makes no lock files.
    let _lock = if file.exists() {
        let held = if dry_run {
            disk::probe_image_lock(paths, &file, LockMode::Shared)
        } else {
            disk::lock_image(paths, &file, LockMode::Shared).map(Some)
        };
        match held {
            Ok(lock) => lock,
            Err(_) => {
                return Ok(SystemOutcome::Conflict(format!(
                    "system image {:?} is in use, so it can't be compared; try again when it's \
                     free",
                    system.name
                )));
            }
        }
    } else {
        None
    };
    let taken = || {
        SystemOutcome::Conflict(format!(
            "a different system image is already named {:?}; nothing was changed",
            system.name
        ))
    };
    match registry::local_system(paths, &image)? {
        registry::LocalSystem::Same => Ok(SystemOutcome::Unchanged),
        registry::LocalSystem::Different => Ok(taken()),
        registry::LocalSystem::Unfinished if dry_run => Ok(SystemOutcome::Recovered),
        registry::LocalSystem::Unfinished => Ok(if registry::recover_system(paths, &image)? {
            SystemOutcome::Recovered
        } else {
            taken()
        }),
        registry::LocalSystem::Absent if dry_run => Ok(SystemOutcome::Imported),
        registry::LocalSystem::Absent => {
            match registry::add_system_blob(paths, &blobs.join(&system.sha256), &image) {
                Ok(()) => Ok(SystemOutcome::Imported),
                Err(_) if registry::system_exists(paths, &system.name) => {
                    Ok(SystemOutcome::Conflict(format!(
                        "system image {:?} appeared during the import; nothing was changed",
                        system.name
                    )))
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Bring in a medium under its own name, as for a system image. A preview
/// needs no blob, as a medium is compared by its digest.
fn register_media(
    paths: &Paths,
    blobs: &Path,
    medium: &Media,
    dry_run: bool,
) -> Result<SystemOutcome> {
    let file = media::media_file(paths, &medium.name);
    // As `media remove` takes an exclusive lock, this keeps it from being
    // removed while it's compared. A preview makes no lock files.
    let _lock = if file.exists() {
        let held = if dry_run {
            disk::probe_image_lock(paths, &file, LockMode::Shared)
        } else {
            disk::lock_image(paths, &file, LockMode::Shared).map(Some)
        };
        match held {
            Ok(lock) => lock,
            Err(_) => {
                return Ok(SystemOutcome::Conflict(format!(
                    "media {:?} is in use, so it can't be compared; try again when it's free",
                    medium.name
                )));
            }
        }
    } else {
        None
    };
    let taken = || {
        SystemOutcome::Conflict(format!(
            "different media is already named {:?}; nothing was changed",
            medium.name
        ))
    };
    match media::local_media(paths, medium)? {
        LocalMedia::Same => Ok(SystemOutcome::Unchanged),
        LocalMedia::Different => Ok(taken()),
        LocalMedia::Unfinished if dry_run => Ok(SystemOutcome::Recovered),
        LocalMedia::Unfinished => Ok(if media::recover_media(paths, medium)? {
            SystemOutcome::Recovered
        } else {
            taken()
        }),
        LocalMedia::Absent if dry_run => Ok(SystemOutcome::Imported),
        LocalMedia::Absent => {
            match media::add_media_blob(paths, &blobs.join(&medium.sha256), medium) {
                Ok(()) => Ok(SystemOutcome::Imported),
                Err(_) if media::media_exists(paths, &medium.name) => {
                    Ok(SystemOutcome::Conflict(format!(
                        "media {:?} appeared during the import; nothing was changed",
                        medium.name
                    )))
                }
                Err(error) => Err(error),
            }
        }
    }
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read the whole archive into `blobs`, verifying everything. Nothing outside
/// `blobs` is written, and only under hash-derived names. With `only_registry`,
/// everything is still verified, but only ROMs and system images are kept, as
/// deciding needs them; machines' disks and media are discarded.
fn unpack(archive: &Path, blobs: &Path, only_registry: bool) -> Result<Manifest> {
    let file = File::open(archive).context("cannot read the archive")?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut entries = tar.entries().context("not a macman archive")?;

    let first = entries
        .next()
        .context("the archive is empty")?
        .context("not a macman archive")?;
    let manifest = read_manifest(first)?;
    let expected = validate(&manifest)?;
    let registry: BTreeSet<_> = manifest
        .roms
        .iter()
        .map(|rom| rom.sha256.as_str())
        .chain(manifest.systems.iter().map(|system| system.sha256.as_str()))
        .collect();

    let mut seen = BTreeSet::new();
    for entry in entries {
        let entry = entry.context("the archive is damaged")?;
        let keep = |sha256: &str| !only_registry || registry.contains(sha256);
        unpack_blob(entry, &expected, &mut seen, blobs, keep)?;
    }
    if let Some(missing) = expected.keys().find(|sha256| !seen.contains(*sha256)) {
        bail!("the archive is incomplete: {missing} is missing");
    }
    // Export takes only what `system add` would; anything else is damage.
    for system in &manifest.systems {
        disk::detect_format(&blobs.join(&system.sha256)).with_context(|| {
            format!(
                "system image {:?} in the archive is not a usable volume",
                system.name
            )
        })?;
    }
    Ok(manifest)
}

fn require_regular<R: Read>(entry: &tar::Entry<'_, R>) -> Result<()> {
    if entry.header().entry_type() != tar::EntryType::Regular {
        bail!(
            "the archive has an unexpected entry of type {:?}",
            entry.header().entry_type()
        );
    }
    Ok(())
}

fn read_manifest<R: Read>(entry: tar::Entry<'_, R>) -> Result<Manifest> {
    require_regular(&entry)?;
    if entry.path_bytes().as_ref() != MANIFEST.as_bytes() {
        bail!("not a macman archive: it doesn't start with {MANIFEST}");
    }
    let size = entry.size();
    if size > MAX_MANIFEST_BYTES {
        bail!("the archive's manifest is too large ({size} bytes)");
    }
    let mut text = String::new();
    entry
        .take(MAX_MANIFEST_BYTES)
        .read_to_string(&mut text)
        .context("the manifest is unreadable")?;

    #[derive(Deserialize)]
    struct Version {
        format_version: u32,
    }
    let version: Version = toml::from_str(&text).context("the manifest is invalid")?;
    if !(MACHINES_ONLY_FORMAT..=FORMAT_VERSION).contains(&version.format_version) {
        bail!(
            "this archive uses format {}, but this macman reads formats \
             {MACHINES_ONLY_FORMAT} to {FORMAT_VERSION}; upgrade macman",
            version.format_version
        );
    }
    toml::from_str(&text).context("the manifest is invalid")
}

fn validate_disk_path(path: &str) -> Result<()> {
    let safe_components = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    // These mean something on some other host's filesystem, so they're refused
    // everywhere: an archive made here may be imported on Windows.
    let portable = !path.is_empty()
        && !path.contains(['\\', ':'])
        && path
            .split('/')
            .all(|part| windows_can_name(part) && !part.ends_with(' '));
    if !safe_components || !portable || path.eq_ignore_ascii_case("machine.toml") {
        bail!("the manifest has an unsafe disk path {path:?}");
    }
    Ok(())
}

/// Check everything in the manifest that doesn't need the blobs, returning the
/// size of each blob the archive must contain.
fn validate(manifest: &Manifest) -> Result<BTreeMap<String, u64>> {
    let mut expected: BTreeMap<String, u64> = BTreeMap::new();
    let mut expect = |sha256: &str, size: u64| -> Result<()> {
        if !is_sha256(sha256) {
            bail!("the manifest has an invalid digest {sha256:?}");
        }
        if size > MAX_BLOB_BYTES {
            bail!("the manifest lists an item too large to import ({size} bytes)");
        }
        if *expected.entry(sha256.to_string()).or_insert(size) != size {
            bail!("the manifest gives two sizes for {sha256}");
        }
        Ok(())
    };

    let mut rom_names = BTreeSet::new();
    let mut rom_digests = BTreeSet::new();
    for rom in &manifest.roms {
        validate_name("ROM", &rom.name)?;
        expect(&rom.sha256, rom.size)?;
        // Names registered for their own sake may share contents; otherwise a
        // ROM is listed once, as a machine finds its ROM by digest.
        let new_digest = rom_digests.insert(rom.sha256.as_str());
        if !rom_names.insert(rom.name.as_str()) || (!new_digest && !rom.register) {
            bail!("the manifest lists ROM {:?} twice", rom.name);
        }
    }

    let mut system_names = BTreeSet::new();
    for system in &manifest.systems {
        validate_name("system", &system.name)?;
        expect(&system.sha256, system.size)?;
        if !system_names.insert(system.name.as_str()) {
            bail!("the manifest lists system image {:?} twice", system.name);
        }
        if system
            .source_sha256
            .as_deref()
            .is_some_and(|sha256| !is_sha256(sha256))
        {
            bail!(
                "the manifest has an invalid digest for system image {:?}",
                system.name
            );
        }
        // They're printed by `system list`, so nothing in them may steer a terminal.
        let printable = [&system.version, &system.extracted_from]
            .into_iter()
            .flatten()
            .all(|text| !text.chars().any(char::is_control));
        if !printable {
            bail!(
                "the manifest has unprintable details for system image {:?}",
                system.name
            );
        }
    }

    let mut media_names = BTreeSet::new();
    for medium in &manifest.media {
        validate_name("media", &medium.name)?;
        expect(&medium.sha256, medium.size)?;
        if !media_names.insert(medium.name.as_str()) {
            bail!("the manifest lists media {:?} twice", medium.name);
        }
        // They're printed by `media list`, so nothing in them may steer a terminal.
        let printable = [&medium.version, &medium.source]
            .into_iter()
            .flatten()
            .all(|text| !text.chars().any(char::is_control));
        if !printable {
            bail!(
                "the manifest has unprintable details for media {:?}",
                medium.name
            );
        }
    }

    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for entry in &manifest.machines {
        let machine = &entry.machine;
        validate_name("machine", &machine.name)?;
        if machine.schema_version != SCHEMA_VERSION {
            bail!(
                "machine {:?} uses schema version {}; upgrade macman",
                machine.name,
                machine.schema_version
            );
        }
        let id = machine.id.as_deref().unwrap_or_default();
        if uuid::Uuid::parse_str(id).is_err() {
            bail!("machine {:?} has an invalid id {id:?}", machine.name);
        }
        if !names.insert(machine.name.as_str()) || !ids.insert(id) {
            bail!("the manifest lists machine {:?} twice", machine.name);
        }
        if !rom_digests.contains(entry.rom_sha256.as_str()) {
            bail!(
                "machine {:?} refers to a ROM not in the archive",
                machine.name
            );
        }

        let mut paths = BTreeSet::new();
        for disk in &entry.contents {
            validate_disk_path(&disk.path)?;
            expect(&disk.sha256, disk.size)?;
            // Hosts may fold case, so these would collide.
            if !paths.insert(disk.path.to_ascii_lowercase()) {
                bail!(
                    "machine {:?} lists disk {:?} twice",
                    machine.name,
                    disk.path
                );
            }
        }
        let listed: BTreeSet<_> = machine
            .disks
            .iter()
            .map(|disk| disk.path.to_ascii_lowercase())
            .collect();
        if listed != paths || machine.disks.len() != entry.contents.len() {
            bail!("machine {:?} doesn't match its disk list", machine.name);
        }
    }

    let total = expected
        .values()
        .try_fold(0u64, |sum, size| sum.checked_add(*size));
    if total.is_none_or(|total| total > MAX_TOTAL_BYTES) {
        bail!("the archive is too large to import");
    }
    Ok(expected)
}

fn unpack_blob<R: Read>(
    entry: tar::Entry<'_, R>,
    expected: &BTreeMap<String, u64>,
    seen: &mut BTreeSet<String>,
    blobs: &Path,
    keep: impl Fn(&str) -> bool,
) -> Result<()> {
    require_regular(&entry)?;
    let path = entry.path_bytes().into_owned();
    let path = String::from_utf8_lossy(&path).into_owned();
    let Some(sha256) = path
        .strip_prefix(BLOB_DIR)
        .filter(|sha256| is_sha256(sha256))
    else {
        bail!("the archive has an unexpected entry {path:?}");
    };
    let Some(size) = expected.get(sha256).copied() else {
        bail!("the archive has an unlisted entry {path:?}");
    };
    if !seen.insert(sha256.to_string()) {
        bail!("the archive has a duplicate entry {path:?}");
    }
    // Checked against the manifest before reading, which bounds what's written.
    if entry.size() != size {
        bail!(
            "{sha256} is {} bytes, but the manifest says {size}",
            entry.size()
        );
    }

    let mut output: Box<dyn Write> = if keep(sha256) {
        let target = blobs.join(sha256);
        Box::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)
                .with_context(|| format!("cannot stage {sha256}"))?,
        )
    } else {
        Box::new(io::sink())
    };
    let mut reader = HashingReader {
        inner: entry.take(size),
        hasher: Sha256::new(),
    };
    let written = io::copy(&mut reader, &mut output).context("the archive is damaged")?;
    output.flush()?;
    if written != size || hex_digest(reader.hasher) != sha256 {
        bail!("{sha256} doesn't match its digest; the archive is damaged");
    }
    Ok(())
}

/// The parts of a machine that decide whether two copies are the same. Names,
/// locations, and ROM aliases don't count; the ROM's and disks' contents do.
/// Nor does the revision a copy happens to record, only what it runs as here,
/// so a machine that has been to a host with another revision is still itself.
#[derive(Debug, PartialEq)]
struct Fingerprint<'a> {
    backend: &'a str,
    /// Whether a plain run discards the boot disk's changes, which is how the
    /// owner meant the machine to be used.
    ephemeral: bool,
    revision: Option<String>,
    model: Option<Model>,
    memory: &'a str,
    display: &'a Display,
    system: &'a SystemInfo,
    /// Without the shared folder, which belongs to a host, not the machine.
    basilisk2: Option<Basilisk2Settings>,
    /// As for Basilisk II.
    sheepshaver: Option<SheepShaverSettings>,
    disks: &'a [Disk],
}

impl<'a> Fingerprint<'a> {
    fn of(machine: &'a Machine, backends: &Backends) -> Self {
        let emulator = &machine.emulator;
        Self {
            backend: &emulator.backend,
            ephemeral: machine.ephemeral,
            revision: backends
                .get(&emulator.backend)
                .and_then(|backend| backend.resolve_revision(&emulator.revision))
                .ok(),
            model: machine.hardware.model,
            memory: &machine.hardware.memory,
            display: &machine.display,
            system: &machine.system,
            basilisk2: machine.basilisk2.clone().map(|settings| Basilisk2Settings {
                shared_folder: None,
                ..settings
            }),
            sheepshaver: machine
                .sheepshaver
                .clone()
                .map(|settings| SheepShaverSettings {
                    shared_folder: None,
                    ..settings
                }),
            disks: &machine.disks,
        }
    }
}

struct Importing<'a> {
    paths: &'a Paths,
    backends: &'a Backends,
    staging: &'a Path,
    manifest: &'a Manifest,
    entry: &'a ArchivedMachine,
    rename: Option<&'a str>,
    /// Decide, but change nothing.
    dry_run: bool,
}

impl Importing<'_> {
    fn blob(&self, sha256: &str) -> PathBuf {
        self.staging.join("blobs").join(sha256)
    }

    fn run(&self, roms: &mut BTreeMap<String, RomOutcome>) -> (String, Outcome) {
        let name = self.rename.unwrap_or(&self.entry.machine.name).to_string();
        let outcome = self
            .decide(&name, roms)
            .unwrap_or_else(|error| Outcome::Failed(format!("{error:#}")));
        (name, outcome)
    }

    fn decide(&self, name: &str, roms: &mut BTreeMap<String, RomOutcome>) -> Result<Outcome> {
        let machine = &self.entry.machine;
        let backend = self.backends.get(&machine.emulator.backend)?;
        backend.validate(machine)?;
        backend.validate_disks(machine, machine.disks.len())?;
        let Some(rom) = self
            .manifest
            .roms
            .iter()
            .filter(|rom| rom.sha256 == self.entry.rom_sha256)
            .reduce(|best, rom| {
                if rom.name == machine.hardware.rom {
                    rom
                } else {
                    best
                }
            })
        else {
            bail!("the ROM is not in the archive");
        };

        if self.rename.is_none()
            && let Some(local) = self.find_by_id()?
        {
            let outcome = self.compare(&local)?;
            if outcome == Outcome::Unchanged {
                roms.entry(rom.name.clone())
                    .or_insert_with(|| RomOutcome::Unchanged {
                        local: local.hardware.rom.clone(),
                    });
            }
            return Ok(outcome);
        }
        if self.paths.machine(name).symlink_metadata().is_ok() {
            let reason = if self.rename.is_some() {
                format!("machine {name:?} already exists; pick another name")
            } else {
                format!(
                    "a different machine is already named {name:?}; import it under another \
                     name with --rename"
                )
            };
            return Ok(Outcome::Conflict(reason));
        }

        let blob = self.blob(&rom.sha256);
        let incoming = registry::rom_from_blob(&blob, &rom.name, &rom.sha256)?;
        backend.check_rom(machine, &incoming, &blob)?;
        let mut existing = local_rom(self.paths, rom)?;
        if existing.is_none()
            && self.dry_run
            && matches!(
                roms.get(&rom.name),
                Some(RomOutcome::Imported | RomOutcome::Recovered)
            )
        {
            // A machine before this one would have registered it.
            existing = Some(incoming.clone());
        }
        if existing.is_none() && registry::rom_exists(self.paths, &rom.name) {
            let recovered = if self.dry_run {
                registry::can_recover_rom(self.paths, &incoming)?
            } else {
                registry::recover_rom(self.paths, &incoming)?
            };
            if !recovered {
                return Ok(Outcome::Conflict(format!(
                    "a different ROM is already named {:?}; nothing was changed",
                    rom.name
                )));
            }
            roms.insert(rom.name.clone(), RomOutcome::Recovered);
            existing = Some(incoming.clone());
        }
        if self.dry_run {
            roms.entry(rom.name.clone())
                .or_insert_with(|| rom_outcome(&incoming, existing.as_ref()));
            return Ok(Outcome::Imported);
        }

        let built = self.staging.join("machines").join(name);
        let result = self.publish(name, incoming, existing, &built);
        let _ = fs::remove_dir_all(&built);
        match result {
            Ok(line) => {
                roms.entry(rom.name.clone()).or_insert(line);
                Ok(Outcome::Imported)
            }
            Err(_) if self.paths.machine(name).symlink_metadata().is_ok() => Ok(Outcome::Conflict(
                format!("machine {name:?} appeared during the import; nothing was changed"),
            )),
            Err(error) => Err(error),
        }
    }

    /// Build the machine in staging, then bring it in along with any new ROM,
    /// returning what happened to the ROM.
    fn publish(
        &self,
        name: &str,
        incoming: registry::Rom,
        existing: Option<registry::Rom>,
        built: &Path,
    ) -> Result<RomOutcome> {
        let mut machine = self.entry.machine.clone();
        machine.name = name.to_string();
        if self.rename.is_some() {
            // An independent copy has no lineage in common with the original.
            machine.id = Some(new_id());
        }

        fs::create_dir_all(built)?;
        for disk in &self.entry.contents {
            let verify = |copy: &Path| {
                if sha256_file(copy)? != disk.sha256 {
                    bail!("disk {} failed its digest after copying", disk.path);
                }
                Ok(())
            };
            let blob = self.blob(&disk.sha256);
            atomic_copy_range_checked(&blob, 0, disk.size, &built.join(&disk.path), verify)?;
        }

        machine.hardware.rom = existing
            .as_ref()
            .map_or(&incoming.name, |rom| &rom.name)
            .clone();
        machine.save_to(built)?;

        if existing.is_none() {
            registry::add_rom_blob(self.paths, &self.blob(&incoming.sha256), &incoming)?;
        }
        if let Err(error) = publish_dir(self.paths, built, name) {
            if existing.is_none() {
                let _ = registry::remove_rom(self.paths, &incoming.name);
            }
            return Err(error);
        }

        Ok(rom_outcome(&incoming, existing.as_ref()))
    }

    fn find_by_id(&self) -> Result<Option<Machine>> {
        for name in list_dir(&self.paths.machines())? {
            if let Ok(machine) = Machine::load(self.paths, &name)
                && machine.id.is_some()
                && machine.id == self.entry.machine.id
            {
                return Ok(Some(machine));
            }
        }
        Ok(None)
    }

    /// Whether `local`, which shares this machine's id, has the same contents.
    fn compare(&self, local: &Machine) -> Result<Outcome> {
        let name = &local.name;
        let diverged = |what: &str| {
            Outcome::Conflict(format!(
                "local machine {name:?} has the same id but a different {what}; import a copy \
                 with --rename"
            ))
        };
        if Fingerprint::of(local, self.backends)
            != Fingerprint::of(&self.entry.machine, self.backends)
        {
            return Ok(diverged("configuration"));
        }
        let Ok((rom, _)) = registry::load_rom(self.paths, &local.hardware.rom) else {
            return Ok(diverged("ROM"));
        };
        if rom.sha256 != self.entry.rom_sha256 {
            return Ok(diverged("ROM"));
        }

        let mut locks = Vec::new();
        for (path, _) in local.disk_paths(self.paths)? {
            let lock = if self.dry_run {
                disk::probe_image_lock(self.paths, &path, LockMode::Shared)
            } else {
                disk::lock_image(self.paths, &path, LockMode::Shared).map(Some)
            };
            match lock {
                Ok(lock) => locks.push(lock),
                Err(_) if !path.exists() => return Ok(diverged("disk")),
                Err(_) => {
                    return Ok(Outcome::Conflict(format!(
                        "local machine {name:?} is in use, so it can't be compared; close it \
                         and try again"
                    )));
                }
            }
        }
        for disk in &self.entry.contents {
            let path = self.paths.machine(name).join(&disk.path);
            if sha256_file(&path).ok().as_deref() != Some(disk.sha256.as_str()) {
                return Ok(diverged("disk"));
            }
        }
        Ok(Outcome::Unchanged)
    }
}

/// The registered ROM with these contents, preferring one of the same name.
fn local_rom(paths: &Paths, rom: &ArchivedRom) -> Result<Option<registry::Rom>> {
    let matching = registry::list_roms(paths)?.into_iter().filter(|local| {
        local.sha256 == rom.sha256 && registry::rom_file_exists(paths, &local.name)
    });
    let Some(local) =
        matching.reduce(|best, local| if local.name == rom.name { local } else { best })
    else {
        return Ok(None);
    };
    let (_, file) = registry::load_rom(paths, &local.name)?;
    registry::verify(&file, "ROM", &local.name, &local.sha256)?;
    Ok(Some(local))
}

/// What becomes of the ROM `incoming` when a machine is imported, given the
/// registered ROM with the same contents, if there is one.
fn rom_outcome(incoming: &registry::Rom, existing: Option<&registry::Rom>) -> RomOutcome {
    match existing {
        None => RomOutcome::Imported,
        Some(rom) if rom.name == incoming.name => RomOutcome::Unchanged {
            local: rom.name.clone(),
        },
        Some(rom) => RomOutcome::Reused {
            local: rom.name.clone(),
        },
    }
}
