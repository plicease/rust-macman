//! Drives the real binary through commands that never start an emulator.

mod common;

use common::{II_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_rom};
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_macman"))
            .args(args)
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("cfg"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        stderr(&output)
    }

    fn register(&self) {
        let rom = write_rom(self.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        let disk = write_disk(self.path(), "sys6.dsk");
        self.ok(&["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"]);
        self.ok(&[
            "system",
            "add",
            disk.to_str().unwrap(),
            "--name",
            "system-6.0.8",
            "--version",
            "6.0.8",
        ]);
    }

    fn create(&self, name: &str) {
        self.ok(&[
            "create",
            name,
            "--model",
            "plus",
            "--rom",
            "plus-v3",
            "--system",
            "system-6.0.8",
            "--memory",
            "4M",
        ]);
    }

    fn machine_disk(&self, name: &str) -> std::path::PathBuf {
        self.path()
            .join("data/macman/machines")
            .join(name)
            .join("system.dsk")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn registers_files_under_xdg_paths() {
    let env = Env::new();
    env.register();
    assert!(env.path().join("data/macman/roms/plus-v3.rom").is_file());
    assert!(
        env.path()
            .join("data/macman/systems/system-6.0.8.dsk")
            .is_file()
    );
    assert!(
        env.ok(&["rom", "list"])
            .contains("plus-v3  128K  512ke/plus")
    );
    let systems = env.ok(&["system", "list"]);
    assert!(
        systems.contains("system-6.0.8  6.0.8  800K  raw HFS"),
        "{systems}"
    );
}

#[test]
fn create_clones_the_template_and_writes_the_design_format() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");

    // A machine's writes must not reach the template.
    fs::write(env.machine_disk("wombat-6"), b"changed").unwrap();
    let template = env.path().join("data/macman/systems/system-6.0.8.dsk");
    assert_eq!(fs::metadata(template).unwrap().len(), 800 * 1024);

    let shown = env.ok(&["config", "show", "wombat-6"]);
    for expected in [
        "schema_version = 1",
        "backend = \"minivmac\"",
        "revision = \"36.04\"",
        "model = \"plus\"",
        "memory = \"4M\"",
        "version = \"6.0.8\"",
        "path = \"system.dsk\"",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
}

#[test]
fn use_current_and_list() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    env.create("wombat-7");

    assert!(env.fails(&["current"]).contains("macman use"));
    assert!(env.fails(&["use", "nope"]).contains("no machine named"));
    env.ok(&["use", "wombat-6"]);
    assert_eq!(env.ok(&["current"]).trim(), "wombat-6");

    let list = env.ok(&["list"]);
    assert!(
        list.contains("* wombat-6  plus 4M 512x342x1 system 6.0.8"),
        "{list}"
    );
    assert!(list.contains("  wombat-7"), "{list}");
    assert!(env.ok(&["config", "show"]).contains("wombat-6"));
}

#[test]
fn clone_gets_independent_disks() {
    let env = Env::new();
    env.register();
    env.create("a");
    env.ok(&["clone", "a", "b"]);
    fs::write(env.machine_disk("a"), b"changed").unwrap();
    assert_eq!(
        fs::metadata(env.machine_disk("b")).unwrap().len(),
        800 * 1024
    );
    assert!(env.ok(&["config", "show", "b"]).contains("name = \"b\""));
    assert!(env.fails(&["clone", "a", "b"]).contains("already exists"));
}

#[test]
fn remove_needs_confirmation_and_keeps_templates() {
    let env = Env::new();
    env.register();
    env.create("gone");
    env.ok(&["use", "gone"]);

    assert!(env.fails(&["remove", "gone"]).contains("--delete-disks"));
    assert!(env.machine_disk("gone").exists());

    env.ok(&["remove", "gone", "--delete-disks"]);
    assert!(!env.machine_disk("gone").exists());
    assert!(
        env.path()
            .join("data/macman/systems/system-6.0.8.dsk")
            .exists()
    );
    assert!(env.path().join("data/macman/roms/plus-v3.rom").exists());
    assert!(env.fails(&["current"]).contains("no current machine"));
}

#[test]
fn bad_inputs_get_useful_errors() {
    let env = Env::new();
    env.register();
    let create = |extra: &[&str]| {
        let mut args = vec![
            "create",
            "m",
            "--rom",
            "plus-v3",
            "--system",
            "system-6.0.8",
        ];
        args.extend_from_slice(extra);
        env.fails(&args)
    };
    assert!(create(&["--model", "lisa"]).contains("unknown model"));
    assert!(create(&["--model", "plus", "--memory", "8M"]).contains("supported sizes"));
    assert!(create(&["--model", "plus", "--display", "512x342x8"]).contains("monochrome"));
    assert!(create(&["--model", "ii"]).contains("256"));
    assert!(!env.path().join("data/macman/machines/m").exists());

    let ii = write_rom(env.path(), "ii.bin", II_ROM_CHECKSUM, 256 * 1024);
    env.ok(&["rom", "add", ii.to_str().unwrap(), "--name", "macii"]);
    let se = env.fails(&[
        "create",
        "m",
        "--model",
        "se",
        "--rom",
        "macii",
        "--system",
        "system-6.0.8",
    ]);
    assert!(se.contains("is for ii"), "{se}");
    assert!(
        env.fails(&[
            "create",
            "m",
            "--model",
            "plus",
            "--rom",
            "none",
            "--system",
            "system-6.0.8"
        ])
        .contains("macman rom add")
    );
    assert!(
        env.fails(&[
            "create", "../evil", "--model", "plus", "--rom", "plus-v3", "--system", "x"
        ])
        .contains("invalid machine name")
    );

    let junk = env.path().join("junk.dsk");
    fs::write(&junk, vec![0u8; 4096]).unwrap();
    assert!(
        env.fails(&["system", "add", junk.to_str().unwrap(), "--name", "j"])
            .contains("not a supported disk image")
    );
    assert!(env.fails(&["run", "nope"]).contains("no machine named"));
}

#[test]
fn readonly_requires_a_disk() {
    let output = Env::new().run(&["run", "m", "--readonly"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn doctor_reports_damage_without_leaking_paths() {
    let env = Env::new();
    env.register();
    env.create("m");
    fs::write(env.path().join("data/macman/roms/plus-v3.rom"), b"tampered").unwrap();
    fs::remove_file(env.machine_disk("m")).unwrap();

    let output = env.run(&["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("[FAIL] ROM plus-v3 is missing or changed"),
        "{text}"
    );
    assert!(text.contains("[FAIL] machine m cannot run"), "{text}");
    assert!(text.contains("disk system.dsk is missing"), "{text}");
    assert!(!text.contains(env.path().to_str().unwrap()), "{text}");
}
