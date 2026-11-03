//! Command implementations.

use crate::archive::{self, Outcome};
use crate::backend::minivmac::{MANIFEST, Manifest};
use crate::backend::{
    Backends, CdImage, Check, CheckLevel, EmulatorBackend, LaunchRequest, ResolvedDisk,
};
use crate::backend::{basilisk2, sheepshaver};
use crate::cli::{
    Cli, Command, ConfigCommand, CreateArgs, DiskCommand, RomCommand, RunArgs, SystemCommand,
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
use crate::model::{Model, parse_display};
use crate::paths::Paths;
use crate::registry;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const SYSTEM_DISK: &str = "system.dsk";

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
        match cli.command {
            Command::Doctor => return self.doctor(verbose),
            Command::List { builds: false } => self.list_machines()?,
            Command::List { builds: true } => self.list_builds()?,
            Command::Rom(RomCommand::Add { file, name }) => {
                let rom = registry::add_rom(&self.paths, &file, &name)?;
                println!("Imported ROM {} ({})", rom.name, rom.sha256);
            }
            Command::Rom(RomCommand::List) => self.list_roms()?,
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
            Command::System(SystemCommand::List) => self.list_systems()?,
            Command::System(SystemCommand::Remove { name, yes }) => {
                self.remove_system(&name, yes)?
            }
            Command::Disk(DiskCommand::Info { file }) => self.disk_info(&file)?,
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
            Command::Current => println!("{}", self.current()?),
            Command::Run(args) => return self.run(&args, verbose),
            Command::Config(ConfigCommand::Show { name }) => self.config_show(name)?,
            Command::Clone { source, dest } => self.clone_machine(&source, &dest)?,
            Command::Export { names, all, output } => {
                let names = if all {
                    list_dir(&self.paths.machines())?
                } else {
                    names
                };
                if names.is_empty() {
                    bail!("there are no machines to export");
                }
                let count = archive::export(&self.paths, &names, &output)?;
                println!("Exported {count} machine(s) to {}", output.display());
            }
            Command::Import { file, rename } => {
                return self.import(&file, rename.as_deref());
            }
            Command::Remove { name, delete_disks } => self.remove(&name, delete_disks)?,
        }
        Ok(0)
    }

    fn import(&self, file: &Path, rename: Option<&str>) -> Result<u8> {
        let report = archive::import(&self.paths, &self.backends, file, rename)?;
        for line in report.roms.values() {
            println!("{line}");
        }
        for result in &report.machines {
            let name = &result.name;
            match &result.outcome {
                Outcome::Imported => println!("imported machine {name}"),
                Outcome::Unchanged => println!("unchanged machine {name}"),
                Outcome::Conflict(reason) => println!("conflict machine {name}: {reason}"),
                Outcome::Failed(reason) => println!("failed machine {name}: {reason}"),
            }
        }
        println!(
            "{} imported, {} unchanged, {} unresolved",
            report.imported(),
            report.unchanged(),
            report.unresolved()
        );
        Ok(u8::from(report.unresolved() > 0))
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

    fn list_machines(&self) -> Result<()> {
        let current = Config::load(&self.paths)?.current;
        let mut rows = Vec::new();
        for name in list_dir(&self.paths.machines())? {
            let marker = if current.as_deref() == Some(&name) {
                '*'
            } else {
                ' '
            };
            let label = format!("{marker} {name}");
            rows.push(match Machine::load(&self.paths, &name) {
                Ok(m) => {
                    let version = m.system.version.as_deref().unwrap_or("-");
                    let hardware = m
                        .hardware
                        .model
                        .map_or(m.emulator.backend.clone(), |model| model.to_string());
                    vec![
                        label,
                        hardware,
                        m.hardware.memory.clone(),
                        format!(
                            "{}x{}x{}",
                            m.display.width, m.display.height, m.display.depth
                        ),
                        format!("system {version}"),
                    ]
                }
                Err(e) => vec![label, format!("(unreadable: {e:#})")],
            });
        }
        print_table(&rows, &[2]);
        Ok(())
    }

    fn list_builds(&self) -> Result<()> {
        let mut rows = Vec::new();
        for key in list_dir(&self.paths.builds())? {
            rows.push(
                match read_toml::<Manifest>(&self.paths.builds().join(&key).join(MANIFEST)) {
                    Ok(m) => vec![
                        key[..12.min(key.len())].to_string(),
                        m.backend,
                        m.revision,
                        m.options.join(" "),
                    ],
                    // One cell, as the whole key would widen the column for every other row.
                    Err(_) => vec![format!("{key}  (no valid manifest)")],
                },
            );
        }
        print_table(&rows, &[]);
        Ok(())
    }

    fn list_roms(&self) -> Result<()> {
        let mut rows = Vec::new();
        for rom in registry::list_roms(&self.paths)? {
            let models: Vec<_> = rom.models.iter().map(|m| m.name()).collect();
            let file = self.paths.roms().join(format!("{}.rom", rom.name));
            let models = if !models.is_empty() {
                models.join("/")
            } else if registry::rom_file_is_32_bit_clean(&file, rom.size)? {
                "32-bit clean, for Basilisk II".to_string()
            } else if registry::rom_file_is_powerpc(&file, rom.size)? {
                "PowerPC, for SheepShaver".to_string()
            } else {
                "unrecognised".to_string()
            };
            rows.push(vec![
                rom.name,
                format!("{}K", rom.size / 1024),
                models,
                rom.sha256[..12].to_string(),
            ]);
        }
        print_table(&rows, &[1]);
        Ok(())
    }

    fn list_systems(&self) -> Result<()> {
        let mut rows = Vec::new();
        for system in registry::list_systems(&self.paths)? {
            let version = system.version.as_deref().unwrap_or("-");
            let (_, file) = registry::load_system(&self.paths, &system.name)?;
            let format = disk::detect_format(&file)
                .map(registry::describe_format)
                .unwrap_or("unknown format");
            rows.push(vec![
                system.name.clone(),
                version.to_string(),
                format!("{}K", system.size / 1024),
                format.to_string(),
            ]);
        }
        print_table(&rows, &[2]);
        Ok(())
    }

    fn disk_info(&self, file: &Path) -> Result<()> {
        let _guard = disk::lock_image(&self.paths, file, LockMode::Shared)?;
        let size = fs::metadata(file)?.len();
        println!("{}: {size} bytes", file.display());
        match disk::classify(file)? {
            Classification::Supported(DiskFormat::DiskCopy42) => {
                let (offset, length, volume) = disk::disk_copy_contents(file)?;
                println!("Format: Disk Copy 4.2, read-only in Mini vMac");
                println!("Data: {length} bytes at offset {offset}");
                println!(
                    "Volume: {}",
                    volume.map_or("none recognized", disk::volume_name)
                );
            }
            Classification::Supported(format) => {
                println!("Format: raw {} volume", disk::volume_name(format));
            }
            Classification::Partitioned(partitions) => {
                println!("Format: partitioned disk, not mountable by Mini vMac as it stands");
                println!(
                    "{:>3}  {:<24} {:>12} {:>12}  Volume",
                    "#", "Type", "Offset", "Length"
                );
                for p in &partitions {
                    let volume = match (p.in_bounds, p.volume) {
                        (false, _) => "out of bounds",
                        (true, Some(format)) => disk::volume_name(format),
                        (true, None) => "-",
                    };
                    println!(
                        "{:>3}  {:<24} {:>12} {:>12}  {volume}",
                        p.index, p.kind, p.offset, p.length
                    );
                }
            }
            Classification::Blank => println!("Format: blank"),
            Classification::Unsupported(what) => println!("Format: {what}; not supported"),
            Classification::Unrecognized => println!("Format: not recognized"),
        }
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
        match boot_disk {
            BootDisk::Template(_) => println!("Created machine {}", args.name),
            BootDisk::Blank(size) => println!(
                "Created machine {} with a blank {size}-byte boot disk; install onto it with \
                 `macman run {} --cdrom FILE`",
                args.name, args.name
            ),
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
        if !delete_disks && !disks.is_empty() {
            let disks = disks.len();
            confirm(
                &format!("Delete machine {name:?} and its {disks} private disk(s)?"),
                &format!(
                    "removing {name:?} deletes its {disks} private disk(s); \
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

    fn config_show(&self, name: Option<String>) -> Result<()> {
        let name = self.resolve_name(name)?;
        Machine::load(&self.paths, &name)?;
        let file = self.paths.machine(&name).join("machine.toml");
        print!("{}", fs::read_to_string(file)?);
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

        let saved_disks = machine.disk_paths(&self.paths)?;
        for (path, saved) in &saved_disks {
            if !path.exists() {
                bail!("machine {name:?} is missing its disk {}", path.display());
            }
            // On Windows, a running emulator shuts out even readers.
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            backend.check_disk(path, !saved.readonly)?;
            if saved.boot
                && args.cdroms.is_empty()
                && disk::classify(path)? == Classification::Blank
            {
                eprintln!(
                    "note: the boot disk of {name} is blank, so it will stop at a question \
                     mark; install onto it with `macman run {name} --cdrom FILE`"
                );
            }
            if saved_disk_format(backend, path)? == Some(disk::DiskFormat::DiskCopy42)
                && !saved.readonly
            {
                disk::warn_disk_copy_read_only(path);
            }
        }
        for path in &args.disks {
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            backend.check_disk(path, !args.readonly)?;
            let partitioned = matches!(disk::classify(path)?, Classification::Partitioned(_));
            if !(partitioned && backend.mounts_partitioned_disks()) {
                disk::check_attachable(path, args.any_format, !args.readonly)?;
            }
        }
        if !args.cdroms.is_empty() && !backend.installs_from_cd() {
            bail!(
                "--cdrom isn't available for {} on this host; try --disk",
                backend.name()
            );
        }
        let mut volumes = Vec::new();
        for path in &args.cdroms {
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            volumes.push(disk::check_cd_image(path)?);
        }
        backend.validate_disks(&machine, saved_disks.len() + args.disks.len())?;
        self.offer_boot_disk_repair(&machine)?;

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
        for (path, disk) in &saved_disks {
            disks.push(self.attach(&session, &mut locks, path, !disk.readonly, disks.len())?);
        }
        for path in &args.disks {
            disks.push(self.attach(&session, &mut locks, path, !args.readonly, disks.len())?);
        }
        // Not copied, as these are big, and the emulator opens them read-only.
        let mut cdroms: Vec<CdImage> = Vec::new();
        for (path, volume) in args.cdroms.iter().zip(volumes) {
            locks.push(disk::lock_image(&self.paths, path, LockMode::Shared)?);
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
        let mut record = SessionRecord {
            machine: name.clone(),
            pid: std::process::id(),
            outcome: None,
            writable_disks: disks
                .iter()
                .filter(|disk| disk.writable)
                .map(|disk| disk.path.clone())
                .collect(),
        };
        session.record(&record)?;

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
                session.abandon(&record);
                return Err(error);
            }
        };
        match abnormal_outcome(status) {
            None => session.finish()?,
            Some(outcome) => {
                eprintln!("warning: {outcome}; disks were left in place");
                record.outcome = Some(outcome);
                session.abandon(&record);
            }
        }
        Ok(exit_code(status))
    }

    /// The boot disk of `machine`, if a guest stopped without unmounting it and that can
    /// keep the machine from starting. A read-only disk is left out, as guests only see
    /// a copy of it.
    fn unclean_boot_disk(&self, machine: &Machine) -> Result<Option<PathBuf>> {
        if !self
            .backends
            .get(&machine.emulator.backend)?
            .needs_unmounted_boot_volume()
        {
            return Ok(None);
        }
        for (path, disk) in machine.disk_paths(&self.paths)? {
            if disk.boot && !disk.readonly && disk::left_mounted(&path)? {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// Ask before anything is built or started, so the answer is about this run.
    fn offer_boot_disk_repair(&self, machine: &Machine) -> Result<()> {
        let Some(path) = self.unclean_boot_disk(machine)? else {
            return Ok(());
        };
        let _lock = disk::lock_image(&self.paths, &path, LockMode::Exclusive)?;
        let name = &machine.name;
        let problem = format!(
            "the boot disk of {name} wasn't shut down cleanly, so it may stop at a flashing \
             question mark"
        );
        if !std::io::stdin().is_terminal() {
            eprintln!("warning: {problem}; run macman from a terminal to repair it");
            return Ok(());
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
        Ok(())
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
            eprintln!(
                "warning: a previous run of {} (pid {}) ended abnormally: {}; its disks were \
                 kept but may need checking:",
                stale.record.machine,
                stale.record.pid,
                stale.record.describe()
            );
            for path in &stale.record.writable_disks {
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

    fn doctor(&self, verbose: bool) -> Result<u8> {
        let mut failed = false;
        for check in &self.doctor_checks(verbose)? {
            let label = match check.level {
                CheckLevel::Ok => "ok",
                CheckLevel::Warn => "warn",
                CheckLevel::Fail => "FAIL",
            };
            failed |= check.level == CheckLevel::Fail;
            println!("[{label:>4}] {}", check.message);
            if let Some(fix) = &check.fix {
                println!("       {fix}");
            }
        }
        Ok(u8::from(failed))
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
        let unclean = self.unclean_boot_disk(&machine).ok().flatten();
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
        bail!("cancelled; nothing was removed");
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

#[cfg(test)]
mod tests {
    use super::{accepts_repair, table};

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
