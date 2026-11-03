//! Command implementations.

use crate::archive::{self, Outcome, RomOutcome, SystemOutcome};
use crate::backend::minivmac::{MANIFEST, Manifest};
use crate::backend::{
    Backends, CdImage, Check, CheckLevel, EmulatorBackend, LaunchRequest, ResolvedDisk,
};
use crate::backend::{basilisk2, minivmac, sheepshaver};
use crate::checkpoint;
use crate::cli::{
    CheckpointCommand, Cli, Command, ConfigCommand, CreateArgs, DiskCommand, MediaCommand,
    NameKind, RomCommand, RunArgs, SystemCommand,
};
use crate::disk::{self, Classification, DiskFormat, ImageLock, LockMode, Session, SessionRecord};
use crate::fsutil::{
    atomic_copy, list_dir, parse_disk_size, read_toml, sha256_file, validate_name,
    without_verbatim_prefix,
};
use crate::machine::{
    Basilisk2Settings, Config, Disk, Display, EmulatorConfig, Hardware, Machine, SCHEMA_VERSION,
    SheepShaverSettings, SystemInfo, new_id, publish_dir,
};
use crate::media::{self, MediaFormat, MediaKind};
use crate::model::{Model, parse_display};
use crate::paths::Paths;
use crate::registry::{self, PowerPcRom};
use crate::report::{
    BuildDetails, BuildEntry, BuildList, CheckpointEntry, CheckpointList, Current, DiskInfo,
    DiskUsage, DisplayReport, DoctorReport, Format, ImportResult, Layout, MachineConfig,
    MachineDetails, MachineEntry, MachineList, MediaEntry, MediaInfo, MediaList, PartitionReport,
    RomEntry, RomList, SystemEntry, SystemList, UsageEntry, UsageKind, print_json,
};
use crate::usage;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const SYSTEM_DISK: &str = "system.dsk";

/// A disk attached for one run, given by path or as registered media.
struct ExtraDisk {
    path: PathBuf,
    writable: bool,
    any_format: bool,
}
/// How a run holds its CD images. The emulators open them read-only, so runs
/// may share them, except on macOS, where each emulator takes an exclusive lock
/// on every image it opens, and one started second would quietly go without.
const CD_LOCK: LockMode = if cfg!(target_os = "macos") {
    LockMode::Exclusive
} else {
    LockMode::Shared
};

fn print_table(rows: &[Vec<String>], right_aligned: &[usize]) {
    for line in table(rows, right_aligned) {
        println!("{line}");
    }
}

/// Lays out rows in columns two spaces apart. The columns in `right_aligned` are padded on
/// the left. A row's last cell runs on, so it neither pads nor widens its column: that keeps
/// long options or an error from pushing every other row across.
fn table(rows: &[Vec<String>], right_aligned: &[usize]) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (column, cell) in row.iter().enumerate().take(row.len().saturating_sub(1)) {
            if column == widths.len() {
                widths.push(0);
            }
            widths[column] = widths[column].max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            let cells = row.iter().enumerate().map(|(column, cell)| {
                let width = widths.get(column).copied().unwrap_or(0);
                if column + 1 == row.len() {
                    cell.clone()
                } else if right_aligned.contains(&column) {
                    format!("{cell:>width$}")
                } else {
                    format!("{cell:<width$}")
                }
            });
            cells.collect::<Vec<_>>().join("  ").trim_end().to_string()
        })
        .collect()
}

pub struct App {
    pub paths: Paths,
    pub backends: Backends,
}

impl App {
    pub fn new(paths: Paths, backends: Backends) -> Self {
        Self { paths, backends }
    }

    /// Run a command, returning the process exit code.
    pub fn execute(&self, cli: Cli) -> Result<u8> {
        let verbose = cli.verbose;
        let json = cli.json;
        if json && let Some(command) = without_json(&cli.command) {
            let hint = if command == "import" {
                ", or preview the import with --dry-run"
            } else {
                ""
            };
            bail!(
                "--json isn't supported by `macman {command}`, which changes things rather than \
                 reporting them; leave it off{hint}"
            );
        }
        match cli.command {
            Command::Doctor => return self.doctor(verbose, json),
            Command::List { builds: false } => self.list_machines(json)?,
            Command::List { builds: true } => self.list_builds(json)?,
            Command::Rom(RomCommand::Add { file, name }) => {
                let rom = registry::add_rom(&self.paths, &file, &name)?;
                println!("Imported ROM {} ({})", rom.name, rom.sha256);
            }
            Command::Rom(RomCommand::List) => self.list_roms(json)?,
            Command::System(SystemCommand::Add {
                file,
                name,
                version,
                partition,
            }) => {
                let image =
                    registry::add_system(&self.paths, &file, &name, version.as_deref(), partition)?;
                match image.extracted_from {
                    Some(from) => println!(
                        "Registered system image {name}, extracted from {from} ({}K)",
                        image.size / 1024
                    ),
                    None => println!("Registered system image {name}"),
                }
            }
            Command::System(SystemCommand::List) => self.list_systems(json)?,
            Command::System(SystemCommand::Remove { name, yes }) => {
                self.remove_system(&name, yes)?
            }
            Command::Media(MediaCommand::Add {
                file,
                name,
                kind,
                version,
                any_format,
            }) => {
                let options = media::AddOptions {
                    kind,
                    version: version.as_deref(),
                    any_format,
                };
                let medium = media::add_media(&self.paths, &file, &name, &options)?;
                println!(
                    "Registered media {name}: {}, {}, {}",
                    medium.kind.as_str(),
                    medium.format.describe(),
                    if medium.bootable {
                        "with boot blocks"
                    } else {
                        "no boot blocks"
                    }
                );
                if medium.format == MediaFormat::Unrecognized {
                    eprintln!(
                        "note: macman doesn't recognize what's on {name}, so only an emulator \
                         can tell whether it works"
                    );
                }
            }
            Command::Media(MediaCommand::List) => self.list_media(json)?,
            Command::Media(MediaCommand::Info { name }) => return self.media_info(&name, json),
            Command::Media(MediaCommand::Remove { name, yes }) => self.remove_media(&name, yes)?,
            Command::Disk(DiskCommand::Info { file }) => self.disk_info(&file, json)?,
            Command::Disk(DiskCommand::Extract {
                source,
                dest,
                partition,
                raw,
            }) => self.disk_extract(&source, &dest, partition, raw)?,
            Command::Create(args) => {
                self.create(args)?;
            }
            Command::Use { name } => self.select(&name)?,
            Command::Current if json => print_json(&Current {
                current: self.current()?,
            })?,
            Command::Current => println!("{}", self.current()?),
            Command::Run(args) => return self.run(&args, verbose),
            Command::Config(ConfigCommand::Show { name }) => self.config_show(name, json)?,
            Command::Config(ConfigCommand::Edit { name }) => self.config_edit(name)?,
            Command::Checkpoint(CheckpointCommand::Create { name, checkpoint }) => {
                self.checkpoint_create(name, checkpoint.as_deref())?
            }
            Command::Checkpoint(CheckpointCommand::List { name }) => {
                self.checkpoint_list(name, json)?
            }
            Command::Checkpoint(CheckpointCommand::Delete {
                name,
                checkpoint,
                yes,
            }) => self.checkpoint_delete(name, &checkpoint, yes)?,
            Command::Rollback {
                name,
                checkpoint,
                save,
                yes,
            } => self.rollback(name, &checkpoint, save.as_deref(), yes)?,
            Command::Clone { source, dest } => self.clone_machine(&source, &dest)?,
            Command::Export {
                names,
                all,
                systems,
                all_systems,
                media,
                all_media,
                output,
            } => {
                let every_system = || -> Result<Vec<String>> {
                    Ok(registry::list_systems(&self.paths)?
                        .into_iter()
                        .map(|system| system.name)
                        .collect())
                };
                let selection = archive::Selection {
                    machines: if all {
                        list_dir(&self.paths.machines())?
                    } else {
                        names
                    },
                    systems: if all || all_systems {
                        every_system()?
                    } else {
                        systems
                    },
                    media: if all || all_media {
                        media::list_media(&self.paths)?
                            .into_iter()
                            .map(|medium| medium.name)
                            .collect()
                    } else {
                        media
                    },
                    all_roms: all,
                };
                let empty = selection.machines.is_empty()
                    && selection.systems.is_empty()
                    && selection.media.is_empty();
                if all && empty && registry::list_roms(&self.paths)?.is_empty() {
                    bail!("there are no machines, ROMs, system images, or media to export");
                }
                if !all && empty {
                    bail!("there are no machines, system images, or media to export");
                }
                let exported = archive::export(&self.paths, &selection, &output)?;
                println!(
                    "Exported {} machine(s), {} ROM(s), {} system image(s), and {} media \
                     to {}",
                    exported.machines,
                    exported.roms,
                    exported.systems,
                    exported.media,
                    output.display()
                );
            }
            Command::Import {
                file,
                rename,
                dry_run,
            } => {
                return self.import(&file, rename.as_deref(), dry_run, json);
            }
            Command::Remove { name, delete_disks } => self.remove(&name, delete_disks)?,
            Command::Du => self.disk_usage(json)?,
            Command::Completions { shell } => return print_completions(shell, json),
        }
        Ok(0)
    }

    fn import(&self, file: &Path, rename: Option<&str>, dry_run: bool, json: bool) -> Result<u8> {
        let report = archive::import(&self.paths, &self.backends, file, rename, dry_run)?;
        let code = u8::from(report.unresolved() > 0);
        if json {
            print_json(&ImportResult::from(&report))?;
            return Ok(code);
        }
        for (name, outcome) in &report.roms {
            match outcome {
                RomOutcome::Imported if dry_run => println!("would import rom {name}"),
                RomOutcome::Imported => println!("imported rom {name}"),
                RomOutcome::Recovered if dry_run => println!("would recover rom {name}"),
                RomOutcome::Recovered => println!("recovered rom {name}"),
                RomOutcome::Unchanged { local } => println!("unchanged rom {local}"),
                RomOutcome::Reused { local } if dry_run => {
                    println!("would reuse rom {name} as {local}");
                }
                RomOutcome::Reused { local } => println!("reused rom {name} as {local}"),
                RomOutcome::Conflict(reason) => println!("conflict rom {name}: {reason}"),
                RomOutcome::Failed(reason) => println!("failed rom {name}: {reason}"),
            }
        }
        for result in &report.systems {
            let name = &result.name;
            match &result.outcome {
                SystemOutcome::Imported if dry_run => println!("would import system {name}"),
                SystemOutcome::Imported => println!("imported system {name}"),
                SystemOutcome::Recovered if dry_run => println!("would recover system {name}"),
                SystemOutcome::Recovered => println!("recovered system {name}"),
                SystemOutcome::Unchanged => println!("unchanged system {name}"),
                SystemOutcome::Conflict(reason) => println!("conflict system {name}: {reason}"),
                SystemOutcome::Failed(reason) => println!("failed system {name}: {reason}"),
            }
        }
        for result in &report.media {
            let name = &result.name;
            match &result.outcome {
                SystemOutcome::Imported if dry_run => println!("would import media {name}"),
                SystemOutcome::Imported => println!("imported media {name}"),
                SystemOutcome::Recovered if dry_run => println!("would recover media {name}"),
                SystemOutcome::Recovered => println!("recovered media {name}"),
                SystemOutcome::Unchanged => println!("unchanged media {name}"),
                SystemOutcome::Conflict(reason) => println!("conflict media {name}: {reason}"),
                SystemOutcome::Failed(reason) => println!("failed media {name}: {reason}"),
            }
        }
        for result in &report.machines {
            let name = &result.name;
            match &result.outcome {
                Outcome::Imported if dry_run => println!("would import machine {name}"),
                Outcome::Imported => println!("imported machine {name}"),
                Outcome::Unchanged => println!("unchanged machine {name}"),
                Outcome::Conflict(reason) => println!("conflict machine {name}: {reason}"),
                Outcome::Failed(reason) => println!("failed machine {name}: {reason}"),
            }
        }
        if dry_run {
            println!(
                "dry run: {} would be imported, {} unchanged, {} unresolved; nothing was changed",
                report.imported(),
                report.unchanged(),
                report.unresolved()
            );
            println!("(this is how an import would go now; changes before then may change it)");
        } else {
            println!(
                "{} imported, {} unchanged, {} unresolved",
                report.imported(),
                report.unchanged(),
                report.unresolved()
            );
        }
        Ok(code)
    }

    fn current(&self) -> Result<String> {
        Config::load(&self.paths)?
            .current
            .context("no current machine; pick one with `macman use NAME`")
    }

    fn resolve_name(&self, name: Option<String>) -> Result<String> {
        let name = match name {
            Some(name) => name,
            None => self.current()?,
        };
        validate_name("machine", &name)?;
        Ok(name)
    }

    fn select(&self, name: &str) -> Result<()> {
        validate_name("machine", name)?;
        Machine::load(&self.paths, name)?;
        let mut config = Config::load(&self.paths)?;
        config.current = Some(name.to_string());
        config.save(&self.paths)?;
        println!("Now using {name}");
        Ok(())
    }

    fn list_machines(&self, json: bool) -> Result<()> {
        let current = Config::load(&self.paths)?.current;
        let mut machines = Vec::new();
        for name in list_dir(&self.paths.machines())? {
            let details = match Machine::load(&self.paths, &name) {
                Ok(m) => MachineDetails::Readable {
                    backend: m.emulator.backend.clone(),
                    model: m.hardware.model.map(|model| model.to_string()),
                    memory: m.hardware.memory.clone(),
                    display: DisplayReport {
                        width: m.display.width,
                        height: m.display.height,
                        depth: m.display.depth,
                        scale: m.display.scale,
                        fullscreen: m.display.fullscreen,
                    },
                    system_version: m.system.version.clone(),
                    ephemeral: m.ephemeral,
                    system_disk_capacity_bytes: usage::boot_disk_capacity(&self.paths, &m),
                    host_disk_used_bytes: usage::machine_used(&self.paths, &m),
                },
                Err(e) => MachineDetails::Unreadable {
                    error: format!("{e:#}"),
                },
            };
            machines.push(MachineEntry {
                current: current.as_deref() == Some(&name),
                name,
                details,
            });
        }
        if json {
            return print_json(&MachineList { machines });
        }
        let mut rows = Vec::new();
        for machine in machines {
            let marker = if machine.current { '*' } else { ' ' };
            let label = format!("{marker} {}", machine.name);
            rows.push(match machine.details {
                MachineDetails::Readable {
                    backend,
                    model,
                    memory,
                    display,
                    system_version,
                    ephemeral,
                    system_disk_capacity_bytes,
                    host_disk_used_bytes,
                } => vec![
                    label,
                    model.unwrap_or(backend),
                    memory,
                    format!("{}x{}x{}", display.width, display.height, display.depth),
                    format!("disk {}", usage::format_usage(system_disk_capacity_bytes)),
                    format!("used {}", usage::format_usage(host_disk_used_bytes)),
                    if ephemeral { "ephemeral" } else { "persistent" }.to_string(),
                    format!("system {}", system_version.as_deref().unwrap_or("-")),
                ],
                MachineDetails::Unreadable { error } => {
                    vec![label, format!("(unreadable: {error})")]
                }
            });
        }
        print_table(&rows, &[2]);
        Ok(())
    }

    fn list_builds(&self, json: bool) -> Result<()> {
        let mut builds = Vec::new();
        for key in list_dir(&self.paths.builds())? {
            let details =
                match read_toml::<Manifest>(&self.paths.builds().join(&key).join(MANIFEST)) {
                    Ok(m) => BuildDetails::Valid {
                        backend: m.backend,
                        revision: m.revision,
                        options: m.options,
                    },
                    Err(_) => BuildDetails::Invalid {
                        error: "no valid manifest".to_string(),
                    },
                };
            builds.push(BuildEntry { key, details });
        }
        if json {
            return print_json(&BuildList { builds });
        }
        let rows: Vec<Vec<String>> = builds
            .into_iter()
            .map(|build| match build.details {
                BuildDetails::Valid {
                    backend,
                    revision,
                    options,
                } => vec![
                    build.key[..12.min(build.key.len())].to_string(),
                    backend,
                    revision,
                    options.join(" "),
                ],
                // One cell, as the whole key would widen the column for every other row.
                BuildDetails::Invalid { error } => vec![format!("{}  ({error})", build.key)],
            })
            .collect();
        print_table(&rows, &[]);
        Ok(())
    }

    fn list_roms(&self, json: bool) -> Result<()> {
        let mut roms = Vec::new();
        let mut rows = Vec::new();
        for rom in registry::list_roms(&self.paths)? {
            let file = self.paths.roms().join(format!("{}.rom", rom.name));
            // Each backend's own test, so a ROM two of them take is listed for both.
            let clean = registry::rom_file_is_32_bit_clean(&file, rom.size)?;
            let powerpc = registry::rom_file_powerpc(&file, rom.size)?;
            let mut kinds = Vec::new();
            if !rom.models.is_empty() {
                kinds.push(
                    rom.models
                        .iter()
                        .map(|m| m.name())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
            if clean {
                kinds.push("32-bit clean".to_string());
            }
            match powerpc {
                Some(PowerPcRom::OldWorld) => kinds.push("PowerPC Old World".to_string()),
                Some(PowerPcRom::NewWorld) => kinds.push("PowerPC New World".to_string()),
                None => {}
            }
            let emulators: Vec<&'static str> = [
                (!rom.models.is_empty(), minivmac::BACKEND_NAME),
                (clean, basilisk2::BACKEND_NAME),
                (powerpc.is_some(), sheepshaver::BACKEND_NAME),
            ]
            .into_iter()
            .filter_map(|(takes, backend)| takes.then_some(backend))
            .collect();
            let used = usage::allocated(&file);
            rows.push(vec![
                rom.name.clone(),
                format!("{}K", rom.size / 1024),
                format!("used {}", usage::format_usage(used)),
                if kinds.is_empty() {
                    "unrecognised".to_string()
                } else {
                    kinds.join(", ")
                },
                rom.sha256[..12].to_string(),
                if emulators.is_empty() {
                    "unknown".to_string()
                } else {
                    emulators.join("/")
                },
            ]);
            roms.push(RomEntry {
                models: rom.models.iter().map(|m| m.name().to_string()).collect(),
                name: rom.name,
                size: rom.size,
                sha256: rom.sha256,
                backend: emulators.first().copied(),
                emulators,
                host_disk_used_bytes: used,
            });
        }
        if json {
            return print_json(&RomList { roms });
        }
        print_table(&rows, &[1]);
        Ok(())
    }

    fn list_systems(&self, json: bool) -> Result<()> {
        let mut systems = Vec::new();
        for system in registry::list_systems(&self.paths)? {
            let (_, file) = registry::load_system(&self.paths, &system.name)?;
            systems.push(SystemEntry {
                format: disk::detect_format(&file).ok().map(Format::from),
                name: system.name,
                version: system.version,
                size: system.size,
                sha256: system.sha256,
                host_disk_used_bytes: usage::allocated(&file),
            });
        }
        if json {
            return print_json(&SystemList { systems });
        }
        let rows: Vec<Vec<String>> = systems
            .into_iter()
            .map(|system| {
                let format = match system.format {
                    Some(Format::Mfs) => "raw MFS",
                    Some(Format::Hfs) => "raw HFS",
                    Some(Format::DiskCopy42) => "Disk Copy 4.2",
                    None => "unknown format",
                };
                vec![
                    system.name,
                    system.version.unwrap_or_else(|| "-".to_string()),
                    format!("{}K", system.size / 1024),
                    format!("used {}", usage::format_usage(system.host_disk_used_bytes)),
                    format.to_string(),
                ]
            })
            .collect();
        print_table(&rows, &[2]);
        Ok(())
    }

    fn list_media(&self, json: bool) -> Result<()> {
        let media: Vec<MediaEntry> = media::list_media(&self.paths)?
            .into_iter()
            .map(|medium| {
                let used = usage::allocated(&media::media_file(&self.paths, &medium.name));
                MediaEntry::new(medium, used)
            })
            .collect();
        if json {
            return print_json(&MediaList { media });
        }
        let rows: Vec<Vec<String>> = media
            .into_iter()
            .map(|medium| {
                vec![
                    medium.name,
                    medium.kind.as_str().to_string(),
                    format!("{}K", medium.size / 1024),
                    format!("used {}", usage::format_usage(medium.host_disk_used_bytes)),
                    medium.format.describe().to_string(),
                    if medium.bootable { "boot blocks" } else { "-" }.to_string(),
                    medium.version.unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect();
        print_table(&rows, &[2]);
        Ok(())
    }

    /// Everything registered, by what it takes on the host, smallest first:
    /// measured entries by size, then type and name; unavailable ones last.
    fn disk_usage(&self, json: bool) -> Result<()> {
        let mut entries = Vec::new();
        for name in list_dir(&self.paths.machines())? {
            let used = Machine::load(&self.paths, &name)
                .ok()
                .and_then(|machine| usage::machine_used(&self.paths, &machine));
            entries.push(UsageEntry {
                name,
                kind: UsageKind::Machine,
                host_disk_used_bytes: used,
            });
        }
        for rom in registry::list_roms(&self.paths)? {
            let file = self.paths.roms().join(format!("{}.rom", rom.name));
            entries.push(UsageEntry {
                host_disk_used_bytes: usage::allocated(&file),
                name: rom.name,
                kind: UsageKind::Rom,
            });
        }
        for system in registry::list_systems(&self.paths)? {
            entries.push(UsageEntry {
                host_disk_used_bytes: usage::allocated(&registry::system_file(
                    &self.paths,
                    &system.name,
                )),
                name: system.name,
                kind: UsageKind::System,
            });
        }
        for medium in media::list_media(&self.paths)? {
            entries.push(UsageEntry {
                host_disk_used_bytes: usage::allocated(&media::media_file(
                    &self.paths,
                    &medium.name,
                )),
                name: medium.name,
                kind: UsageKind::Media,
            });
        }
        sort_usage(&mut entries);
        if json {
            return print_json(&DiskUsage { entries });
        }
        let used: Vec<String> = entries
            .iter()
            .map(|entry| usage::format_usage(entry.host_disk_used_bytes))
            .collect();
        let width = used
            .iter()
            .map(|text| text.chars().count())
            .chain(["DISK USED".len()])
            .max()
            .unwrap_or(0);
        let mut rows = vec![vec![
            "NAME".to_string(),
            "TYPE".to_string(),
            format!("{:>width$}", "DISK USED"),
        ]];
        for (entry, used) in entries.into_iter().zip(used) {
            rows.push(vec![
                entry.name,
                entry.kind.as_str().to_string(),
                format!("{used:>width$}"),
            ]);
        }
        print_table(&rows, &[]);
        Ok(())
    }

    fn media_info(&self, name: &str, json: bool) -> Result<u8> {
        let (medium, file) = media::load_media(&self.paths, name)?;
        let _guard = disk::lock_image(&self.paths, &file, LockMode::Shared)?;
        let verified = sha256_file(&file)? == medium.sha256;
        let layout = layout_of(&file)?;
        let changed = || {
            anyhow::anyhow!(
                "media {name:?} doesn't match its digest, so it changed since it was \
                 registered; remove it and register it again"
            )
        };
        if json {
            print_json(&MediaInfo {
                media: MediaEntry::new(medium, usage::allocated(&file)),
                verified,
                layout,
            })?;
            // One document on stdout, so the report says it, and the code.
            return Ok(u8::from(!verified));
        }
        println!("Name: {}", medium.name);
        let given = if medium.kind_given {
            "as registered"
        } else {
            "as found"
        };
        println!("Kind: {} ({given})", medium.kind.as_str());
        println!("Format: {}", medium.format.describe());
        println!("Size: {} bytes", medium.size);
        println!(
            "Boot blocks: {}",
            if medium.bootable {
                "yes; a Mac may start from it"
            } else {
                "none; a Mac can't start from it"
            }
        );
        println!(
            "Version: {}",
            medium.version.as_deref().unwrap_or("not given")
        );
        if let Some(source) = &medium.source {
            println!("Registered from: {source}");
        }
        println!("SHA-256: {}", medium.sha256);
        println!(
            "Disk used: {}",
            usage::format_usage(usage::allocated(&file))
        );
        if let Layout::Partitioned { partitions } = &layout {
            println!("Partitions:");
            for p in partitions {
                let volume = match (p.in_bounds, p.volume) {
                    (false, _) => "out of bounds",
                    (true, Some(Format::Hfs)) => "HFS",
                    (true, Some(Format::Mfs)) => "MFS",
                    (true, Some(Format::DiskCopy42)) => "Disk Copy 4.2",
                    (true, None) => "-",
                };
                println!(
                    "  {:>3}  {:<24} {:>12} bytes  {volume}",
                    p.index, p.kind, p.length
                );
            }
        }
        if !verified {
            return Err(changed());
        }
        println!("Verified: it matches its digest");
        Ok(0)
    }

    fn remove_media(&self, name: &str, yes: bool) -> Result<()> {
        validate_name("media", name)?;
        if !yes && media::media_exists(&self.paths, name) {
            confirm(
                &format!(
                    "Delete media {name:?}? Machines installed from it keep their disks, and \
                     the file it was registered from is untouched."
                ),
                &format!("removing media {name:?} deletes it; pass --yes to confirm"),
            )?;
        }
        media::remove_media(&self.paths, name)?;
        println!("Removed media {name}");
        Ok(())
    }

    fn disk_info(&self, file: &Path, json: bool) -> Result<()> {
        let _guard = disk::lock_image(&self.paths, file, LockMode::Shared)?;
        let size = fs::metadata(file)?.len();
        let layout = layout_of(file)?;
        if json {
            return print_json(&DiskInfo {
                file: file.display().to_string(),
                size,
                layout,
            });
        }
        print_layout(file, size, layout);
        Ok(())
    }

    fn disk_extract(
        &self,
        source: &Path,
        dest: &Path,
        partition: Option<u32>,
        raw: bool,
    ) -> Result<()> {
        let _guard = disk::lock_image(&self.paths, source, LockMode::Shared)?;
        let import = disk::extract_volume(source, dest, partition, raw)?;
        let what = import.extracted.as_deref().unwrap_or("a raw volume");
        println!(
            "Extracted {what} to {} ({}K)",
            dest.display(),
            import.length / 1024
        );
        if raw {
            println!("Not checked as a volume, because of --raw");
        }
        Ok(())
    }

    fn create(&self, args: CreateArgs) -> Result<()> {
        validate_name("machine", &args.name)?;
        if self.paths.machine(&args.name).exists() {
            bail!("machine {:?} already exists", args.name);
        }
        let backend = match &args.backend {
            Some(name) => self.backends.get(name)?,
            None => self.backends.default_backend(),
        };
        let is_basilisk2 = backend.name() == basilisk2::BACKEND_NAME;
        let is_sheepshaver = backend.name() == sheepshaver::BACKEND_NAME;
        let (basilisk2_settings, sheepshaver_settings) =
            backend_settings(&args, is_basilisk2, is_sheepshaver)?;
        let model: Option<Model> = match &args.model {
            Some(_) if is_basilisk2 => {
                bail!("--model is for Mini vMac; Basilisk II has --cpu instead")
            }
            Some(_) if is_sheepshaver => {
                bail!("--model is for Mini vMac; SheepShaver is a generic PowerPC Mac")
            }
            Some(model) => Some(model.parse()?),
            None if is_basilisk2 || is_sheepshaver => None,
            None => bail!("--model is required, such as --model plus"),
        };
        let (rom, rom_path) = registry::load_rom(&self.paths, &args.rom)?;
        let (version, boot_disk) = match (&args.system, &args.disk_size) {
            (Some(system), None) => {
                let (system, template) = registry::load_system(&self.paths, system)?;
                let format = disk::detect_format(&template)?;
                if let Some(model) = model
                    && format == disk::DiskFormat::RawHfs
                    && !model.reads_hfs()
                {
                    eprintln!(
                        "warning: model {model} cannot read HFS, so {} will not boot",
                        system.name
                    );
                }
                (system.version, BootDisk::Template(template))
            }
            (None, Some(size)) => {
                if !backend.installs_from_cd() {
                    bail!(
                        "--disk-size needs a backend that installs from a CD, which {} doesn't \
                         on this host; use --system",
                        backend.name()
                    );
                }
                (None, BootDisk::Blank(parse_disk_size(size)?))
            }
            _ => bail!("give --system or --disk-size"),
        };

        // Checked now, so a machine isn't made around media it can't use. They
        // aren't saved with it: a later plain `run` boots its own disk.
        for name in &args.media {
            let (medium, file) = media::load_media(&self.paths, name)?;
            media::check_size(&medium, &file)?;
            if medium.kind == MediaKind::Cdrom && !backend.installs_from_cd() {
                bail!(
                    "media {name:?} is a CD, which {} can't attach on this host; register a \
                     floppy, or pick another --backend",
                    backend.name()
                );
            }
        }

        let (width, height, depth) = match (&args.display, model) {
            (Some(display), _) => parse_display(display)?,
            (None, Some(model)) => model.default_display(),
            (None, None) if is_sheepshaver => sheepshaver::DEFAULT_DISPLAY,
            (None, None) => basilisk2::DEFAULT_DISPLAY,
        };
        let memory = match (args.memory, model) {
            (Some(memory), _) => memory.to_ascii_uppercase(),
            (None, Some(model)) => model.default_memory().to_string(),
            (None, None) if is_sheepshaver => sheepshaver::DEFAULT_MEMORY.to_string(),
            (None, None) => basilisk2::DEFAULT_MEMORY.to_string(),
        };
        let machine = Machine {
            schema_version: SCHEMA_VERSION,
            name: args.name.clone(),
            id: Some(new_id()),
            ephemeral: args.ephemeral,
            emulator: EmulatorConfig {
                backend: backend.name().to_string(),
                revision: backend.revision().to_string(),
            },
            hardware: Hardware {
                model,
                rom: rom.name.clone(),
                memory,
            },
            display: Display {
                width,
                height,
                depth,
                scale: args.scale,
                fullscreen: args.fullscreen,
            },
            system: SystemInfo { version },
            basilisk2: basilisk2_settings,
            sheepshaver: sheepshaver_settings,
            disks: vec![Disk {
                path: SYSTEM_DISK.to_string(),
                readonly: false,
                boot: true,
            }],
        };
        backend.check_host()?;
        backend.check_rom(&machine, &rom, &rom_path)?;
        backend.validate(&machine)?;

        self.publish_machine(&machine, |dir| match &boot_disk {
            BootDisk::Template(template) => {
                let _guard = disk::lock_image(&self.paths, template, LockMode::Shared)?;
                atomic_copy(template, &dir.join(SYSTEM_DISK))
            }
            BootDisk::Blank(size) => create_blank_disk(&dir.join(SYSTEM_DISK), *size),
        })?;
        let name = &args.name;
        let run = if args.media.is_empty() {
            format!("macman run {name} --cdrom FILE")
        } else {
            let media: Vec<String> = args.media.iter().map(|m| format!("--media {m}")).collect();
            format!("macman run {name} {}", media.join(" "))
        };
        match boot_disk {
            BootDisk::Template(_) if args.media.is_empty() => {
                println!("Created machine {name}")
            }
            BootDisk::Template(_) => {
                println!("Created machine {name}; attach its media with `{run}`")
            }
            BootDisk::Blank(size) => println!(
                "Created machine {name} with a blank {size}-byte boot disk; install onto it \
                 with `{run}`"
            ),
        }
        if !args.media.is_empty() {
            println!(
                "The media aren't saved with the machine, so a plain `macman run {name}` starts \
                 from its own disk; the installer itself is up to you"
            );
        }
        Ok(())
    }

    /// Fill a hidden directory, then rename it into place, so that a failure
    /// partway never leaves a half-made machine.
    fn publish_machine(
        &self,
        machine: &Machine,
        populate: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        let staging =
            self.paths
                .machines()
                .join(format!(".new-{}-{}", machine.name, std::process::id()));
        fs::create_dir_all(&staging)
            .with_context(|| format!("cannot create {}", staging.display()))?;
        let result = populate(&staging).and_then(|()| {
            machine.save_to(&staging)?;
            publish_dir(&self.paths, &staging, &machine.name)
        });
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn clone_machine(&self, source: &str, dest: &str) -> Result<()> {
        validate_name("machine", source)?;
        validate_name("machine", dest)?;
        if self.paths.machine(dest).exists() {
            bail!("machine {dest:?} already exists");
        }
        let original = Machine::load(&self.paths, source)?;
        let mut copy = original.clone();
        copy.name = dest.to_string();
        copy.id = Some(new_id());

        self.publish_machine(&copy, |dir| {
            for (from, disk) in original.disk_paths(&self.paths)? {
                let _guard = disk::lock_image(&self.paths, &from, LockMode::Shared)?;
                atomic_copy(&from, &dir.join(&disk.path))?;
            }
            Ok(())
        })?;
        println!("Cloned {source} to {dest}");
        Ok(())
    }

    fn remove_system(&self, name: &str, yes: bool) -> Result<()> {
        validate_name("system", name)?;
        if !yes && registry::system_exists(&self.paths, name) {
            confirm(
                &format!("Delete system image {name:?}? Its machines keep their own disks."),
                &format!("removing system image {name:?} deletes it; pass --yes to confirm"),
            )?;
        }
        registry::remove_system(&self.paths, name)?;
        println!("Removed system image {name}");
        Ok(())
    }

    fn remove(&self, name: &str, delete_disks: bool) -> Result<()> {
        validate_name("machine", name)?;
        let machine = Machine::load(&self.paths, name)?;
        let disks = machine.disk_paths(&self.paths)?;
        let dir = self.paths.machine(name);

        // Holding these proves no emulator has the machine open.
        let mut locks: Vec<ImageLock> = Vec::new();
        for (path, _) in &disks {
            if path.exists() {
                locks.push(disk::lock_image(&self.paths, path, LockMode::Exclusive)?);
            }
        }
        let checkpoints = checkpoint::list(&self.paths, name)?.len();
        if !delete_disks && (!disks.is_empty() || checkpoints > 0) {
            let disks = disks.len();
            let also = if checkpoints > 0 {
                format!(" and {checkpoints} checkpoint(s)")
            } else {
                String::new()
            };
            confirm(
                &format!("Delete machine {name:?} and its {disks} private disk(s){also}?"),
                &format!(
                    "removing {name:?} deletes its {disks} private disk(s){also}; \
                     pass --delete-disks to confirm"
                ),
            )?;
        }
        fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;

        let mut config = Config::load(&self.paths)?;
        if config.current.as_deref() == Some(name) {
            config.current = None;
            config.save(&self.paths)?;
        }
        println!("Removed machine {name}");
        Ok(())
    }

    fn checkpoint_create(&self, name: Option<String>, checkpoint: Option<&str>) -> Result<()> {
        let name = self.resolve_name(name)?;
        let machine = Machine::load(&self.paths, &name)?;
        let saved = checkpoint::create(&self.paths, &machine, checkpoint)?;
        println!(
            "Saved checkpoint {} of {name} ({}, {} on the host)",
            saved.name,
            usage::format_bytes(saved.size),
            usage::format_usage(usage::allocated(&checkpoint::image_file(
                &self.paths,
                &name,
                &saved.name
            )))
        );
        Ok(())
    }

    fn checkpoint_list(&self, name: Option<String>, json: bool) -> Result<()> {
        let name = self.resolve_name(name)?;
        Machine::load(&self.paths, &name)?;
        let checkpoints: Vec<CheckpointEntry> = checkpoint::list(&self.paths, &name)?
            .into_iter()
            .map(|saved| CheckpointEntry {
                host_disk_used_bytes: usage::allocated(&checkpoint::image_file(
                    &self.paths,
                    &name,
                    &saved.name,
                )),
                name: saved.name,
                created: saved.created,
                disk: saved.disk,
                size: saved.size,
                sha256: saved.sha256,
            })
            .collect();
        if json {
            return print_json(&CheckpointList {
                machine: name,
                checkpoints,
            });
        }
        let rows: Vec<Vec<String>> = checkpoints
            .into_iter()
            .map(|saved| {
                vec![
                    saved.name,
                    saved.created,
                    usage::format_bytes(saved.size),
                    format!("used {}", usage::format_usage(saved.host_disk_used_bytes)),
                    saved.disk,
                ]
            })
            .collect();
        print_table(&rows, &[]);
        Ok(())
    }

    fn checkpoint_delete(&self, name: Option<String>, checkpoint: &str, yes: bool) -> Result<()> {
        let name = self.resolve_name(name)?;
        let machine = Machine::load(&self.paths, &name)?;
        checkpoint::load(&self.paths, &name, checkpoint)?;
        if !yes {
            confirm(
                &format!(
                    "Delete checkpoint {checkpoint:?} of {name}? It can't be rolled back to after."
                ),
                &format!(
                    "deleting checkpoint {checkpoint:?} can't be undone; pass --yes to confirm"
                ),
            )?;
        }
        checkpoint::delete(&self.paths, &machine, checkpoint)?;
        println!("Deleted checkpoint {checkpoint} of {name}");
        Ok(())
    }

    fn rollback(
        &self,
        name: Option<String>,
        checkpoint: &str,
        save: Option<&str>,
        yes: bool,
    ) -> Result<()> {
        let name = self.resolve_name(name)?;
        let machine = Machine::load(&self.paths, &name)?;
        let (saved, _) = checkpoint::load(&self.paths, &name, checkpoint)?;
        let missing = machine
            .disk_paths(&self.paths)?
            .iter()
            .any(|(path, disk)| disk.boot && !path.exists());
        if !yes && !(missing && save.is_some()) {
            let keep = match save {
                Some(save) => format!(" It's saved first as checkpoint {save:?}."),
                None if missing => " The boot disk is missing, so this restores it.".to_string(),
                None => " Its current contents will be lost; --save NAME keeps them.".to_string(),
            };
            confirm(
                &format!(
                    "Replace the boot disk of {name} with checkpoint {checkpoint:?}, taken {}?{keep}",
                    saved.created
                ),
                &format!(
                    "rolling {name} back replaces its boot disk; pass --yes to confirm, and \
                     --save NAME to keep what's there"
                ),
            )?;
        }
        let done = checkpoint::rollback(&self.paths, &machine, checkpoint, save)?;
        if let Some(kept) = done.saved {
            println!("Saved the boot disk of {name} as checkpoint {}", kept.name);
        }
        if done.restored_missing {
            println!("Restored the missing boot disk of {name} from checkpoint {checkpoint}");
        } else {
            println!("Rolled {name} back to checkpoint {checkpoint}");
        }
        Ok(())
    }

    fn config_show(&self, name: Option<String>, json: bool) -> Result<()> {
        let name = self.resolve_name(name)?;
        let machine = Machine::load(&self.paths, &name)?;
        if json {
            return print_json(&MachineConfig { machine });
        }
        let file = self.paths.machine(&name).join("machine.toml");
        print!("{}", fs::read_to_string(file)?);
        Ok(())
    }

    fn config_edit(&self, name: Option<String>) -> Result<()> {
        let name = self.resolve_name(name)?;
        Machine::load(&self.paths, &name)?;
        // Refuse before drawing anything, so redirected output gets no escapes.
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!(
                "`macman config edit` needs an interactive terminal; use `macman config show {name}` \
                 to read the configuration, or edit its machine.toml by hand"
            );
        }
        let editor = crate::edit::Editor::open(&self.paths, &self.backends, &name)?;
        match crate::tui::run(editor)? {
            crate::tui::Ending::Saved => println!("Saved machine {name}"),
            crate::tui::Ending::Cancelled => println!("Nothing was changed"),
        }
        Ok(())
    }

    fn run(&self, args: &RunArgs, verbose: bool) -> Result<u8> {
        let name = self.resolve_name(args.name.clone())?;
        self.sweep_stale_sessions()?;

        let machine = Machine::load(&self.paths, &name)?;
        let backend = self.backends.get(&machine.emulator.backend)?;
        backend.validate(&machine)?;

        let (rom, rom_path) = registry::load_rom(&self.paths, &machine.hardware.rom)?;
        registry::verify(&rom_path, "ROM", &rom.name, &rom.sha256)?;
        backend.check_rom(&machine, &rom, &rom_path)?;

        // Registered media join the CDs and disks given by path, each by its kind.
        // A medium is never written, so a disk one goes as a throwaway copy.
        let mut cd_paths = args.cdroms.clone();
        let mut extra: Vec<ExtraDisk> = args
            .disks
            .iter()
            .map(|path| ExtraDisk {
                path: path.clone(),
                writable: !args.readonly,
                any_format: args.any_format,
            })
            .collect();
        let mut attaching = Vec::new();
        for name in &args.media {
            let (medium, file) = media::load_media(&self.paths, name)?;
            media::check_size(&medium, &file)?;
            attaching.push(format!("{name} ({})", medium.kind.as_str()));
            match medium.kind {
                MediaKind::Cdrom => cd_paths.push(file),
                MediaKind::Floppy | MediaKind::Disk => extra.push(ExtraDisk {
                    path: file,
                    writable: false,
                    any_format: medium.format == MediaFormat::Unrecognized,
                }),
            }
        }

        let saved_disks = machine.disk_paths(&self.paths)?;
        let ephemeral = args.ephemeral(machine.ephemeral);
        if ephemeral && !saved_disks.iter().any(|(_, saved)| saved.boot) {
            bail!("machine {name:?} has no boot disk, so it can't run on a temporary copy of one");
        }
        // The guest may write to a temporary boot disk, whatever the saved one is.
        let writable = |saved: &Disk| !saved.readonly || (ephemeral && saved.boot);
        for (path, saved) in &saved_disks {
            if !path.exists() {
                bail!("machine {name:?} is missing its disk {}", path.display());
            }
            // On Windows, a running emulator shuts out even readers.
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            backend.check_disk(path, writable(saved))?;
            if saved.boot
                && cd_paths.is_empty()
                && extra.is_empty()
                && disk::classify(path)? == Classification::Blank
            {
                eprintln!(
                    "note: the boot disk of {name} is blank, so it will stop at a question \
                     mark; install onto it with `macman run {name} --media NAME` or `--cdrom FILE`"
                );
            }
            if saved_disk_format(backend, path)? == Some(disk::DiskFormat::DiskCopy42)
                && writable(saved)
            {
                disk::warn_disk_copy_read_only(path);
            }
        }
        for disk in &extra {
            let path = &disk.path;
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            backend.check_disk(path, disk.writable)?;
            let partitioned = matches!(disk::classify(path)?, Classification::Partitioned(_));
            if !(partitioned && backend.mounts_partitioned_disks()) {
                disk::check_attachable(path, disk.any_format, disk.writable)?;
            }
        }
        if !cd_paths.is_empty() && !backend.installs_from_cd() {
            bail!(
                "--cdrom isn't available for {} on this host, nor is CD media; try --disk, or \
                 floppy media",
                backend.name()
            );
        }
        let mut volumes = Vec::new();
        for path in &cd_paths {
            drop(disk::lock_image(&self.paths, path, CD_LOCK)?);
            volumes.push(disk::check_cd_image(path)?);
        }
        backend.validate_disks(&machine, saved_disks.len() + extra.len())?;
        let repair_copy = self.offer_boot_disk_repair(&machine, ephemeral)?;

        if verbose && backend.revision() != machine.emulator.revision {
            println!(
                "Machine records {} {}; this host runs it on {}",
                backend.name(),
                machine.emulator.revision,
                backend.revision()
            );
        }
        let executable = backend.prepare(&machine, verbose)?;
        if executable.built {
            println!("Built emulator {}", &executable.build_key[..12]);
        } else if verbose {
            println!("Using cached emulator {}", &executable.build_key[..12]);
        }

        let session = Session::start(&self.paths, &name)?;
        let mut locks: Vec<ImageLock> = Vec::new();
        let mut disks: Vec<ResolvedDisk> = Vec::new();
        let mut temporary = None;
        for (path, disk) in &saved_disks {
            if ephemeral && disk.boot {
                // Held for the whole run, so the saved disk can't change, or be
                // run on, underneath the copy; other ephemeral runs may share it.
                locks.push(disk::lock_image(&self.paths, path, LockMode::Shared)?);
                let copy = session.ephemeral_copy(path, disks.len())?;
                if repair_copy {
                    disk::mark_unmounted(&copy)?;
                }
                temporary = Some(copy.clone());
                disks.push(ResolvedDisk {
                    path: copy,
                    writable: true,
                });
                continue;
            }
            disks.push(self.attach(&session, &mut locks, path, !disk.readonly, disks.len())?);
        }
        for disk in &extra {
            disks.push(self.attach(
                &session,
                &mut locks,
                &disk.path,
                disk.writable,
                disks.len(),
            )?);
        }
        if !attaching.is_empty() {
            println!(
                "Attaching media {}, read-only; nothing the guest does changes it",
                attaching.join(", ")
            );
        }
        // Not copied, as these are big, and the emulator opens them read-only.
        let mut cdroms: Vec<CdImage> = Vec::new();
        for (path, volume) in cd_paths.iter().zip(volumes) {
            locks.push(disk::lock_image(&self.paths, path, CD_LOCK)?);
            cdroms.push(CdImage {
                path: fs::canonicalize(path)
                    .with_context(|| format!("cannot resolve {}", path.display()))?,
                volume,
            });
        }
        if args.readonly {
            println!(
                "Attaching read-only throwaway copies of {} disk(s); guest changes cannot reach \
                 the originals",
                args.disks.len()
            );
        }
        // What the guest writes to these is kept: everything writable but the copy.
        let kept: Vec<PathBuf> = disks
            .iter()
            .filter(|disk| disk.writable && Some(&disk.path) != temporary.as_ref())
            .map(|disk| disk.path.clone())
            .collect();
        if temporary.is_some() {
            println!(
                "Booting from a temporary copy of the boot disk; the guest's changes to it are \
                 discarded when the run ends"
            );
            if !kept.is_empty() {
                eprintln!(
                    "note: only the boot disk is temporary; changes to the {} other writable \
                     disk(s) are kept",
                    kept.len()
                );
            }
        }
        let mut record = SessionRecord {
            machine: name.clone(),
            pid: std::process::id(),
            outcome: None,
            writable_disks: kept,
        };
        session.record(&record)?;
        // A run that ends badly keeps its session for `doctor`, but a temporary
        // disk has nothing worth keeping. One an emulator still holds stays
        // until the next run clears the session.
        let discard = || {
            if let Some(copy) = &temporary {
                let _ = fs::remove_file(copy);
            }
        };

        let launched = backend.launch(&LaunchRequest {
            machine: &machine,
            executable: &executable,
            rom: &rom_path,
            disks: &disks,
            cdroms: &cdroms,
            workdir: &session.dir,
            verbose,
        });
        let status = match launched {
            Ok(status) => status,
            Err(error) => {
                record.outcome = Some("the emulator failed to start".to_string());
                discard();
                session.abandon(&record);
                return Err(error);
            }
        };
        match abnormal_outcome(status) {
            None => session.finish()?,
            Some(outcome) => {
                if temporary.is_some() {
                    eprintln!(
                        "warning: {outcome}; the temporary boot disk was discarded, and other \
                         disks were left in place"
                    );
                } else {
                    eprintln!("warning: {outcome}; disks were left in place");
                }
                record.outcome = Some(outcome);
                discard();
                session.abandon(&record);
            }
        }
        Ok(exit_code(status))
    }

    /// The boot disk of `machine`, if a guest stopped without unmounting it and that can
    /// keep the machine from starting. A read-only disk is left out, as guests only see
    /// a copy of it.
    fn unclean_boot_disk(&self, machine: &Machine, ephemeral: bool) -> Result<Option<PathBuf>> {
        if !self
            .backends
            .get(&machine.emulator.backend)?
            .needs_unmounted_boot_volume()
        {
            return Ok(None);
        }
        for (path, disk) in machine.disk_paths(&self.paths)? {
            // A temporary copy is writable, even of a read-only disk.
            if disk.boot && (!disk.readonly || ephemeral) && disk::left_mounted(&path)? {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// Ask before anything is built or started, so the answer is about this run. For an
    /// ephemeral run, only the copy may be repaired, so this returns whether to.
    fn offer_boot_disk_repair(&self, machine: &Machine, ephemeral: bool) -> Result<bool> {
        let Some(path) = self.unclean_boot_disk(machine, ephemeral)? else {
            return Ok(false);
        };
        if ephemeral {
            return Ok(self.offer_copy_repair(&machine.name));
        }
        let _lock = disk::lock_image(&self.paths, &path, LockMode::Exclusive)?;
        let name = &machine.name;
        let problem = format!(
            "the boot disk of {name} wasn't shut down cleanly, so it may stop at a flashing \
             question mark"
        );
        if !std::io::stdin().is_terminal() {
            eprintln!("warning: {problem}; run macman from a terminal to repair it");
            return Ok(false);
        }
        print!(
            "{problem}. This doesn't check or repair the disk. Set the HFS cleanly-unmounted \
             flag to allow booting? [y/N] "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if accepts_repair(&answer) {
            disk::mark_unmounted(&path)?;
            println!("Set the flag, without checking the disk; run Disk First Aid in the guest");
        }
        Ok(false)
    }

    /// [`Self::offer_boot_disk_repair`] for an ephemeral run, which leaves the saved disk
    /// as it is, and can only offer to set the flag on this run's copy.
    fn offer_copy_repair(&self, name: &str) -> bool {
        let problem = format!(
            "the boot disk of {name} wasn't shut down cleanly, so it may stop at a flashing \
             question mark"
        );
        if !std::io::stdin().is_terminal() {
            eprintln!("warning: {problem}; run macman from a terminal to repair this run's copy");
            return false;
        }
        print!(
            "{problem}. Set the HFS cleanly-unmounted flag on this run's temporary copy, \
             leaving the saved disk as it is? [y/N] "
        );
        let mut answer = String::new();
        let answered = std::io::stdout()
            .flush()
            .and_then(|()| std::io::stdin().lock().read_line(&mut answer));
        answered.is_ok() && accepts_repair(&answer)
    }

    /// Lock one disk and produce the form the emulator should see. Read-only
    /// disks become throwaway copies, so read-only is enforced by construction.
    fn attach(
        &self,
        session: &Session,
        locks: &mut Vec<ImageLock>,
        path: &Path,
        writable: bool,
        index: usize,
    ) -> Result<ResolvedDisk> {
        if writable {
            locks.push(disk::lock_image(&self.paths, path, LockMode::Exclusive)?);
            let path = fs::canonicalize(path)
                .with_context(|| format!("cannot resolve {}", path.display()))?;
            Ok(ResolvedDisk {
                path,
                writable: true,
            })
        } else {
            let path = session.throwaway_copy(&self.paths, path, index)?;
            Ok(ResolvedDisk {
                path,
                writable: false,
            })
        }
    }

    fn sweep_stale_sessions(&self) -> Result<()> {
        for stale in disk::stale_sessions(&self.paths)? {
            let record = &stale.record;
            if record.writable_disks.is_empty() {
                // Such as an ephemeral run, whose temporary disk goes with the session.
                eprintln!(
                    "warning: a previous run of {} (pid {}) ended abnormally: {}; it kept no \
                     changes to any saved disk",
                    record.machine,
                    record.pid,
                    record.describe()
                );
            } else {
                eprintln!(
                    "warning: a previous run of {} (pid {}) ended abnormally: {}; its disks \
                     were kept but may need checking:",
                    record.machine,
                    record.pid,
                    record.describe()
                );
            }
            for path in &record.writable_disks {
                eprintln!("  {}", without_verbatim_prefix(path).display());
            }
            // An emulator that outlived macman keeps its running files from
            // being removed on Windows; that shouldn't stop this run.
            if let Err(error) = stale.clear() {
                eprintln!("warning: {error:#}");
            }
        }
        Ok(())
    }

    fn doctor(&self, verbose: bool, json: bool) -> Result<u8> {
        let checks = self.doctor_checks(verbose)?;
        let report = DoctorReport::new(&checks);
        if json {
            print_json(&report)?;
        } else {
            let color = stdout_takes_color();
            for check in &checks {
                print!("{}", doctor_line(check, color));
            }
        }
        Ok(u8::from(!report.ok))
    }

    pub fn doctor_checks(&self, verbose: bool) -> Result<Vec<Check>> {
        let mut checks: Vec<Check> = Vec::new();
        let in_use = self.backends_in_use()?;
        let default = self.backends.default_backend().name();
        for backend in self.backends.iter() {
            let needed = backend.name() == default || in_use.contains(backend.name());
            checks.extend(backend.doctor().into_iter().map(|check| match check.level {
                // What an unused backend lacks is no fault of this host.
                CheckLevel::Fail if !needed => Check {
                    level: CheckLevel::Warn,
                    ..check
                },
                _ => check,
            }));
        }
        self.check_registry(&mut checks)?;
        self.check_cache(&mut checks, verbose)?;
        self.check_machines(&mut checks)?;
        for stale in disk::stale_sessions(&self.paths)? {
            checks.push(Check::warn(
                format!(
                    "an earlier run of {} ended abnormally: {}",
                    stale.record.machine,
                    stale.record.describe()
                ),
                "check its disks; the next `macman run` clears this notice",
            ));
        }
        Ok(checks)
    }

    /// The backends of the machines that can be read.
    fn backends_in_use(&self) -> Result<BTreeSet<String>> {
        Ok(list_dir(&self.paths.machines())?
            .iter()
            .filter_map(|name| Machine::load(&self.paths, name).ok())
            .map(|machine| machine.emulator.backend)
            .collect())
    }

    fn check_registry(&self, checks: &mut Vec<Check>) -> Result<()> {
        let roms = registry::list_roms(&self.paths)?;
        if roms.is_empty() {
            checks.push(Check::warn(
                "no ROMs imported",
                "run `macman rom add FILE --name NAME`",
            ));
        }
        for rom in roms {
            let file = self.paths.roms().join(format!("{}.rom", rom.name));
            match sha256_file(&file) {
                Ok(sha) if sha == rom.sha256 => {
                    checks.push(Check::ok(format!("ROM {} intact", rom.name)))
                }
                _ => checks.push(Check::fail(
                    format!("ROM {} is missing or changed", rom.name),
                    "import it again under a new name",
                )),
            }
        }
        let systems = registry::list_systems(&self.paths)?;
        if systems.is_empty() {
            checks.push(Check::warn(
                "no system images registered",
                "run `macman system add FILE --name NAME`",
            ));
        }
        for system in systems {
            let file = self.paths.systems().join(format!("{}.dsk", system.name));
            match sha256_file(&file) {
                Ok(sha) if sha == system.sha256 => {
                    checks.push(Check::ok(format!("system image {} intact", system.name)));
                }
                _ => checks.push(Check::fail(
                    format!("system image {} is missing or changed", system.name),
                    "register it again under a new name",
                )),
            }
        }
        Ok(())
    }

    fn check_cache(&self, checks: &mut Vec<Check>, verbose: bool) -> Result<()> {
        for key in list_dir(&self.paths.builds())? {
            let dir = self.paths.builds().join(&key);
            let short = &key[..12.min(key.len())];
            let intact =
                read_toml::<Manifest>(&dir.join(MANIFEST)).is_ok_and(|m| m.is_intact(&dir));
            if intact {
                checks.push(Check::ok(format!("cached build {short} intact")));
            } else {
                let place = if verbose {
                    format!(" ({})", dir.display())
                } else {
                    String::new()
                };
                checks.push(Check::warn(
                    format!("cached build {short} is damaged{place}"),
                    "it will be rebuilt when next needed",
                ));
            }
        }
        Ok(())
    }

    fn check_machines(&self, checks: &mut Vec<Check>) -> Result<()> {
        for name in list_dir(&self.paths.machines())? {
            match self.check_machine(&name) {
                Ok(()) => {
                    checks.push(Check::ok(format!("machine {name} is runnable")));
                    checks.extend(self.check_boot_disk(&name)?);
                }
                Err(e) => checks.push(Check::fail(
                    format!("machine {name} cannot run"),
                    format!("{e:#}"),
                )),
            }
        }
        Ok(())
    }

    fn check_boot_disk(&self, name: &str) -> Result<Option<Check>> {
        let machine = Machine::load(&self.paths, name)?;
        // A machine in use has its volume mounted, as it should.
        let unclean = self
            .unclean_boot_disk(&machine, machine.ephemeral)
            .ok()
            .flatten();
        let Some(path) = unclean else {
            return Ok(None);
        };
        if disk::lock_image(&self.paths, &path, LockMode::Exclusive).is_err() {
            return Ok(None);
        }
        Ok(Some(Check::warn(
            format!("machine {name} wasn't shut down cleanly, and may not start"),
            format!(
                "`macman run {name}` offers to set its boot disk's cleanly-unmounted flag; \
                 then run Disk First Aid in the guest"
            ),
        )))
    }

    fn check_machine(&self, name: &str) -> Result<()> {
        let machine = Machine::load(&self.paths, name)?;
        let backend = self.backends.get(&machine.emulator.backend)?;
        backend.check_host()?;
        backend.validate(&machine)?;
        let (rom, file) = registry::load_rom(&self.paths, &machine.hardware.rom)?;
        backend.check_rom(&machine, &rom, &file)?;
        for (path, _) in machine.disk_paths(&self.paths)? {
            if !path.exists() {
                let file: PathBuf = path.file_name().map(PathBuf::from).unwrap_or_default();
                bail!("disk {} is missing", file.display());
            }
            // A disk in use belongs to a running machine, and may be unreadable.
            if disk::lock_image(&self.paths, &path, LockMode::Shared).is_ok() {
                saved_disk_format(backend, &path)?;
            }
        }
        Ok(())
    }
}

/// What a new machine's boot disk comes from.
enum BootDisk {
    /// A registered system image, to copy.
    Template(PathBuf),
    /// Zeros, of this many bytes.
    Blank(u64),
}

/// A sparse file where the filesystem allows, and all zeros to whoever reads it.
/// The guest initializes it, so nothing here formats it.
fn create_blank_disk(path: &Path, size: u64) -> Result<()> {
    let file =
        fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    file.set_len(size)
        .with_context(|| format!("cannot make {} {size} bytes", path.display()))
}

/// The format of a disk a machine owns. A machine that installs from a CD starts
/// blank, and what the guest then does with its disk is its own business, so
/// those give `None`. Anything else has to be a format macman knows.
fn saved_disk_format(backend: &dyn EmulatorBackend, path: &Path) -> Result<Option<DiskFormat>> {
    if backend.installs_from_cd()
        && matches!(
            disk::classify(path)?,
            Classification::Blank | Classification::Partitioned(_)
        )
    {
        return Ok(None);
    }
    disk::detect_format(path).map(Some)
}

/// What `macman create` was asked for under `[basilisk2]` or `[sheepshaver]`.
fn backend_settings(
    args: &CreateArgs,
    basilisk2: bool,
    sheepshaver: bool,
) -> Result<(Option<Basilisk2Settings>, Option<SheepShaverSettings>)> {
    let cpu_flags = [(args.cpu.is_some(), "--cpu"), (args.no_fpu, "--no-fpu")];
    if !basilisk2 && let Some((_, flag)) = cpu_flags.iter().find(|(set, _)| *set) {
        bail!("{flag} needs --backend basilisk2");
    }
    let flags = [
        (args.no_sound, "--no-sound"),
        (args.network.is_some(), "--network"),
        (args.shared_folder.is_some(), "--shared-folder"),
    ];
    if !(basilisk2 || sheepshaver) {
        return match flags.iter().find(|(set, _)| *set) {
            Some((_, flag)) => bail!("{flag} needs --backend basilisk2 or sheepshaver"),
            None => Ok((None, None)),
        };
    }
    let shared_folder = args
        .shared_folder
        .as_ref()
        .map(|folder| {
            fs::canonicalize(folder)
                .with_context(|| format!("cannot use shared folder {}", folder.display()))
        })
        .transpose()?;
    let sound = !args.no_sound;
    let network = args.network.unwrap_or_default();
    if sheepshaver {
        let settings = SheepShaverSettings {
            sound,
            network,
            shared_folder,
        };
        return Ok((None, Some(settings)));
    }
    let settings = Basilisk2Settings {
        cpu: args.cpu.unwrap_or_default(),
        fpu: !args.no_fpu,
        sound,
        network,
        shared_folder,
    };
    Ok((Some(settings), None))
}

/// Ask `question`, or fail with `unattended` when there's nobody to answer.
fn confirm(question: &str, unattended: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("{unattended}");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("cancelled; nothing was changed");
    }
    Ok(())
}

/// Only a plain yes writes to the disk. Enter, end of input, and a typo all decline.
fn accepts_repair(answer: &str) -> bool {
    let answer = answer.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// Describe a run that didn't end normally, or `None` if it did.
///
/// Mini vMac's `main` always returns 0 and it installs no signal handlers. So
/// a guest shutdown and a user quit are indistinguishable (both exit 0), while
/// host-side trouble shows up as a signal or a non-zero code from the loader.
fn abnormal_outcome(status: ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    if let Some(code) = status.code() {
        #[cfg(windows)]
        if let Some(outcome) = windows_abnormal_outcome(code as u32) {
            return Some(outcome);
        }
        return Some(format!("the emulator exited with code {code}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(if is_crash_signal(signal) {
                format!("the emulator crashed on the host (signal {signal})")
            } else {
                format!("the emulator was killed by signal {signal}")
            });
        }
    }
    Some(format!("the emulator ended abnormally ({status})"))
}

/// SIGILL, SIGABRT, SIGBUS, SIGFPE, or SIGSEGV. SIGBUS is 10 on macOS, where
/// 7 is SIGEMT, and 7 on Linux, where 10 is SIGUSR1.
#[cfg(unix)]
fn is_crash_signal(signal: i32) -> bool {
    let sigbus = if cfg!(target_os = "macos") { 10 } else { 7 };
    matches!(signal, 4 | 6 | 8 | 11) || signal == sigbus
}

/// Windows has no signals: a process killed by an exception or Ctrl-C exits
/// with an error-severity NTSTATUS as its code.
#[cfg(windows)]
fn windows_abnormal_outcome(status: u32) -> Option<String> {
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
    match status {
        STATUS_CONTROL_C_EXIT => Some(format!("the emulator was interrupted ({status:#X})")),
        // Above this are codes like -1, which come from a program, not NT.
        0xC000_0000..=0xCFFF_FFFF => Some(format!(
            "the emulator crashed on the host (status {status:#X})"
        )),
        _ => None,
    }
}

fn exit_code(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        // Windows codes span 32 bits; failure must not round to success.
        return if code == 0 {
            0
        } else {
            code.clamp(1, 255) as u8
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128u8.saturating_add(signal as u8);
        }
    }
    1
}

/// The name of `command`, if it doesn't take `--json`. These change things,
/// and print what they did as they go, so they have no report to give.
/// `macman completions`: the script for `shell` on stdout, written whole or
/// not at all. It needs nothing from this host, not even its directories.
pub fn print_completions(shell: clap_complete::Shell, json: bool) -> Result<u8> {
    if json {
        bail!("--json doesn't apply to `macman completions`, which prints a shell script");
    }
    let script = crate::completion::script(shell);
    let mut stdout = std::io::stdout().lock();
    std::io::Write::write_all(&mut stdout, &script)
        .and_then(|()| std::io::Write::flush(&mut stdout))
        .context("cannot write the completion script")?;
    Ok(0)
}

/// `du`'s order: measured entries smallest first, ties by type then name, and
/// unavailable ones after them all, by type then name.
fn sort_usage(entries: &mut [UsageEntry]) {
    entries.sort_by(|a, b| {
        let key = |e: &UsageEntry| (e.host_disk_used_bytes.is_none(), e.host_disk_used_bytes);
        key(a)
            .cmp(&key(b))
            .then(a.kind.cmp(&b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// What `disk info` and `media info` show of an image's layout.
fn layout_of(file: &Path) -> Result<Layout> {
    Ok(match disk::classify(file)? {
        Classification::Supported(DiskFormat::DiskCopy42) => {
            let (offset, length, volume) = disk::disk_copy_contents(file)?;
            Layout::DiskCopy42 {
                data_offset: offset,
                data_length: length,
                volume: volume.map(Format::from),
            }
        }
        Classification::Supported(format) => Layout::Volume {
            volume: format.into(),
        },
        Classification::Partitioned(partitions) => Layout::Partitioned {
            partitions: partitions.iter().map(PartitionReport::from).collect(),
        },
        Classification::Blank => Layout::Blank,
        Classification::Unsupported(what) => Layout::Unsupported { description: what },
        Classification::Unrecognized => Layout::Unrecognized,
    })
}

fn print_layout(file: &Path, size: u64, layout: Layout) {
    println!("{}: {size} bytes", file.display());
    let name = |format: Format| match format {
        Format::Mfs => "MFS",
        Format::Hfs => "HFS",
        Format::DiskCopy42 => "Disk Copy 4.2",
    };
    match layout {
        Layout::DiskCopy42 {
            data_offset,
            data_length,
            volume,
        } => {
            println!("Format: Disk Copy 4.2, read-only in Mini vMac");
            println!("Data: {data_length} bytes at offset {data_offset}");
            println!("Volume: {}", volume.map_or("none recognized", name));
        }
        Layout::Volume { volume } => println!("Format: raw {} volume", name(volume)),
        Layout::Partitioned { partitions } => {
            println!("Format: partitioned disk, not mountable by Mini vMac as it stands");
            println!(
                "{:>3}  {:<24} {:>12} {:>12}  Volume",
                "#", "Type", "Offset", "Length"
            );
            for p in &partitions {
                let volume = match (p.in_bounds, p.volume) {
                    (false, _) => "out of bounds",
                    (true, Some(format)) => name(format),
                    (true, None) => "-",
                };
                println!(
                    "{:>3}  {:<24} {:>12} {:>12}  {volume}",
                    p.index, p.kind, p.offset, p.length
                );
            }
        }
        Layout::Blank => println!("Format: blank"),
        Layout::Unsupported { description } => {
            println!("Format: {description}; not supported")
        }
        Layout::Unrecognized => println!("Format: not recognized"),
    }
}

/// `macman __complete-names`: the names of one kind, one per line, for a
/// completion script. It reads only directory listings, and on any trouble
/// prints nothing, because a shell would show an error mid-line.
pub fn print_names(paths: &Paths, kind: NameKind) {
    let registered = |dir: PathBuf| -> Vec<String> {
        list_dir(&dir)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|file| file.strip_suffix(".toml").map(str::to_string))
            .collect()
    };
    let mut names = match kind {
        NameKind::Machines => list_dir(&paths.machines())
            .unwrap_or_default()
            .into_iter()
            .filter(|name| paths.machine(name).join("machine.toml").is_file())
            .collect(),
        NameKind::Roms => registered(paths.roms()),
        NameKind::Systems => registered(paths.systems()),
        NameKind::Media => registered(paths.media()),
    };
    names.sort();
    let mut stdout = std::io::stdout().lock();
    for name in names {
        if writeln!(stdout, "{name}").is_err() {
            return;
        }
    }
}

fn without_json(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::Doctor
        | Command::List { .. }
        | Command::Rom(RomCommand::List)
        | Command::System(SystemCommand::List)
        | Command::Disk(DiskCommand::Info { .. })
        | Command::Current
        | Command::Du
        | Command::Checkpoint(CheckpointCommand::List { .. })
        | Command::Media(MediaCommand::List | MediaCommand::Info { .. })
        | Command::Config(ConfigCommand::Show { .. }) => return None,
        Command::Media(MediaCommand::Add { .. }) => "media add",
        Command::Checkpoint(CheckpointCommand::Create { .. }) => "checkpoint create",
        Command::Checkpoint(CheckpointCommand::Delete { .. }) => "checkpoint delete",
        Command::Rollback { .. } => "rollback",
        Command::Media(MediaCommand::Remove { .. }) => "media remove",
        Command::Rom(RomCommand::Add { .. }) => "rom add",
        Command::System(SystemCommand::Add { .. }) => "system add",
        Command::System(SystemCommand::Remove { .. }) => "system remove",
        Command::Disk(DiskCommand::Extract { .. }) => "disk extract",
        Command::Create(_) => "create",
        Command::Use { .. } => "use",
        Command::Run(_) => "run",
        Command::Clone { .. } => "clone",
        Command::Export { .. } => "export",
        Command::Import { dry_run: true, .. } => return None,
        Command::Import { .. } => "import",
        Command::Remove { .. } => "remove",
        Command::Config(ConfigCommand::Edit { .. }) => "config edit",
        // It explains this itself; it doesn't change anything.
        Command::Completions { .. } => return None,
    })
}

/// One `doctor` check as printed: its status, its message, and any fix on the
/// next line. With `color`, only the status word is colored, so the rest reads
/// the same, and lines up the same, as without.
fn doctor_line(check: &Check, color: bool) -> String {
    let (label, ansi) = match check.level {
        CheckLevel::Ok => ("ok", "32"),
        CheckLevel::Warn => ("warn", "33"),
        CheckLevel::Fail => ("FAIL", "31"),
    };
    let label = format!("{label:>4}");
    let label = if color {
        format!("\x1b[{ansi}m{label}\x1b[0m")
    } else {
        label
    };
    let mut line = format!("[{label}] {}\n", check.message);
    if let Some(fix) = &check.fix {
        line.push_str(&format!("       {fix}\n"));
    }
    line
}

/// Whether to color what goes to stdout: only a terminal there counts, never
/// one on stderr alone, and `NO_COLOR` turns it off.
fn stdout_takes_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && enable_ansi()
}

/// Windows consoles show escape codes literally until asked not to.
#[cfg(windows)]
fn enable_ansi() -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode,
    };
    let handle = std::io::stdout().as_raw_handle();
    let mut mode = 0;
    // SAFETY: the handle is this process's stdout, and `mode` outlives both calls.
    unsafe {
        GetConsoleMode(handle, &mut mode) != 0
            && (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0)
    }
}

#[cfg(not(windows))]
fn enable_ansi() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{Check, accepts_repair, doctor_line, table};

    #[test]
    fn doctor_colors_only_the_status_and_only_when_asked() {
        let checks = [
            (Check::ok("fine"), "32", "  ok"),
            (Check::warn("iffy", "do this"), "33", "warn"),
            (Check::fail("broken", "do that"), "31", "FAIL"),
        ];
        for (check, ansi, label) in checks {
            let plain = doctor_line(&check, false);
            assert!(!plain.contains('\x1b'), "{plain:?}");
            assert!(plain.starts_with(&format!("[{label}] {}\n", check.message)));

            let colored = doctor_line(&check, true);
            let status = format!("[\x1b[{ansi}m{label}\x1b[0m] {}\n", check.message);
            assert!(colored.starts_with(&status), "{colored:?}");
            assert_eq!(
                colored
                    .replace(&format!("\x1b[{ansi}m"), "")
                    .replace("\x1b[0m", ""),
                plain,
                "the same text, with only the status colored"
            );
        }
    }

    #[test]
    fn du_puts_the_largest_last_ties_by_type_and_name_and_unknowns_after() {
        use super::sort_usage;
        use crate::report::{UsageEntry, UsageKind};
        let entry = |name: &str, kind, bytes| UsageEntry {
            name: name.to_string(),
            kind,
            host_disk_used_bytes: bytes,
        };
        let mut entries = vec![
            entry("lost", UsageKind::Machine, None),
            entry("big", UsageKind::Machine, Some(9000)),
            entry("b", UsageKind::System, Some(100)),
            entry("a", UsageKind::System, Some(100)),
            entry("plus", UsageKind::Rom, Some(100)),
            entry("gone", UsageKind::Rom, None),
            entry("empty", UsageKind::Media, Some(0)),
        ];
        sort_usage(&mut entries);
        let order: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(order, ["empty", "plus", "a", "b", "big", "lost", "gone"]);
    }

    fn rows(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect()
    }

    #[test]
    fn only_an_explicit_yes_accepts_a_repair() {
        for answer in ["y\n", "Y\n", "yes\n", "YES\n", " Yes \n"] {
            assert!(accepts_repair(answer), "{answer:?}");
        }
        // The empty string is what `read_line` leaves at end of input.
        for answer in ["", "\n", "n\n", "no\n", "maybe\n", "?\n", "ye\n", "yy\n"] {
            assert!(!accepts_repair(answer), "{answer:?}");
        }
    }

    #[test]
    fn columns_line_up() {
        let rows = rows(&[&["a", "4M", "x"], &["long-name", "128M", "y"]]);
        assert_eq!(
            table(&rows, &[1]),
            ["a            4M  x", "long-name  128M  y"]
        );
    }

    #[test]
    fn the_last_cell_of_a_row_doesnt_set_a_width() {
        let rows = rows(&[
            &["a", "plus", "4M"],
            &["bb", "(unreadable: a long message)"],
            &["a whole line in one cell"],
        ]);
        assert_eq!(
            table(&rows, &[]),
            [
                "a   plus  4M",
                "bb  (unreadable: a long message)",
                "a whole line in one cell",
            ]
        );
    }

    #[test]
    fn empty_last_cells_leave_no_trailing_space() {
        assert_eq!(table(&rows(&[&["a", "b", ""]]), &[]), ["a  b"]);
    }
}
