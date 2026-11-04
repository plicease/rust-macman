//! The QEMU PowerPC adapter, with a script standing in for `qemu-system-ppc`.
//! The script answers `--version` and `-machine help` as QEMU does, records how
//! it was run, and acts out a failure on request. None of this shows that the
//! real QEMU boots a Mac: for that, see docs/SMOKE_TEST.md.
#![cfg(unix)]

mod common;

use anyhow::Result;
use clap::Parser;
use common::{write_disk, write_partitioned_disk};
use macman::app::App;
use macman::archive;
use macman::backend::Backends;
use macman::backend::qemu::{BACKEND_NAME, Qemu};
use macman::cli::Cli;
use macman::machine::{Machine, Network, QemuMachine};
use macman::paths::Paths;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use tempfile::TempDir;

/// Stands in for QEMU. `-S` is only ever the doctor's firmware check.
const QEMU: &str = r#"#!/bin/sh
case "$1" in
    --version)
        echo "QEMU emulator version VERSION (fixture)"
        echo "Copyright (c) 2003-2025 Fabrice Bellard and the QEMU Project developers"
        exit 0 ;;
    -machine)
        echo "Supported machines are:"
        echo "40p                  IBM RS/6000 7020 (40p)"
        echo "g3beige              Heathrow based PowerMAC (default)"
        echo "mac99                Mac99 based PowerMAC"
        echo "none                 empty machine"
        exit 0 ;;
    -display)
        echo "Available display backend types:"
        echo "none"
        echo "gtk"
        if [ "$(cat MODE 2>/dev/null)" != nosdl ]; then echo "sdl"; fi
        echo
        echo "Some display backends support suboptions, which can be set with"
        exit 0 ;;
esac
mode=$(cat MODE 2>/dev/null)
for argument in "$@"; do
    if [ "$argument" = -S ]; then
        if [ "$mode" = nofirmware ]; then
            echo "qemu-system-ppc: Could not find PowerPC BIOS 'openbios-ppc'" >&2
            exit 1
        fi
        sleep 10
        exit 0
    fi
done
echo "run $*" >> LOG
echo "cwd $(pwd -P)" >> LOG
case "$mode" in
    fail)
        echo "qemu-system-ppc: -drive file=x: Could not open 'x': Permission denied" >&2
        exit 1 ;;
    crash) kill -SEGV $$ ;;
esac
exit 0
"#;

/// Spawning tests run one at a time: a child holds its parent's descriptors
/// between fork and exec, and with them any file lock.
static ALONE: Mutex<()> = Mutex::new(());

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
    qemu: PathBuf,
    log: PathBuf,
    _turn: MutexGuard<'static, ()>,
}

fn write_qemu(dir: &Path, version: &str, log: &Path, mode: &Path) -> PathBuf {
    let path = dir.join("qemu-system-ppc");
    let text = QEMU
        .replace("VERSION", version)
        .replace("LOG", log.to_str().unwrap())
        .replace("MODE", mode.to_str().unwrap());
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn backends(qemu: &Path) -> Backends {
    Backends::new(vec![Box::new(Qemu::new(Some(qemu.to_path_buf())))])
}

impl Harness {
    fn new() -> Self {
        let turn = ALONE.lock().unwrap_or_else(PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("qemu.log");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let qemu = write_qemu(&bin, "10.2.1", &log, &root.path().join("mode"));
        let paths = Paths::under(&fs::canonicalize(root.path()).unwrap());
        let app = App::new(paths.clone(), backends(&qemu));
        let harness = Self {
            root,
            paths,
            app,
            qemu,
            log,
            _turn: turn,
        };
        let system = write_disk(harness.root.path(), "system.dsk");
        harness
            .cmd(&["system", "add", system.to_str().unwrap(), "--name", "sys"])
            .unwrap();
        harness
    }

    fn cmd(&self, args: &[&str]) -> Result<u8> {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        self.app.execute(Cli::parse_from(argv))
    }

    fn create_with(&self, name: &str, extra: &[&str]) {
        let mut args = vec!["create", name, "--system", "sys"];
        args.extend(extra);
        self.cmd(&args).unwrap();
    }

    fn create(&self, name: &str) {
        self.create_with(name, &[]);
    }

    fn create_error(&self, extra: &[&str]) -> String {
        let mut args = vec!["create", "bad", "--system", "sys"];
        args.extend(extra);
        format!("{:#}", self.cmd(&args).unwrap_err())
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn runs(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter_map(|line| line.strip_prefix("run "))
            .map(str::to_string)
            .collect()
    }

    fn act(&self, mode: &str) {
        fs::write(self.root.path().join("mode"), mode).unwrap();
    }

    fn doctor(&self) -> Vec<String> {
        self.app
            .doctor_checks(false)
            .unwrap()
            .iter()
            .map(|check| format!("{:?} {}", check.level, check.message))
            .collect()
    }
}

#[test]
fn a_machine_needs_no_rom_and_records_the_qemu_version() {
    let h = Harness::new();
    h.create("g4");
    let m = Machine::load(&h.paths, "g4").unwrap();
    assert_eq!(m.emulator.backend, BACKEND_NAME);
    assert_eq!(m.emulator.revision, "10.2.1");
    assert_eq!(m.hardware.rom, None);
    assert_eq!(m.hardware.model, None);
    assert_eq!(m.hardware.memory, "512M");
    assert_eq!(
        (
            m.display.width,
            m.display.height,
            m.display.depth,
            m.display.scale
        ),
        (1024, 768, 32, 1)
    );
    let settings = m.qemu.unwrap();
    assert_eq!(settings.machine, QemuMachine::Mac99);
    assert!(settings.sound);
    assert_eq!(settings.network, Network::None);

    let text = fs::read_to_string(h.paths.machine("g4").join("machine.toml")).unwrap();
    assert!(!text.contains("rom"), "{text}");
    assert!(text.contains("[qemu]"), "{text}");
    assert!(text.contains("machine = \"mac99\""), "{text}");

    h.create_with(
        "beige",
        &[
            "--model",
            "g3beige",
            "--memory",
            "256M",
            "--display",
            "800x600x8",
            "--network",
            "slirp",
            "--no-sound",
        ],
    );
    let m = Machine::load(&h.paths, "beige").unwrap();
    let settings = m.qemu.unwrap();
    assert_eq!(settings.machine, QemuMachine::G3beige);
    assert!(!settings.sound);
    assert_eq!(settings.network, Network::Slirp);
    assert_eq!(m.hardware.memory, "256M");
}

#[test]
fn what_qemu_does_not_have_is_refused() {
    let h = Harness::new();
    fs::write(h.root.path().join("r.rom"), vec![0u8; 4 << 20]).unwrap();
    for (extra, expected) in [
        (&["--rom", "r"][..], "Open Firmware"),
        (&["--model", "plus"], "mac99"),
        (&["--cpu", "68040"], "needs --backend basilisk2"),
        (&["--memory", "4G"], "32M to 2048M"),
        (&["--model", "g3beige", "--memory", "2048M"], "32M to 2047M"),
        (&["--display", "640x480x16"], "8, 15, or 32"),
        (&["--display", "320x200x8"], "out of range"),
        (&["--scale", "2"], "use 1"),
        (
            &["--shared-folder", "/tmp"],
            "needs --backend basilisk2 or sheepshaver",
        ),
    ] {
        let error = h.create_error(extra);
        assert!(error.contains(expected), "{extra:?}: {error}");
    }
    assert!(!h.paths.machine("bad").exists());
    assert!(h.runs().is_empty());
}

#[test]
fn a_run_describes_the_machine_to_qemu() {
    let h = Harness::new();
    h.create("g4");
    assert_eq!(h.cmd(&["run", "g4"]).unwrap(), 0);
    let disk = h.paths.machine("g4").join("system.dsk");
    let runs = h.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0],
        format!(
            "-M mac99,via=pmu -m 512 -g 1024x768x32 -name g4 -prom-env auto-boot?=true -boot c \
             -drive file={},format=raw,media=disk -nic none{}",
            disk.display(),
            if cfg!(target_os = "linux") {
                " -display sdl"
            } else {
                ""
            }
        )
    );

    h.create_with(
        "beige",
        &[
            "--model",
            "g3beige",
            "--network",
            "slirp",
            "--no-sound",
            "--fullscreen",
        ],
    );
    h.cmd(&["run", "beige"]).unwrap();
    let run = h.runs().pop().unwrap();
    assert!(run.starts_with("-M g3beige -m 512 "), "{run}");
    assert!(
        run.contains("-nic user,model=sungem -audio none -full-screen"),
        "{run}"
    );
}

#[test]
fn an_hfs_cd_is_started_from_and_never_written() {
    let h = Harness::new();
    h.cmd(&[
        "create",
        "new",
        "--backend",
        "qemu-ppc",
        "--disk-size",
        "8M",
    ])
    .unwrap();
    let disk = h.paths.machine("new").join("system.dsk");
    assert_eq!(fs::metadata(&disk).unwrap().len(), 8 << 20);

    let (cd, _) = write_partitioned_disk(h.root.path(), "Mac OS 9, installer.iso");
    h.cmd(&["run", "new", "--cdrom", cd.to_str().unwrap()])
        .unwrap();
    let run = h.runs().pop().unwrap();
    let resolved = fs::canonicalize(&cd).unwrap();
    let doubled = resolved.to_str().unwrap().replace(',', ",,");
    assert!(run.contains(" -boot d "), "{run}");
    assert!(
        run.contains(&format!(
            "-drive file={},format=raw,media=disk -drive file={doubled},format=raw,media=cdrom,readonly=on",
            disk.display()
        )),
        "{run}"
    );

    // Registered media go the same way.
    h.cmd(&["media", "add", cd.to_str().unwrap(), "--name", "os9"])
        .unwrap();
    h.cmd(&["run", "new", "--media", "os9"]).unwrap();
    let run = h.runs().pop().unwrap();
    assert!(run.contains(" -boot d "), "{run}");
    assert!(run.contains("media=cdrom,readonly=on"), "{run}");

    // Without one, it starts from its disk.
    h.cmd(&["run", "new"]).unwrap();
    let run = h.runs().pop().unwrap();
    assert!(run.contains(" -boot c "), "{run}");
    assert!(!run.contains("cdrom"), "{run}");
}

#[test]
fn qemu_has_four_drives() {
    let h = Harness::new();
    h.create("g4");
    let dir = h.root.path();
    let disks: Vec<PathBuf> = (0..3)
        .map(|i| write_disk(dir, &format!("data{i}.dsk")))
        .collect();
    let (cd, _) = write_partitioned_disk(dir, "cd.iso");
    let mut args = vec!["run", "g4"];
    for disk in &disks {
        args.extend(["--disk", disk.to_str().unwrap()]);
    }
    assert_eq!(h.cmd(&args).unwrap(), 0);
    args.extend(["--cdrom", cd.to_str().unwrap()]);
    let error = format!("{:#}", h.cmd(&args).unwrap_err());
    assert!(error.contains("4 IDE drives"), "{error}");
    assert_eq!(h.runs().len(), 1);
}

/// GTK's window can't keep the mouse on Wayland, so SDL's is asked for on
/// Linux when QEMU has it, and its absence is reported.
#[cfg(target_os = "linux")]
#[test]
fn sdl_s_window_is_chosen_when_qemu_has_one() {
    let h = Harness::new();
    h.create("g4");
    let says_no_sdl = |h: &Harness| h.doctor().iter().any(|l| l.contains("no SDL window"));
    assert!(!says_no_sdl(&h));
    h.cmd(&["run", "g4"]).unwrap();
    assert!(h.runs().pop().unwrap().ends_with(" -display sdl"));

    // A new macman run asks QEMU again.
    h.act("nosdl");
    let h2 = App::new(h.paths.clone(), backends(&h.qemu));
    h2.execute(Cli::parse_from(["macman", "run", "g4"]))
        .unwrap();
    assert!(!h.runs().pop().unwrap().contains("-display"));
    let doctor: Vec<String> = h2
        .doctor_checks(false)
        .unwrap()
        .iter()
        .map(|check| check.message.clone())
        .collect();
    assert!(
        doctor.iter().any(|l| l.contains("no SDL window")),
        "{doctor:?}"
    );
}

#[test]
fn qemu_s_own_words_explain_a_failure() {
    let h = Harness::new();
    h.create("g4");
    h.act("fail");
    let error = format!("{:#}", h.cmd(&["run", "g4"]).unwrap_err());
    assert!(error.contains("QEMU failed"), "{error}");
    assert!(error.contains("Permission denied"), "{error}");
}

#[test]
fn doctor_reports_the_version_and_each_machine() {
    let h = Harness::new();
    h.create("g4");
    let doctor = h.doctor();
    let found = format!("QEMU 10.2.1 found at {}", h.qemu.display());
    assert!(
        doctor.iter().any(|line| line.contains(&found)),
        "{doctor:?}"
    );
    assert!(
        doctor
            .iter()
            .any(|line| line.contains("QEMU starts a mac99")),
        "{doctor:?}"
    );
    assert!(
        doctor
            .iter()
            .any(|line| line.contains("machine g4 is runnable")),
        "{doctor:?}"
    );

    h.act("nofirmware");
    let doctor = h.doctor();
    assert!(
        doctor
            .iter()
            .any(|line| line.contains("QEMU can't start a g3beige") && line.contains("openbios")),
        "{doctor:?}"
    );
}

#[test]
fn without_qemu_nothing_is_made_or_run() {
    let h = Harness::new();
    let missing = h.root.path().join("no-such-qemu");
    let app = App::new(h.paths.clone(), backends(&missing));
    let run = |args: &[&str]| {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        app.execute(Cli::parse_from(argv))
    };
    let error = format!(
        "{:#}",
        run(&["create", "g4", "--system", "sys"]).unwrap_err()
    );
    assert!(error.contains("[qemu-ppc] executable"), "{error}");
    assert!(!h.paths.machine("g4").exists());

    h.create("g4");
    let error = format!("{:#}", run(&["run", "g4"]).unwrap_err());
    assert!(error.contains("[qemu-ppc] executable"), "{error}");
    let doctor: Vec<String> = app
        .doctor_checks(false)
        .unwrap()
        .iter()
        .map(|check| {
            format!(
                "{} {}",
                check.message,
                check.fix.clone().unwrap_or_default()
            )
        })
        .collect();
    assert!(
        doctor
            .iter()
            .any(|line| line.contains("QEMU isn't available")),
        "{doctor:?}"
    );
}

#[test]
fn a_machine_moves_to_another_host_s_qemu() {
    let h = Harness::new();
    h.create_with("g4", &["--model", "g3beige"]);
    let exported = h.root.path().join("g4.tar.gz");
    h.cmd(&["export", "g4", "-o", exported.to_str().unwrap()])
        .unwrap();

    let other = tempfile::tempdir().unwrap();
    let log = other.path().join("qemu.log");
    let qemu = write_qemu(other.path(), "9.2.0", &log, &other.path().join("mode"));
    let paths = Paths::under(other.path());
    let report = archive::import(&paths, &backends(&qemu), &exported, None, false).unwrap();
    assert_eq!(report.imported(), 1);
    let imported = Machine::load(&paths, "g4").unwrap();
    assert_eq!(imported.hardware.rom, None);
    assert_eq!(imported.emulator.revision, "10.2.1");
    assert_eq!(imported.qemu.unwrap().machine, QemuMachine::G3beige);

    let app = App::new(paths.clone(), backends(&qemu));
    let argv = ["macman", "run", "g4"];
    assert_eq!(app.execute(Cli::parse_from(argv)).unwrap(), 0);
    assert!(fs::read_to_string(&log).unwrap().contains("-M g3beige"));

    // Coming back changes nothing.
    let report = archive::import(&h.paths, &h.app.backends, &exported, None, false).unwrap();
    assert_eq!(report.unchanged(), 1);
}

#[test]
fn config_toml_names_the_executable() {
    let h = Harness::new();
    fs::create_dir_all(h.paths.config_file().parent().unwrap()).unwrap();
    fs::write(
        h.paths.config_file(),
        format!("[qemu-ppc]\nexecutable = {:?}\n", h.qemu.to_str().unwrap()),
    )
    .unwrap();
    let backends = macman::default_backends(&h.paths).unwrap();
    let qemu = backends.get(BACKEND_NAME).unwrap();
    assert_eq!(qemu.revision(), "10.2.1");
    assert_eq!(backends.default_backend().name(), "minivmac");
}
