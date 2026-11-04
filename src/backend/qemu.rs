//! QEMU PowerPC adapter, for later classic Mac OS and PowerPC Mac OS X. It
//! runs the host's own `qemu-system-ppc` and builds nothing: the one on `PATH`,
//! or the one `[qemu-ppc] executable` names in `config.toml`.
//!
//! Unlike the other backends:
//!
//! - There's no Mac ROM. QEMU starts from its own Open Firmware, OpenBIOS, so a
//!   machine names no ROM, and `--rom` is refused.
//! - It emulates two Power Macs, chosen with `--model` and kept as `[qemu]
//!   machine`: `mac99`, a New World G4, with the PMU and USB keyboard and mouse
//!   that Mac OS 9 and Mac OS X expect, and `g3beige`, an Old World beige G3
//!   with ADB.
//! - Disks and CDs are IDE drives, four in all: QEMU refuses a fifth. Images are
//!   given whole, as raw disks, so partitioned disks and CDs mount as they are.
//!   OpenBIOS starts from the first CD when one has an HFS volume (`-boot d`),
//!   and otherwise from the first disk (`-boot c`).
//! - The display size and depth go to OpenBIOS (`-g`), which offers 8, 15, and
//!   32 bits. The window isn't magnified, so the scale is 1.
//! - Networking is QEMU's user-mode NAT with a Sun GEM card, which Mac OS 9 and
//!   Mac OS X drive as Apple's own.
//!
//! A machine records the QEMU version it was made with as its revision. That
//! describes it, but doesn't bind it: another host's QEMU runs it too.

use super::{CdImage, Check, EmulatorBackend, Executable, LaunchRequest};
use crate::disk::{self, CdVolume, Classification, DiskFormat};
use crate::machine::{Display, Machine, Network, QemuMachine, QemuSettings};
use crate::registry::Rom;
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const BACKEND_NAME: &str = "qemu-ppc";
const LABEL: &str = "QEMU";
const EXECUTABLE: &str = if cfg!(windows) {
    "qemu-system-ppc.exe"
} else {
    "qemu-system-ppc"
};
pub const DEFAULT_MEMORY: &str = "512M";
pub const DEFAULT_DISPLAY: (u32, u32, u32) = (1024, 768, 32);
/// IDE drives: two buses of two, for disks and CDs together.
pub const MAX_DRIVES: usize = 4;
pub const DEPTHS: [u32; 3] = [8, 15, 32];
const INSTALL_HINT: &str = "install QEMU's PowerPC emulator (Debian and Ubuntu: `apt install \
                            qemu-system-ppc`; Fedora: `dnf install qemu-system-ppc`; macOS: `brew \
                            install qemu`; Windows: qemu.org), or set `executable` under \
                            [qemu-ppc] in macman's config.toml";

/// The QEMU this host has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub version: String,
    /// The machine types it emulates, as `-machine help` lists them.
    pub machines: Vec<String>,
}

pub struct Qemu {
    executable: Option<PathBuf>,
    found: OnceLock<Result<Found, String>>,
}

impl Qemu {
    /// `executable`, if given, is used instead of looking on `PATH`.
    pub fn new(executable: Option<PathBuf>) -> Self {
        Self {
            executable,
            found: OnceLock::new(),
        }
    }

    /// The QEMU to run, found once per macman run.
    pub fn found(&self) -> Result<&Found> {
        self.found
            .get_or_init(|| discover(self.executable.as_deref()).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    fn check_machine(&self, kind: QemuMachine) -> Result<&Found> {
        let found = self.found()?;
        if !found.machines.iter().any(|m| m == kind.as_str()) {
            bail!(
                "QEMU {} at {} doesn't emulate {kind}; it has {}",
                found.version,
                found.path.display(),
                found.machines.join(", ")
            );
        }
        Ok(found)
    }
}

/// Where `qemu-system-ppc` is: `chosen`, or the first on `PATH`.
fn locate(chosen: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = chosen {
        if !path.is_file() {
            bail!(
                "QEMU isn't at {}, where [qemu-ppc] executable in config.toml says it is",
                path.display()
            );
        }
        return Ok(path.to_path_buf());
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(EXECUTABLE))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("{EXECUTABLE} isn't on PATH; {INSTALL_HINT}"))
}

fn output(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} {} failed: {}",
            path.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn discover(chosen: Option<&Path>) -> Result<Found> {
    let path = locate(chosen)?;
    let version = parse_version(&output(&path, &["--version"])?)
        .with_context(|| format!("{} doesn't say it's QEMU in its --version", path.display()))?;
    let machines = parse_machines(&output(&path, &["-machine", "help"])?);
    Ok(Found {
        path,
        version,
        machines,
    })
}

/// The version in `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1)`.
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().next()?;
    let rest = line.split("version ").nth(1)?;
    let version = rest.split_whitespace().next()?;
    version
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// The machine names in `-machine help`, one per line after its heading.
pub fn parse_machines(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|line| !line.starts_with("Supported machines"))
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

pub fn settings(machine: &Machine) -> QemuSettings {
    machine.qemu.clone().unwrap_or_default()
}

fn megabytes(memory: &str) -> Option<u32> {
    match memory.strip_suffix('G') {
        Some(gigabytes) => gigabytes.parse::<u32>().ok()?.checked_mul(1024),
        None => memory.strip_suffix('M')?.parse().ok(),
    }
}

/// QEMU's own limits: a G4 mac99 takes 2048M, and a g3beige 2047M.
fn validate_memory(kind: QemuMachine, memory: &str) -> Result<()> {
    let most = match kind {
        QemuMachine::Mac99 => 2048,
        QemuMachine::G3beige => 2047,
    };
    match megabytes(memory) {
        Some(size) if (32..=most).contains(&size) => Ok(()),
        _ => bail!(
            "memory {memory} is not supported on a QEMU {kind}; use whole megabytes, 32M to \
             {most}M, as in 512M"
        ),
    }
}

fn validate_display(display: &Display) -> Result<()> {
    let Display {
        width,
        height,
        depth,
        scale,
        ..
    } = *display;
    if !(640..=3840).contains(&width) || !(480..=2160).contains(&height) {
        bail!("display size {width}x{height} is out of range; 640x480 to 3840x2160 work");
    }
    if !DEPTHS.contains(&depth) {
        bail!("display depth {depth} isn't one QEMU's Open Firmware offers; use 8, 15, or 32");
    }
    if scale != 1 {
        bail!(
            "display scale {scale} isn't supported with QEMU, whose window shows the Mac's \
             screen at its own size; use 1"
        );
    }
    Ok(())
}

/// A path as one value of a `-drive` option, in which a comma is doubled.
fn drive_value(path: &Path) -> OsString {
    let text = path.to_string_lossy();
    if text.contains(',') {
        text.replace(',', ",,").into()
    } else {
        path.as_os_str().to_owned()
    }
}

fn drive(path: &Path, media: &str, writable: bool) -> OsString {
    let mut value = OsString::from("file=");
    value.push(drive_value(path));
    value.push(format!(",format=raw,media={media}"));
    if !writable {
        value.push(",readonly=on");
    }
    value
}

/// QEMU's arguments for a run.
pub fn arguments(request: &LaunchRequest<'_>) -> Result<Vec<OsString>> {
    let machine = request.machine;
    let settings = settings(machine);
    let drives = request.disks.len() + request.cdroms.len();
    if drives > MAX_DRIVES {
        bail!(
            "QEMU has {MAX_DRIVES} IDE drives, and this run needs {drives}, for {} disk(s) and {} \
             CD(s); attach fewer",
            request.disks.len(),
            request.cdroms.len()
        );
    }
    let display = &machine.display;
    let megabytes = megabytes(&machine.hardware.memory).unwrap_or(512);
    let model = match settings.machine {
        QemuMachine::Mac99 => "mac99,via=pmu",
        QemuMachine::G3beige => "g3beige",
    };
    let starts_from_cd = request.cdroms.iter().any(|cd| cd.volume == CdVolume::Hfs);
    let mut arguments: Vec<OsString> = vec![
        "-M".into(),
        model.into(),
        "-m".into(),
        megabytes.to_string().into(),
        "-g".into(),
        format!("{}x{}x{}", display.width, display.height, display.depth).into(),
        "-name".into(),
        machine.name.clone().into(),
        "-prom-env".into(),
        "auto-boot?=true".into(),
        "-boot".into(),
        if starts_from_cd { "d" } else { "c" }.into(),
    ];
    for disk in request.disks {
        arguments.extend(["-drive".into(), drive(&disk.path, "disk", disk.writable)]);
    }
    // Discs that can start the Mac first, as the first is the one it starts from.
    let mut cdroms: Vec<&CdImage> = request.cdroms.iter().collect();
    cdroms.sort_by_key(|cd| cd.volume != CdVolume::Hfs);
    for cdrom in cdroms {
        arguments.extend(["-drive".into(), drive(&cdrom.path, "cdrom", false)]);
    }
    arguments.extend(match settings.network {
        Network::None => ["-nic".into(), "none".into()],
        Network::Slirp => ["-nic".into(), "user,model=sungem".into()],
    });
    if !settings.sound {
        arguments.extend(["-audio".into(), "none".into()]);
    }
    if display.fullscreen {
        arguments.push("-full-screen".into());
    }
    Ok(arguments)
}

/// Start QEMU and wait for it. It says what's wrong on stderr, and exits with
/// a failure, so its last words are the error.
fn supervise(mut command: Command, verbose: bool) -> Result<ExitStatus> {
    command.stdin(Stdio::null()).stderr(Stdio::piped());
    super::macemu::die_with_macman(&mut command);
    if verbose {
        eprintln!("Launching {command:?}");
    }
    let program = command.get_program().to_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {}", program.to_string_lossy()))?;
    let mut lines = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap_or_default();
            if verbose {
                eprintln!("{line}");
            }
            lines.push(line);
        }
    }
    let status = child.wait().context("cannot wait for QEMU")?;
    if !status.success() {
        let said: Vec<&str> = lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect();
        if !said.is_empty() {
            bail!("{LABEL} failed: {}", said.join("; "));
        }
    }
    Ok(status)
}

/// Whether QEMU can start a machine of `kind` at all, which needs its Open
/// Firmware to be installed: it's paused before the guest's first instruction
/// and stopped after a moment, or reports why it can't start.
fn firmware_check(path: &Path, kind: QemuMachine) -> Check {
    let model = match kind {
        QemuMachine::Mac99 => "mac99,via=pmu",
        QemuMachine::G3beige => "g3beige",
    };
    let child = Command::new(path)
        .args(["-M", model, "-S", "-display", "none", "-monitor", "none"])
        .args(["-serial", "none", "-nic", "none", "-audio", "none"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return Check::fail(format!("QEMU can't start a {kind}"), INSTALL_HINT);
    };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if let Ok(Some(_)) = child.try_wait() {
            let mut said = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut said);
            }
            return Check::fail(
                format!("QEMU can't start a {kind}: {}", said.trim()),
                "install QEMU's PowerPC firmware (Debian and Ubuntu: `apt install openbios-ppc`)",
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Check::ok(format!("QEMU starts a {kind}"))
}

impl EmulatorBackend for Qemu {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    /// The host QEMU's version, which a new machine records.
    fn revision(&self) -> &str {
        self.found()
            .map(|found| found.version.as_str())
            .unwrap_or("unknown")
    }

    /// Any QEMU runs a machine made with another: QEMU keeps its Power Macs
    /// as they were, and the machine records only what it was made with.
    fn resolve_revision(&self, _recorded: &str) -> Result<String> {
        Ok(self.found()?.version.clone())
    }

    fn validate(&self, machine: &Machine) -> Result<()> {
        if machine.hardware.model.is_some() {
            bail!(
                "machine {:?} names a Mini vMac model; a QEMU machine has [qemu] machine instead",
                machine.name
            );
        }
        if let Some(rom) = &machine.hardware.rom {
            bail!(
                "machine {:?} names ROM {rom:?}, but QEMU starts from its own Open Firmware and \
                 takes no Mac ROM; leave it out",
                machine.name
            );
        }
        if machine.basilisk2.is_some() || machine.sheepshaver.is_some() {
            bail!(
                "machine {:?} has settings for another emulator; a QEMU machine has only [qemu]",
                machine.name
            );
        }
        validate_memory(settings(machine).machine, &machine.hardware.memory)?;
        validate_display(&machine.display)
    }

    fn validate_disks(&self, _machine: &Machine, count: usize) -> Result<()> {
        if count > MAX_DRIVES {
            bail!("QEMU has {MAX_DRIVES} IDE drives, so a machine can't have {count} disks");
        }
        Ok(())
    }

    fn check_host(&self) -> Result<()> {
        let found = self.found()?;
        if !["mac99", "g3beige"]
            .iter()
            .any(|kind| found.machines.iter().any(|m| m == kind))
        {
            bail!(
                "QEMU {} at {} emulates neither mac99 nor g3beige",
                found.version,
                found.path.display()
            );
        }
        Ok(())
    }

    fn uses_rom(&self) -> bool {
        false
    }

    fn check_rom(&self, _machine: &Machine, rom: &Rom, _file: &Path) -> Result<()> {
        bail!(
            "ROM {:?} isn't used: QEMU starts from its own Open Firmware",
            rom.name
        )
    }

    fn check_disk(&self, path: &Path, _writable: bool) -> Result<()> {
        if disk::classify(path)? == Classification::Supported(DiskFormat::DiskCopy42) {
            bail!(
                "{} is a Disk Copy 4.2 image, which QEMU reads as a raw disk, header and all; \
                 take the volume out with `macman disk extract`",
                path.display()
            );
        }
        Ok(())
    }

    fn installs_from_cd(&self) -> bool {
        true
    }

    fn mounts_partitioned_disks(&self) -> bool {
        true
    }

    fn prepare(&self, machine: &Machine, _verbose: bool) -> Result<Executable> {
        self.validate(machine)?;
        let found = self.check_machine(settings(machine).machine)?;
        Ok(Executable {
            path: found.path.clone(),
            build_key: format!("qemu-system-ppc {}", found.version),
            built: false,
        })
    }

    fn launch(&self, request: &LaunchRequest<'_>) -> Result<ExitStatus> {
        let mut command = Command::new(&request.executable.path);
        command
            .args(arguments(request)?)
            .current_dir(request.workdir);
        supervise(command, request.verbose)
    }

    fn doctor(&self) -> Vec<Check> {
        let found = match self.found() {
            Ok(found) => found,
            Err(error) => {
                return vec![Check::warn(
                    "QEMU isn't available, so qemu-ppc machines can't run",
                    format!("{error:#}"),
                )];
            }
        };
        let mut checks = vec![Check::ok(format!(
            "QEMU {} found at {}, for qemu-ppc",
            found.version,
            found.path.display()
        ))];
        for kind in [QemuMachine::Mac99, QemuMachine::G3beige] {
            if found.machines.iter().any(|m| m == kind.as_str()) {
                checks.push(firmware_check(&found.path, kind));
            } else {
                checks.push(Check::warn(
                    format!("QEMU {} doesn't emulate {kind}", found.version),
                    "install a QEMU built with PowerPC Mac support",
                ));
            }
        }
        checks
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::ResolvedDisk;
    use crate::machine::{Disk, EmulatorConfig, Hardware, SCHEMA_VERSION, SystemInfo};

    pub(crate) fn machine(kind: QemuMachine) -> Machine {
        Machine {
            schema_version: SCHEMA_VERSION,
            name: "g4".into(),
            id: None,
            ephemeral: false,
            emulator: EmulatorConfig {
                backend: BACKEND_NAME.into(),
                revision: "10.2.1".into(),
            },
            hardware: Hardware {
                model: None,
                rom: None,
                memory: "512M".into(),
            },
            display: Display {
                width: 1024,
                height: 768,
                depth: 32,
                scale: 1,
                fullscreen: false,
            },
            system: SystemInfo::default(),
            basilisk2: None,
            sheepshaver: None,
            qemu: Some(QemuSettings {
                machine: kind,
                ..QemuSettings::default()
            }),
            disks: vec![Disk {
                path: "system.dsk".into(),
                readonly: false,
                boot: true,
            }],
        }
    }

    fn strings(arguments: &[OsString]) -> Vec<String> {
        arguments
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn versions_and_machines_are_read_from_qemu() {
        assert_eq!(
            parse_version(
                "QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.3)\nCopyright..."
            )
            .as_deref(),
            Some("10.2.1")
        );
        assert_eq!(
            parse_version("QEMU emulator version 8.2.0\n").as_deref(),
            Some("8.2.0")
        );
        assert_eq!(parse_version("something else"), None);
        let help = "Supported machines are:\n40p                  IBM RS/6000 7020 (40p)\n\
                    g3beige              Heathrow based PowerMac (default)\n\
                    mac99                Mac99 based PowerMac\n";
        assert_eq!(parse_machines(help), ["40p", "g3beige", "mac99"]);
    }

    #[test]
    fn arguments_describe_the_machine_and_its_drives() {
        let mut g4 = machine(QemuMachine::Mac99);
        g4.qemu.as_mut().unwrap().network = Network::Slirp;
        g4.display.fullscreen = true;
        let executable = Executable {
            path: "qemu-system-ppc".into(),
            build_key: String::new(),
            built: false,
        };
        let disks = [
            ResolvedDisk {
                path: "/m/g4/system.dsk".into(),
                writable: true,
            },
            ResolvedDisk {
                path: "/data/a,b.img".into(),
                writable: false,
            },
        ];
        let cdroms = [
            CdImage {
                path: "/cds/data.iso".into(),
                volume: CdVolume::Iso9660,
            },
            CdImage {
                path: "/cds/Mac OS 9.toast".into(),
                volume: CdVolume::Hfs,
            },
        ];
        let request = LaunchRequest {
            machine: &g4,
            executable: &executable,
            rom: Path::new(""),
            disks: &disks,
            cdroms: &cdroms,
            workdir: Path::new("/tmp"),
            verbose: false,
        };
        assert_eq!(
            strings(&arguments(&request).unwrap()).join(" "),
            "-M mac99,via=pmu -m 512 -g 1024x768x32 -name g4 -prom-env auto-boot?=true -boot d \
             -drive file=/m/g4/system.dsk,format=raw,media=disk \
             -drive file=/data/a,,b.img,format=raw,media=disk,readonly=on \
             -drive file=/cds/Mac OS 9.toast,format=raw,media=cdrom,readonly=on \
             -drive file=/cds/data.iso,format=raw,media=cdrom,readonly=on \
             -nic user,model=sungem -full-screen"
        );

        // Without a disc that starts a Mac, it starts from the disk.
        let g3 = machine(QemuMachine::G3beige);
        let request = LaunchRequest {
            machine: &g3,
            cdroms: &cdroms[..1],
            disks: &disks[..1],
            ..request
        };
        let arguments = strings(&arguments(&request).unwrap()).join(" ");
        assert!(arguments.starts_with("-M g3beige -m 512"), "{arguments}");
        assert!(arguments.contains("-boot c"), "{arguments}");
        assert!(arguments.ends_with("-nic none"), "{arguments}");

        let mut quiet = machine(QemuMachine::Mac99);
        quiet.qemu.as_mut().unwrap().sound = false;
        let request = LaunchRequest {
            machine: &quiet,
            cdroms: &[],
            ..request
        };
        assert!(strings(&super::arguments(&request).unwrap()).ends_with(&[
            "-nic".to_string(),
            "none".into(),
            "-audio".into(),
            "none".into()
        ]));
    }

    #[test]
    fn a_fifth_drive_is_refused() {
        let g4 = machine(QemuMachine::Mac99);
        let executable = Executable {
            path: "qemu-system-ppc".into(),
            build_key: String::new(),
            built: false,
        };
        let disks: Vec<ResolvedDisk> = (0..3)
            .map(|i| ResolvedDisk {
                path: format!("/d{i}").into(),
                writable: true,
            })
            .collect();
        let cdroms: Vec<CdImage> = (0..2)
            .map(|i| CdImage {
                path: format!("/c{i}").into(),
                volume: CdVolume::Hfs,
            })
            .collect();
        let request = LaunchRequest {
            machine: &g4,
            executable: &executable,
            rom: Path::new(""),
            disks: &disks,
            cdroms: &cdroms,
            workdir: Path::new("/tmp"),
            verbose: false,
        };
        let error = arguments(&request).unwrap_err().to_string();
        assert!(
            error.contains("needs 5, for 3 disk(s) and 2 CD(s)"),
            "{error}"
        );
    }

    #[test]
    fn machines_are_held_to_what_qemu_emulates() {
        let qemu = Qemu::new(Some("/no/such/qemu".into()));
        let mut g4 = machine(QemuMachine::Mac99);
        qemu.validate(&g4).unwrap();
        g4.hardware.memory = "2048M".into();
        qemu.validate(&g4).unwrap();
        g4.hardware.memory = "2G".into();
        qemu.validate(&g4).unwrap();
        let mut g3 = machine(QemuMachine::G3beige);
        g3.hardware.memory = "2048M".into();
        let error = qemu.validate(&g3).unwrap_err().to_string();
        assert!(error.contains("32M to 2047M"), "{error}");

        let mut bad = machine(QemuMachine::Mac99);
        bad.display.scale = 2;
        assert!(
            qemu.validate(&bad)
                .unwrap_err()
                .to_string()
                .contains("use 1")
        );
        let mut bad = machine(QemuMachine::Mac99);
        bad.display.depth = 16;
        assert!(
            qemu.validate(&bad)
                .unwrap_err()
                .to_string()
                .contains("8, 15, or 32")
        );
        let mut bad = machine(QemuMachine::Mac99);
        bad.hardware.rom = Some("newworld".into());
        assert!(
            qemu.validate(&bad)
                .unwrap_err()
                .to_string()
                .contains("takes no Mac ROM")
        );
        assert!(qemu.validate_disks(&g4, 4).is_ok());
        assert!(qemu.validate_disks(&g4, 5).is_err());

        // Validating needs no QEMU, so a machine can pass through a host
        // without one; preparing to run it does.
        let error = qemu.prepare(&g4, false).unwrap_err().to_string();
        assert!(error.contains("/no/such/qemu"), "{error}");
        assert!(qemu.check_host().is_err());
        assert_eq!(qemu.revision(), "unknown");
    }
}
