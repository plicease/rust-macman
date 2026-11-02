//! Command implementations.

use crate::archive::{self, Outcome};
use crate::backend::minivmac::{MANIFEST, Manifest};
use crate::backend::{Backends, Check, CheckLevel, LaunchRequest, ResolvedDisk};
use crate::cli::{
    Cli, Command, ConfigCommand, CreateArgs, DiskCommand, RomCommand, RunArgs, SystemCommand,
};
use crate::disk::{self, Classification, DiskFormat, ImageLock, LockMode, Session, SessionRecord};
use crate::fsutil::{
    atomic_copy, list_dir, read_toml, sha256_file, validate_name, without_verbatim_prefix,
};
use crate::machine::{
    Config, Disk, Display, EmulatorConfig, Hardware, Machine, SCHEMA_VERSION, SystemInfo, new_id,
};
use crate::model::{Model, parse_display};
use crate::paths::Paths;
use crate::registry;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const SYSTEM_DISK: &str = "system.dsk";

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
            Command::Export { names, output } => {
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
        for name in list_dir(&self.paths.machines())? {
            let marker = if current.as_deref() == Some(&name) {
                '*'
            } else {
                ' '
            };
            match Machine::load(&self.paths, &name) {
                Ok(m) => {
                    let version = m.system.version.as_deref().unwrap_or("-");
                    println!(
                        "{marker} {name}  {} {} {}x{}x{} system {version}",
                        m.hardware.model,
                        m.hardware.memory,
                        m.display.width,
                        m.display.height,
                        m.display.depth
                    );
                }
                Err(e) => println!("{marker} {name}  (unreadable: {e:#})"),
            }
        }
        Ok(())
    }

    fn list_builds(&self) -> Result<()> {
        for key in list_dir(&self.paths.builds())? {
            match read_toml::<Manifest>(&self.paths.builds().join(&key).join(MANIFEST)) {
                Ok(m) => println!(
                    "{}  {} {}  {}",
                    &key[..12.min(key.len())],
                    m.backend,
                    m.revision,
                    m.options.join(" ")
                ),
                Err(_) => println!("{key}  (no valid manifest)"),
            }
        }
        Ok(())
    }

    fn list_roms(&self) -> Result<()> {
        for rom in registry::list_roms(&self.paths)? {
            let models: Vec<_> = rom.models.iter().map(|m| m.name()).collect();
            let models = if models.is_empty() {
                "unrecognised".to_string()
            } else {
                models.join("/")
            };
            println!(
                "{}  {}K  {models}  {}",
                rom.name,
                rom.size / 1024,
                &rom.sha256[..12]
            );
        }
        Ok(())
    }

    fn list_systems(&self) -> Result<()> {
        for system in registry::list_systems(&self.paths)? {
            let version = system.version.as_deref().unwrap_or("-");
            let (_, file) = registry::load_system(&self.paths, &system.name)?;
            let format = disk::detect_format(&file)
                .map(registry::describe_format)
                .unwrap_or("unknown format");
            println!(
                "{}  {version}  {}K  {format}",
                system.name,
                system.size / 1024
            );
        }
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
        let model: Model = args.model.parse()?;
        let (rom, _) = registry::load_rom(&self.paths, &args.rom)?;
        registry::check_rom_for_model(&rom, model)?;
        let (system, template) = registry::load_system(&self.paths, &args.system)?;
        let format = disk::detect_format(&template)?;
        if format == disk::DiskFormat::RawHfs && !model.reads_hfs() {
            eprintln!(
                "warning: model {model} cannot read HFS, so {} will not boot",
                system.name
            );
        }

        let (width, height, depth) = match &args.display {
            Some(display) => parse_display(display)?,
            None => model.default_display(),
        };
        let backend = self.backends.default_backend();
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
                rom: rom.name,
                memory: args
                    .memory
                    .unwrap_or_else(|| model.default_memory().to_string())
                    .to_ascii_uppercase(),
            },
            display: Display {
                width,
                height,
                depth,
                scale: args.scale,
                fullscreen: args.fullscreen,
            },
            system: SystemInfo {
                version: system.version,
            },
            disks: vec![Disk {
                path: SYSTEM_DISK.to_string(),
                readonly: false,
                boot: true,
            }],
        };
        backend.validate(&machine)?;

        self.publish_machine(&machine, |dir| {
            let _guard = disk::lock_image(&self.paths, &template, LockMode::Shared)?;
            atomic_copy(&template, &dir.join(SYSTEM_DISK))
        })?;
        println!("Created machine {}", args.name);
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
            fs::rename(&staging, self.paths.machine(&machine.name))
                .context("cannot publish machine")
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
            confirm_delete(name, disks.len())?;
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
        registry::check_rom_for_model(&rom, machine.hardware.model)?;

        let saved_disks = machine.disk_paths(&self.paths)?;
        for (path, saved) in &saved_disks {
            if !path.exists() {
                bail!("machine {name:?} is missing its disk {}", path.display());
            }
            // On Windows, a running emulator shuts out even readers.
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            if disk::detect_format(path)? == disk::DiskFormat::DiskCopy42 && !saved.readonly {
                disk::warn_disk_copy_read_only(path);
            }
        }
        for path in &args.disks {
            drop(disk::lock_image(&self.paths, path, LockMode::Shared)?);
            disk::check_attachable(path, args.any_format, !args.readonly)?;
        }
        backend.validate_disks(&machine, saved_disks.len() + args.disks.len())?;

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
        for backend in self.backends.iter() {
            checks.extend(backend.doctor());
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
            let intact = read_toml::<Manifest>(&dir.join(MANIFEST)).is_ok_and(|manifest| {
                sha256_file(&dir.join(&manifest.executable))
                    .is_ok_and(|sha| sha == manifest.executable_sha256)
            });
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
                Ok(()) => checks.push(Check::ok(format!("machine {name} is runnable"))),
                Err(e) => checks.push(Check::fail(
                    format!("machine {name} cannot run"),
                    format!("{e:#}"),
                )),
            }
        }
        Ok(())
    }

    fn check_machine(&self, name: &str) -> Result<()> {
        let machine = Machine::load(&self.paths, name)?;
        self.backends
            .get(&machine.emulator.backend)?
            .validate(&machine)?;
        let (rom, _) = registry::load_rom(&self.paths, &machine.hardware.rom)?;
        registry::check_rom_for_model(&rom, machine.hardware.model)?;
        for (path, _) in machine.disk_paths(&self.paths)? {
            if !path.exists() {
                let file: PathBuf = path.file_name().map(PathBuf::from).unwrap_or_default();
                bail!("disk {} is missing", file.display());
            }
            // A disk in use belongs to a running machine, and may be unreadable.
            if disk::lock_image(&self.paths, &path, LockMode::Shared).is_ok() {
                disk::detect_format(&path)?;
            }
        }
        Ok(())
    }
}

fn confirm_delete(name: &str, disks: usize) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "removing {name:?} deletes its {disks} private disk(s); pass --delete-disks to confirm"
        );
    }
    print!("Delete machine {name:?} and its {disks} private disk(s)? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("cancelled; nothing was removed");
    }
    Ok(())
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
        // SIGILL, SIGABRT, SIGBUS, SIGFPE, and SIGSEGV on Linux.
        if let Some(signal) = status.signal() {
            return Some(if matches!(signal, 4 | 6 | 7 | 8 | 11) {
                format!("the emulator crashed on the host (signal {signal})")
            } else {
                format!("the emulator was killed by signal {signal}")
            });
        }
    }
    Some(format!("the emulator ended abnormally ({status})"))
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
