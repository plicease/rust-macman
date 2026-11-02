//! The `machine.toml` format (design §6) and per-user `config.toml`.

use crate::fsutil::{read_toml, write_toml};
use crate::model::Model;
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub schema_version: u32,
    pub name: String,
    pub emulator: EmulatorConfig,
    pub hardware: Hardware,
    pub display: Display,
    #[serde(default)]
    pub system: SystemInfo,
    #[serde(default)]
    pub disks: Vec<Disk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmulatorConfig {
    pub backend: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    pub model: Model,
    pub rom: String,
    pub memory: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub scale: u32,
    #[serde(default)]
    pub fullscreen: bool,
}

/// Descriptive metadata; never passed to an emulator.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub boot: bool,
}

impl Machine {
    pub fn load(paths: &Paths, name: &str) -> Result<Self> {
        let file = paths.machine(name).join("machine.toml");
        if !file.exists() {
            bail!(
                "no machine named {name:?}; see `macman list`, or create it with `macman create`"
            );
        }
        let machine: Machine = read_toml(&file)?;
        if machine.schema_version != SCHEMA_VERSION {
            bail!(
                "machine {name:?} uses schema version {}, but this macman reads version \
                 {SCHEMA_VERSION}; upgrade macman",
                machine.schema_version
            );
        }
        if machine.name != name {
            bail!(
                "machine directory {name:?} holds a definition named {:?}; make them match",
                machine.name
            );
        }
        Ok(machine)
    }

    pub fn save_to(&self, dir: &Path) -> Result<()> {
        write_toml(&dir.join("machine.toml"), self)
    }

    /// Absolute paths of the machine's disks in attachment order, boot disk first.
    pub fn disk_paths(&self, paths: &Paths) -> Result<Vec<(PathBuf, &Disk)>> {
        let dir = paths.machine(&self.name);
        let mut disks: Vec<_> = self.disks.iter().collect();
        disks.sort_by_key(|disk| !disk.boot);
        disks
            .into_iter()
            .map(|disk| {
                let path = Path::new(&disk.path);
                if !path.components().all(|c| matches!(c, Component::Normal(_))) {
                    bail!(
                        "machine {:?}: disk path {:?} must be relative to the machine directory",
                        self.name,
                        disk.path
                    );
                }
                Ok((dir.join(path), disk))
            })
            .collect()
    }
}

/// `config.toml`: the persistent selection and backend settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "MiniVmacConfig::is_empty")]
    pub minivmac: MiniVmacConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MiniVmacConfig {
    /// Local copy of the pinned source archive, for machines without network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_archive: Option<PathBuf>,
}

impl MiniVmacConfig {
    fn is_empty(&self) -> bool {
        self.source_archive.is_none()
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(Self::default());
        }
        read_toml(&file).context("cannot load macman configuration")
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_toml(&paths.config_file(), self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
schema_version = 1
name = "wombat-6"

[emulator]
backend = "minivmac"
revision = "pinned-source-identifier"

[hardware]
model = "plus"
rom = "plus-v3"
memory = "4M"

[display]
width = 512
height = 342
depth = 1
scale = 2
fullscreen = false

[system]
version = "6.0.8"  # descriptive metadata, not an emulator switch

[[disks]]
path = "system.dsk"
readonly = false
boot = true

[[disks]]
path = "data.dsk"
readonly = false
"#;

    #[test]
    fn design_example_round_trips() {
        let machine: Machine = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(machine.hardware.model, Model::Plus);
        assert_eq!(machine.disks.len(), 2);
        assert!(machine.disks[0].boot && !machine.disks[1].boot);
        let text = toml::to_string_pretty(&machine).unwrap();
        assert_eq!(toml::from_str::<Machine>(&text).unwrap(), machine);
    }

    #[test]
    fn boot_disk_sorts_first_and_parent_paths_are_refused() {
        let paths = Paths::under(Path::new("/x"));
        let mut machine: Machine = toml::from_str(EXAMPLE).unwrap();
        machine.disks.swap(0, 1);
        let disks = machine.disk_paths(&paths).unwrap();
        assert!(disks[0].0.ends_with("system.dsk"));

        for escape in ["../../other/system.dsk", "/etc/passwd"] {
            machine.disks[0].path = escape.into();
            assert!(machine.disk_paths(&paths).is_err());
        }
    }
}
