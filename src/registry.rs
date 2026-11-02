//! Registered ROMs and system images. Each entry is the imported file plus a
//! TOML sidecar of metadata.

use crate::disk::{self, DiskFormat, LockMode};
use crate::fsutil::{
    atomic_copy_range, atomic_copy_range_checked, list_dir, read_toml, sha256_file, validate_name,
    write_toml, write_toml_new,
};
use crate::model::{ALL_MODELS, Model};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rom {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// Models this is a genuine ROM for; empty for an unrecognised ROM.
    #[serde(default)]
    pub models: Vec<Model>,
    /// Bytes after the ROM in the imported file, which aren't stored.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub trailing_bytes: u64,
    /// The imported file, when that differs from what's stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemImage {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What the volume was extracted from, if not stored whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
}

fn rom_path(paths: &Paths, name: &str) -> PathBuf {
    paths.roms().join(format!("{name}.rom"))
}

fn system_path(paths: &Paths, name: &str) -> PathBuf {
    paths.systems().join(format!("{name}.dsk"))
}

fn sidecar(file: &Path) -> PathBuf {
    file.with_extension("toml")
}

/// Whether anything at all occupies this ROM name.
pub fn rom_exists(paths: &Paths, name: &str) -> bool {
    let file = rom_path(paths, name);
    file.symlink_metadata().is_ok() || sidecar(&file).symlink_metadata().is_ok()
}

/// Whether this ROM name has its ROM file, as opposed to only metadata.
pub fn rom_file_exists(paths: &Paths, name: &str) -> bool {
    rom_path(paths, name).is_file()
}

/// Describe the ROM at `blob`, which came from an archive, without storing it.
pub fn rom_from_blob(blob: &Path, name: &str, sha256: &str) -> Result<Rom> {
    validate_name("ROM", name)?;
    Ok(Rom {
        name: name.to_string(),
        sha256: sha256.to_string(),
        size: std::fs::metadata(blob)?.len(),
        models: genuine_models(&read_rom_prefix(blob)?),
        trailing_bytes: 0,
        source_sha256: None,
    })
}

/// Register `rom`, as described by [`rom_from_blob`].
pub fn add_rom_blob(paths: &Paths, blob: &Path, rom: &Rom) -> Result<()> {
    let dest = rom_path(paths, &rom.name);
    atomic_copy_range_checked(blob, 0, rom.size, &dest, |copy| {
        if sha256_file(copy)? != rom.sha256 {
            bail!("ROM {} failed its digest after copying", rom.name);
        }
        Ok(())
    })?;
    if let Err(error) = write_toml(&sidecar(&dest), rom) {
        let _ = std::fs::remove_file(&dest);
        return Err(error);
    }
    Ok(())
}

/// Finish a registration that stopped between the ROM file and its metadata,
/// if `rom`, as described by [`rom_from_blob`], is certainly what was stored.
/// Returns false, having changed nothing, for anything less clear-cut.
pub fn recover_rom(paths: &Paths, rom: &Rom) -> Result<bool> {
    let file = rom_path(paths, &rom.name);
    let metadata = sidecar(&file);
    let is_plain_file = file
        .symlink_metadata()
        .is_ok_and(|found| found.file_type().is_file());
    if !is_plain_file || metadata.symlink_metadata().is_ok() || sha256_file(&file)? != rom.sha256 {
        return Ok(false);
    }
    match write_toml_new(&metadata, rom) {
        Ok(()) => Ok(true),
        // Whatever else was registering this name finished first.
        Err(_) if metadata.symlink_metadata().is_ok() => Ok(false),
        Err(error) => Err(error),
    }
}

/// Undo [`add_rom_blob`].
pub fn remove_rom(paths: &Paths, name: &str) -> Result<()> {
    let file = rom_path(paths, name);
    std::fs::remove_file(sidecar(&file))?;
    std::fs::remove_file(file)?;
    Ok(())
}

/// The largest ROM macman knows, which is all it needs to read of a file.
const MAX_ROM_SIZE: u64 = 512 * 1024;

/// Read up to the largest ROM size from the start of `path`.
fn read_rom_prefix(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_ROM_SIZE).read_to_end(&mut bytes))
        .with_context(|| format!("cannot read ROM {}", path.display()))?;
    Ok(bytes)
}

/// The models `bytes` is a genuine ROM for.
fn genuine_models(bytes: &[u8]) -> Vec<Model> {
    ALL_MODELS
        .into_iter()
        .filter(|model| model.accepts_rom(bytes))
        .collect()
}

/// The models whose header checksum `bytes` claims, genuine or not.
fn claimed_models(bytes: &[u8]) -> Vec<Model> {
    let Some(header) = bytes.first_chunk::<4>() else {
        return Vec::new();
    };
    let header = u32::from_be_bytes(*header);
    ALL_MODELS
        .into_iter()
        .filter(|model| model.rom_checksums().contains(&header))
        .collect()
}

pub fn add_rom(paths: &Paths, source: &Path, name: &str) -> Result<Rom> {
    add_rom_checked(paths, source, name, || {})
}

/// [`add_rom`], with `before_copy` running between validating the source and
/// copying it, the window in which a process ignoring macman's lock could
/// change it.
fn add_rom_checked(
    paths: &Paths,
    source: &Path,
    name: &str,
    before_copy: impl FnOnce(),
) -> Result<Rom> {
    validate_name("ROM", name)?;
    let dest = rom_path(paths, name);
    if dest.exists() {
        bail!("a ROM named {name:?} is already registered; pick another --name");
    }
    // Held throughout, so identification, the copy, and the recorded source
    // hash all describe one state of the source, for cooperating processes.
    let _guard = disk::lock_image(paths, source, LockMode::Shared)?;
    let file_size = std::fs::metadata(source)
        .with_context(|| format!("cannot read ROM {}", source.display()))?
        .len();
    let bytes = read_rom_prefix(source)?;
    let models = genuine_models(&bytes);

    // A genuine ROM fixes the size, so anything beyond it is extra.
    let size = models.first().map_or(file_size, |model| model.rom_size());
    let trailing_bytes = file_size - size;
    if models.is_empty() {
        reject_damaged_rom(source, file_size, &bytes)?;
        eprintln!(
            "warning: unrecognised ROM; no supported model can use it, so `macman create` will \
             refuse it"
        );
    }
    if trailing_bytes > 0 {
        eprintln!(
            "note: storing the {}K ROM and ignoring {trailing_bytes} extra bytes after it; your \
             file is unchanged",
            size / 1024
        );
    }

    before_copy();
    atomic_copy_range(source, 0, size, &dest)?;
    // Check what was stored, not just what was read, before registering it.
    let stored = read_rom_prefix(&dest)?;
    if stored.len() as u64 != size.min(MAX_ROM_SIZE) || genuine_models(&stored) != models {
        let _ = std::fs::remove_file(&dest);
        bail!(
            "{} changed while it was being imported; nothing was registered",
            source.display()
        );
    }
    let sha256 = sha256_file(&dest)?;
    let existing = list_roms(paths)?;
    if let Some(same) = existing.iter().find(|rom| rom.sha256 == sha256) {
        eprintln!("warning: this is the same ROM as {:?}", same.name);
    }
    let rom = Rom {
        name: name.to_string(),
        sha256,
        size,
        models,
        trailing_bytes,
        source_sha256: (trailing_bytes > 0)
            .then(|| sha256_file(source))
            .transpose()?,
    };
    write_toml(&sidecar(&dest), &rom)?;
    Ok(rom)
}

/// Fail for a file that claims to be a known ROM but isn't a valid one.
fn reject_damaged_rom(source: &Path, file_size: u64, bytes: &[u8]) -> Result<()> {
    let claimed = claimed_models(bytes);
    let Some(needed) = claimed.first().map(|model| model.rom_size()) else {
        return Ok(());
    };
    let names: Vec<_> = claimed.iter().map(|model| model.name()).collect();
    let names = names.join("/");
    if file_size < needed {
        bail!(
            "{} is truncated: it starts like a {names} ROM, but it is {file_size} bytes and needs \
             {}K",
            source.display(),
            needed / 1024
        );
    }
    bail!(
        "{} is corrupted: it starts like a {names} ROM, but its contents fail the ROM's checksum",
        source.display()
    );
}

pub fn add_system(
    paths: &Paths,
    source: &Path,
    name: &str,
    version: Option<&str>,
    partition: Option<u32>,
) -> Result<SystemImage> {
    validate_name("system", name)?;
    let dest = system_path(paths, name);
    if dest.exists() {
        bail!("a system image named {name:?} is already registered; pick another --name");
    }
    // Held throughout, so the map, the copied volume, and the recorded source
    // hash all describe one state of the source, for cooperating processes.
    let _guard = disk::lock_image(paths, source, LockMode::Shared)?;
    let import = disk::extract_volume(source, &dest, partition, false)?;
    if !disk::has_boot_blocks(&dest)? {
        eprintln!("warning: {name} has no boot blocks, so it cannot start up a machine");
    }
    let image = SystemImage {
        name: name.to_string(),
        sha256: sha256_file(&dest)?,
        size: import.length,
        version: version.map(str::to_string),
        source_sha256: import
            .extracted
            .is_some()
            .then(|| sha256_file(source))
            .transpose()?,
        extracted_from: import.extracted,
    };
    write_toml(&sidecar(&dest), &image)?;
    Ok(image)
}

pub fn list_roms(paths: &Paths) -> Result<Vec<Rom>> {
    load_all(&paths.roms())
}

pub fn list_systems(paths: &Paths) -> Result<Vec<SystemImage>> {
    load_all(&paths.systems())
}

fn load_all<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    list_dir(dir)?
        .iter()
        .filter(|name| name.ends_with(".toml"))
        .map(|name| read_toml(&dir.join(name)))
        .collect()
}

pub fn load_rom(paths: &Paths, name: &str) -> Result<(Rom, PathBuf)> {
    validate_name("ROM", name)?;
    let file = rom_path(paths, name);
    if !file.exists() {
        bail!("no ROM named {name:?}; import one with `macman rom add FILE --name {name}`");
    }
    Ok((read_toml(&sidecar(&file))?, file))
}

pub fn load_system(paths: &Paths, name: &str) -> Result<(SystemImage, PathBuf)> {
    validate_name("system", name)?;
    let file = system_path(paths, name);
    if !file.exists() {
        bail!(
            "no system image named {name:?}; register one with `macman system add FILE --name {name}`"
        );
    }
    Ok((read_toml(&sidecar(&file))?, file))
}

/// Confirm the stored file still matches what was registered.
pub fn verify(file: &Path, kind: &str, name: &str, sha256: &str) -> Result<()> {
    if sha256_file(file)? != sha256 {
        bail!("{kind} {name:?} changed since it was registered; remove it and import it again");
    }
    Ok(())
}

/// Refuse a ROM that cannot work for `model`; warn about one that may not.
pub fn check_rom_for_model(rom: &Rom, model: Model) -> Result<()> {
    let expected = model.rom_size();
    if rom.size != expected {
        bail!(
            "ROM {:?} is {}K, but model {model} needs a {}K ROM; {}",
            rom.name,
            rom.size / 1024,
            expected / 1024,
            import_hint(model)
        );
    }
    if rom.models.is_empty() {
        bail!(
            "ROM {:?} is unrecognised, and Mini vMac only starts ROMs it knows; {}",
            rom.name,
            import_hint(model)
        );
    }
    if !rom.models.contains(&model) {
        let names: Vec<_> = rom.models.iter().map(|m| m.name()).collect();
        bail!(
            "ROM {:?} is for {}, not model {model}; {}",
            rom.name,
            names.join("/"),
            import_hint(model)
        );
    }
    Ok(())
}

fn import_hint(model: Model) -> String {
    format!(
        "import a matching one with `macman rom add FILE --name NAME`, or pick another model than {model}"
    )
}

/// Used by `system list` and `doctor`.
pub fn describe_format(format: DiskFormat) -> &'static str {
    match format {
        DiskFormat::RawMfs => "raw MFS",
        DiskFormat::RawHfs => "raw HFS",
        DiskFormat::DiskCopy42 => "Disk Copy 4.2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(size: u64, models: Vec<Model>) -> Rom {
        Rom {
            name: "r".into(),
            sha256: String::new(),
            size,
            models,
            trailing_bytes: 0,
            source_sha256: None,
        }
    }

    #[test]
    fn rom_compatibility() {
        check_rom_for_model(&rom(128 * 1024, vec![Model::Plus]), Model::Plus).unwrap();
        // Unrecognised, right size: the emulator would refuse it.
        let err = check_rom_for_model(&rom(128 * 1024, vec![]), Model::Plus).unwrap_err();
        assert!(err.to_string().contains("unrecognised"), "{err}");
        // Right size, but recognisably another model's ROM.
        let err = check_rom_for_model(&rom(256 * 1024, vec![Model::Se]), Model::II).unwrap_err();
        assert!(err.to_string().contains("is for se"), "{err}");
        // Wrong size.
        assert!(check_rom_for_model(&rom(128 * 1024, vec![]), Model::II).is_err());
    }

    /// A valid synthetic ROM, summing to `checksum`, with no Apple code.
    fn synthetic_rom(checksum: u32, size: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; size];
        bytes[..4].copy_from_slice(&checksum.to_be_bytes());
        let mut remaining = checksum;
        for word in bytes[4..].as_chunks_mut::<2>().0 {
            let value = remaining.min(0xFFFF) as u16;
            *word = value.to_be_bytes();
            remaining -= u32::from(value);
        }
        bytes
    }

    const PLUS_V3: u32 = 0x4D1F_8172;
    const K128: usize = 128 * 1024;

    struct Import {
        dir: tempfile::TempDir,
        paths: Paths,
        source: PathBuf,
    }

    fn import(bytes: &[u8]) -> (Import, Result<Rom>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = dir.path().join("vMac.ROM");
        std::fs::write(&source, bytes).unwrap();
        let result = add_rom(&paths, &source, "r");
        (Import { dir, paths, source }, result)
    }

    #[test]
    fn stores_a_rom_as_is_when_it_has_no_extra_data() {
        let bytes = synthetic_rom(PLUS_V3, K128);
        let (env, result) = import(&bytes);
        let rom = result.unwrap();
        assert_eq!(rom.models, vec![Model::Mac512Ke, Model::Plus]);
        assert_eq!((rom.size, rom.trailing_bytes), (K128 as u64, 0));
        assert_eq!(rom.source_sha256, None);
        assert_eq!(
            std::fs::read(env.paths.roms().join("r.rom")).unwrap(),
            bytes
        );
    }

    #[test]
    fn trailing_data_is_not_stored_and_the_source_is_kept() {
        let mut bytes = synthetic_rom(PLUS_V3, K128);
        bytes.extend_from_slice(&[0xAA; 7504]);
        let (env, result) = import(&bytes);
        let rom = result.unwrap();

        assert_eq!(rom.size, K128 as u64);
        assert_eq!(rom.trailing_bytes, 7504);
        assert!(rom.models.contains(&Model::Plus));
        assert_eq!(
            rom.source_sha256.as_deref(),
            Some(sha256_file(&env.source).unwrap().as_str())
        );
        check_rom_for_model(&rom, Model::Plus).unwrap();

        let stored = env.paths.roms().join("r.rom");
        assert_eq!(std::fs::read(&stored).unwrap(), bytes[..K128]);
        assert_eq!(rom.sha256, sha256_file(&stored).unwrap());
        assert_eq!(std::fs::read(&env.source).unwrap(), bytes);
        drop(env.dir);
    }

    #[test]
    fn a_huge_trailing_section_needs_no_more_than_the_rom_read() {
        let mut bytes = synthetic_rom(PLUS_V3, K128);
        bytes.resize(2 * 1024 * 1024, 0xAA);
        let (_env, result) = import(&bytes);
        assert_eq!(
            result.unwrap().trailing_bytes,
            2 * 1024 * 1024 - K128 as u64
        );
    }

    #[test]
    fn rejects_truncated_and_corrupted_roms() {
        let bytes = synthetic_rom(PLUS_V3, K128);

        let (env, result) = import(&bytes[..100_000]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("truncated"), "{err}");
        assert!(list_roms(&env.paths).unwrap().is_empty());

        // The header alone must not be enough, with or without trailing data.
        let mut damaged = bytes;
        damaged[5000] ^= 1;
        let (_env, result) = import(&damaged);
        assert!(result.unwrap_err().to_string().contains("corrupted"));
        damaged.extend_from_slice(&[0xAA; 7504]);
        let (env, result) = import(&damaged);
        assert!(result.unwrap_err().to_string().contains("corrupted"));
        assert!(!env.paths.roms().join("r.rom").exists());
    }

    #[test]
    fn add_rom_locks_the_source_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        // Truncated, so reading first would report that instead.
        let source = dir.path().join("vMac.ROM");
        std::fs::write(&source, &synthetic_rom(PLUS_V3, K128)[..100_000]).unwrap();

        let held = disk::lock_image(&paths, &source, LockMode::Exclusive).unwrap();
        let err = add_rom(&paths, &source, "r").unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(held);

        let err = add_rom(&paths, &source, "r").unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    /// Import a valid ROM while `change` rewrites the source mid-import, as
    /// something ignoring macman's lock might.
    fn import_racing(change: impl FnOnce(&Path)) -> (tempfile::TempDir, Paths, Result<Rom>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = dir.path().join("vMac.ROM");
        std::fs::write(&source, synthetic_rom(PLUS_V3, K128)).unwrap();
        let result = add_rom_checked(&paths, &source, "r", || change(&source));
        (dir, paths, result)
    }

    fn assert_nothing_registered(paths: &Paths) {
        assert!(!paths.roms().join("r.rom").exists());
        assert!(!paths.roms().join("r.toml").exists());
        assert!(list_roms(paths).unwrap().is_empty());
    }

    #[test]
    fn a_source_changed_mid_import_is_not_registered() {
        let (_dir, paths, result) = import_racing(|source| {
            let mut bytes = std::fs::read(source).unwrap();
            bytes[5000] ^= 1;
            std::fs::write(source, bytes).unwrap();
        });
        let err = result.unwrap_err().to_string();
        assert!(err.contains("changed while it was being imported"), "{err}");
        assert_nothing_registered(&paths);
    }

    #[test]
    fn a_source_swapped_for_another_model_mid_import_is_not_registered() {
        let (_dir, paths, result) = import_racing(|source| {
            std::fs::write(source, synthetic_rom(0xB2E3_62A8, K128)).unwrap();
        });
        assert!(result.is_err());
        assert_nothing_registered(&paths);
    }

    #[test]
    fn a_source_cut_short_mid_import_is_not_registered() {
        let (_dir, paths, result) = import_racing(|source| {
            let bytes = std::fs::read(source).unwrap();
            std::fs::write(source, &bytes[..100_000]).unwrap();
        });
        assert!(result.is_err());
        assert_nothing_registered(&paths);
    }

    #[test]
    fn an_unrecognised_source_changed_mid_import_is_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = dir.path().join("mystery.rom");
        std::fs::write(&source, vec![7u8; K128]).unwrap();
        let result = add_rom_checked(&paths, &source, "r", || {
            std::fs::write(&source, synthetic_rom(PLUS_V3, K128)).unwrap();
        });
        assert!(result.is_err());
        assert_nothing_registered(&paths);
    }

    #[test]
    fn unrecognised_files_are_kept_whole_and_cannot_make_a_machine() {
        let bytes = vec![7u8; 138_576];
        let (env, result) = import(&bytes);
        let rom = result.unwrap();
        assert!(rom.models.is_empty());
        assert_eq!((rom.size, rom.trailing_bytes), (138_576, 0));
        assert_eq!(
            std::fs::read(env.paths.roms().join("r.rom")).unwrap(),
            bytes
        );
        assert!(check_rom_for_model(&rom, Model::Plus).is_err());
    }

    #[test]
    fn add_system_locks_the_source_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        // Unusable as a system, so planning first would report that instead.
        let source = dir.path().join("blank.dsk");
        std::fs::write(&source, vec![0u8; 4096]).unwrap();

        let held = disk::lock_image(&paths, &source, LockMode::Exclusive).unwrap();
        let err = add_system(&paths, &source, "s", None, None).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(held);

        let err = add_system(&paths, &source, "s", None, None).unwrap_err();
        assert!(err.to_string().contains("blank"), "{err}");
        assert!(list_systems(&paths).unwrap().is_empty());
    }
}
