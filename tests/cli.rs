//! Drives the real binary through commands that never start an emulator.

mod common;

use common::{II_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_partitioned_disk, write_rom};
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
    fs::write(
        &junk,
        b"PK\x03\x04 this is really a zip archive............",
    )
    .unwrap();
    assert!(
        env.fails(&["system", "add", junk.to_str().unwrap(), "--name", "j"])
            .contains("not a supported disk image")
    );
    assert!(env.fails(&["run", "nope"]).contains("no machine named"));
}

#[test]
fn system_add_extracts_the_volume_from_a_partitioned_disk() {
    let env = Env::new();
    let (image, volume) = write_partitioned_disk(env.path(), "system-zip.image");
    let before = fs::read(&image).unwrap();

    let added = env.ok(&["system", "add", image.to_str().unwrap(), "--name", "zip"]);
    assert!(
        added.contains("extracted from partition 3 (Apple_HFS)"),
        "{added}"
    );

    let stored = env.path().join("data/macman/systems/zip.dsk");
    assert_eq!(fs::read(stored).unwrap(), volume);
    assert_eq!(
        fs::read(&image).unwrap(),
        before,
        "the source is left alone"
    );

    let sidecar = fs::read_to_string(env.path().join("data/macman/systems/zip.toml")).unwrap();
    assert!(
        sidecar.contains("extracted_from = \"partition 3 (Apple_HFS)\""),
        "{sidecar}"
    );
    assert!(sidecar.contains("source_sha256"), "{sidecar}");
    assert!(env.ok(&["system", "list"]).contains("raw HFS"));
}

#[test]
fn system_add_unwraps_disk_copy_so_machines_get_a_writable_disk() {
    let env = Env::new();
    let mut image = vec![0u8; 84 + 1600 * 512];
    image[64..68].copy_from_slice(&(1600u32 * 512).to_be_bytes());
    image[82] = 1;
    image[84..86].copy_from_slice(b"LK");
    image[84 + 1024..84 + 1026].copy_from_slice(b"BD");
    let path = env.path().join("tools.image");
    fs::write(&path, &image).unwrap();

    let added = env.ok(&["system", "add", path.to_str().unwrap(), "--name", "dc42"]);
    assert!(
        added.contains("extracted from Disk Copy 4.2 image"),
        "{added}"
    );
    let stored = fs::read(env.path().join("data/macman/systems/dc42.dsk")).unwrap();
    assert_eq!(stored, image[84..]);
}

#[test]
fn system_add_refuses_blank_and_warns_about_unbootable_volumes() {
    let env = Env::new();
    let blank = env.path().join("blank.dsk");
    fs::write(&blank, vec![0u8; 8192]).unwrap();
    let error = env.fails(&["system", "add", blank.to_str().unwrap(), "--name", "b"]);
    assert!(error.contains("is blank"), "{error}");
    assert!(env.ok(&["system", "list"]).is_empty());

    let data = write_disk(env.path(), "data.dsk");
    let output = env.run(&["system", "add", data.to_str().unwrap(), "--name", "data"]);
    assert!(output.status.success());
    assert!(
        stderr(&output).contains("no boot blocks"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn rom_with_trailing_data_works_and_says_so() {
    let env = Env::new();
    let rom = write_rom(env.path(), "vMac.ROM", PLUS_ROM_CHECKSUM, 128 * 1024);
    let mut bytes = fs::read(&rom).unwrap();
    bytes.extend_from_slice(&[0xAA; 7504]);
    fs::write(&rom, &bytes).unwrap();

    let output = env.run(&["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ignoring 7504 extra bytes"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read(&rom).unwrap(), bytes, "the source is left alone");

    let disk = write_disk(env.path(), "sys.dsk");
    env.ok(&["system", "add", disk.to_str().unwrap(), "--name", "sys"]);
    env.ok(&[
        "create", "m", "--model", "plus", "--rom", "plus-v3", "--system", "sys",
    ]);
    assert!(
        env.ok(&["rom", "list"])
            .contains("plus-v3  128K  512ke/plus")
    );
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

/// The native folders, reached through a path with a space and non-ASCII
/// characters, on a system where nothing sets `HOME` or `XDG_*`.
#[cfg(windows)]
#[test]
fn windows_uses_its_native_folders_without_home() {
    let env = Env::new();
    let profile = env.path().join("Zoë Q 日本");
    let (roaming, local) = (profile.join("Roaming"), profile.join("Local"));
    let rom = write_rom(env.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
    let output = Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"])
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env("APPDATA", &roaming)
        .env("LOCALAPPDATA", &local)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(local.join("macman/roms/plus-v3.rom").is_file());
    assert!(!roaming.join("macman/roms").exists());
}
