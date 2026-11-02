//! Registered ROMs and system images. Each entry is the imported file plus a
//! TOML sidecar of metadata.

use crate::disk::{self, DiskFormat, LockMode};
use crate::fsutil::{
    atomic_copy, atomic_copy_range, list_dir, read_toml, sha256_file, validate_name, write_toml,
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
    /// Models whose known checksum matches; empty for an unrecognised ROM.
    #[serde(default)]
    pub models: Vec<Model>,
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

const STANDARD_ROM_SIZES: [u64; 4] = [64 * 1024, 128 * 1024, 256 * 1024, 512 * 1024];

fn rom_path(paths: &Paths, name: &str) -> PathBuf {
    paths.roms().join(format!("{name}.rom"))
}

fn system_path(paths: &Paths, name: &str) -> PathBuf {
    paths.systems().join(format!("{name}.dsk"))
}

fn sidecar(file: &Path) -> PathBuf {
    file.with_extension("toml")
}

/// Which models a ROM belongs to, from the checksum in its first 4 bytes.
fn identify_rom(path: &Path) -> Result<Vec<Model>> {
    let mut checksum = [0u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut checksum))
        .with_context(|| format!("cannot read ROM {}", path.display()))?;
    let checksum = u32::from_be_bytes(checksum);
    Ok(ALL_MODELS
        .into_iter()
        .filter(|model| model.rom_checksums().contains(&checksum))
        .collect())
}

pub fn add_rom(paths: &Paths, source: &Path, name: &str) -> Result<Rom> {
    validate_name("ROM", name)?;
    let dest = rom_path(paths, name);
    if dest.exists() {
        bail!("a ROM named {name:?} is already registered; pick another --name");
    }
    let size = std::fs::metadata(source)
        .with_context(|| format!("cannot read ROM {}", source.display()))?
        .len();
    let models = identify_rom(source)?;
    if !STANDARD_ROM_SIZES.contains(&size) {
        eprintln!(
            "warning: {size} bytes is not a size macman knows for a Macintosh ROM \
             (64K, 128K, 256K or 512K)"
        );
    }
    if models.is_empty() {
        eprintln!("warning: unrecognised ROM checksum; macman cannot tell which model it is for");
    }
    let sha256 = sha256_file(source)?;
    let existing = list_roms(paths)?;
    if let Some(same) = existing.iter().find(|rom| rom.sha256 == sha256) {
        eprintln!("warning: this is the same ROM as {:?}", same.name);
    }
    atomic_copy(source, &dest)?;
    let rom = Rom {
        name: name.to_string(),
        sha256,
        size,
        models,
    };
    write_toml(&sidecar(&dest), &rom)?;
    Ok(rom)
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
    let import = disk::plan_import(source, partition)?;
    atomic_copy_range(source, import.offset, import.length, &dest)?;
    // Check what was stored, not just the plan, before registering it.
    if let Err(error) = disk::detect_format(&dest) {
        let _ = std::fs::remove_file(&dest);
        return Err(error.context("the extracted volume is not usable; nothing was registered"));
    }
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
        eprintln!(
            "warning: ROM {:?} has an unrecognised checksum and may not work with model {model}",
            rom.name
        );
    } else if !rom.models.contains(&model) {
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
        }
    }

    #[test]
    fn rom_compatibility() {
        check_rom_for_model(&rom(128 * 1024, vec![Model::Plus]), Model::Plus).unwrap();
        // Unrecognised but the right size: a warning, not an error.
        check_rom_for_model(&rom(128 * 1024, vec![]), Model::Plus).unwrap();
        // Right size, but recognisably another model's ROM.
        let err = check_rom_for_model(&rom(256 * 1024, vec![Model::Se]), Model::II).unwrap_err();
        assert!(err.to_string().contains("is for se"), "{err}");
        // Wrong size.
        assert!(check_rom_for_model(&rom(128 * 1024, vec![]), Model::II).is_err());
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

    #[test]
    fn identifies_rom_from_leading_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rom");
        let mut bytes = vec![0u8; 256];
        bytes[..4].copy_from_slice(&0x9722_1136u32.to_be_bytes());
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(identify_rom(&file).unwrap(), vec![Model::II, Model::IIx]);
    }
}
