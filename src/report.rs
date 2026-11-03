//! What `--json` prints. Each command that reports data builds one of these,
//! and prints it as text or as JSON, so both say the same thing. They're the
//! public schema, documented in `docs/JSON.md`: add fields freely, but don't
//! rename or remove one, or change its type.
//!
//! Internal types aren't serialized as they are, except a machine's own
//! definition in [`MachineConfig`], which is its `machine.toml` as JSON.

use crate::archive::{ImportReport, Outcome, RomOutcome, SystemOutcome, SystemResult};
use crate::backend::{Check, CheckLevel};
use crate::disk::{DiskFormat, Partition};
use crate::machine::Machine;
use crate::media::{Media, MediaFormat, MediaKind};
use serde::Serialize;

/// Print `report` as one JSON document on stdout.
pub fn print_json(report: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

/// What a failed command prints on stdout under `--json`.
pub fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

#[derive(Debug, Serialize)]
pub struct MachineList {
    pub machines: Vec<MachineEntry>,
}

#[derive(Debug, Serialize)]
pub struct MachineEntry {
    pub name: String,
    pub current: bool,
    #[serde(flatten)]
    pub details: MachineDetails,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum MachineDetails {
    Readable {
        backend: String,
        /// The Mini vMac model, for Mini vMac machines.
        model: Option<String>,
        memory: String,
        display: DisplayReport,
        system_version: Option<String>,
        /// Whether a plain `run` uses a temporary copy of the boot disk.
        ephemeral: bool,
        /// The boot disk's capacity as the guest sees it; null if unknown.
        system_disk_capacity_bytes: Option<u64>,
        /// Host storage allocated to the machine's disks; null if unavailable.
        host_disk_used_bytes: Option<u64>,
    },
    Unreadable {
        error: String,
    },
}

#[derive(Debug, Serialize)]
pub struct DisplayReport {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub scale: u32,
    pub fullscreen: bool,
}

#[derive(Debug, Serialize)]
pub struct BuildList {
    pub builds: Vec<BuildEntry>,
}

#[derive(Debug, Serialize)]
pub struct BuildEntry {
    /// The whole build key, which names its directory in the cache.
    pub key: String,
    #[serde(flatten)]
    pub details: BuildDetails,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BuildDetails {
    Valid {
        backend: String,
        revision: String,
        options: Vec<String>,
    },
    Invalid {
        error: String,
    },
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    /// Whether nothing failed; `doctor` exits 1 otherwise.
    pub ok: bool,
    pub checks: Vec<CheckReport>,
}

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub status: Status,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Error,
}

impl DoctorReport {
    pub fn new(checks: &[Check]) -> Self {
        Self {
            ok: !checks.iter().any(|check| check.level == CheckLevel::Fail),
            checks: checks
                .iter()
                .map(|check| CheckReport {
                    status: match check.level {
                        CheckLevel::Ok => Status::Ok,
                        CheckLevel::Warn => Status::Warning,
                        CheckLevel::Fail => Status::Error,
                    },
                    message: check.message.clone(),
                    fix: check.fix.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RomList {
    pub roms: Vec<RomEntry>,
}

#[derive(Debug, Serialize)]
pub struct RomEntry {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    /// Mini vMac models this is a genuine ROM for.
    pub models: Vec<String>,
    /// The first of `emulators`, kept for scripts written before it.
    pub backend: Option<&'static str>,
    /// Every backend whose ROM check it passes; empty if none.
    pub emulators: Vec<&'static str>,
    /// Host storage allocated to the stored ROM; null if unavailable.
    pub host_disk_used_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SystemList {
    pub systems: Vec<SystemEntry>,
}

#[derive(Debug, Serialize)]
pub struct SystemEntry {
    pub name: String,
    pub version: Option<String>,
    pub size: u64,
    pub sha256: String,
    /// None if the stored image can't be identified.
    pub format: Option<Format>,
    /// Host storage allocated to the stored image; null if unavailable.
    pub host_disk_used_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MediaList {
    pub media: Vec<MediaEntry>,
}

#[derive(Debug, Serialize)]
pub struct MediaEntry {
    pub name: String,
    pub kind: MediaKind,
    /// Whether `kind` was given when it was registered, rather than found.
    pub kind_given: bool,
    pub format: MediaFormat,
    /// Whether some volume on it has boot blocks.
    pub bootable: bool,
    pub size: u64,
    pub sha256: String,
    pub version: Option<String>,
    /// The name of the file it was registered from.
    pub source: Option<String>,
    /// Host storage allocated to the stored image; null if unavailable.
    pub host_disk_used_bytes: Option<u64>,
}

impl MediaEntry {
    pub fn new(media: Media, host_disk_used_bytes: Option<u64>) -> Self {
        Self {
            name: media.name,
            kind: media.kind,
            kind_given: media.kind_given,
            format: media.format,
            bootable: media.bootable,
            size: media.size,
            sha256: media.sha256,
            version: media.version,
            source: media.source,
            host_disk_used_bytes,
        }
    }
}

/// `macman du`: everything registered, smallest first.
#[derive(Debug, Serialize)]
pub struct DiskUsage {
    pub entries: Vec<UsageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: UsageKind,
    pub host_disk_used_bytes: Option<u64>,
}

/// Declared in the order `du` breaks ties in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageKind {
    Machine,
    Rom,
    System,
    Media,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Rom => "rom",
            Self::System => "system",
            Self::Media => "media",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MediaInfo {
    pub media: MediaEntry,
    /// Whether the stored image still matches its digest.
    pub verified: bool,
    #[serde(flatten)]
    pub layout: Layout,
}

/// A volume or image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Mfs,
    Hfs,
    DiskCopy42,
}

impl From<DiskFormat> for Format {
    fn from(format: DiskFormat) -> Self {
        match format {
            DiskFormat::RawMfs => Format::Mfs,
            DiskFormat::RawHfs => Format::Hfs,
            DiskFormat::DiskCopy42 => Format::DiskCopy42,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DiskInfo {
    /// The path as it was given.
    pub file: String,
    pub size: u64,
    #[serde(flatten)]
    pub layout: Layout,
}

#[derive(Debug, Serialize)]
#[serde(tag = "layout", rename_all = "snake_case")]
pub enum Layout {
    /// A volume with nothing around it.
    Volume {
        volume: Format,
    },
    DiskCopy42 {
        data_offset: u64,
        data_length: u64,
        volume: Option<Format>,
    },
    Partitioned {
        partitions: Vec<PartitionReport>,
    },
    Blank,
    /// Known, and known not to work.
    Unsupported {
        description: String,
    },
    Unrecognized,
}

#[derive(Debug, Serialize)]
pub struct PartitionReport {
    pub index: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: u64,
    pub length: u64,
    pub in_bounds: bool,
    pub volume: Option<Format>,
}

impl From<&Partition> for PartitionReport {
    fn from(partition: &Partition) -> Self {
        Self {
            index: partition.index,
            kind: partition.kind.clone(),
            offset: partition.offset,
            length: partition.length,
            in_bounds: partition.in_bounds,
            volume: partition.volume.map(Format::from),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MachineConfig {
    pub machine: Machine,
}

#[derive(Debug, Serialize)]
pub struct Current {
    pub current: String,
}

/// `import --dry-run`: what an import would do. With `dry_run` false, the same
/// fields would say what one did.
#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub dry_run: bool,
    pub roms: Vec<RomImport>,
    pub systems: Vec<SystemImport>,
    /// As for system images.
    pub media: Vec<SystemImport>,
    pub machines: Vec<MachineImport>,
    pub imported: usize,
    pub unchanged: usize,
    pub unresolved: usize,
}

#[derive(Debug, Serialize)]
pub struct RomImport {
    /// Its name in the archive.
    pub name: String,
    pub outcome: RomStatus,
    /// The registered ROM the machines use, when it's already here.
    pub local: Option<String>,
    /// Why, for a conflict or failure.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RomStatus {
    Imported,
    Recovered,
    Unchanged,
    Reused,
    Conflict,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct SystemImport {
    pub name: String,
    pub outcome: SystemStatus,
    /// Why, for a conflict or failure.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemStatus {
    Imported,
    Recovered,
    Unchanged,
    Conflict,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct MachineImport {
    /// The name it has, or would have, here.
    pub name: String,
    pub outcome: ImportStatus,
    /// Why, for a conflict or failure.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportStatus {
    Imported,
    Unchanged,
    Conflict,
    Failed,
}

impl From<&ImportReport> for ImportResult {
    fn from(report: &ImportReport) -> Self {
        Self {
            dry_run: report.dry_run,
            roms: report
                .roms
                .iter()
                .map(|(name, outcome)| {
                    let (outcome, local, reason) = match outcome {
                        RomOutcome::Imported => (RomStatus::Imported, None, None),
                        RomOutcome::Recovered => (RomStatus::Recovered, Some(name.clone()), None),
                        RomOutcome::Unchanged { local } => {
                            (RomStatus::Unchanged, Some(local.clone()), None)
                        }
                        RomOutcome::Reused { local } => {
                            (RomStatus::Reused, Some(local.clone()), None)
                        }
                        RomOutcome::Conflict(reason) => {
                            (RomStatus::Conflict, None, Some(reason.clone()))
                        }
                        RomOutcome::Failed(reason) => {
                            (RomStatus::Failed, None, Some(reason.clone()))
                        }
                    };
                    RomImport {
                        name: name.clone(),
                        outcome,
                        local,
                        reason,
                    }
                })
                .collect(),
            systems: report.systems.iter().map(system_import).collect(),
            media: report.media.iter().map(system_import).collect(),
            machines: report
                .machines
                .iter()
                .map(|machine| {
                    let (outcome, reason) = match &machine.outcome {
                        Outcome::Imported => (ImportStatus::Imported, None),
                        Outcome::Unchanged => (ImportStatus::Unchanged, None),
                        Outcome::Conflict(reason) => (ImportStatus::Conflict, Some(reason.clone())),
                        Outcome::Failed(reason) => (ImportStatus::Failed, Some(reason.clone())),
                    };
                    MachineImport {
                        name: machine.name.clone(),
                        outcome,
                        reason,
                    }
                })
                .collect(),
            imported: report.imported(),
            unchanged: report.unchanged(),
            unresolved: report.unresolved(),
        }
    }
}

fn system_import(system: &SystemResult) -> SystemImport {
    let (outcome, reason) = match &system.outcome {
        SystemOutcome::Imported => (SystemStatus::Imported, None),
        SystemOutcome::Recovered => (SystemStatus::Recovered, None),
        SystemOutcome::Unchanged => (SystemStatus::Unchanged, None),
        SystemOutcome::Conflict(reason) => (SystemStatus::Conflict, Some(reason.clone())),
        SystemOutcome::Failed(reason) => (SystemStatus::Failed, Some(reason.clone())),
    };
    SystemImport {
        name: system.name.clone(),
        outcome,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_docs_json_md() {
        assert_eq!(
            serde_json::to_value(Format::DiskCopy42).unwrap(),
            "disk_copy42"
        );
        let layout = serde_json::to_value(Layout::DiskCopy42 {
            data_offset: 84,
            data_length: 0,
            volume: None,
        })
        .unwrap();
        assert_eq!(layout["layout"], "disk_copy42");
        assert_eq!(serde_json::to_value(Status::Warning).unwrap(), "warning");
    }
}
