//! The SheepShaver adapter, fed a fixture source tree in place of macemu. The
//! fixture follows upstream's build steps and stands in for the emulator with a
//! script, which records how it was called and acts out what the real one does.
//! None of this shows that the real emulator builds or boots: for that, see
//! docs/SMOKE_TEST.md.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
))]

mod common;

use anyhow::Result;
use clap::Parser;
use common::{write_disk, write_partitioned_disk};
use macman::app::App;
use macman::archive;
use macman::backend::macemu::pin;
use macman::backend::sheepshaver::{BACKEND_NAME, SheepShaver};
use macman::backend::source::SourcePin;
use macman::backend::{Backends, CheckLevel};
use macman::cli::Cli;
#[cfg(target_os = "macos")]
use macman::disk::{self, LockMode};
use macman::fsutil::{list_dir, sha256_file};
use macman::machine::{Machine, Network};
use macman::paths::Paths;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use tempfile::TempDir;

const AUTOGEN: &str = "#!/bin/sh\necho \"autogen NO_CONFIGURE=$NO_CONFIGURE\" >> LOG\n";
const CONFIGURE: &str = "#!/bin/sh\necho \"configure $*\" >> LOG\ncp fixture.mk Makefile\n";
const MAKEFILE: &str = "all:\n\techo build >> LOG\n\tcp fake-emulator SheepShaver\n";
/// `$2` is the preferences file, and `$4` the ROM.
const EMULATOR: &str = r#"#!/bin/sh
echo "run $*" >> LOG
echo "cwd $(pwd -P)" >> LOG
cat "$2" >> LOG
mode=$(cat MODE 2>/dev/null)
case "$mode" in
    refuse) echo "ERROR: Unsupported ROM type."; exit 0 ;;
    lowmem) echo "ERROR: Cannot map Low Memory Globals: Operation not permitted."; exit 0 ;;
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
    let unix = root.join("fixture/macemu/SheepShaver/src/Unix");
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

/// A 4M dump with no Apple code, whose nanokernel says what it's from.
fn powerpc_rom(nanokernel: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    bytes[0x30_D064..0x30_D064 + nanokernel.len()].copy_from_slice(nanokernel);
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
        let rom = harness.root.path().join("beige.bin");
        fs::write(&rom, powerpc_rom(b"Boot Gossamer")).unwrap();
        let system = write_disk(harness.root.path(), "system.dsk");
        harness
            .cmd(&["rom", "add", rom.to_str().unwrap(), "--name", "beige"])
            .unwrap();
        harness
            .cmd(&["system", "add", system.to_str().unwrap(), "--name", "sys"])
            .unwrap();
        Some(harness)
    }

    fn backends(paths: &Paths, pin: &SourcePin, archive: &Path) -> Backends {
        Backends::new(vec![Box::new(SheepShaver::new(
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
        let mut args = vec!["create", name, "--rom", "beige", "--system", "sys"];
        args.extend(extra);
        self.cmd(&args).unwrap();
    }

    fn create_error(&self, extra: &[&str]) -> String {
        let mut args = vec!["create", "bad", "--rom", "beige", "--system", "sys"];
        args.extend(extra);
        format!("{:#}", self.cmd(&args).unwrap_err())
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
fn creating_needs_no_model_or_cpu() {
    let h = harness!();
    h.create_with(
        "b",
        &[
            "--no-sound",
            "--network",
            "slirp",
            "--memory",
            "256m",
            "--display",
            "800x600x32",
        ],
    );
    let m = Machine::load(&h.paths, "b").unwrap();
    assert_eq!(m.emulator.backend, BACKEND_NAME);
    assert_eq!(m.hardware.model, None);
    assert_eq!(m.hardware.memory, "256M");
    assert_eq!(m.basilisk2, None);
    let settings = m.sheepshaver.unwrap();
    assert!(!settings.sound);
    assert_eq!(settings.network, Network::Slirp);

    let text = fs::read_to_string(h.paths.machine("b").join("machine.toml")).unwrap();
    assert!(!text.contains("model"), "{text}");
    assert!(text.contains("[sheepshaver]"), "{text}");
    assert!(!text.contains("[basilisk2]"), "{text}");
}

#[test]
fn defaults_suit_a_powerpc_guest() {
    let h = harness!();
    h.create("b");
    let m = Machine::load(&h.paths, "b").unwrap();
    assert_eq!(m.hardware.memory, "64M");
    assert_eq!(
        (m.display.width, m.display.height, m.display.depth),
        (640, 480, 32)
    );
    assert!(m.sheepshaver.unwrap().sound);
}

#[test]
fn what_it_does_not_have_is_refused() {
    let h = harness!();
    assert!(
        h.create_error(&["--model", "plus"])
            .contains("--model is for Mini vMac")
    );
    assert!(
        h.create_error(&["--cpu", "68040"])
            .contains("needs --backend basilisk2")
    );
    assert!(
        h.create_error(&["--no-fpu"])
            .contains("needs --backend basilisk2")
    );
    assert!(
        h.create_error(&["--display", "640x480x8"])
            .contains("can't be chosen")
    );
    assert!(h.create_error(&["--memory", "8M"]).contains("16M to 1024M"));
    assert!(!h.paths.machine("bad").exists());
}

#[test]
fn only_a_powerpc_rom_makes_a_machine() {
    let h = harness!();
    let unknown = h.root.path().join("unknown.bin");
    fs::write(&unknown, powerpc_rom(b"Boot Unheard")).unwrap();
    let quadra = h.root.path().join("quadra.bin");
    let mut bytes = vec![0u8; 1024 * 1024];
    bytes[8..10].copy_from_slice(&[0x06, 0x7C]);
    fs::write(&quadra, bytes).unwrap();
    for (file, name) in [(&unknown, "unknown"), (&quadra, "quadra")] {
        h.cmd(&["rom", "add", file.to_str().unwrap(), "--name", name])
            .unwrap();
        let error = h
            .cmd(&["create", "q", "--rom", name, "--system", "sys"])
            .unwrap_err();
        assert!(error.to_string().contains("Mac OS ROM"), "{error}");
    }
    assert!(!h.paths.machine("q").exists());
}

#[test]
fn a_mac_os_rom_file_makes_a_machine() {
    let h = harness!();
    let file = h.root.path().join("Mac OS ROM");
    let mut bytes = b"<CHRP-BOOT>\n".to_vec();
    bytes.extend_from_slice(b"001000 constant lzss-offset\n");
    bytes.resize(1536 * 1024, 0);
    fs::write(&file, bytes).unwrap();
    h.cmd(&["rom", "add", file.to_str().unwrap(), "--name", "newworld"])
        .unwrap();
    h.cmd(&["create", "b", "--rom", "newworld", "--system", "sys"])
        .unwrap();
    assert_eq!(h.cmd(&["run", "b"]).unwrap(), 0);
}

#[test]
fn identically_configured_machines_share_one_build() {
    let h = harness!();
    h.create("a");
    h.create_with("b", &["--memory", "128M", "--no-sound"]);
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
    assert!(log.contains("ramsize 67108864"), "{log}");
    assert!(log.contains("ramsize 134217728"), "{log}");
    assert!(log.contains("nosound true"), "{log}");
}

#[test]
fn a_damaged_build_is_rebuilt() {
    let h = harness!();
    h.create("a");
    h.cmd(&["run", "a"]).unwrap();
    let build = h.paths.builds().join(&h.builds()[0]);
    write_script(&build.join("SheepShaver"), "#!/bin/sh\nexit 0\n");
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
fn a_disk_in_use_is_refused() {
    let h = harness!();
    h.create("a");
    let disk = h.paths.machine("a").join("system.dsk");
    let lock = macman::disk::lock_image(&h.paths, &disk, macman::disk::LockMode::Exclusive);
    assert!(lock.is_ok());
    assert!(h.cmd(&["run", "a"]).is_err());
    assert_eq!(h.count("build"), 0);
    drop(lock);
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
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

/// Only Linux maps low memory at address 0.
#[cfg(target_os = "linux")]
#[test]
fn a_failure_to_map_low_memory_names_the_fix() {
    let h = harness!();
    h.create("a");
    h.act("lowmem");
    let error = h.cmd(&["run", "a"]).unwrap_err().to_string();
    assert!(error.contains("Low Memory Globals"), "{error}");
    assert!(error.contains("vm.mmap_min_addr=0"), "{error}");
    assert!(error.contains("not permitted; run `sudo"), "{error}");
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
fn a_missing_rom_is_reported_by_doctor() {
    let h = harness!();
    h.create("a");
    fs::remove_file(h.paths.roms().join("beige.rom")).unwrap();
    let checks = h.app.doctor_checks(false).unwrap();
    let failed = checks
        .iter()
        .find(|check| check.level == CheckLevel::Fail && check.message.contains("machine a"));
    assert!(failed.is_some(), "{checks:?}");
}

#[test]
fn an_imported_machine_builds_on_its_first_run_without_the_host_s_folder() {
    let h = harness!();
    let shared = h.root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    h.create_with("a", &["--shared-folder", shared.to_str().unwrap()]);
    let saved = Machine::load(&h.paths, "a").unwrap();
    assert!(saved.sheepshaver.unwrap().shared_folder.is_some());
    let exported = h.root.path().join("a.tar.gz");
    h.cmd(&["export", "a", "-o", exported.to_str().unwrap()])
        .unwrap();

    let other = tempfile::tempdir().unwrap();
    let paths = Paths::under(other.path());
    let backends = Harness::backends(&paths, &h.pin, &h.archive);
    let report = archive::import(&paths, &backends, &exported, None, false).unwrap();
    assert_eq!(report.imported(), 1);
    assert!(
        list_dir(&paths.builds()).unwrap().is_empty(),
        "lazily built"
    );
    let imported = Machine::load(&paths, "a").unwrap();
    assert_eq!(imported.sheepshaver.unwrap().shared_folder, None);

    let app = App::new(paths.clone(), backends);
    let argv = ["macman", "run", "a"];
    assert_eq!(app.execute(Cli::parse_from(argv)).unwrap(), 0);
    assert_eq!(list_dir(&paths.builds()).unwrap().len(), 1);

    // Coming back changes nothing, though one copy has a folder.
    let report = archive::import(&h.paths, &h.app.backends, &exported, None, false).unwrap();
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

/// Reached through the backends a real macman has, so the name is really offered.
#[test]
fn the_production_backends_offer_it_after_the_others() {
    let root = tempfile::tempdir().unwrap();
    let backends = macman::default_backends(&Paths::under(root.path())).unwrap();
    assert!(backends.get(BACKEND_NAME).is_ok());
    assert_eq!(backends.default_backend().name(), "minivmac");
}

#[test]
fn a_blank_machine_installs_from_a_cd_it_does_not_keep() {
    let h = harness!();
    h.cmd(&["create", "new", "--rom", "beige", "--disk-size", "2M"])
        .unwrap();
    let disk = h.paths.machine("new").join("system.dsk");
    assert_eq!(fs::metadata(&disk).unwrap().len(), 2 << 20);
    assert!(fs::read(&disk).unwrap().iter().all(|byte| *byte == 0));
    assert!(h.doctor_says("machine new is runnable"));

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
    let saved = fs::read_to_string(h.paths.machine("new").join("machine.toml")).unwrap();
    assert!(!saved.contains("disc"), "{saved}");

    // Without it, this is an ordinary machine, which keeps what was written.
    fs::remove_file(&h.log).unwrap();
    assert_eq!(h.cmd(&["run", "new"]).unwrap(), 0);
    let log = h.log();
    assert!(!log.contains("--cdrom"), "{log}");
    assert!(log.contains("nocdrom true"), "{log}");
    assert!(log.contains("bootdriver 0"), "{log}");
}

#[test]
fn registered_media_install_a_blank_machine_and_are_never_written() {
    let h = harness!();
    let (cd, _) = write_partitioned_disk(h.root.path(), "Mac OS 9.toast");
    let original = fs::read(&cd).unwrap();
    h.cmd(&["media", "add", cd.to_str().unwrap(), "--name", "os9"])
        .unwrap();
    let floppy = write_disk(h.root.path(), "tools.image");
    h.cmd(&["media", "add", floppy.to_str().unwrap(), "--name", "tools"])
        .unwrap();
    let stored = h.paths.media().join("os9.img");
    assert_eq!(fs::read(&stored).unwrap(), original, "kept whole");

    h.cmd(&[
        "create",
        "new",
        "--rom",
        "beige",
        "--disk-size",
        "2M",
        "--media",
        "os9",
    ])
    .unwrap();
    let saved = fs::read_to_string(h.paths.machine("new").join("machine.toml")).unwrap();
    assert!(
        !saved.contains("os9"),
        "not bundled with the machine: {saved}"
    );
    let error = h
        .cmd(&[
            "create",
            "x",
            "--rom",
            "beige",
            "--disk-size",
            "2M",
            "--media",
            "nope",
        ])
        .unwrap_err();
    assert!(error.to_string().contains("no media named"), "{error}");
    assert!(!h.paths.machine("x").exists());

    assert_eq!(
        h.cmd(&["run", "new", "--media", "os9", "--media", "tools"])
            .unwrap(),
        0
    );
    let log = h.log();
    let run = log.lines().find(|l| l.starts_with("run ")).unwrap();
    let resolved = fs::canonicalize(&stored).unwrap();
    assert!(
        run.contains(&format!("--cdrom {}", resolved.display())),
        "{run}"
    );
    // The floppy goes as a read-only throwaway copy, never the stored file.
    assert!(run.contains("--disk *"), "{run}");
    assert!(!run.contains("media/tools.img"), "{run}");
    assert!(log.contains("nocdrom false"), "{log}");
    assert_eq!(fs::read(&stored).unwrap(), original);
    assert_eq!(fs::read(&cd).unwrap(), original);

    // A medium in use can't be removed, and an unknown one isn't run.
    assert!(h.cmd(&["run", "new", "--media", "nope"]).is_err());
    h.cmd(&["media", "remove", "os9", "--yes"]).unwrap();
    let error = h.cmd(&["run", "new", "--media", "os9"]).unwrap_err();
    assert!(error.to_string().contains("no media named"), "{error}");
    assert!(cd.exists(), "the source is the user's");
}

#[test]
fn a_machine_installed_from_a_cd_checkpoints_once_it_has_a_system() {
    let h = harness!();
    h.cmd(&["create", "new", "--rom", "beige", "--disk-size", "2M"])
        .unwrap();
    let error = h
        .cmd(&["checkpoint", "create", "new", "--name", "empty"])
        .unwrap_err();
    assert!(error.to_string().contains("blank"), "{error}");

    // The emulator stands in for an installer, writing to the blank disk.
    let (cd, _) = write_partitioned_disk(h.root.path(), "installer.iso");
    h.cmd(&["run", "new", "--cdrom", cd.to_str().unwrap()])
        .unwrap();
    let disk = h.paths.machine("new").join("system.dsk");
    let installed = fs::read(&disk).unwrap();
    h.cmd(&["checkpoint", "create", "new", "--name", "installed"])
        .unwrap();
    assert!(installed.windows(7).any(|w| w == b"written"));
    // Something the guest does afterward, which the rollback undoes.
    fs::write(&disk, vec![0xEEu8; 2 << 20]).unwrap();
    h.cmd(&["rollback", "new", "--checkpoint", "installed", "--yes"])
        .unwrap();
    assert!(fs::read(&disk).unwrap() == installed, "not restored");
    assert_eq!(fs::metadata(&disk).unwrap().len(), 2 << 20);
}

#[test]
fn a_blank_machine_can_be_cloned_exported_and_imported() {
    let h = harness!();
    h.cmd(&["create", "new", "--rom", "beige", "--disk-size", "2M"])
        .unwrap();
    h.cmd(&["clone", "new", "copy"]).unwrap();
    let copy = h.paths.machine("copy").join("system.dsk");
    assert_eq!(fs::metadata(&copy).unwrap().len(), 2 << 20);

    let archive = h.root.path().join("new.tar.gz");
    let archive = archive.to_str().unwrap();
    h.cmd(&["export", "new", "-o", archive]).unwrap();
    h.cmd(&["import", archive, "--rename", "back"]).unwrap();
    let back = h.paths.machine("back").join("system.dsk");
    assert_eq!(fs::metadata(&back).unwrap().len(), 2 << 20);
    assert_eq!(h.cmd(&["run", "back"]).unwrap(), 0);
}

#[test]
fn only_cd_images_are_attached_as_cds() {
    let h = harness!();
    h.create("a");
    let dir = h.root.path();
    let junk = dir.join("junk.iso");
    fs::write(&junk, vec![7u8; 8192]).unwrap();
    let blank = dir.join("blank.iso");
    fs::write(&blank, vec![0u8; 8192]).unwrap();
    let archive = dir.join("cd.zip");
    fs::write(&archive, b"PK\x03\x04 and more").unwrap();
    for (file, expected) in [
        (&junk, "in a format macman doesn't know"),
        (&blank, "is blank"),
        (&archive, "a zip archive"),
    ] {
        let error = h
            .cmd(&["run", "a", "--cdrom", file.to_str().unwrap()])
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    assert!(h.cmd(&["run", "a", "--cdrom", "/no/such.iso"]).is_err());
    assert_eq!(h.count("run"), 0);
}

#[test]
fn a_disk_and_a_size_for_the_boot_disk_are_exclusive() {
    let both = [
        "macman",
        "create",
        "a",
        "--rom",
        "r",
        "--system",
        "s",
        "--disk-size",
        "1G",
    ];
    assert!(Cli::try_parse_from(both).is_err());
    assert!(Cli::try_parse_from(["macman", "create", "a", "--rom", "r"]).is_err());
}

#[test]
fn a_bad_size_makes_no_machine() {
    let h = harness!();
    for size in ["0M", "1", "2049G"] {
        let args = ["create", "new", "--rom", "beige", "--disk-size", size];
        assert!(h.cmd(&args).is_err(), "{size}");
    }
    assert!(!h.paths.machine("new").exists());
}

/// macman's own locks go with it, so only the emulator's show one left behind.
#[cfg(target_os = "macos")]
#[test]
fn a_disk_held_by_an_emulator_that_outlived_macman_is_refused() {
    let h = harness!();
    h.create("a");
    let held = fs::File::open(h.paths.machine("a").join("system.dsk")).unwrap();
    held.lock().unwrap();

    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(error.to_string().contains("outlived macman"), "{error}");
    assert!(!h.log().contains("run --config"), "{}", h.log());

    drop(held);
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
}

/// Neither the JIT nor low memory comes into a macOS build.
#[cfg(target_os = "macos")]
#[test]
fn a_macos_build_is_interpreted() {
    let h = harness!();
    h.create("a");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    let log = h.log();
    assert!(log.contains("--disable-jit"), "{log}");
    assert!(log.lines().any(|line| line == "jit false"), "{log}");
    assert!(!h.doctor_says("mmap_min_addr"));
}

/// The emulator locks CD images on macOS too, so a second run would go
/// without the disc, and one left behind by a killed macman holds it.
#[cfg(target_os = "macos")]
#[test]
fn a_cd_another_run_or_emulator_holds_is_refused() {
    let h = harness!();
    h.create("a");
    let (cd, _) = write_partitioned_disk(h.root.path(), "installer disc.iso");
    let cd_path = cd.to_str().unwrap();

    let other = disk::lock_image(&h.paths, &cd, LockMode::Shared).unwrap();
    let error = h.cmd(&["run", "a", "--cdrom", cd_path]).unwrap_err();
    assert!(error.to_string().contains("in use"), "{error}");
    drop(other);

    let held = fs::File::open(&cd).unwrap();
    held.lock().unwrap();
    let error = h.cmd(&["run", "a", "--cdrom", cd_path]).unwrap_err();
    assert!(error.to_string().contains("outlived macman"), "{error}");
    assert!(!h.log().contains("run --config"), "{}", h.log());
    drop(held);

    assert_eq!(h.cmd(&["run", "a", "--cdrom", cd_path]).unwrap(), 0);
}
