//! The narrow boundary between MacMan and an emulator (design §7).

pub mod basilisk2;
pub mod macemu;
pub mod macemu_windows;
pub mod minivmac;
pub mod sheepshaver;
pub mod source;

use crate::disk::CdVolume;
use crate::machine::Machine;
use crate::registry::{self, Rom};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// A ready-to-run emulator binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    pub path: PathBuf,
    pub build_key: String,
    /// False when an existing cached build was reused.
    pub built: bool,
}

/// A CD image for one run, which the guest can't write to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdImage {
    pub path: PathBuf,
    pub volume: CdVolume,
}

/// A disk as presented to the emulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDisk {
    pub path: PathBuf,
    pub writable: bool,
}

pub struct LaunchRequest<'a> {
    pub machine: &'a Machine,
    pub executable: &'a Executable,
    pub rom: &'a Path,
    /// Boot disk first.
    pub disks: &'a [ResolvedDisk],
    /// CD images for this run, which the guest starts from and can't write to.
    pub cdroms: &'a [CdImage],
    /// Private working directory for this run.
    pub workdir: &'a Path,
    pub verbose: bool,
}

/// One line of `macman doctor` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: CheckLevel,
    pub message: String,
    /// What the user should do about it.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    Ok,
    Warn,
    Fail,
}

impl Check {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Ok,
            message: message.into(),
            fix: None,
        }
    }

    pub fn warn(message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Warn,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    pub fn fail(message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Fail,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }
}

pub trait EmulatorBackend {
    fn name(&self) -> &str;

    /// The revision this host builds, which new machines record.
    fn revision(&self) -> &str;

    /// The revision this host would build for a machine that records
    /// `recorded`, or why it can't stand in for that one. A machine's recorded
    /// revision says where it came from, so it never has to match this host.
    fn resolve_revision(&self, recorded: &str) -> Result<String>;

    /// Reject configurations the backend cannot honour, without side effects.
    fn validate(&self, machine: &Machine) -> Result<()>;

    /// Reject a disk list the backend cannot attach.
    fn validate_disks(&self, machine: &Machine, count: usize) -> Result<()>;

    /// Reject a host this backend can't run on. Unlike [`Self::validate`], this
    /// isn't part of importing, because a machine may just be passing through.
    fn check_host(&self) -> Result<()> {
        Ok(())
    }

    /// Reject a ROM that `machine` cannot start from. `file` is its contents.
    fn check_rom(&self, machine: &Machine, rom: &Rom, _file: &Path) -> Result<()> {
        registry::check_rom_for_model(rom, machine.hardware.require_model()?)
    }

    /// Reject a disk this backend can't attach safely in this mode.
    fn check_disk(&self, _path: &Path, _writable: bool) -> Result<()> {
        Ok(())
    }

    /// Whether a boot volume left mounted can stop the guest from starting at all.
    fn needs_unmounted_boot_volume(&self) -> bool {
        false
    }

    /// Whether the emulator finds the volume in a partitioned disk by itself.
    fn mounts_partitioned_disks(&self) -> bool {
        false
    }

    /// Whether a machine can start from a blank disk and take `run --cdrom`.
    fn installs_from_cd(&self) -> bool {
        false
    }

    /// Build or reuse the emulator for `machine`.
    fn prepare(&self, machine: &Machine, verbose: bool) -> Result<Executable>;

    fn launch(&self, request: &LaunchRequest<'_>) -> Result<ExitStatus>;

    /// Host-level checks for `macman doctor`.
    fn doctor(&self) -> Vec<Check>;
}

/// The machines' `[emulator] backend` values this binary can run.
pub struct Backends {
    backends: Vec<Box<dyn EmulatorBackend>>,
}

impl Backends {
    pub fn new(backends: Vec<Box<dyn EmulatorBackend>>) -> Self {
        Self { backends }
    }

    pub fn default_backend(&self) -> &dyn EmulatorBackend {
        self.backends[0].as_ref()
    }

    pub fn get(&self, name: &str) -> Result<&dyn EmulatorBackend> {
        self.backends
            .iter()
            .find(|backend| backend.name() == name)
            .map(|backend| backend.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown emulator backend {name:?}; this macman supports: {}",
                    self.names().join(", ")
                )
            })
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn EmulatorBackend> {
        self.backends.iter().map(|backend| backend.as_ref())
    }

    fn names(&self) -> Vec<&str> {
        self.backends.iter().map(|backend| backend.name()).collect()
    }
}
