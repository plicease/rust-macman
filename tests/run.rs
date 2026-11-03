//! `macman run`, against a mock backend and against the real Mini vMac adapter
//! fed a small fixture source tree, which needs only gcc, make, bash, and tar.
//! That fixture is Unix only; see `tests/windows_adapter.rs` for its counterpart.

mod common;

use anyhow::{Result, bail};
use clap::Parser;
use common::{II_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_partitioned_disk, write_rom};
use macman::app::App;
#[cfg(unix)]
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
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::process::ExitStatus;
use std::rc::Rc;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
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

#[cfg(unix)]
fn raw_status(exit: i32, signal: i32) -> ExitStatus {
    ExitStatus::from_raw(exit << 8 | signal)
}

/// `signal` holds the NTSTATUS that ends a process abnormally on Windows.
#[cfg(windows)]
fn raw_status(exit: i32, signal: i32) -> ExitStatus {
    ExitStatus::from_raw(if signal != 0 { signal } else { exit } as u32)
}

/// A second name for `target`. Windows users can rarely create symlinks.
fn alias_file(target: &Path, alias: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, alias).unwrap();
    #[cfg(not(unix))]
    fs::hard_link(target, alias).unwrap();
}

impl EmulatorBackend for Mock {
    fn name(&self) -> &str {
        "minivmac"
    }
    fn revision(&self) -> &str {
        "36.04"
    }

    fn resolve_revision(&self, _recorded: &str) -> Result<String> {
        Ok(self.revision().to_string())
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
        Ok(raw_status(state.exit, state.signal))
    }
    fn doctor(&self) -> Vec<Check> {
        vec![]
    }
}

/// A child holds copies of its parent's descriptors between fork and exec, and
/// with them any file lock, which can make a finished session look alive. So
/// tests that spawn processes run alone, apart from those that check locks.
static SPAWNING: RwLock<()> = RwLock::new(());

#[allow(dead_code)] // Held for its lifetime, never read.
enum Turn {
    NoSpawning(RwLockReadGuard<'static, ()>),
    Alone(RwLockWriteGuard<'static, ()>),
}

fn turn_without_spawning() -> Turn {
    Turn::NoSpawning(SPAWNING.read().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(unix)]
fn turn_alone() -> Turn {
    Turn::Alone(SPAWNING.write().unwrap_or_else(PoisonError::into_inner))
}

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
    _turn: Turn,
}

impl Harness {
    fn new(
        backend: impl EmulatorBackend + 'static,
        paths: Paths,
        root: TempDir,
        turn: Turn,
    ) -> Self {
        let app = App::new(paths.clone(), Backends::new(vec![Box::new(backend)]));
        let harness = Self {
            root,
            paths,
            app,
            _turn: turn,
        };
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
        let turn = turn_without_spawning();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let mock = Mock::default();
        let harness = Self::new(mock.clone(), paths, root, turn);
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
fn a_machine_runs_after_its_system_image_is_removed() {
    let (h, mock) = Harness::mock();
    h.cmd(&["system", "remove", "sys", "--yes"]).unwrap();
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);
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
fn floppy_media_go_as_read_only_copies_in_order_and_a_cd_is_refused() {
    let (h, mock) = Harness::mock();
    for name in ["install-1", "install-2"] {
        let file = write_disk(h.dir(), &format!("{name}.image"));
        h.cmd(&["media", "add", file.to_str().unwrap(), "--name", name])
            .unwrap();
    }
    let stored = h.paths.media().join("install-1.img");
    let before = sha256_file(&stored).unwrap();
    mock.0.borrow_mut().during = Some(Box::new(|launch: &Launch| {
        // Boot disk, then the media in the order given.
        assert_eq!(launch.disks.len(), 3);
        for disk in &launch.disks[1..] {
            assert!(!disk.writable);
            assert!(disk.path.starts_with(&launch.workdir));
        }
        assert!(launch.disks[1].path.ends_with("1-install-2.img"));
        assert!(launch.disks[2].path.ends_with("2-install-1.img"));
    }));
    h.cmd(&[
        "run",
        "plus-a",
        "--media",
        "install-2",
        "--media",
        "install-1",
    ])
    .unwrap();
    assert_eq!(mock.0.borrow().launches.len(), 1);
    assert_eq!(sha256_file(&stored).unwrap(), before);
    assert_eq!(Machine::load(&h.paths, "plus-a").unwrap().disks.len(), 1);

    // Mini vMac has no CD drive, which `create` says before making anything.
    let (cd, _) = write_partitioned_disk(h.dir(), "cd.toast");
    h.cmd(&["media", "add", cd.to_str().unwrap(), "--name", "cd"])
        .unwrap();
    let error = h
        .cmd(&["run", "plus-a", "--media", "cd"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("isn't available"), "{error}");
    let error = h
        .cmd(&[
            "create", "b", "--model", "plus", "--rom", "plus", "--system", "sys", "--media", "cd",
        ])
        .unwrap_err()
        .to_string();
    assert!(error.contains("is a CD"), "{error}");
    assert!(!h.paths.machine("b").exists());
    assert_eq!(mock.0.borrow().launches.len(), 1);
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
    alias_file(&shared, &alias);

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

#[cfg(windows)]
#[test]
fn host_crash_is_told_apart_from_being_interrupted() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().signal = 0xC000_0005_u32 as i32; // STATUS_ACCESS_VIOLATION
    assert_eq!(
        h.cmd(&["run", "plus-a"]).unwrap(),
        1,
        "a crash is a failure"
    );
    assert_reported(&h, "crashed on the host (status 0xC0000005)");
    assert_recovers(&h, &mock);

    mock.0.borrow_mut().signal = 0xC000_013A_u32 as i32; // STATUS_CONTROL_C_EXIT
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 1);
    assert_reported(&h, "interrupted (0xC000013A)");
    assert_recovers(&h, &mock);
}

#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(all(unix, not(target_os = "macos")))]
const FIXTURE_MAKEFILE: &str = "minivmac:\n\techo build >> LOG\n\tcp fake-emulator minivmac\n";
#[cfg(all(unix, not(target_os = "macos")))]
const FIXTURE_EMULATOR: &str = "#!/bin/sh\necho \"run $*\" >> LOG\n";

/// Names the SDK as upstream's does, and installs a bundle, not a binary.
#[cfg(target_os = "macos")]
const FIXTURE_MAKEFILE: &str = "\
mk_COptionsCommon = -c -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX11.1.sdk

default:
\techo build >> LOG
\techo sdk $(mk_COptionsCommon) >> LOG
\tmkdir -p minivmac.app/Contents/MacOS minivmac.app/Contents/Resources
\tcp fake-emulator minivmac.app/Contents/MacOS/minivmac
\techo plist > minivmac.app/Contents/Info.plist
\techo icon > minivmac.app/Contents/Resources/Icon.icns
";

/// Records what the Cocoa glue would find: it ignores its arguments, and looks
/// beside its bundle for the ROM and `disk1.dsk` onwards, up to the first gap.
#[cfg(target_os = "macos")]
const FIXTURE_EMULATOR: &str = r#"#!/bin/sh
echo "run $*" >> LOG
echo "exe $0" >> LOG
echo "cwd $(pwd -P)" >> LOG
echo "plist $(cat "$(dirname "$0")/../Info.plist")" >> LOG
for rom in *.ROM; do echo "rom $rom $(wc -c < "$rom" | tr -d ' ')" >> LOG; done
i=1
while [ -e "disk$i.dsk" ]; do
    echo "disk$i $(readlink "disk$i.dsk")" >> LOG
    if [ -w "disk$i.dsk" ]; then
        printf written | dd of="disk$i.dsk" bs=1 seek=4096 conv=notrunc 2>/dev/null
    else
        echo "readonly$i" >> LOG
    fi
    i=$((i + 1))
done
"#;

#[cfg(unix)]
fn has_build_tools() -> bool {
    ["gcc", "make", "tar", "bash"]
        .iter()
        .all(|tool| Command::new(tool).arg("--version").output().is_ok())
}

#[cfg(unix)]
fn real_adapter() -> Option<(Harness, PathBuf)> {
    let turn = turn_alone();
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
        url: "http://invalid.example/not-used".into(),
        sha256: sha256_file(&archive).unwrap(),
        ..SourcePin::default()
    };
    let paths = Paths::under(root.path());
    let backend = MiniVmac::new(paths.clone(), pin, Some(archive));
    Some((Harness::new(backend, paths, root, turn), log))
}

#[test]
#[cfg(unix)]
fn an_imported_machine_builds_on_its_first_run_and_reuses_that_build() {
    let Some((h, log)) = real_adapter() else {
        return;
    };
    let builds = || {
        let text = fs::read_to_string(&log).unwrap_or_default();
        text.lines().filter(|line| *line == "build").count()
    };
    h.create("plus-a");
    let archive = h.dir().join("plus.tar.gz");
    let archive = archive.to_str().unwrap();
    h.cmd(&["export", "plus-a", "-o", archive]).unwrap();

    assert_eq!(h.cmd(&["import", archive, "--rename", "copy"]).unwrap(), 0);
    assert_eq!(builds(), 0);
    assert_eq!(h.cmd(&["run", "copy"]).unwrap(), 0);
    assert_eq!(builds(), 1);
    assert_eq!(h.cmd(&["run", "copy"]).unwrap(), 0);
    assert_eq!(builds(), 1);
}

#[test]
#[cfg(unix)]
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
    let target = if cfg!(target_os = "macos") {
        "-e xcd -cl -t mcar -ndp 0"
    } else {
        "-t lx64"
    };
    let options = format!(
        "options: {target} -m Plus -mem 4M -hres 512 -vres 342 -depth 0 -fullscreen 0 -mf 2 \
         -magnify 1"
    );
    assert!(read_log().contains(&options), "{}", read_log());
    assert_launched_with_rom_and_boot_disk(&h, "plus-a", &read_log());

    // Same hardware shares the build, with separate disks.
    h.create("plus-b");
    h.cmd(&["run", "plus-a"]).unwrap();
    h.cmd(&["run", "plus-b"]).unwrap();
    assert_eq!(builds(), 1);
    assert_launched_with_rom_and_boot_disk(&h, "plus-b", &read_log());
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
    let executable = if cfg!(target_os = "macos") {
        "minivmac.app/Contents/MacOS/minivmac"
    } else {
        "minivmac"
    };
    fs::write(build.join(executable), b"#!/bin/sh\nexit 9\n").unwrap();
    assert_eq!(h.cmd(&["run", machine]).unwrap(), 0);
    assert_eq!(builds(), 3);

    assert_eq!(h.cmd(&["list", "--builds"]).unwrap(), 0);
}

/// A machine from a host on the other revision uses this host's revision, and
/// shares its build with machines made here.
#[test]
#[cfg(unix)]
fn the_build_depends_on_this_host_not_the_revision_a_machine_records() {
    let Some((h, log)) = real_adapter() else {
        return;
    };
    let builds = || {
        let text = fs::read_to_string(&log).unwrap_or_default();
        text.lines().filter(|l| *l == "build").count()
    };
    let host = SourcePin::default().revision;
    let other = if host == "36.04" { "37.03" } else { "36.04" };
    let set_revision = |machine: &str, from: &str, to: &str| {
        let file = h.paths.machine(machine).join("machine.toml");
        let text = fs::read_to_string(&file).unwrap();
        let (from, to) = (
            format!("revision = \"{from}\""),
            format!("revision = \"{to}\""),
        );
        assert!(text.contains(&from), "{text}");
        fs::write(&file, text.replace(&from, &to)).unwrap();
    };

    h.create("native");
    h.create("visitor");
    set_revision("visitor", &host, other);
    assert_eq!(h.cmd(&["run", "visitor"]).unwrap(), 0);
    assert_eq!(h.cmd(&["run", "native"]).unwrap(), 0);
    assert_eq!(builds(), 1, "both use one build");

    let builds_dir = fs::read_dir(h.paths.builds()).unwrap();
    let entries: Vec<_> = builds_dir.collect();
    assert_eq!(entries.len(), 1);
    let build = entries[0].as_ref().unwrap().path();
    let manifest = fs::read_to_string(build.join("manifest.toml")).unwrap();
    assert!(
        manifest.contains(&format!("revision = \"{host}\"")),
        "{manifest}"
    );

    // What the machine records is left as it was.
    let text = fs::read_to_string(h.paths.machine("visitor").join("machine.toml")).unwrap();
    assert!(text.contains(&format!("revision = \"{other}\"")), "{text}");

    set_revision("visitor", other, "99.0");
    let error = h.cmd(&["run", "visitor"]).unwrap_err();
    assert!(
        format!("{error:#}").contains("pinned to Mini vMac 99.0"),
        "{error:#}"
    );
    assert_eq!(builds(), 1);
}

/// The emulator got the ROM by flag and the private boot disk by path.
#[cfg(all(unix, not(target_os = "macos")))]
fn assert_launched_with_rom_and_boot_disk(h: &Harness, machine: &str, log: &str) {
    let run = log.lines().rfind(|l| l.starts_with("run ")).unwrap();
    let rom = h.paths.roms().join("plus.rom");
    let boot = fs::canonicalize(h.system_disk(machine)).unwrap();
    assert!(run.contains(&format!("-r {}", rom.display())), "{run}");
    assert!(run.ends_with(boot.to_str().unwrap()), "{run}");
    assert!(run.contains("-d "), "{run}");
}

/// The last run's lines of each kind.
#[cfg(target_os = "macos")]
fn last_line<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines().rfind(|l| l.starts_with(prefix)).unwrap()
}

/// No arguments, a private bundle, and the ROM and boot disk beside it.
#[cfg(target_os = "macos")]
fn assert_launched_with_rom_and_boot_disk(h: &Harness, machine: &str, log: &str) {
    assert_eq!(last_line(log, "run"), "run ", "the glue ignores arguments");
    assert_eq!(last_line(log, "plist "), "plist plist");
    assert_eq!(
        last_line(log, "rom "),
        format!("rom vMac.ROM {}", 128 * 1024)
    );

    let sessions = fs::canonicalize(h.paths.sessions()).unwrap();
    let cwd = last_line(log, "cwd ").strip_prefix("cwd ").unwrap();
    assert!(Path::new(cwd).starts_with(&sessions), "{cwd}");
    let exe = last_line(log, "exe ").strip_prefix("exe ").unwrap();
    assert!(
        exe.ends_with("/minivmac.app/Contents/MacOS/minivmac"),
        "{exe}"
    );
    assert!(!exe.contains("/builds/"), "runs a private copy: {exe}");

    let boot = fs::canonicalize(h.system_disk(machine)).unwrap();
    assert_eq!(
        last_line(log, "disk1 "),
        format!("disk1 {}", boot.display())
    );
    assert_eq!(fs::read_dir(h.paths.sessions()).unwrap().count(), 0);
}

#[test]
#[cfg(target_os = "macos")]
fn macos_bundle_build_cache_and_disks() {
    let Some((h, log)) = real_adapter() else {
        return;
    };
    let read_log = || fs::read_to_string(&log).unwrap_or_default();
    let builds = || read_log().lines().filter(|l| *l == "build").count();
    h.create("plus-a");
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);

    // The Makefile was pointed at the SDK that's really there.
    let sdk = Command::new("xcrun")
        .arg("--show-sdk-path")
        .output()
        .unwrap();
    let sdk = String::from_utf8(sdk.stdout).unwrap();
    assert_eq!(
        last_line(&read_log(), "sdk "),
        format!("sdk -c -isysroot {}", sdk.trim())
    );

    // A write through the link reaches the machine's own disk, and only that.
    let boot = fs::read(h.system_disk("plus-a")).unwrap();
    assert_eq!(&boot[4096..4103], b"written");
    assert_eq!(boot.len(), 800 * 1024);

    // The cached build keeps its whole bundle, and records what it's for.
    let build = fs::read_dir(h.paths.builds())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let manifest = fs::read_to_string(build.join("manifest.toml")).unwrap();
    for expected in [
        "host = \"aarch64-apple-darwin\"",
        "executable = \"minivmac.app/Contents/MacOS/minivmac\"",
        "bundle = \"minivmac.app\"",
        "bundle_sha256 = ",
        "toolchain = \"arm64-apple-darwin",
        "\"mcar\"",
    ] {
        assert!(manifest.contains(expected), "{expected}: {manifest}");
    }
    assert!(build.join("minivmac.app/Contents/Info.plist").is_file());
    assert!(!build.join("disk1.dsk").exists());

    // Disks follow the boot disk in order, and read-only ones can't be written.
    let extra = write_disk(h.dir(), "extra disk.dsk");
    let original = fs::read(&extra).unwrap();
    let path = extra.to_str().unwrap();
    assert_eq!(
        h.cmd(&["run", "plus-a", "--disk", path, "--readonly"])
            .unwrap(),
        0
    );
    assert_eq!(builds(), 1, "the cached bundle is reused");
    let text = read_log();
    let disk2 = last_line(&text, "disk2 ");
    assert!(disk2.ends_with("/1-extra disk.dsk"), "{disk2}");
    assert_eq!(last_line(&text, "readonly"), "readonly2");
    assert_eq!(fs::read(&extra).unwrap(), original);

    // Damage to any part of the bundle, not just the binary, forces a rebuild.
    fs::write(build.join("minivmac.app/Contents/Info.plist"), b"damaged").unwrap();
    let checks = h.app.doctor_checks(false).unwrap();
    assert!(
        checks
            .iter()
            .any(|check| check.level == CheckLevel::Warn && check.message.contains("is damaged")),
        "{checks:?}"
    );
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);
    assert_eq!(builds(), 2);
    assert_eq!(
        fs::read(build.join("minivmac.app/Contents/Info.plist")).unwrap(),
        b"plist\n"
    );

    // An emulator's own files never reach the cache: it ran from a copy.
    let digest_of_cache = sha256_file(&build.join("minivmac.app/Contents/MacOS/minivmac")).unwrap();
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 0);
    assert_eq!(
        sha256_file(&build.join("minivmac.app/Contents/MacOS/minivmac")).unwrap(),
        digest_of_cache
    );
    assert_eq!(builds(), 2);
}

#[test]
#[cfg(target_os = "macos")]
fn macos_sigbus_counts_as_a_host_crash() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().signal = 10; // SIGBUS, where Linux has SIGUSR1
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 138);
    assert_reported(&h, "crashed on the host (signal 10)");
    assert_recovers(&h, &mock);
}

#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_sigbus_counts_as_a_host_crash() {
    let (h, mock) = Harness::mock();
    mock.0.borrow_mut().signal = 7; // SIGBUS, where macOS has SIGEMT
    assert_eq!(h.cmd(&["run", "plus-a"]).unwrap(), 135);
    assert_reported(&h, "crashed on the host (signal 7)");
    assert_recovers(&h, &mock);
}

// Ephemeral runs: a temporary, writable copy of the boot disk.

impl Harness {
    fn set_ephemeral(&self, machine: &str, ephemeral: bool) {
        let mut definition = Machine::load(&self.paths, machine).unwrap();
        definition.ephemeral = ephemeral;
        definition.save_to(&self.paths.machine(machine)).unwrap();
    }

    /// Where the last launch's boot disk was.
    fn last_boot_disk(mock: &Mock) -> ResolvedDisk {
        mock.0.borrow().launches.last().unwrap().disks[0].clone()
    }
}

/// While the emulator runs: the boot disk is a writable copy in the session,
/// it holds what the saved disk does, and what the guest writes stays there.
fn scribble_on_the_copy(original: PathBuf) -> DuringLaunch {
    Box::new(move |launch: &Launch| {
        let disk = &launch.disks[0];
        assert!(disk.writable);
        assert!(
            disk.path.starts_with(&launch.workdir),
            "{}",
            disk.path.display()
        );
        assert!(!fs::metadata(&disk.path).unwrap().permissions().readonly());
        assert_eq!(fs::read(&disk.path).unwrap(), fs::read(&original).unwrap());
        let mut bytes = fs::read(&disk.path).unwrap();
        bytes[4096] = 0xEE;
        fs::write(&disk.path, &bytes).unwrap();
        assert_eq!(
            fs::read(&disk.path).unwrap()[4096],
            0xEE,
            "seen within the run"
        );
    })
}

#[test]
fn an_ephemeral_run_writes_to_a_copy_and_leaves_the_boot_disk_alone() {
    let (h, mock) = Harness::mock();
    let original = h.system_disk("plus-a");
    let before = sha256_file(&original).unwrap();
    for flag in ["--ephemeral", "-e"] {
        mock.0.borrow_mut().during = Some(scribble_on_the_copy(original.clone()));
        assert_eq!(h.cmd(&["run", "plus-a", flag]).unwrap(), 0);
        // Each run starts again from the saved disk, which never changed.
        assert_eq!(sha256_file(&original).unwrap(), before, "{flag}");
        let copy = Harness::last_boot_disk(&mock).path;
        assert!(!copy.exists(), "the copy goes with the run");
        assert_eq!(session_count(&h.paths), 0);
    }
    assert!(lock_image(&h.paths, &original, LockMode::Exclusive).is_ok());
    assert!(!Machine::load(&h.paths, "plus-a").unwrap().ephemeral);
}

#[test]
fn a_machine_set_to_be_ephemeral_is_unless_told_otherwise() {
    let (h, mock) = Harness::mock();
    let original = fs::canonicalize(h.system_disk("plus-a")).unwrap();
    h.set_ephemeral("plus-a", true);
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_ne!(Harness::last_boot_disk(&mock).path, original);
    h.cmd(&["run", "plus-a", "--no-ephemeral"]).unwrap();
    assert_eq!(Harness::last_boot_disk(&mock).path, original);

    h.set_ephemeral("plus-a", false);
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(Harness::last_boot_disk(&mock).path, original);

    // Definitions from before the setting existed are persistent.
    let file = h.paths.machine("plus-a").join("machine.toml");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("ephemeral = false\n"), "{text}");
    fs::write(&file, text.replace("ephemeral = false\n", "")).unwrap();
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(Harness::last_boot_disk(&mock).path, original);

    let both = Cli::try_parse_from(["macman", "run", "plus-a", "-e", "--no-ephemeral"]);
    assert!(both.is_err());

    h.cmd(&[
        "create",
        "plus-e",
        "--model",
        "plus",
        "--rom",
        "plus",
        "--system",
        "sys",
        "--ephemeral",
    ])
    .unwrap();
    assert!(Machine::load(&h.paths, "plus-e").unwrap().ephemeral);
}

#[test]
fn only_the_boot_disk_is_ephemeral() {
    let (h, mock) = Harness::mock();
    let data = write_disk(h.dir(), "data.dsk");
    let reference = write_disk(h.dir(), "reference.dsk");
    let reference_before = sha256_file(&reference).unwrap();
    let data_path = fs::canonicalize(&data).unwrap();
    mock.0.borrow_mut().during = Some(Box::new(move |launch: &Launch| {
        assert!(launch.disks[0].path.starts_with(&launch.workdir));
        assert_eq!(
            launch.disks[1].path, data_path,
            "an extra disk is the real file"
        );
        assert!(launch.disks[1].writable);
        let mut bytes = fs::read(&launch.disks[1].path).unwrap();
        bytes[4096] = 0x42;
        fs::write(&launch.disks[1].path, bytes).unwrap();
    }));
    h.cmd(&["run", "plus-a", "-e", "--disk", data.to_str().unwrap()])
        .unwrap();
    assert_eq!(fs::read(&data).unwrap()[4096], 0x42, "kept, as always");

    // --readonly still applies to --disk files alone.
    h.cmd(&[
        "run",
        "plus-a",
        "-e",
        "--disk",
        reference.to_str().unwrap(),
        "--readonly",
    ])
    .unwrap();
    let disks = mock.0.borrow().launches.last().unwrap().disks.clone();
    assert!(disks[0].writable, "the boot disk's copy is writable");
    assert!(!disks[1].writable);
    assert_eq!(sha256_file(&reference).unwrap(), reference_before);
}

#[test]
fn a_read_only_boot_disk_becomes_a_writable_copy() {
    let (h, mock) = Harness::mock();
    let mut definition = Machine::load(&h.paths, "plus-a").unwrap();
    definition.disks[0].readonly = true;
    definition.save_to(&h.paths.machine("plus-a")).unwrap();
    h.cmd(&["run", "plus-a", "-e"]).unwrap();
    let disk = Harness::last_boot_disk(&mock);
    assert!(disk.writable);
    h.cmd(&["run", "plus-a"]).unwrap();
    assert!(!Harness::last_boot_disk(&mock).writable);
}

#[test]
fn a_machine_without_a_boot_disk_cannot_be_ephemeral() {
    let (h, mock) = Harness::mock();
    let mut definition = Machine::load(&h.paths, "plus-a").unwrap();
    definition.disks[0].boot = false;
    definition.save_to(&h.paths.machine("plus-a")).unwrap();
    let error = h.cmd(&["run", "plus-a", "-e"]).unwrap_err().to_string();
    assert!(error.contains("no boot disk"), "{error}");
    assert_eq!(mock.0.borrow().launches.len(), 0);
    assert_eq!(session_count(&h.paths), 0);
}

#[test]
fn a_failed_or_abnormal_ephemeral_run_discards_its_copy_at_once() {
    let (h, mock) = Harness::mock();
    let original = h.system_disk("plus-a");
    let before = sha256_file(&original).unwrap();

    mock.0.borrow_mut().fail_launch = true;
    assert!(h.cmd(&["run", "plus-a", "-e"]).is_err());
    let stale = macman::disk::stale_sessions(&h.paths).unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].record.describe().contains("failed to start"));
    assert!(
        stale[0].record.writable_disks.is_empty(),
        "nothing kept to check"
    );
    let left: Vec<_> = macman::fsutil::list_dir(&stale[0].dir).unwrap();
    assert!(
        !left.iter().any(|name| name.contains("ephemeral")),
        "{left:?}"
    );
    mock.0.borrow_mut().fail_launch = false;

    mock.0.borrow_mut().exit = 3;
    mock.0.borrow_mut().during = Some(scribble_on_the_copy(original.clone()));
    assert_eq!(h.cmd(&["run", "plus-a", "-e"]).unwrap(), 3);
    assert!(!Harness::last_boot_disk(&mock).path.exists());
    assert_eq!(sha256_file(&original).unwrap(), before);

    // The next run clears both sessions.
    mock.0.borrow_mut().exit = 0;
    h.cmd(&["run", "plus-a"]).unwrap();
    assert_eq!(session_count(&h.paths), 0);
    assert_eq!(sha256_file(&original).unwrap(), before);
}

#[test]
fn a_copy_left_by_a_killed_macman_is_reclaimed_by_the_next_run() {
    let (h, mock) = Harness::mock();
    let session = macman::disk::Session::start(&h.paths, "plus-a").unwrap();
    let copy = session.ephemeral_copy(&h.system_disk("plus-a"), 0).unwrap();
    session
        .record(&macman::disk::SessionRecord {
            machine: "plus-a".into(),
            pid: 1,
            outcome: None,
            writable_disks: vec![],
        })
        .unwrap();
    drop(session);
    assert!(copy.exists());

    h.cmd(&["run", "plus-a"]).unwrap();
    assert!(!copy.exists());
    assert_eq!(session_count(&h.paths), 0);
    assert_eq!(mock.0.borrow().launches.len(), 1);
}

#[test]
fn ephemeral_runs_share_the_boot_disk_but_a_persistent_run_waits() {
    let (h, mock) = Harness::mock();
    let paths = h.paths.clone();
    let nested = Mock::default();
    let inner = nested.clone();
    mock.0.borrow_mut().during = Some(Box::new(move |_: &Launch| {
        let other = App::new(paths.clone(), Backends::new(vec![Box::new(inner.clone())]));
        let cmd = |args: &[&str]| {
            let argv = std::iter::once("macman").chain(args.iter().copied());
            other.execute(Cli::parse_from(argv))
        };
        // Its own copy, from the same saved disk.
        assert_eq!(cmd(&["run", "plus-a", "-e"]).unwrap(), 0);
        let error = cmd(&["run", "plus-a"]).unwrap_err();
        assert!(error.to_string().contains("in use"), "{error}");
        // Nor can the disk be rewritten under it.
        let error = cmd(&["remove", "plus-a", "--delete-disks"]).unwrap_err();
        assert!(error.to_string().contains("in use"), "{error}");
        assert_eq!(macman::disk::stale_sessions(&paths).unwrap().len(), 0);
    }));
    h.cmd(&["run", "plus-a", "-e"]).unwrap();
    assert_eq!(nested.0.borrow().launches.len(), 1);
    assert_ne!(
        nested.0.borrow().launches[0].disks[0].path,
        Harness::last_boot_disk(&mock).path
    );
}

/// What a second macman, as another process would be, does with a machine.
fn other_macman(paths: &Paths, args: &[&str]) -> Result<u8> {
    let app = App::new(
        paths.clone(),
        Backends::new(vec![Box::new(Mock::default())]),
    );
    let argv = std::iter::once("macman").chain(args.iter().copied());
    app.execute(Cli::parse_from(argv))
}

#[test]
fn a_checkpoint_restores_the_boot_disk_exactly_and_again() {
    let (h, _mock) = Harness::mock();
    let disk = h.system_disk("plus-a");
    let original = fs::read(&disk).unwrap();
    // An extra disk, which checkpoints neither take nor touch.
    let extra = h.paths.machine("plus-a").join("data.dsk");
    fs::write(&extra, b"data").unwrap();
    let definition = h.paths.machine("plus-a").join("machine.toml");
    let mut text = fs::read_to_string(&definition).unwrap();
    text.push_str("\n[[disks]]\npath = \"data.dsk\"\n");
    fs::write(&definition, text).unwrap();
    let rom = sha256_file(&h.paths.roms().join("plus.rom")).unwrap();
    let template = sha256_file(&h.paths.systems().join("sys.dsk")).unwrap();

    h.cmd(&["checkpoint", "create", "plus-a", "--name", "clean"])
        .unwrap();
    let image = h.paths.machine("plus-a").join("checkpoints/clean.dsk");
    assert_eq!(fs::read(&image).unwrap(), original);
    assert!(fs::metadata(&image).unwrap().permissions().readonly());
    let error = h
        .cmd(&["checkpoint", "create", "plus-a", "--name", "clean"])
        .unwrap_err();
    assert!(
        error.to_string().contains("already has a checkpoint"),
        "{error}"
    );

    for round in 0..2 {
        let mut changed = original.clone();
        changed[4096] = 0xEE;
        changed[5000 + round] = 0x11;
        fs::write(&disk, &changed).unwrap();
        fs::write(&extra, b"changed").unwrap();
        h.cmd(&["rollback", "plus-a", "--checkpoint", "clean", "--yes"])
            .unwrap();
        assert_eq!(fs::read(&disk).unwrap(), original, "round {round}");
        assert_eq!(
            fs::read(&image).unwrap(),
            original,
            "the checkpoint is kept"
        );
        assert_eq!(
            fs::read(&extra).unwrap(),
            b"changed",
            "other disks are left"
        );
        assert!(!fs::metadata(&disk).unwrap().permissions().readonly());
        fs::write(&extra, b"data").unwrap();
    }

    // Rolling back can keep the state it replaces, so it can be undone.
    let mut changed = original.clone();
    changed[4096] = 0x42;
    fs::write(&disk, &changed).unwrap();
    h.cmd(&[
        "rollback",
        "plus-a",
        "--checkpoint",
        "clean",
        "--save",
        "before-rollback",
        "--yes",
    ])
    .unwrap();
    assert_eq!(fs::read(&disk).unwrap(), original);
    h.cmd(&[
        "rollback",
        "plus-a",
        "--checkpoint",
        "before-rollback",
        "--yes",
    ])
    .unwrap();
    assert_eq!(fs::read(&disk).unwrap(), changed);

    let error = h
        .cmd(&["rollback", "plus-a", "--checkpoint", "nope", "--yes"])
        .unwrap_err();
    assert!(error.to_string().contains("no checkpoint named"), "{error}");
    assert!(
        h.cmd(&["rollback", "plus-a", "--checkpoint", "clean"])
            .is_err(),
        "needs --yes"
    );
    assert_eq!(fs::read(&disk).unwrap(), changed);
    assert_eq!(sha256_file(&h.paths.roms().join("plus.rom")).unwrap(), rom);
    assert_eq!(
        sha256_file(&h.paths.systems().join("sys.dsk")).unwrap(),
        template
    );

    h.cmd(&["checkpoint", "delete", "plus-a", "--name", "clean", "--yes"])
        .unwrap();
    assert!(!image.exists());
    assert!(
        h.cmd(&["checkpoint", "delete", "plus-a", "--name", "clean", "--yes"])
            .is_err()
    );
}

#[test]
fn nothing_is_checkpointed_or_rolled_back_while_the_machine_runs() {
    let (h, mock) = Harness::mock();
    h.cmd(&["checkpoint", "create", "plus-a", "--name", "clean"])
        .unwrap();
    let disk = h.system_disk("plus-a");
    let paths = h.paths.clone();
    let tried = Rc::new(RefCell::new(Vec::new()));
    let seen = tried.clone();
    mock.0.borrow_mut().during = Some(Box::new(move |_: &Launch| {
        for args in [
            &["checkpoint", "create", "plus-a", "--name", "live"][..],
            &["rollback", "plus-a", "--checkpoint", "clean", "--yes"],
            &["checkpoint", "delete", "plus-a", "--name", "clean", "--yes"],
        ] {
            let error = other_macman(&paths, args).unwrap_err();
            seen.borrow_mut().push(format!("{error:#}"));
        }
    }));
    let before = fs::read(&disk).unwrap();
    h.cmd(&["run", "plus-a"]).unwrap();
    let tried = tried.borrow();
    assert_eq!(tried.len(), 3);
    for error in tried.iter() {
        assert!(error.contains("is running"), "{error}");
    }
    assert_eq!(fs::read(&disk).unwrap(), before);
    assert!(
        !h.paths
            .machine("plus-a")
            .join("checkpoints/live.dsk")
            .exists()
    );
    assert!(
        h.paths
            .machine("plus-a")
            .join("checkpoints/clean.toml")
            .exists()
    );

    // An ephemeral run holds its saved disk too, as does any other reader.
    let held = lock_image(&h.paths, &disk, LockMode::Shared).unwrap();
    let error = h
        .cmd(&["checkpoint", "create", "plus-a", "--name", "x"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("is running"), "{error}");
    drop(held);
    h.cmd(&["checkpoint", "create", "plus-a", "--name", "x"])
        .unwrap();
}

#[test]
fn a_damaged_checkpoint_or_an_unfinished_one_is_never_used() {
    let (h, _mock) = Harness::mock();
    h.cmd(&["checkpoint", "create", "plus-a", "--name", "clean"])
        .unwrap();
    let dir = h.paths.machine("plus-a").join("checkpoints");
    let disk = h.system_disk("plus-a");
    let live = fs::read(&disk).unwrap();

    // Changed behind macman's back: refused, and the live disk is kept.
    let image = dir.join("clean.dsk");
    let mut perms = fs::metadata(&image).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&image, perms).unwrap();
    let mut bytes = fs::read(&image).unwrap();
    bytes[2000] ^= 1;
    fs::write(&image, bytes).unwrap();
    let error = h
        .cmd(&["rollback", "plus-a", "--checkpoint", "clean", "--yes"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("damaged"), "{error}");
    assert_eq!(fs::read(&disk).unwrap(), live);
    assert_eq!(
        fs::read_dir(h.paths.machine("plus-a")).unwrap().count(),
        3,
        "no staging left"
    );

    // An image with no metadata is an interrupted create: not listed, and
    // a create of that name replaces it.
    fs::write(dir.join("half.dsk"), b"partial").unwrap();
    let names = |h: &Harness| {
        macman::checkpoint::list(&h.paths, "plus-a")
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&h), ["clean"]);
    assert!(
        h.cmd(&["rollback", "plus-a", "--checkpoint", "half", "--yes"])
            .is_err()
    );
    h.cmd(&["checkpoint", "create", "plus-a", "--name", "half"])
        .unwrap();
    assert_eq!(fs::read(dir.join("half.dsk")).unwrap(), live);

    // Without a name, one is made from the time.
    h.cmd(&["checkpoint", "create", "plus-a"]).unwrap();
    let all = names(&h);
    assert_eq!(all.len(), 3);
    assert!(
        all.iter().any(|n| n.ends_with('Z') && n.contains('T')),
        "{all:?}"
    );
}
