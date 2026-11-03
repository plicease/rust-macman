//! The Basilisk II adapter, fed a fixture source tree in place of macemu. The
//! fixture follows upstream's build steps and stands in for the emulator with a
//! script, which records how it was called and acts out what the real one does.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
))]

mod common;

use anyhow::Result;
use clap::Parser;
use common::{PLUS_ROM_CHECKSUM, write_disk, write_partitioned_disk, write_rom};
use macman::app::App;
use macman::archive;
use macman::backend::basilisk2::{BACKEND_NAME, Basilisk2};
use macman::backend::macemu::pin;
use macman::backend::source::SourcePin;
use macman::backend::{Backends, Check, CheckLevel, EmulatorBackend, Executable, LaunchRequest};
use macman::cli::Cli;
use macman::fsutil::{list_dir, sha256_file};
use macman::machine::{Cpu, Machine, Network};
use macman::paths::Paths;
use macman::registry::Rom;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::{Mutex, MutexGuard, PoisonError};
use tempfile::TempDir;

const AUTOGEN: &str = "#!/bin/sh\necho \"autogen NO_CONFIGURE=$NO_CONFIGURE\" >> LOG\n";
const CONFIGURE: &str = "#!/bin/sh\necho \"configure $*\" >> LOG\ncp fixture.mk Makefile\n";
const MAKEFILE: &str = "all:\n\techo build >> LOG\n\tcp fake-emulator BasiliskII\n";
/// `$2` is the preferences file, and `$4` the ROM.
const EMULATOR: &str = r#"#!/bin/sh
echo "run $*" >> LOG
echo "cwd $(pwd -P)" >> LOG
cat "$2" >> LOG
mode=$(cat MODE 2>/dev/null)
case "$mode" in
    refuse) echo "Reading ROM file..."; echo "ERROR: Unsupported ROM type."; exit 0 ;;
    crash) kill -SEGV $$ ;;
    fail) exit 3 ;;
esac
shift 4
while [ $# -gt 0 ]; do
    if [ "$1" = --disk ]; then
        case "$2" in
            \**) ;;
            *) printf written | dd of="$2" bs=1 seek=4096 conv=notrunc 2>/dev/null ;;
        esac
    fi
    shift 2
done
"#;

/// Spawning tests run one at a time: a child holds its parent's descriptors
/// between fork and exec, and with them any file lock.
static ALONE: Mutex<()> = Mutex::new(());

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
    log: PathBuf,
    archive: PathBuf,
    pin: SourcePin,
    _turn: MutexGuard<'static, ()>,
}

fn write_script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn fixture_archive(root: &Path, log: &Path) -> PathBuf {
    let log = log.to_str().unwrap();
    let unix = root.join("fixture/macemu/BasiliskII/src/Unix");
    fs::create_dir_all(&unix).unwrap();
    write_script(&unix.join("autogen.sh"), &AUTOGEN.replace("LOG", log));
    write_script(&unix.join("configure"), &CONFIGURE.replace("LOG", log));
    fs::write(unix.join("fixture.mk"), MAKEFILE.replace("LOG", log)).unwrap();
    write_script(
        &unix.join("fake-emulator"),
        &EMULATOR
            .replace("LOG", log)
            .replace("MODE", root.join("mode").to_str().unwrap()),
    );
    let archive = root.join("fixture.tgz");
    let status = Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(root.join("fixture"))
        .arg("macemu")
        .status()
        .unwrap();
    assert!(status.success());
    archive
}

fn rom_bytes(version: [u8; 2]) -> Vec<u8> {
    let mut bytes = vec![0u8; 1024 * 1024];
    bytes[8..10].copy_from_slice(&version);
    bytes
}

impl Harness {
    /// None if this host lacks what the fixture's build needs.
    fn new() -> Option<Self> {
        let turn = ALONE.lock().unwrap_or_else(PoisonError::into_inner);
        let tools = ["g++", "make", "tar", "bash"];
        if !tools
            .iter()
            .all(|tool| Command::new(tool).arg("--version").output().is_ok())
        {
            eprintln!("skipping: build tools not installed");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("emulator.log");
        let archive = fixture_archive(root.path(), &log);
        let pin = SourcePin {
            url: "http://invalid.example/not-used".into(),
            sha256: sha256_file(&archive).unwrap(),
            ..pin()
        };
        // Whole, as macOS's temporary directory is behind a symbolic link.
        let paths = Paths::under(&fs::canonicalize(root.path()).unwrap());
        let app = App::new(paths.clone(), Self::backends(&paths, &pin, &archive));
        let harness = Self {
            root,
            paths,
            app,
            log,
            archive,
            pin,
            _turn: turn,
        };
        let quadra = harness.root.path().join("quadra.bin");
        fs::write(&quadra, rom_bytes([0x06, 0x7C])).unwrap();
        let system = write_disk(harness.root.path(), "system.dsk");
        harness
            .cmd(&["rom", "add", quadra.to_str().unwrap(), "--name", "quadra"])
            .unwrap();
        harness
            .cmd(&["system", "add", system.to_str().unwrap(), "--name", "sys"])
            .unwrap();
        Some(harness)
    }

    fn backends(paths: &Paths, pin: &SourcePin, archive: &Path) -> Backends {
        Backends::new(vec![Box::new(Basilisk2::new(
            paths.clone(),
            pin.clone(),
            Some(archive.to_path_buf()),
        ))])
    }

    fn cmd(&self, args: &[&str]) -> Result<u8> {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        self.app.execute(Cli::parse_from(argv))
    }

    fn create(&self, name: &str) {
        self.create_with(name, &[]);
    }

    fn create_with(&self, name: &str, extra: &[&str]) {
        let mut args = vec!["create", name, "--rom", "quadra", "--system", "sys"];
        args.extend(extra);
        self.cmd(&args).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn count(&self, needle: &str) -> usize {
        self.log().lines().filter(|line| *line == needle).count()
    }

    fn act(&self, mode: &str) {
        fs::write(self.root.path().join("mode"), mode).unwrap();
    }

    fn builds(&self) -> Vec<String> {
        list_dir(&self.paths.builds()).unwrap()
    }

    fn sessions(&self) -> usize {
        list_dir(&self.paths.sessions()).unwrap().len()
    }

    fn doctor_says(&self, text: &str) -> bool {
        self.app
            .doctor_checks(false)
            .unwrap()
            .iter()
            .any(|check| check.message.contains(text))
    }
}

macro_rules! harness {
    () => {
        match Harness::new() {
            Some(harness) => harness,
            None => return,
        }
    };
}

#[test]
fn creating_needs_a_cpu_not_a_model() {
    let h = harness!();
    h.create_with(
        "q",
        &[
            "--backend",
            "basilisk2",
            "--cpu",
            "68030",
            "--no-fpu",
            "--no-sound",
            "--network",
            "slirp",
            "--memory",
            "64m",
            "--display",
            "800x600x16",
        ],
    );
    let m = Machine::load(&h.paths, "q").unwrap();
    assert_eq!(m.emulator.backend, BACKEND_NAME);
    assert_eq!(m.hardware.model, None);
    assert_eq!(m.hardware.memory, "64M");
    assert_eq!((m.display.width, m.display.depth), (800, 16));
    let settings = m.basilisk2.unwrap();
    assert_eq!(settings.cpu, Cpu::M68030);
    assert!(!settings.fpu && !settings.sound);
    assert_eq!(settings.network, Network::Slirp);

    let text = fs::read_to_string(h.paths.machine("q").join("machine.toml")).unwrap();
    assert!(!text.contains("model"), "{text}");
    assert!(text.contains("[basilisk2]"), "{text}");
}

#[test]
fn settings_of_one_backend_are_refused_for_the_other() {
    let h = harness!();
    let base = ["create", "q", "--rom", "quadra", "--system", "sys"];
    let with = |extra: &[&'static str]| {
        let args: Vec<_> = base.iter().chain(extra).copied().collect();
        format!("{:#}", h.cmd(&args).unwrap_err())
    };
    assert!(with(&["--model", "plus"]).contains("--model is for Mini vMac"));
    assert!(with(&["--no-fpu"]).contains("always has an FPU"));
    assert!(!h.paths.machine("q").exists());
}

#[test]
fn only_a_32_bit_clean_rom_makes_a_machine() {
    let h = harness!();
    let plus = write_rom(h.root.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
    h.cmd(&["rom", "add", plus.to_str().unwrap(), "--name", "plus"])
        .unwrap();
    let error = h
        .cmd(&["create", "q", "--rom", "plus", "--system", "sys"])
        .unwrap_err();
    assert!(error.to_string().contains("32-bit clean"), "{error}");
}

#[test]
fn identically_configured_machines_share_one_build() {
    let h = harness!();
    h.create("a");
    h.create_with("b", &["--memory", "128M", "--cpu", "68030"]);
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert_eq!(h.cmd(&["run", "b"]).unwrap(), 0);
    assert_eq!(h.count("build"), 1);
    assert_eq!(h.builds().len(), 1);

    let log = h.log();
    assert!(log.contains("autogen NO_CONFIGURE=1"), "{log}");
    assert!(
        log.contains("configure --enable-sdl-video --enable-sdl-audio --with-gtk=no"),
        "{log}"
    );
    // Both ran, each on its own settings.
    assert!(log.contains("ramsize 33554432"), "{log}");
    assert!(log.contains("ramsize 134217728"), "{log}");
    assert_eq!(h.cmd(&["list", "--builds"]).unwrap(), 0);
}

#[test]
fn a_damaged_build_is_rebuilt() {
    let h = harness!();
    h.create("a");
    h.cmd(&["run", "a"]).unwrap();
    let build = h.paths.builds().join(&h.builds()[0]);
    write_script(&build.join("BasiliskII"), "#!/bin/sh\nexit 0\n");
    assert!(h.doctor_says("is damaged"));

    h.cmd(&["run", "a"]).unwrap();
    assert_eq!(h.count("build"), 2);
    assert!(h.doctor_says("intact"));
}

#[test]
fn the_source_must_match_its_pin() {
    let h = harness!();
    h.create("a");
    let wrong = SourcePin {
        sha256: "0".repeat(64),
        ..h.pin.clone()
    };
    let app = App::new(
        h.paths.clone(),
        Harness::backends(&h.paths, &wrong, &h.archive),
    );
    let argv = ["macman", "run", "a"];
    let error = app.execute(Cli::parse_from(argv)).unwrap_err();
    assert!(error.to_string().contains("pinned SHA-256"), "{error}");
    assert_eq!(h.count("build"), 0);
}

#[test]
fn each_run_has_preferences_of_its_own_in_its_session() {
    let h = harness!();
    h.create("a");
    h.cmd(&["run", "a"]).unwrap();
    let log = h.log();
    let sessions = fs::canonicalize(h.paths.sessions()).unwrap();
    let cwd = log.lines().find_map(|l| l.strip_prefix("cwd ")).unwrap();
    assert!(Path::new(cwd).starts_with(&sessions), "{log}");
    let run = log.lines().find(|l| l.starts_with("run ")).unwrap();
    assert!(
        run.starts_with(&format!("run --config {cwd}/prefs --rom ")),
        "{run}"
    );
    assert!(log.contains("nogui true"), "{log}");
    assert_eq!(h.sessions(), 0, "a clean exit leaves no session");
}

#[test]
fn writes_reach_the_private_disk_only() {
    let h = harness!();
    h.create("a");
    let template = h.paths.systems().join("sys.dsk");
    let before = sha256_file(&template).unwrap();
    let disk = h.paths.machine("a").join("system.dsk");
    let private = sha256_file(&disk).unwrap();

    h.cmd(&["run", "a"]).unwrap();
    assert_ne!(sha256_file(&disk).unwrap(), private);
    assert_eq!(sha256_file(&template).unwrap(), before);
}

#[test]
fn a_readonly_extra_disk_cannot_change_its_source() {
    let h = harness!();
    h.create("a");
    let extra = write_disk(h.root.path(), "extra.dsk");
    let before = sha256_file(&extra).unwrap();
    let path = extra.to_str().unwrap();

    h.cmd(&["run", "a", "--disk", path, "--readonly"]).unwrap();
    assert_eq!(sha256_file(&extra).unwrap(), before);
    let log = h.log();
    let run = log.lines().find(|l| l.starts_with("run ")).unwrap();
    assert!(run.contains("--disk *"), "{run}");
    assert!(!run.contains(path), "the emulator never hears of it: {run}");
}

#[test]
fn doctor_warns_of_a_boot_disk_left_mounted_unless_it_is_in_use() {
    let h = harness!();
    h.create("a");
    let warning = "wasn't shut down cleanly";
    assert!(!h.doctor_says(warning), "a fresh disk is clean");

    let disk = h.paths.machine("a").join("system.dsk");
    let mut image = fs::read(&disk).unwrap();
    image[1034] = 0;
    fs::write(&disk, image).unwrap();
    assert!(h.doctor_says(warning));

    let lock = macman::disk::lock_image(&h.paths, &disk, macman::disk::LockMode::Exclusive);
    assert!(lock.is_ok());
    assert!(!h.doctor_says(warning), "a running guest has it mounted");
    drop(lock);

    macman::disk::mark_unmounted(&disk).unwrap();
    assert!(!h.doctor_says(warning));
}

#[test]
fn a_partitioned_disk_is_left_for_the_emulator_to_mount() {
    let h = harness!();
    h.create("a");
    let mut image = vec![0u8; 256 * 512];
    image[..4].copy_from_slice(b"ER\x02\x00");
    image[512..514].copy_from_slice(b"PM");
    image[512 + 7] = 1; // One entry in the map.
    image[512 + 11] = 64; // Its first block.
    image[512 + 15] = 128; // Its length.
    image[512 + 48..512 + 57].copy_from_slice(b"Apple_HFS");
    let extra = h.root.path().join("whole.img");
    fs::write(&extra, image).unwrap();

    let path = extra.to_str().unwrap();
    h.cmd(&["run", "a", "--disk", path, "--readonly"]).unwrap();
    assert!(h.log().contains("--disk *"), "{}", h.log());
}

#[test]
fn a_refusal_that_exits_zero_is_still_a_failure() {
    let h = harness!();
    h.create("a");
    h.act("refuse");
    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(
        error.to_string().contains("Unsupported ROM type"),
        "{error}"
    );
    assert!(h.doctor_says("the emulator failed to start"));

    h.act("");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert!(!h.doctor_says("ended abnormally"), "the next run clears it");
    assert_eq!(h.sessions(), 0);
}

#[test]
fn forced_termination_and_failure_are_kept_for_doctor() {
    let h = harness!();
    h.create("a");
    h.act("crash");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 128 + 11);
    assert!(h.doctor_says("crashed on the host"));
    h.act("fail");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 3);
    assert!(h.doctor_says("exited with code 3"));
    let disk = h.paths.machine("a").join("system.dsk");
    assert!(disk.exists(), "disks are left in place");
}

#[test]
fn an_imported_machine_builds_on_its_first_run_without_the_host_s_folder() {
    let h = harness!();
    let shared = h.root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    h.create_with("a", &["--shared-folder", shared.to_str().unwrap()]);
    let saved = Machine::load(&h.paths, "a").unwrap();
    assert!(saved.basilisk2.unwrap().shared_folder.is_some());
    let exported = h.root.path().join("a.tar.gz");
    h.cmd(&["export", "a", "-o", exported.to_str().unwrap()])
        .unwrap();

    let other = tempfile::tempdir().unwrap();
    let paths = Paths::under(other.path());
    let backends = Harness::backends(&paths, &h.pin, &h.archive);
    let report = archive::import(&paths, &backends, &exported, None).unwrap();
    assert_eq!(report.imported(), 1);
    assert!(
        list_dir(&paths.builds()).unwrap().is_empty(),
        "lazily built"
    );
    let imported = Machine::load(&paths, "a").unwrap();
    assert_eq!(imported.basilisk2.unwrap().shared_folder, None);

    let app = App::new(paths.clone(), backends);
    let argv = ["macman", "run", "a"];
    assert_eq!(app.execute(Cli::parse_from(argv)).unwrap(), 0);
    assert_eq!(list_dir(&paths.builds()).unwrap().len(), 1);

    // Coming back changes nothing, though one copy has a folder.
    let report = archive::import(&h.paths, &h.app.backends, &exported, None).unwrap();
    assert_eq!(report.unchanged(), 1);
}

#[test]
fn a_shared_folder_goes_to_the_emulator_and_must_exist() {
    let h = harness!();
    let shared = h.root.path().join("shared folder");
    fs::create_dir(&shared).unwrap();
    h.create_with("a", &["--shared-folder", shared.to_str().unwrap()]);
    h.cmd(&["run", "a"]).unwrap();
    let expected = format!("--extfs {}", fs::canonicalize(&shared).unwrap().display());
    assert!(h.log().contains(&expected), "{}", h.log());

    fs::remove_dir(&shared).unwrap();
    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(error.to_string().contains("shared folder"), "{error}");
}

/// macman's own locks go with it, so only the emulator's show one left behind.
#[test]
#[cfg(target_os = "macos")]
fn a_disk_held_by_an_emulator_that_outlived_macman_is_refused() {
    let h = harness!();
    h.create("a");
    let held = fs::File::open(h.paths.machine("a").join("system.dsk")).unwrap();
    held.lock().unwrap();

    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(error.to_string().contains("outlived macman"), "{error}");
    assert_eq!(h.count("build"), 1);
    assert!(!h.log().contains("run --config"), "{}", h.log());

    drop(held);
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
}

/// Stands in for a backend whose host checks fail.
struct Lacking(&'static str);

impl EmulatorBackend for Lacking {
    fn name(&self) -> &str {
        self.0
    }
    fn revision(&self) -> &str {
        "1"
    }
    fn resolve_revision(&self, recorded: &str) -> Result<String> {
        Ok(recorded.to_string())
    }
    fn validate(&self, _: &Machine) -> Result<()> {
        Ok(())
    }
    fn validate_disks(&self, _: &Machine, _: usize) -> Result<()> {
        Ok(())
    }
    fn check_rom(&self, _: &Machine, _: &Rom, _: &Path) -> Result<()> {
        Ok(())
    }
    fn prepare(&self, _: &Machine, _: bool) -> Result<Executable> {
        unreachable!()
    }
    fn launch(&self, _: &LaunchRequest<'_>) -> Result<ExitStatus> {
        unreachable!()
    }
    fn doctor(&self) -> Vec<Check> {
        vec![Check::fail(
            format!("{} lacks a tool", self.0),
            "install it",
        )]
    }
}

#[test]
fn doctor_fails_for_a_missing_dependency_only_if_a_machine_needs_it() {
    let h = harness!();
    let app = App::new(
        h.paths.clone(),
        Backends::new(vec![
            Box::new(Lacking("minivmac")),
            Box::new(Lacking(BACKEND_NAME)),
        ]),
    );
    let level = |name: &str| {
        app.doctor_checks(false)
            .unwrap()
            .into_iter()
            .find(|check| check.message == format!("{name} lacks a tool"))
            .unwrap()
            .level
    };
    assert_eq!(
        level("minivmac"),
        CheckLevel::Fail,
        "the default always counts"
    );
    assert_eq!(level(BACKEND_NAME), CheckLevel::Warn);

    h.create("a");
    assert_eq!(level(BACKEND_NAME), CheckLevel::Fail);
}

#[test]
fn a_blank_machine_installs_from_a_cd_it_does_not_keep() {
    let h = harness!();
    h.cmd(&["create", "new", "--rom", "quadra", "--disk-size", "2M"])
        .unwrap();
    let disk = h.paths.machine("new").join("system.dsk");
    assert_eq!(fs::metadata(&disk).unwrap().len(), 2 << 20);

    let (cd, _) = write_partitioned_disk(h.root.path(), "installer disc.iso");
    let before = fs::read(&cd).unwrap();
    let cd_path = cd.to_str().unwrap();
    assert_eq!(h.cmd(&["run", "new", "--cdrom", cd_path]).unwrap(), 0);
    let log = h.log();
    let resolved = fs::canonicalize(&cd).unwrap();
    assert!(
        log.contains(&format!("--cdrom {}", resolved.display())),
        "{log}"
    );
    assert!(log.contains("nocdrom false"), "{log}");
    assert!(log.contains("bootdriver -62"), "{log}");
    assert_eq!(fs::read(&cd).unwrap(), before);
    assert!(fs::read(&disk).unwrap().windows(7).any(|w| w == b"written"));

    fs::remove_file(&h.log).unwrap();
    assert_eq!(h.cmd(&["run", "new"]).unwrap(), 0);
    let log = h.log();
    assert!(!log.contains("--cdrom"), "{log}");
    assert!(log.contains("nocdrom true"), "{log}");
    assert!(log.contains("bootdriver 0"), "{log}");
}
