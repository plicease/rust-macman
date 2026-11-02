//! Portable machine archives: `macman export` and `macman import`.
//!
//! An archive is a gzipped tar holding `manifest.toml`, first, followed by
//! `blobs/<sha256>` for each distinct ROM or disk. Addressing blobs by content
//! dedupes them, and leaves exactly two valid forms of entry name. No name,
//! path, or permission from an archive is ever used to reach the filesystem
//! without validation. See `docs/EXPORT_IMPORT.md`.

use crate::backend::Backends;
use crate::disk::{self, ImageLock, LockMode};
use crate::fsutil::{
    atomic_copy_range_checked, hex_digest, list_dir, publish_new, sha256_file, temp_sibling,
    validate_name,
};
use crate::machine::{Disk, Display, EmulatorConfig, Machine, SCHEMA_VERSION, SystemInfo, new_id};
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

pub const FORMAT_VERSION: u32 = 1;
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
    #[serde(default)]
    machines: Vec<ArchivedMachine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedRom {
    name: String,
    sha256: String,
    size: u64,
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

/// Write `names` to a new archive at `output`, returning how many it holds.
pub fn export(paths: &Paths, names: &[String], output: &Path) -> Result<usize> {
    if output.symlink_metadata().is_ok() {
        bail!("{} already exists", output.display());
    }
    let names: BTreeSet<_> = names.iter().collect();

    // Every disk is locked before any is read, so the archive is one coherent
    // snapshot. The locks are held until the archive is finished.
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

    let mut manifest = Manifest {
        format_version: FORMAT_VERSION,
        roms: Vec::new(),
        machines: Vec::new(),
    };
    let mut blobs: BTreeMap<String, PathBuf> = BTreeMap::new();
    for mut machine in machines {
        if machine.id.is_none() {
            machine.id = Some(new_id());
            machine.save_to(&paths.machine(&machine.name))?;
        }
        let (rom, rom_path) = registry::load_rom(paths, &machine.hardware.rom)?;
        registry::verify(&rom_path, "ROM", &rom.name, &rom.sha256)?;
        if !manifest.roms.iter().any(|r| r.sha256 == rom.sha256) {
            manifest.roms.push(ArchivedRom {
                name: rom.name.clone(),
                sha256: rom.sha256.clone(),
                size: rom.size,
            });
        }
        blobs.insert(rom.sha256.clone(), rom_path);

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
            rom_sha256: rom.sha256,
            contents,
            machine,
        });
    }

    // Don't produce something that a later import is bound to refuse.
    validate(&manifest).context("cannot export this machine")?;

    let temp = temp_sibling(output)?;
    let result = write_archive(&temp, &manifest, &blobs).and_then(|()| publish_new(&temp, output));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("cannot write {}", output.display()))?;
    Ok(manifest.machines.len())
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// What became of each ROM, by its name in the archive.
    pub roms: BTreeMap<String, String>,
    pub machines: Vec<MachineResult>,
}

impl ImportReport {
    fn count(&self, matches: impl Fn(&Outcome) -> bool) -> usize {
        self.machines.iter().filter(|m| matches(&m.outcome)).count()
    }

    pub fn imported(&self) -> usize {
        self.count(|o| *o == Outcome::Imported)
    }

    pub fn unchanged(&self) -> usize {
        self.count(|o| *o == Outcome::Unchanged)
    }

    pub fn unresolved(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Conflict(_) | Outcome::Failed(_)))
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
pub fn import(
    paths: &Paths,
    backends: &Backends,
    archive: &Path,
    rename: Option<&str>,
) -> Result<ImportReport> {
    if let Some(name) = rename {
        validate_name("machine", name)?;
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
    if paths.staging().exists() {
        fs::remove_dir_all(paths.staging()).context("cannot clear an abandoned import")?;
    }

    let staging = StagingDir(paths.staging());
    fs::create_dir_all(staging.0.join("blobs")).context("cannot create the staging directory")?;
    let manifest = unpack(archive, &staging.0.join("blobs"))
        .with_context(|| format!("cannot import {}", archive.display()))?;

    if rename.is_some() && manifest.machines.len() != 1 {
        bail!(
            "--rename needs an archive with one machine, but this one has {}",
            manifest.machines.len()
        );
    }

    let mut report = ImportReport::default();
    for entry in &manifest.machines {
        let context = Importing {
            paths,
            backends,
            staging: &staging.0,
            manifest: &manifest,
            entry,
            rename,
        };
        let (name, outcome) = context.run(&mut report.roms);
        report.machines.push(MachineResult { name, outcome });
    }
    Ok(report)
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read the whole archive into `blobs`, verifying everything. Nothing outside
/// `blobs` is written, and only under hash-derived names.
fn unpack(archive: &Path, blobs: &Path) -> Result<Manifest> {
    let file = File::open(archive).context("cannot read the archive")?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut entries = tar.entries().context("not a macman archive")?;

    let first = entries
        .next()
        .context("the archive is empty")?
        .context("not a macman archive")?;
    let manifest = read_manifest(first)?;
    let expected = validate(&manifest)?;

    let mut seen = BTreeSet::new();
    for entry in entries {
        let entry = entry.context("the archive is damaged")?;
        unpack_blob(entry, &expected, &mut seen, blobs)?;
    }
    if let Some(missing) = expected.keys().find(|sha256| !seen.contains(*sha256)) {
        bail!("the archive is incomplete: {missing} is missing");
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
    if version.format_version != FORMAT_VERSION {
        bail!(
            "this archive uses format {}, but this macman reads format {FORMAT_VERSION}; upgrade \
             macman",
            version.format_version
        );
    }
    toml::from_str(&text).context("the manifest is invalid")
}

fn validate_disk_path(path: &str) -> Result<()> {
    let safe_components = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    // These mean something on some other host's filesystem.
    let portable = !path.is_empty() && !path.contains(['\\', ':']);
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
        if !rom_names.insert(rom.name.as_str()) || !rom_digests.insert(rom.sha256.as_str()) {
            bail!("the manifest lists ROM {:?} twice", rom.name);
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

    let target = blobs.join(sha256);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&target)
        .with_context(|| format!("cannot stage {sha256}"))?;
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
#[derive(Debug, PartialEq)]
struct Fingerprint<'a> {
    emulator: &'a EmulatorConfig,
    model: Model,
    memory: &'a str,
    display: &'a Display,
    system: &'a SystemInfo,
    disks: &'a [Disk],
}

impl<'a> Fingerprint<'a> {
    fn of(machine: &'a Machine) -> Self {
        Self {
            emulator: &machine.emulator,
            model: machine.hardware.model,
            memory: &machine.hardware.memory,
            display: &machine.display,
            system: &machine.system,
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
}

impl Importing<'_> {
    fn blob(&self, sha256: &str) -> PathBuf {
        self.staging.join("blobs").join(sha256)
    }

    fn run(&self, roms: &mut BTreeMap<String, String>) -> (String, Outcome) {
        let name = self.rename.unwrap_or(&self.entry.machine.name).to_string();
        let outcome = self
            .decide(&name, roms)
            .unwrap_or_else(|error| Outcome::Failed(format!("{error:#}")));
        (name, outcome)
    }

    fn decide(&self, name: &str, roms: &mut BTreeMap<String, String>) -> Result<Outcome> {
        let machine = &self.entry.machine;
        let backend = self.backends.get(&machine.emulator.backend)?;
        backend.validate(machine)?;
        backend.validate_disks(machine, machine.disks.len())?;
        let Some(rom) = self
            .manifest
            .roms
            .iter()
            .find(|rom| rom.sha256 == self.entry.rom_sha256)
        else {
            bail!("the ROM is not in the archive");
        };

        if self.rename.is_none()
            && let Some(local) = self.find_by_id()?
        {
            let outcome = self.compare(&local)?;
            if outcome == Outcome::Unchanged {
                roms.entry(rom.name.clone())
                    .or_insert_with(|| format!("unchanged rom {}", local.hardware.rom));
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
        registry::check_rom_for_model(&incoming, machine.hardware.model)?;
        let existing = self.local_rom(rom)?;
        if existing.is_none() && registry::rom_exists(self.paths, &rom.name) {
            return Ok(Outcome::Conflict(format!(
                "a different ROM is already named {:?}; nothing was changed",
                rom.name
            )));
        }

        let built = self.staging.join("machines").join(name);
        let result = self.publish(name, incoming, existing, &built);
        let _ = fs::remove_dir_all(&built);
        let line = result?;
        roms.entry(rom.name.clone()).or_insert(line);
        Ok(Outcome::Imported)
    }

    /// The registered ROM with these contents, preferring one of the same name.
    fn local_rom(&self, rom: &ArchivedRom) -> Result<Option<registry::Rom>> {
        let matching = registry::list_roms(self.paths)?
            .into_iter()
            .filter(|local| local.sha256 == rom.sha256);
        let Some(local) =
            matching.reduce(|best, local| if local.name == rom.name { local } else { best })
        else {
            return Ok(None);
        };
        let (_, file) = registry::load_rom(self.paths, &local.name)?;
        registry::verify(&file, "ROM", &local.name, &local.sha256)?;
        Ok(Some(local))
    }

    /// Build the machine in staging, then bring it in along with any new ROM,
    /// returning what happened to the ROM.
    fn publish(
        &self,
        name: &str,
        incoming: registry::Rom,
        existing: Option<registry::Rom>,
        built: &Path,
    ) -> Result<String> {
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
        let published = fs::create_dir_all(self.paths.machines())
            .and_then(|()| fs::rename(built, self.paths.machine(name)))
            .context("cannot publish the machine");
        if let Err(error) = published {
            if existing.is_none() {
                let _ = registry::remove_rom(self.paths, &incoming.name);
            }
            return Err(error);
        }

        Ok(match existing {
            None => format!("imported rom {}", incoming.name),
            Some(rom) if rom.name == incoming.name => format!("unchanged rom {}", rom.name),
            Some(rom) => format!("reused rom {} as {}", incoming.name, rom.name),
        })
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
        if Fingerprint::of(local) != Fingerprint::of(&self.entry.machine) {
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
            match disk::lock_image(self.paths, &path, LockMode::Shared) {
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
