//! `macman run`, against a mock backend and against the real Mini vMac adapter
//! fed a small fixture source tree, which needs only gcc, make, bash, and tar.
//!
//! Unix only for now: the mock builds Unix exit statuses and the fixture is a
//! shell script. Windows support (#6) will need its own versions of these.
#![cfg(unix)]

mod common;

use anyhow::{Result, bail};
use clap::Parser;
use common::{II_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_partitioned_disk, write_rom};
use macman::app::App;
use macman::backend::minivmac::{MiniVmac, SourcePin};
use macman::backend::{
    Backends, Check, CheckLevel, EmulatorBackend, Executable, LaunchRequest, ResolvedDisk,
};
use macman::cli::Cli;
use macman::disk::{LockMode, lock_image};
use macman::fsutil::sha256_file;
use macman::machine::Machine;
use macman::paths::Paths;
use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::rc::Rc;
use tempfile::TempDir;

#[derive(Debug, Clone)]
struct Launch {
    workdir: PathBuf,
    rom: PathBuf,
    disks: Vec<ResolvedDisk>,
}

type DuringLaunch = Box<dyn FnOnce(&Launch)>;

#[derive(Default)]
struct MockState {
    launches: Vec<Launch>,
    /// Runs while the emulator is "running".
    during: Option<DuringLaunch>,
    exit: i32,
    signal: i32,
    fail_prepare: bool,
    fail_launch: bool,
}

#[derive(Clone, Default)]
struct Mock(Rc<RefCell<MockState>>);

impl EmulatorBackend for Mock {
    fn name(&self) -> &str {
        "minivmac"
    }
    fn revision(&self) -> &str {
        "36.04"
    }
    fn validate(&self, _: &Machine) -> Result<()> {
        Ok(())
    }
    fn validate_disks(&self, _: &Machine, count: usize) -> Result<()> {
        if count > 6 {
            bail!("too many disks");
        }
        Ok(())
    }
    fn prepare(&self, _: &Machine, _: bool) -> Result<Executable> {
        if self.0.borrow().fail_prepare {
            bail!("build failed");
        }
        Ok(Executable {
            path: "/mock".into(),
            build_key: "0123456789abcdef".into(),
            built: false,
        })
    }
    fn launch(&self, request: &LaunchRequest<'_>) -> Result<ExitStatus> {
        if self.0.borrow().fail_launch {
            bail!("cannot start emulator");
        }
        let launch = Launch {
            workdir: request.workdir.to_path_buf(),
            rom: request.rom.to_path_buf(),
            disks: request.disks.to_vec(),
        };
        let during = self.0.borrow_mut().during.take();
        if let Some(during) = during {
            during(&launch);
        }
        let mut state = self.0.borrow_mut();
        state.launches.push(launch);
        Ok(ExitStatus::from_raw(state.exit << 8 | state.signal))
    }
    fn doctor(&self) -> Vec<Check> {
        vec![]
    }
}

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
}

impl Harness {
    fn new(backend: impl EmulatorBackend + 'static, paths: Paths, root: TempDir) -> Self {
        let app = App::new(paths.clone(), Backends::new(vec![Box::new(backend)]));
        let harness = Self { root, paths, app };
        let rom = write_rom(harness.dir(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        let ii = write_rom(harness.dir(), "ii.bin", II_ROM_CHECKSUM, 256 * 1024);
        let system = write_disk(harness.dir(), "system.dsk");
        harness
            .cmd(&["rom", "add", rom.to_str().unwrap(), "--name", "plus"])
            .unwrap();
        harness
            .cmd(&["rom", "add", ii.to_str().unwrap(), "--name", "macii"])
            .unwrap();
        harness
            .cmd(&["system", "add", system.to_str().unwrap(), "--name", "sys"])
            .unwrap();
        harness
    }

    fn mock() -> (Self, Mock) {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let mock = Mock::default();
        let harness = Self::new(mock.clone(), paths, root);
        harness.create("plus-a");
        (harness, mock)
    }

    fn dir(&self) -> &Path {
        self.root.path()
    }

    fn cmd(&self, args: &[&str]) -> Result<u8> {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        self.app.execute(Cli::parse_from(argv))
    }

    fn create(&self, name: &str) {
        self.cmd(&[
            "create", name, "--model", "plus", "--rom", "plus", "--system", "sys",
        ])
        .unwrap();
    }

    fn system_disk(&self, machine: &str) -> PathBuf {
        self.paths.machine(machine).join("system.dsk")
    }
}

fn session_count(paths: &Paths) -> usize {
    macman::fsutil::list_dir(&paths.sessions()).unwrap().len()
}

#[test]
fn launches_rom_and_private_boot_disk() {
    let (h, mock) = Harness::mock();
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);

    let launches = mock.0.borrow().launches.clone();
    assert_eq!(launches.len(), 1);
    assert!(launches[0].rom.ends_with("roms/plus.rom"));
    assert_eq!(launches[0].disks.len(), 1);
    assert!(launches[0].disks[0].writable);
    assert_eq!(
        launches[0].disks[0].path,
        fs::canonicalize(h.system_disk("plus-a")).unwrap()
    );
    assert_eq!(session_count(&h.paths), 0, "clean exit leaves no session");
}

#[test]
fn run_defaults_to_current_machine() {
    let (h, mock) = Harness::mock();
    assert!(h.cmd(&["run"]).is_err());
    h.cmd(&["use", "plus-a"]).unwrap();
    h.cmd(&["run"]).unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

#[test]
fn readonly_disk_is_a_read_only_throwaway_copy() {
    let (h, mock) = Harness::mock();
    let app_disk = write_disk(h.dir(), "tico68.dsk");
    let before = sha256_file(&app_disk).unwrap();
    let copy_seen = Rc::new(RefCell::new(None));
    let seen = copy_seen.clone();
    mock.0.borrow_mut().during = Some(Box::new(move |launch: &Launch| {
        let disk = &launch.disks[1];
        assert!(!disk.writable);
        assert!(disk.path.starts_with(&launch.workdir));
        assert!(fs::metadata(&disk.path).unwrap().permissions().readonly());
        // Even a hostile emulator ignoring the mode only reaches the copy.
        let mut perms = fs::metadata(&disk.path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&disk.path, perms).unwrap();
        fs::write(&disk.path, b"guest scribble").unwrap();
        *seen.borrow_mut() = Some(disk.path.clone());
    }));

    h.cmd(&[
        "run",
        "plus-a",
        "--disk",
        app_disk.to_str().unwrap(),
        "--readonly",
    ])
    .unwrap();

    assert_eq!(sha256_file(&app_disk).unwrap(), before);
    assert!(
        !copy_seen.borrow().as_ref().unwrap().exists(),
        "copy is discarded"
    );
    assert!(
        lock_image(&h.paths, &app_disk, LockMode::Exclusive).is_ok(),
        "lock is released"
    );
    // The extra disk is not saved to the machine.
    assert_eq!(Machine::load(&h.paths, "plus-a").unwrap().disks.len(), 1);
}

#[test]
fn extra_disk_without_readonly_is_the_real_writable_file() {
    let (h, mock) = Harness::mock();
    let disk = write_disk(h.dir(), "data.dsk");
    h.cmd(&["run", "plus-a", "--disk", disk.to_str().unwrap()])
        .unwrap();
    let launches = mock.0.borrow().launches.clone();
    assert_eq!(launches[0].disks[1].path, fs::canonicalize(&disk).unwrap());
    assert!(launches[0].disks[1].writable);
}

#[test]
fn conflicting_writable_attachment_is_blocked() {
    let (h, mock) = Harness::mock();
    h.create("plus-b");
    let shared = write_disk(h.dir(), "shared.dsk");
    let alias = h.dir().join("alias.dsk");
    std::os::unix::fs::symlink(&shared, &alias).unwrap();

    // Another process's emulator holds the disk.
    let held = lock_image(&h.paths, &shared, LockMode::Exclusive).unwrap();
    for disk in [&shared, &alias] {
        let err = h
            .cmd(&["run", "plus-b", "--disk", disk.to_str().unwrap()])
            .unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        // Copying a disk mid-write would be a torn snapshot, so read-only waits too.
        let err = h
            .cmd(&[
                "run",
                "plus-b",
                "--disk",
                disk.to_str().unwrap(),
                "--readonly",
            ])
            .unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
    }
    drop(held);
    assert_eq!(mock.0.borrow().launches.len(), 0);

    // One run cannot attach the same file twice.
    let path = shared.to_str().unwrap();
    assert!(
        h.cmd(&["run", "plus-b", "--disk", path, "--disk", path])
            .is_err()
    );
    assert_eq!(mock.0.borrow().launches.len(), 0);
}

#[test]
fn running_machine_cannot_be_removed_or_cloned() {
    let (h, mock) = Harness::mock();
    let paths = h.paths.clone();
    mock.0.borrow_mut().during = Some(Box::new(move |_: &Launch| {
        let other = App::new(
            paths.clone(),
            Backends::new(vec![Box::new(Mock::default())]),
        );
        let cmd = |args: &[&str]| {
            let argv = std::iter::once("macman").chain(args.iter().copied());
            other.execute(Cli::parse_from(argv))
        };
        let err = cmd(&["remove", "plus-a", "--delete-disks"]).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        assert!(cmd(&["clone", "plus-a", "copy"]).is_err());
        assert!(!paths.machine("copy").exists());
        assert!(paths.machine("plus-a").join("system.dsk").exists());
        // No stale report for a session that is alive.
        assert_eq!(macman::disk::stale_sessions(&paths).unwrap().len(), 0);
    }));
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

/// The one abnormal session left behind, as reported by `doctor`.
fn assert_reported(h: &Harness, expected: &str) {
    let stale = macman::disk::stale_sessions(&h.paths).unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].record.describe().contains(expected));
    assert_eq!(stale[0].record.writable_disks.len(), 1);

    let checks = h.app.doctor_checks(false).unwrap();
    let report = checks
        .iter()
        .find(|check| check.message.contains("ended abnormally"))
        .expect("doctor reports the abnormal run");
    assert_eq!(report.level, CheckLevel::Warn);
    assert!(report.message.contains("plus-a"), "{}", report.message);
    assert!(report.message.contains(expected), "{}", report.message);
}

/// The disks survive, nothing stays locked, and the next run clears the notice.
fn assert_recovers(h: &Harness, mock: &Mock) {
    assert!(h.system_disk("plus-a").exists());
    let (exit, signal, fail) = {
        let mut state = mock.0.borrow_mut();
        let saved = (state.exit, state.signal, state.fail_launch);
        state.exit = 0;
        state.signal = 0;
        state.fail_launch = false;
        saved
    };
    assert!(exit != 0 || signal != 0 || fail);
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);
    assert_eq!(session_count(&h.paths), 0);
    assert!(h.system_disk("plus-a").exists());
}

#[test]
fn nonzero_exit_is_kept_for_doctor() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().exit = 3;
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 3);
    assert_reported(&h, "exited with code 3");
    assert_recovers(&h, &mock);
}

#[test]
fn launch_failure_is_kept_for_doctor() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().fail_launch = true;
    let error = h.cmd(&["run", "plus-a"]).unwrap_err();
    assert!(error.to_string().contains("cannot start"), "{error}");
    assert_reported(&h, "failed to start");
    assert_recovers(&h, &mock);
}

#[test]
fn host_crash_is_told_apart_from_being_killed() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().signal = 11; // SIGSEGV
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 139);
    assert_reported(&h, "crashed on the host (signal 11)");
    assert_recovers(&h, &mock);

    mock.0.borrow_mut().signal = 15; // SIGTERM
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 143);
    assert_reported(&h, "killed by signal 15");
    assert_recovers(&h, &mock);
}

#[test]
fn abnormal_run_keeps_throwaway_copies_until_cleared_but_never_touches_originals() {
    let (h, mock) = Harness::mock();
    let app_disk = write_disk(h.dir(), "tico68.dsk");
    let before = sha256_file(&app_disk).unwrap();
    mock.0.borrow_mut().exit = 1;
    h.cmd(&[
        "run",
        "plus-a",
        "--disk",
        app_disk.to_str().unwrap(),
        "--readonly",
    ])
    .unwrap();
    assert_eq!(sha256_file(&app_disk).unwrap(), before);
    // The lock must not outlive the run, or the disk stays stuck "in use".
    assert!(lock_image(&h.paths, &app_disk, LockMode::Exclusive).is_ok());
    assert_recovers(&h, &mock);
    assert_eq!(sha256_file(&app_disk).unwrap(), before);
}

#[test]
fn killed_macman_is_reported_and_keeps_disks() {
    let (h, mock) = Harness::mock();
    // What a killed macman leaves: a startup record and no lock holder.
    let session = macman::disk::Session::start(&h.paths, "plus-a").unwrap();
    session
        .record(&macman::disk::SessionRecord {
            machine: "plus-a".into(),
            pid: 1,
            outcome: None,
            writable_disks: vec![h.system_disk("plus-a")],
        })
        .unwrap();
    drop(session);
    assert_reported(&h, "macman stopped before the emulator did");

    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(session_count(&h.paths), 0);
    assert!(h.system_disk("plus-a").exists());
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

#[test]
fn extra_disks_may_be_blank_but_not_partitioned() {
    let (h, mock) = Harness::mock();
    let blank = h.dir().join("blank.dsk");
    fs::write(&blank, vec![0u8; 800 * 1024]).unwrap();
    h.cmd(&["run", "plus-a", "--disk", blank.to_str().unwrap()])
        .unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);

    let (partitioned, _) = write_partitioned_disk(h.dir(), "zip.image");
    let path = partitioned.to_str().unwrap();
    let error = h.cmd(&["run", "plus-a", "--disk", path]).unwrap_err();
    assert!(error.to_string().contains("system add"), "{error}");
    assert!(
        h.cmd(&["run", "plus-a", "--disk", path, "--any-format"])
            .is_err()
    );
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

#[test]
fn any_format_admits_unrecognized_disks() {
    let (h, mock) = Harness::mock();
    let odd = h.dir().join("odd.hda");
    fs::write(&odd, vec![7u8; 8192]).unwrap();
    let path = odd.to_str().unwrap();

    let error = h.cmd(&["run", "plus-a", "--disk", path]).unwrap_err();
    assert!(error.to_string().contains("--any-format"), "{error}");
    h.cmd(&[
        "run",
        "plus-a",
        "--disk",
        path,
        "--readonly",
        "--any-format",
    ])
    .unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

#[test]
fn disk_commands_wait_for_no_one_writing_the_source() {
    let (h, _mock) = Harness::mock();
    let (image, _) = write_partitioned_disk(h.dir(), "zip.image");
    let source = image.to_str().unwrap();
    let dest = h.dir().join("out.img");
    let dest_str = dest.to_str().unwrap();

    // Another macman has the image attached for writing.
    let writer = lock_image(&h.paths, &image, LockMode::Exclusive).unwrap();
    let err = h.cmd(&["disk", "info", source]).unwrap_err();
    assert!(err.to_string().contains("in use"), "{err}");
    let err = h.cmd(&["disk", "extract", source, dest_str]).unwrap_err();
    assert!(err.to_string().contains("in use"), "{err}");
    assert!(!dest.exists());
    drop(writer);

    // Other readers don't get in the way.
    let reader = lock_image(&h.paths, &image, LockMode::Shared).unwrap();
    h.cmd(&["disk", "info", source]).unwrap();
    h.cmd(&["disk", "extract", source, dest_str]).unwrap();
    drop(reader);

    // Nothing keeps the source locked afterwards.
    assert!(lock_image(&h.paths, &image, LockMode::Exclusive).is_ok());
}

#[test]
fn run_refuses_tampered_rom_and_missing_disk() {
    let (h, mock) = Harness::mock();
    fs::remove_file(h.system_disk("plus-a")).unwrap();
    assert!(
        h.cmd(&["run", "plus-a"])
            .unwrap_err()
            .to_string()
            .contains("missing its disk")
    );

    h.create("plus-b");
    fs::write(h.paths.roms().join("plus.rom"), b"tampered").unwrap();
    assert!(
        h.cmd(&["run", "plus-b"])
            .unwrap_err()
            .to_string()
            .contains("changed since")
    );
    assert_eq!(mock.0.borrow().launches.len(), 0);
}

#[test]
fn failed_build_leaves_no_session_and_no_launch() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().fail_prepare = true;
    assert!(h.cmd(&["run", "plus-a"]).is_err());
    assert_eq!(session_count(&h.paths), 0);
    assert_eq!(mock.0.borrow().launches.len(), 0);
}

#[test]
fn too_many_disks_are_refused_before_launch() {
    let (h, mock) = Harness::mock();
    let disks: Vec<_> = (0..6)
        .map(|i| {
            write_disk(h.dir(), &format!("d{i}.dsk"))
                .to_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut args = vec!["run", "plus-a"];
    for disk in &disks {
        args.extend(["--disk", disk]);
    }
    assert!(h.cmd(&args).is_err());
    assert_eq!(mock.0.borrow().launches.len(), 0);
}

/// Stand-ins for upstream's configuration tool and build. The tool records its
/// arguments and emits a script installing the Makefile; make installs a fake
/// emulator that records how it was called.
const FIXTURE_TOOL: &str = r#"
#include <stdio.h>
int main(int argc, char **argv) {
    int i;
    printf("cp fixture.mk Makefile\n");
    printf("echo 'options:");
    for (i = 1; i < argc; i++) printf(" %s", argv[i]);
    printf("' >> LOG\n");
    return 0;
}
"#;
const FIXTURE_MAKEFILE: &str = "minivmac:\n\techo build >> LOG\n\tcp fake-emulator minivmac\n";
const FIXTURE_EMULATOR: &str = "#!/bin/sh\necho \"run $*\" >> LOG\n";

fn has_build_tools() -> bool {
    ["gcc", "make", "tar", "bash"]
        .iter()
        .all(|tool| Command::new(tool).arg("--version").output().is_ok())
}

fn real_adapter() -> Option<(Harness, PathBuf)> {
    if !has_build_tools() {
        eprintln!("skipping: build tools not installed");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("emulator.log");
    let log_text = log.to_str().unwrap();
    let tree = root.path().join("fixture/minivmac");
    fs::create_dir_all(tree.join("setup")).unwrap();
    fs::write(
        tree.join("setup/tool.c"),
        FIXTURE_TOOL.replace("LOG", log_text),
    )
    .unwrap();
    fs::write(
        tree.join("fixture.mk"),
        FIXTURE_MAKEFILE.replace("LOG", log_text),
    )
    .unwrap();
    let emulator = tree.join("fake-emulator");
    fs::write(&emulator, FIXTURE_EMULATOR.replace("LOG", log_text)).unwrap();
    fs::set_permissions(&emulator, fs::Permissions::from_mode(0o755)).unwrap();
    let archive = root.path().join("fixture.tgz");
    let status = Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(root.path().join("fixture"))
        .arg("minivmac")
        .status()
        .unwrap();
    assert!(status.success());

    let pin = SourcePin {
        revision: "36.04".into(),
        url: "http://invalid.example/not-used".into(),
        sha256: sha256_file(&archive).unwrap(),
    };
    let paths = Paths::under(root.path());
    let backend = MiniVmac::new(paths.clone(), pin, Some(archive));
    Some((Harness::new(backend, paths, root), log))
}

#[test]
fn real_adapter_builds_once_per_configuration_and_launches() {
    let Some((h, log)) = real_adapter() else {
        return;
    };
    let read_log = || fs::read_to_string(&log).unwrap_or_default();
    let builds = || read_log().lines().filter(|l| *l == "build").count();

    // First run compiles with translated options.
    h.create("plus-a");
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);
    assert_eq!(builds(), 1);
    assert!(
        read_log().contains("options: -t lx64 -m Plus -mem 4M -hres 512 -vres 342 -depth 0 -fullscreen 0 -mf 2 -magnify 1"),
        "{}",
        read_log()
    );

    // The emulator got the ROM by flag and the private boot disk by path.
    let run = read_log()
        .lines()
        .rfind(|l| l.starts_with("run "))
        .unwrap()
        .to_string();
    let rom = h.paths.roms().join("plus.rom");
    let boot = fs::canonicalize(h.system_disk("plus-a")).unwrap();
    assert!(run.contains(&format!("-r {}", rom.display())), "{run}");
    assert!(run.ends_with(boot.to_str().unwrap()), "{run}");
    assert!(run.contains("-d "), "{run}");

    // Same hardware shares the build, with separate disks.
    h.create("plus-b");
    h.cmd(&["run", "plus-a"]).unwrap();
    h.cmd(&["run", "plus-b"]).unwrap();
    assert_eq!(builds(), 1);
    let last = read_log()
        .lines()
        .rfind(|l| l.starts_with("run "))
        .unwrap()
        .to_string();
    assert!(last.contains("plus-b/system.dsk"), "{last}");
    assert_eq!(fs::read_dir(h.paths.builds()).unwrap().count(), 1);

    // Different hardware gets its own build.
    h.cmd(&[
        "create",
        "ii",
        "--model",
        "ii",
        "--rom",
        "macii",
        "--system",
        "sys",
        "--display",
        "640x480x8",
    ])
    .unwrap();
    h.cmd(&["run", "ii"]).unwrap();
    assert_eq!(builds(), 2);
    assert_eq!(fs::read_dir(h.paths.builds()).unwrap().count(), 2);

    // A damaged artifact fails validation and is rebuilt.
    let build = fs::read_dir(h.paths.builds())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let plus_is_first = fs::read_to_string(build.join("manifest.toml"))
        .unwrap()
        .contains("\"Plus\"");
    let machine = if plus_is_first { "plus-a" } else { "ii" };
    fs::write(build.join("minivmac"), b"#!/bin/sh\nexit 9\n").unwrap();
    assert_eq!(h.cmd(&["run", machine]).unwrap(), 0);
    assert_eq!(builds(), 3);

    assert_eq!(h.cmd(&["list", "--builds"]).unwrap(), 0);
}
