//! Drives the real binary through commands that never start an emulator.

mod common;

use common::{
    II_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_disk_with_volumes,
    write_partitioned_disk, write_rom,
};
use macman::backend::minivmac::SourcePin;
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
    let roms = env.ok(&["rom", "list"]);
    assert!(roms.starts_with("plus-v3  128K  used "), "{roms}");
    assert!(roms.contains("  512ke/plus"), "{roms}");
    let systems = env.ok(&["system", "list"]);
    assert!(
        systems.starts_with("system-6.0.8  6.0.8  800K  used "),
        "{systems}"
    );
    assert!(systems.contains("KiB  raw HFS"), "{systems}");
}

/// macOS shares this layout with Linux, on purpose: see docs/PLATFORMS.md.
#[cfg(unix)]
#[test]
fn with_only_home_set_files_go_to_the_linux_style_directories() {
    let env = Env::new();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_macman"))
            .args(args)
            .env("HOME", env.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CACHE_HOME")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    };
    let rom = write_rom(env.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
    let disk = write_disk(env.path(), "sys.dsk");
    run(&["rom", "add", rom.to_str().unwrap(), "--name", "plus"]);
    run(&["system", "add", disk.to_str().unwrap(), "--name", "sys"]);
    run(&[
        "create", "m", "--model", "plus", "--rom", "plus", "--system", "sys",
    ]);
    run(&["use", "m"]);

    assert!(env.path().join(".config/macman/config.toml").is_file());
    let data = env.path().join(".local/share/macman");
    assert!(data.join("roms/plus.rom").is_file());
    assert!(data.join("machines/m/system.dsk").is_file());
    assert!(!env.path().join("Library").exists());
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
    let revision = format!("revision = \"{}\"", SourcePin::default().revision);
    for expected in [
        "schema_version = 1",
        "backend = \"minivmac\"",
        revision.as_str(),
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
    let line = list.lines().find(|l| l.starts_with("* wombat-6")).unwrap();
    assert!(
        line.starts_with("* wombat-6  plus  4M  512x342x1  disk 800 KiB  used "),
        "{list}"
    );
    assert!(line.ends_with("  persistent  system 6.0.8"), "{list}");
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
fn other_backends_refuse_a_blank_disk_and_a_cd() {
    let env = Env::new();
    env.register();
    env.create("plain");
    let args = [
        "create",
        "new",
        "--model",
        "plus",
        "--rom",
        "plus-v3",
        "--disk-size",
        "1M",
    ];
    assert!(env.fails(&args).contains("--disk-size needs a backend"));
    assert!(!env.path().join("data/macman/machines/new").exists());

    let cd = write_disk(env.path(), "cd.iso");
    let error = env.fails(&["run", "plain", "--cdrom", cd.to_str().unwrap()]);
    assert!(
        error.contains("--cdrom isn't available for minivmac"),
        "{error}"
    );
}

#[test]
fn system_remove_needs_confirmation_and_keeps_machines() {
    let env = Env::new();
    env.register();
    env.create("keeper");
    let image = env.path().join("data/macman/systems/system-6.0.8.dsk");

    let error = env.fails(&["system", "remove", "system-6.0.8"]);
    assert!(error.contains("--yes"), "{error}");
    assert!(image.exists());

    let removed = env.ok(&["system", "remove", "system-6.0.8", "--yes"]);
    assert!(removed.contains("Removed system image system-6.0.8"));
    assert!(!image.exists());
    assert!(!image.with_extension("toml").exists());
    assert!(env.ok(&["system", "list"]).is_empty());
    assert!(env.machine_disk("keeper").exists());

    let error = env.fails(&["system", "remove", "system-6.0.8"]);
    assert!(error.contains("no system image named"), "{error}");
    let error = env.fails(&["system", "remove", "../escape", "--yes"]);
    assert!(error.contains("invalid system name"), "{error}");
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
    let roms = env.ok(&["rom", "list"]);
    assert!(roms.starts_with("plus-v3  128K  used "), "{roms}");
    assert!(roms.contains("  512ke/plus"), "{roms}");
}

#[test]
fn disk_extract_matches_dd_byte_for_byte() {
    let env = Env::new();
    let (image, _) = write_partitioned_disk(env.path(), "system-zip.image");
    let before = fs::read(&image).unwrap();
    let dest = env.path().join("hfs.img");

    let out = env.ok(&[
        "disk",
        "extract",
        image.to_str().unwrap(),
        dest.to_str().unwrap(),
    ]);
    assert!(out.contains("Extracted partition 3 (Apple_HFS)"), "{out}");

    let by_hand = env.path().join("dd.img");
    let dd = Command::new("dd")
        .arg(format!("if={}", image.display()))
        .arg(format!("of={}", by_hand.display()))
        .args(["bs=512", "skip=96", "count=1600"])
        .output()
        .unwrap();
    assert!(dd.status.success());
    assert_eq!(fs::read(&dest).unwrap(), fs::read(by_hand).unwrap());
    assert_eq!(
        fs::read(&image).unwrap(),
        before,
        "the source is left alone"
    );

    // The result is itself a usable image.
    assert!(
        env.ok(&["disk", "info", dest.to_str().unwrap()])
            .contains("raw HFS volume")
    );
}

#[test]
fn disk_extract_requires_a_choice_among_several_volumes() {
    let env = Env::new();
    let (image, volumes) = write_disk_with_volumes(env.path(), "two.image", &[96, 1696]);
    let source = image.to_str().unwrap();
    let dest = env.path().join("out.img");
    let dest_str = dest.to_str().unwrap();

    let error = env.fails(&["disk", "extract", source, dest_str]);
    assert!(error.contains("--partition"), "{error}");
    assert!(
        error.contains("3: Apple_HFS") && error.contains("4: Apple_HFS"),
        "{error}"
    );
    assert!(!dest.exists());

    env.ok(&["disk", "extract", source, dest_str, "--partition", "4"]);
    assert_eq!(fs::read(&dest).unwrap(), volumes[1]);

    let driver_only = env.fails(&["disk", "extract", source, "x.img", "--partition", "2"]);
    assert!(driver_only.contains("holds no MFS or HFS"), "{driver_only}");
}

#[test]
fn disk_extract_raw_is_opt_in() {
    let env = Env::new();
    let (image, _) = write_partitioned_disk(env.path(), "zip.image");
    let source = image.to_str().unwrap();
    let dest = env.path().join("driver.bin");
    let dest_str = dest.to_str().unwrap();

    let out = env.ok(&[
        "disk",
        "extract",
        source,
        dest_str,
        "--partition",
        "2",
        "--raw",
    ]);
    assert!(out.contains("--raw"), "{out}");
    let bytes = fs::read(&dest).unwrap();
    assert_eq!((bytes.len(), bytes[0]), (32 * 512, 0xDD));

    // Without a partition, --raw can't choose.
    assert_eq!(
        env.run(&["disk", "extract", source, "x", "--raw"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn disk_extract_unwraps_disk_copy() {
    let env = Env::new();
    let mut image = vec![0u8; 84 + 1600 * 512];
    image[64..68].copy_from_slice(&(1600u32 * 512).to_be_bytes());
    image[82] = 1;
    image[84 + 1024..84 + 1026].copy_from_slice(b"BD");
    let source = env.path().join("floppy.image");
    fs::write(&source, &image).unwrap();
    let dest = env.path().join("floppy.dsk");

    let info = env.ok(&["disk", "info", source.to_str().unwrap()]);
    assert!(info.contains("Disk Copy 4.2"), "{info}");
    assert!(info.contains("819200 bytes at offset 84"), "{info}");
    assert!(info.contains("Volume: HFS"), "{info}");

    let out = env.ok(&[
        "disk",
        "extract",
        source.to_str().unwrap(),
        dest.to_str().unwrap(),
    ]);
    assert!(out.contains("Disk Copy 4.2 image"), "{out}");
    assert_eq!(fs::read(&dest).unwrap(), image[84..]);
}

#[test]
fn disk_extract_never_overwrites() {
    let env = Env::new();
    let (image, _) = write_partitioned_disk(env.path(), "zip.image");
    let dest = env.path().join("precious.img");
    fs::write(&dest, b"keep me").unwrap();

    let error = env.fails(&[
        "disk",
        "extract",
        image.to_str().unwrap(),
        dest.to_str().unwrap(),
    ]);
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(fs::read(&dest).unwrap(), b"keep me");

    // Extracting onto the source is the same case.
    let source = image.to_str().unwrap();
    assert!(
        env.fails(&["disk", "extract", source, source])
            .contains("already exists")
    );
}

#[test]
fn disk_info_shows_the_partition_map() {
    let env = Env::new();
    let (image, _) = write_partitioned_disk(env.path(), "zip.image");
    let info = env.ok(&["disk", "info", image.to_str().unwrap()]);

    assert!(info.contains(": 1097728 bytes"), "{info}");
    assert!(info.contains("partitioned disk"), "{info}");
    let row = |needle: &str| {
        info.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle}: {info}"))
            .split_whitespace()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        row("Apple_partition_map"),
        ["1", "Apple_partition_map", "512", "32256", "-"]
    );
    assert_eq!(
        row("Apple_Driver"),
        ["2", "Apple_Driver", "32768", "16384", "-"]
    );
    assert_eq!(
        row("Apple_HFS"),
        ["3", "Apple_HFS", "49152", "819200", "HFS"]
    );
}

#[test]
fn disk_info_reports_every_other_kind_of_image() {
    let env = Env::new();
    let info = |name: &str, bytes: &[u8]| {
        let file = env.path().join(name);
        fs::write(&file, bytes).unwrap();
        env.ok(&["disk", "info", file.to_str().unwrap()])
    };

    let raw = write_disk(env.path(), "raw.dsk");
    assert!(
        env.ok(&["disk", "info", raw.to_str().unwrap()])
            .contains("raw HFS volume")
    );
    assert!(info("blank", &[0u8; 8192]).contains("Format: blank"));
    assert!(info("zip", b"PK\x03\x04.........................").contains("zip archive"));
    assert!(info("odd", &[7u8; 8192]).contains("not recognized"));

    let missing = env.fails(&["disk", "info", "/nonexistent/image"]);
    assert!(missing.contains("cannot read"), "{missing}");
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

/// Output that isn't a terminal, as here, is plain, so it can be read by
/// programs and saved to files.
#[test]
fn doctor_writes_no_color_to_a_pipe() {
    let env = Env::new();
    env.register();
    env.create("m");
    fs::remove_file(env.machine_disk("m")).unwrap();
    let output = env.run(&["doctor"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("[FAIL] machine m cannot run"), "{text}");
    assert!(text.contains("[  ok]"), "{text}");
    assert!(!text.contains('\x1b'), "{text:?}");
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

#[test]
fn the_ephemeral_setting_shows_and_lasts_through_a_clone() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    env.ok(&[
        "create",
        "scratch",
        "--model",
        "plus",
        "--rom",
        "plus-v3",
        "--system",
        "system-6.0.8",
        "--ephemeral",
    ]);
    env.ok(&["clone", "scratch", "scratch-2"]);

    let list = env.ok(&["list"]);
    for (name, mode) in [
        ("scratch ", "ephemeral"),
        ("scratch-2", "ephemeral"),
        ("wombat-6", "persistent"),
    ] {
        let line = list.lines().find(|line| line.contains(name)).unwrap();
        assert!(line.contains(&format!("  {mode}  ")), "{line}");
    }
    assert!(
        env.ok(&["config", "show", "scratch-2"])
            .contains("ephemeral = true\n")
    );
    assert!(
        env.ok(&["config", "show", "wombat-6"])
            .contains("ephemeral = false\n")
    );
    let help = env.ok(&["run", "--help"]);
    assert!(
        help.contains("-e, --ephemeral") && help.contains("--no-ephemeral"),
        "{help}"
    );
}

#[test]
fn config_edit_needs_a_terminal_and_writes_no_escapes() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    let file = env
        .path()
        .join("data/macman/machines/wombat-6/machine.toml");
    let before = std::fs::read(&file).unwrap();

    let output = env.run(&["config", "edit", "wombat-6"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = stderr(&output);
    assert!(error.contains("needs an interactive terminal"), "{error}");
    assert!(error.contains("macman config show wombat-6"), "{error}");
    assert!(!error.contains('\x1b'), "{error:?}");

    let error = env.fails(&["--json", "config", "edit", "wombat-6"]);
    assert!(error.contains("config edit"), "{error}");
    let error = env.fails(&["config", "edit", "no-such"]);
    assert!(error.contains("no-such"), "{error}");
    assert!(!error.contains("terminal"), "{error}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn media_are_registered_whole_listed_checked_and_removed() {
    let env = Env::new();
    let (cd, _) = common::write_partitioned_disk(env.path(), "Mac OS 9.0.4.toast");
    let floppy = write_disk(env.path(), "System 6 Install 1.image");
    let original = std::fs::read(&cd).unwrap();
    let out = env.ok(&[
        "media",
        "add",
        cd.to_str().unwrap(),
        "--name",
        "macos-9.0.4",
        "--version",
        "Mac OS 9.0.4",
    ]);
    assert!(out.contains("cdrom, partitioned"), "{out}");
    env.ok(&["media", "add", floppy.to_str().unwrap(), "--name", "sys6-1"]);
    let error = env.fails(&["media", "add", floppy.to_str().unwrap(), "--name", "sys6-1"]);
    assert!(error.contains("already registered"), "{error}");
    let error = env.fails(&["media", "add", "/no/such.iso", "--name", "x"]);
    assert!(error.contains("/no/such.iso"), "{error}");

    let list = env.ok(&["media", "list"]);
    let lines: Vec<_> = list.lines().collect();
    assert_eq!(lines.len(), 2, "{list}");
    assert!(lines[0].starts_with("macos-9.0.4  cdrom"), "{list}");
    assert!(lines[0].ends_with("Mac OS 9.0.4"), "{list}");
    assert!(
        lines[1].contains("floppy") && lines[1].contains("HFS volume"),
        "{list}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "media", "list"])).unwrap();
    let first = &json["media"][0];
    assert_eq!(first["kind"], "cdrom");
    assert_eq!(first["format"], "partitioned");
    assert_eq!(first["source"], "Mac OS 9.0.4.toast");
    assert_eq!(first["size"], original.len());

    let info = env.ok(&["media", "info", "macos-9.0.4"]);
    for expected in ["Kind: cdrom (as found)", "Apple_HFS", "Verified"] {
        assert!(info.contains(expected), "{info}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "media", "info", "macos-9.0.4"])).unwrap();
    assert_eq!(json["verified"], true);
    assert_eq!(json["layout"], "partitioned");

    // The stored copy is read-only; one changed anyway is caught.
    let stored = env.path().join("data/macman/media/macos-9.0.4.img");
    assert_eq!(std::fs::read(&stored).unwrap(), original);
    let mut perms = std::fs::metadata(&stored).unwrap().permissions();
    assert!(perms.readonly());
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&stored, perms).unwrap();
    let mut changed = original.clone();
    changed[600] ^= 1;
    std::fs::write(&stored, changed).unwrap();
    let error = env.fails(&["media", "info", "macos-9.0.4"]);
    assert!(error.contains("doesn't match its digest"), "{error}");
    let output = env.run(&["--json", "media", "info", "macos-9.0.4"]);
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["verified"], false);

    let error = env.fails(&["media", "remove", "macos-9.0.4"]);
    assert!(error.contains("--yes"), "{error}");
    assert!(stored.exists());
    env.ok(&["media", "remove", "macos-9.0.4", "--yes"]);
    assert!(!stored.exists());
    assert_eq!(
        std::fs::read(&cd).unwrap(),
        original,
        "the source is untouched"
    );
    assert_eq!(env.ok(&["media", "list"]).lines().count(), 1);
    let error = env.fails(&[
        "--json",
        "media",
        "add",
        cd.to_str().unwrap(),
        "--name",
        "y",
    ]);
    assert!(error.contains("--json isn't supported"), "{error}");
}

#[test]
fn disk_usage_is_allocated_space_listed_smallest_first() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    env.create("roomy");
    let cd = write_disk(env.path(), "cd.image");
    env.ok(&["media", "add", cd.to_str().unwrap(), "--name", "tools"]);

    // A sparse 64M extra disk adds what it has allocated, not its length, and
    // leaves the boot disk's capacity alone.
    let roomy = env.path().join("data/macman/machines/roomy");
    let data = std::fs::File::create(roomy.join("data.dsk")).unwrap();
    data.set_len(64 << 20).unwrap();
    drop(data);
    let definition = roomy.join("machine.toml");
    let mut text = std::fs::read_to_string(&definition).unwrap();
    text.push_str("\n[[disks]]\npath = \"data.dsk\"\nreadonly = false\nboot = false\n");
    std::fs::write(&definition, text).unwrap();

    let list: serde_json::Value = serde_json::from_str(&env.ok(&["--json", "list"])).unwrap();
    let machine = |name: &str| {
        list["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap()
            .clone()
    };
    let (roomy_json, wombat) = (machine("roomy"), machine("wombat-6"));
    assert_eq!(roomy_json["system_disk_capacity_bytes"], 800 * 1024);
    assert_eq!(wombat["system_disk_capacity_bytes"], 800 * 1024);
    let used = |m: &serde_json::Value| m["host_disk_used_bytes"].as_u64().unwrap();
    assert!(used(&wombat) > 0);
    if cfg!(unix) {
        // Unix file systems here keep holes, so the 64M file adds next to nothing.
        assert!(
            used(&roomy_json) < used(&wombat) + (1 << 20),
            "{roomy_json}"
        );
    }
    assert!(used(&roomy_json) >= used(&wombat));

    let roms: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "rom", "list"])).unwrap();
    assert!(roms["roms"][0]["host_disk_used_bytes"].as_u64().unwrap() >= 128 * 1024);
    let systems: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "system", "list"])).unwrap();
    assert!(systems["systems"][0]["host_disk_used_bytes"].is_u64());
    let media: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "media", "list"])).unwrap();
    assert!(media["media"][0]["host_disk_used_bytes"].is_u64());

    // A machine missing its disk is still listed, as unavailable, not as zero.
    std::fs::remove_file(env.path().join("data/macman/machines/wombat-6/system.dsk")).unwrap();
    let text = env.ok(&["list"]);
    let line = text.lines().find(|l| l.contains("wombat-6")).unwrap();
    let cells: Vec<_> = line
        .split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    assert_eq!(cells[4..6], ["disk -", "used -"], "{text}");

    let du = env.ok(&["du"]);
    let lines: Vec<_> = du.lines().collect();
    assert!(
        lines[0].starts_with("NAME") && lines[0].ends_with("DISK USED"),
        "{du}"
    );
    // The last line is the total, unavailable as one entry is.
    let (total, lines_entries) = lines[1..].split_last().unwrap();
    let cells: Vec<_> = total.split_whitespace().collect();
    assert_eq!(cells, ["total", "-", "-"], "{du}");
    let names: Vec<_> = lines_entries
        .iter()
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(names.len(), 5, "{du}");
    for expected in [
        "plus-v3 rom",
        "system-6.0.8 system",
        "tools media",
        "roomy machine",
    ] {
        assert!(names.contains(&expected.to_string()), "{du}");
    }
    assert_eq!(
        names.last().unwrap(),
        "wombat-6 machine",
        "unavailable last: {du}"
    );
    assert!(lines_entries.last().unwrap().ends_with(" -"), "{du}");
    // Every row's value ends in the same column.
    let width = lines[0].len();
    assert!(lines.iter().all(|l| l.len() == width), "{du}");

    let json: serde_json::Value = serde_json::from_str(&env.ok(&["--json", "du"])).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 5);
    let sizes: Vec<_> = entries
        .iter()
        .filter_map(|e| e["host_disk_used_bytes"].as_u64())
        .collect();
    assert_eq!(sizes.len(), 4);
    assert!(sizes.windows(2).all(|pair| pair[0] <= pair[1]), "{json}");
    assert_eq!(entries[4]["name"], "wombat-6");
    assert_eq!(entries[4]["type"], "machine");
    assert!(entries[4]["host_disk_used_bytes"].is_null());
    let order: Vec<_> = entries.iter().map(|e| e["name"].clone()).collect();
    let text_order: Vec<_> = lines_entries
        .iter()
        .map(|l| serde_json::Value::from(l.split_whitespace().next().unwrap()))
        .collect();
    assert_eq!(order, text_order, "the same order as the text");
    assert!(json["total_host_disk_used_bytes"].is_null(), "{json}");

    // With nothing unavailable, the total is the sum of the entries.
    env.ok(&["remove", "wombat-6", "--delete-disks"]);
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["--json", "du"])).unwrap();
    let sum: u64 = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["host_disk_used_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(json["total_host_disk_used_bytes"], sum, "{json}");
    let du = env.ok(&["du"]);
    let total = du.lines().last().unwrap();
    assert!(total.starts_with("total") && total.contains(" - "), "{du}");
    assert!(
        total.ends_with(&macman::usage::format_bytes(sum)) && !total.ends_with(" -"),
        "{du}"
    );
}

#[test]
fn checkpoints_count_toward_a_machine_but_not_its_capacity() {
    use serde_json::Value;
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    let json = |args: &[&str]| -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&env.ok(&all)).unwrap()
    };
    let listed = || json(&["list"])["machines"][0].clone();
    let du_used = || {
        json(&["du"])["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "machine")
            .unwrap()["host_disk_used_bytes"]
            .clone()
    };
    let checkpoints = || json(&["checkpoint", "list", "wombat-6"])["checkpoints"].clone();
    let alone = listed()["host_disk_used_bytes"].as_u64().unwrap();
    assert_eq!(du_used(), alone);
    assert_eq!(checkpoints(), serde_json::json!([]));

    env.ok(&["checkpoint", "create", "wombat-6", "--name", "first"]);
    env.ok(&["checkpoint", "create", "wombat-6", "--name", "second"]);
    let both = checkpoints();
    let entries = both.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["name"], "first");
    assert_eq!(entries[0]["disk"], "system.dsk");
    assert_eq!(entries[0]["size"], 800 * 1024);
    assert_eq!(entries[0]["sha256"].as_str().unwrap().len(), 64);
    let own: u64 = entries
        .iter()
        .map(|e| e["host_disk_used_bytes"].as_u64().unwrap())
        .sum();
    assert!(own > 0);
    let machine = listed();
    assert_eq!(machine["host_disk_used_bytes"], alone + own);
    assert_eq!(machine["system_disk_capacity_bytes"], 800 * 1024);
    assert_eq!(du_used(), alone + own);
    let text = env.ok(&["checkpoint", "list", "wombat-6"]);
    assert!(text.lines().next().unwrap().starts_with("first"), "{text}");
    assert!(text.contains("used "), "{text}");

    // A staged image, never published, isn't a checkpoint and isn't counted.
    let dir = env.path().join("data/macman/machines/wombat-6/checkpoints");
    std::fs::write(dir.join("partial.dsk"), vec![7u8; 256 * 1024]).unwrap();
    assert_eq!(listed()["host_disk_used_bytes"], alone + own);
    assert_eq!(checkpoints().as_array().unwrap().len(), 2);
    std::fs::remove_file(dir.join("partial.dsk")).unwrap();

    let second = entries[1]["host_disk_used_bytes"].as_u64().unwrap();
    env.ok(&[
        "checkpoint",
        "delete",
        "wombat-6",
        "--name",
        "first",
        "--yes",
    ]);
    assert_eq!(listed()["host_disk_used_bytes"], alone + second);
    assert_eq!(du_used(), alone + second);

    // Rolling back replaces the disk in place: the total is the same pieces.
    env.ok(&["rollback", "wombat-6", "--checkpoint", "second", "--yes"]);
    let after = listed()["host_disk_used_bytes"].as_u64().unwrap();
    assert!(after > second, "{after}");
    assert_eq!(du_used(), after);
    assert_eq!(listed()["system_disk_capacity_bytes"], 800 * 1024);

    // A failed operation leaves nothing behind to count.
    let error = env.fails(&["checkpoint", "create", "wombat-6", "--name", "second"]);
    assert!(error.contains("already has a checkpoint"), "{error}");
    assert_eq!(listed()["host_disk_used_bytes"], after);

    // A checkpoint whose image is gone leaves the total unknown, not short.
    let image = dir.join("second.dsk");
    let mut permissions = std::fs::metadata(&image).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&image, permissions).unwrap();
    std::fs::remove_file(&image).unwrap();
    assert!(listed()["host_disk_used_bytes"].is_null());
    assert!(du_used().is_null());
    let du = env.ok(&["du"]);
    let line = du.lines().find(|l| l.contains("wombat-6")).unwrap();
    assert!(line.ends_with(" -"), "{du}");
    assert!(checkpoints()[0]["host_disk_used_bytes"].is_null());

    let error = env.fails(&["--json", "checkpoint", "create", "wombat-6"]);
    assert!(error.contains("--json isn't supported"), "{error}");
}

#[cfg(unix)]
#[test]
fn du_counts_a_file_shared_by_two_entries_once_in_its_total() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    // The machine's disk replaced by a link to the system it was made from.
    let system = env.path().join("data/macman/systems/system-6.0.8.dsk");
    let disk = env.machine_disk("wombat-6");
    std::fs::remove_file(&disk).unwrap();
    std::fs::hard_link(&system, &disk).unwrap();

    let json: serde_json::Value = serde_json::from_str(&env.ok(&["--json", "du"])).unwrap();
    let used = |name: &str| {
        json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap()["host_disk_used_bytes"]
            .as_u64()
            .unwrap()
    };
    let (machine, system, rom) = (used("wombat-6"), used("system-6.0.8"), used("plus-v3"));
    assert_eq!(machine, system, "each entry still counts the file: {json}");
    assert!(system > 0);
    assert_eq!(
        json["total_host_disk_used_bytes"],
        rom + system,
        "the shared file once: {json}"
    );
    let du = env.ok(&["du"]);
    let total = du.lines().last().unwrap();
    assert!(
        total.ends_with(&macman::usage::format_bytes(rom + system)),
        "{du}"
    );
}

#[cfg(unix)]
#[test]
fn a_checkpoint_hard_linked_to_the_disk_counts_once() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    let used = || -> serde_json::Value {
        let list: serde_json::Value = serde_json::from_str(&env.ok(&["--json", "list"])).unwrap();
        list["machines"][0]["host_disk_used_bytes"].clone()
    };
    let alone = used().as_u64().unwrap();
    env.ok(&["checkpoint", "create", "wombat-6", "--name", "kept"]);
    // Replaced by a link to the live disk, the checkpoint adds nothing.
    let image = env
        .path()
        .join("data/macman/machines/wombat-6/checkpoints/kept.dsk");
    std::fs::remove_file(&image).unwrap();
    std::fs::hard_link(env.machine_disk("wombat-6"), &image).unwrap();
    assert_eq!(used(), alone);
}

#[test]
fn rollback_restores_a_missing_boot_disk_but_wont_pretend_to_save_it() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    env.ok(&["checkpoint", "create", "wombat-6", "--name", "clean"]);
    let disk = env.machine_disk("wombat-6");
    let original = std::fs::read(&disk).unwrap();
    std::fs::remove_file(&disk).unwrap();
    let names = || -> Vec<serde_json::Value> {
        let list: serde_json::Value =
            serde_json::from_str(&env.ok(&["--json", "checkpoint", "list", "wombat-6"])).unwrap();
        list["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].clone())
            .collect()
    };

    // There's nothing to save, so --save is refused, not skipped.
    let error = env.fails(&[
        "rollback",
        "wombat-6",
        "--checkpoint",
        "clean",
        "--save",
        "before",
        "--yes",
    ]);
    assert!(error.contains("missing its boot disk"), "{error}");
    assert!(error.contains("without --save"), "{error}");
    assert!(error.contains("nothing was changed"), "{error}");
    assert!(!disk.exists());
    assert_eq!(names(), ["clean"]);

    // Without it, the rollback restores the disk, and says that's what it did.
    let text = env.ok(&["rollback", "wombat-6", "--checkpoint", "clean", "--yes"]);
    assert!(
        text.contains("Restored the missing boot disk of wombat-6 from checkpoint clean"),
        "{text}"
    );
    assert!(std::fs::read(&disk).unwrap() == original, "not restored");
    assert_eq!(names(), ["clean"]);

    // With the disk back, --save works as usual.
    let text = env.ok(&[
        "rollback",
        "wombat-6",
        "--checkpoint",
        "clean",
        "--save",
        "before",
        "--yes",
    ]);
    assert!(
        text.contains("Saved the boot disk of wombat-6 as checkpoint before"),
        "{text}"
    );
    assert!(
        text.contains("Rolled wombat-6 back to checkpoint clean"),
        "{text}"
    );
    let mut all = names();
    all.sort_by_key(ToString::to_string);
    assert_eq!(all, ["before", "clean"]);
}

#[test]
fn config_set_changes_settings_from_a_script() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    let definition = env
        .path()
        .join("data/macman/machines/wombat-6/machine.toml");
    let disk = std::fs::read(env.machine_disk("wombat-6")).unwrap();

    let output = env.run(&[
        "config",
        "set",
        "wombat-6",
        "hardware.memory",
        "1M",
        "display.scale",
        "3",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(
        text,
        "hardware.memory: 4M -> 1M\ndisplay.scale: 2 -> 3\nSaved machine wombat-6\n"
    );
    // What it means beyond the file goes to stderr, apart from the result.
    assert!(
        stderr(&output).contains("note: Mini vMac builds"),
        "{}",
        stderr(&output)
    );
    let shown = env.ok(&["config", "show", "wombat-6"]);
    assert!(
        shown.contains("memory = \"1M\"") && shown.contains("scale = 3"),
        "{shown}"
    );
    assert_eq!(std::fs::read(env.machine_disk("wombat-6")).unwrap(), disk);

    // A no-op succeeds, and writes nothing.
    let before = std::fs::read_to_string(&definition).unwrap();
    let text = env.ok(&["config", "set", "wombat-6", "display.scale", "3"]);
    assert_eq!(text, "No change to machine wombat-6\n");
    assert_eq!(std::fs::read_to_string(&definition).unwrap(), before);

    // Refusals exit 1, explain, and leave the file as it was, even with a
    // valid pair before the bad one.
    for (args, says) in [
        (
            &["wombat-6", "hardware.memory", "2G"][..],
            "hardware.memory can be one of",
        ),
        (
            &["wombat-6", "display.scale", "2", "display.depth", "8"],
            "monochrome",
        ),
        (&["wombat-6", "name", "other"], "macman clone"),
        (&["wombat-6", "colour", "red"], "unknown key \"colour\""),
        (&["wombat-6", "hardware.memory"], "has no value"),
        (&["missing", "ephemeral", "true"], "missing"),
    ] {
        let mut all = vec!["config", "set"];
        all.extend_from_slice(args);
        let error = env.fails(&all);
        assert!(error.contains(says), "{args:?}: {error}");
        assert_eq!(
            std::fs::read_to_string(&definition).unwrap(),
            before,
            "{args:?}"
        );
    }
    let error = env.fails(&["config", "set", "wombat-6", "hardware.memory", "2G"]);
    assert!(error.contains("nothing was changed"), "{error}");

    // Without a name, the current machine.
    env.fails(&["config", "set", "ephemeral", "true"]);
    env.ok(&["use", "wombat-6"]);
    env.ok(&["config", "set", "ephemeral", "true"]);
    assert!(
        std::fs::read_to_string(&definition)
            .unwrap()
            .contains("ephemeral = true")
    );

    // --json gives the changes, typed, and nothing else on stdout.
    let output = env.run(&["--json", "config", "set", "wombat-6", "ephemeral", "false"]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["machine"], "wombat-6");
    assert_eq!(
        json["changes"],
        serde_json::json!([{"key": "ephemeral", "before": true, "after": false}])
    );
    let output = env.run(&["--json", "config", "set", "wombat-6", "display.depth", "8"]);
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json["error"].as_str().unwrap().contains("monochrome"),
        "{json}"
    );
}

#[test]
fn config_keys_lists_what_config_set_takes() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    let text = env.ok(&["config", "keys", "wombat-6"]);
    let lines: Vec<_> = text.lines().collect();
    assert!(
        lines[0].starts_with("ephemeral") && lines[0].ends_with("true, false"),
        "{text}"
    );
    let depth = lines
        .iter()
        .find(|l| l.starts_with("display.depth"))
        .unwrap();
    assert_eq!(
        depth.split_whitespace().collect::<Vec<_>>(),
        ["display.depth", "1", "1"],
        "{text}"
    );
    let height = lines
        .iter()
        .find(|l| l.starts_with("display.height"))
        .unwrap();
    assert!(height.contains("1024, 1080, 1200, or another"), "{text}");
    let memory = lines
        .iter()
        .find(|l| l.starts_with("hardware.memory"))
        .unwrap();
    assert!(
        memory.contains("4M") && memory.ends_with(", or another"),
        "{text}"
    );
    assert!(!text.contains("basilisk2."), "{text}");

    let json: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "config", "keys", "wombat-6"])).unwrap();
    let keys = json["keys"].as_array().unwrap();
    let scale = keys.iter().find(|k| k["key"] == "display.scale").unwrap();
    assert_eq!(scale["type"], "integer");
    assert_eq!(scale["value"], 2);
    assert_eq!(scale["other_values"], false);
    let fixed = json["fixed"].as_array().unwrap();
    assert!(fixed.iter().any(|k| k["key"] == "id"), "{json}");
}

#[test]
fn concurrent_config_sets_each_take_effect() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    // Each sets a different key; none may lose another's change.
    let settings = [
        ["ephemeral", "true"],
        ["display.scale", "3"],
        ["display.fullscreen", "true"],
        ["hardware.memory", "1M"],
    ];
    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = settings
            .iter()
            .map(|[key, value]| {
                let env = &env;
                scope.spawn(move || env.run(&["config", "set", "wombat-6", key, value]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for output in &outputs {
        assert!(output.status.success(), "{}", stderr(output));
    }
    let json: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "config", "keys", "wombat-6"])).unwrap();
    let value = |key: &str| {
        json["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["key"] == key)
            .unwrap()["value"]
            .clone()
    };
    assert_eq!(value("ephemeral"), true);
    assert_eq!(value("display.scale"), 3);
    assert_eq!(value("display.fullscreen"), true);
    assert_eq!(value("hardware.memory"), "1M");
}

#[test]
fn config_set_takes_the_machine_by_option_or_position_but_not_both() {
    let env = Env::new();
    env.register();
    env.create("wombat-6");
    env.create("other");
    let definition = |name: &str| {
        std::fs::read_to_string(
            env.path()
                .join(format!("data/macman/machines/{name}/machine.toml")),
        )
        .unwrap()
    };

    let text = env.ok(&[
        "config",
        "set",
        "--machine",
        "wombat-6",
        "display.scale",
        "3",
        "ephemeral",
        "true",
    ]);
    assert!(text.ends_with("Saved machine wombat-6\n"), "{text}");
    assert!(definition("wombat-6").contains("scale = 3"));
    assert!(
        !definition("other").contains("scale = 3"),
        "only the one named"
    );
    // The option may come after the pairs, too.
    env.ok(&["config", "set", "display.scale", "1", "--machine=other"]);
    assert!(definition("other").contains("scale = 1"));

    let wombat = definition("wombat-6");
    let other = definition("other");
    for (args, says) in [
        // A value left out is said, however the machine is named.
        (
            &["--machine", "wombat-6", "hardware.memory"][..],
            "\"hardware.memory\" has no value",
        ),
        (
            &[
                "--machine",
                "wombat-6",
                "ephemeral",
                "false",
                "display.scale",
            ],
            "\"display.scale\" has no value",
        ),
        (
            &["wombat-6", "ephemeral", "false", "display.scale"],
            "\"display.scale\" has no value",
        ),
        (
            &["ephemeral", "false", "display.scale"],
            "\"display.scale\" has no value",
        ),
        // Both at once is refused, whichever machine each names.
        (
            &["--machine", "wombat-6", "wombat-6", "ephemeral", "false"],
            "given twice",
        ),
        (
            &["--machine", "other", "wombat-6", "ephemeral", "false"],
            "given twice",
        ),
        (
            &["--machine", "wombat-6", "colour", "ephemeral", "false"],
            "isn't a key",
        ),
        (&["--machine", "nobody", "ephemeral", "false"], "nobody"),
    ] {
        let mut all = vec!["config", "set"];
        all.extend_from_slice(args);
        let error = env.fails(&all);
        assert!(error.contains(says), "{args:?}: {error}");
        assert_eq!(definition("wombat-6"), wombat, "{args:?}");
        assert_eq!(definition("other"), other, "{args:?}");
    }
    let help = env.ok(&["config", "set", "--help"]);
    assert!(
        help.contains("[--machine MACHINE | MACHINE] KEY VALUE"),
        "{help}"
    );
}
