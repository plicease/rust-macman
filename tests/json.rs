//! `--json`, through the real binary: each report parses, has the fields
//! docs/JSON.md promises, and is all there is on stdout, even when it fails.

mod common;

use common::{PLUS_ROM_CHECKSUM, write_disk, write_partitioned_disk, write_rom};
use serde_json::Value;
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
            .env_remove("NO_COLOR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }

    /// Run, and parse stdout as the one JSON document it must be.
    fn json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let output = self.run(args);
        let text = String::from_utf8(output.stdout).unwrap();
        let value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{args:?}: {error}: {text:?}"));
        assert!(!text.contains('\x1b'), "{text:?}");
        (output.status.code(), value)
    }

    fn report(&self, args: &[&str]) -> Value {
        let (code, value) = self.json(args);
        assert_eq!(code, Some(0), "{args:?}: {value}");
        value
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
        ]);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_flag_goes_before_or_after_the_command() {
    let env = Env::new();
    env.register();
    let expected = env.report(&["--json", "rom", "list"]);
    assert_eq!(env.report(&["rom", "--json", "list"]), expected);
    assert_eq!(env.report(&["rom", "list", "--json"]), expected);
    assert_eq!(
        env.report(&["list", "--json"]),
        env.report(&["--json", "list"])
    );
}

#[test]
fn empty_lists_are_empty_arrays() {
    let env = Env::new();
    assert_eq!(
        env.report(&["--json", "list"]),
        serde_json::json!({"machines": []})
    );
    assert_eq!(
        env.report(&["--json", "list", "--builds"]),
        serde_json::json!({"builds": []})
    );
    assert_eq!(
        env.report(&["--json", "rom", "list"]),
        serde_json::json!({"roms": []})
    );
    assert_eq!(
        env.report(&["--json", "system", "list"]),
        serde_json::json!({"systems": []})
    );
}

#[test]
fn machines_are_listed_with_typed_fields() {
    let env = Env::new();
    env.register();
    env.create("a");
    env.create("b");
    env.ok(&["use", "b"]);
    fs::write(
        env.path().join("data/macman/machines/a/machine.toml"),
        "not toml",
    )
    .unwrap();

    let machines = env.report(&["--json", "list"])["machines"].clone();
    let machines = machines.as_array().unwrap();
    assert_eq!(machines.len(), 2);
    assert_eq!(machines[0]["name"], "a");
    assert_eq!(machines[0]["current"], false);
    assert!(machines[0]["error"].is_string(), "{}", machines[0]);

    let b = &machines[1];
    assert_eq!(b["name"], "b");
    assert_eq!(b["current"], true);
    assert_eq!(b["backend"], "minivmac");
    assert_eq!(b["model"], "plus");
    assert_eq!(b["memory"], "4M");
    assert_eq!(b["display"]["width"], 512);
    assert_eq!(b["display"]["depth"], 1);
    assert_eq!(b["display"]["scale"], 2);
    assert_eq!(b["display"]["fullscreen"], false);
    assert_eq!(b["system_version"], "6.0.8");
    assert!(b.get("error").is_none());
}

#[test]
fn roms_and_systems_are_listed() {
    let env = Env::new();
    env.register();
    let roms = env.report(&["--json", "rom", "list"]);
    let rom = &roms["roms"][0];
    assert_eq!(rom["name"], "plus-v3");
    assert_eq!(rom["size"], 128 * 1024);
    assert_eq!(rom["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(rom["models"], serde_json::json!(["512ke", "plus"]));
    assert_eq!(rom["backend"], "minivmac");

    let systems = env.report(&["--json", "system", "list"]);
    let system = &systems["systems"][0];
    assert_eq!(system["name"], "system-6.0.8");
    assert_eq!(system["version"], "6.0.8");
    assert_eq!(system["size"], 800 * 1024);
    assert_eq!(system["format"], "hfs");
}

#[test]
fn a_build_without_a_manifest_says_so() {
    let env = Env::new();
    let key = "0123456789abcdef0123456789abcdef";
    fs::create_dir_all(env.path().join("cache/macman/builds").join(key)).unwrap();
    let builds = env.report(&["--json", "list", "--builds"]);
    assert_eq!(builds["builds"][0]["key"], key);
    assert_eq!(builds["builds"][0]["error"], "no valid manifest");
}

#[test]
fn disk_info_describes_the_layout() {
    let env = Env::new();
    let raw = write_disk(env.path(), "raw.dsk");
    let info = env.report(&["--json", "disk", "info", raw.to_str().unwrap()]);
    assert_eq!(info["file"], raw.to_str().unwrap());
    assert_eq!(info["size"], 800 * 1024);
    assert_eq!(info["layout"], "volume");
    assert_eq!(info["volume"], "hfs");

    let (partitioned, _) = write_partitioned_disk(env.path(), "whole.img");
    let info = env.report(&["--json", "disk", "info", partitioned.to_str().unwrap()]);
    assert_eq!(info["layout"], "partitioned");
    let partitions = info["partitions"].as_array().unwrap();
    assert!(!partitions.is_empty());
    for partition in partitions {
        assert!(partition["index"].is_u64(), "{partition}");
        assert!(partition["type"].is_string(), "{partition}");
        assert!(partition["offset"].is_u64(), "{partition}");
        assert!(partition["length"].is_u64(), "{partition}");
        assert!(partition["in_bounds"].is_boolean(), "{partition}");
    }
    assert!(partitions.iter().any(|p| p["volume"] == "hfs"), "{info}");
}

#[test]
fn config_show_and_current_report_the_machine() {
    let env = Env::new();
    env.register();
    env.create("m");
    env.ok(&["use", "m"]);
    assert_eq!(
        env.report(&["--json", "current"]),
        serde_json::json!({"current": "m"})
    );
    let config = env.report(&["--json", "config", "show"]);
    let machine = &config["machine"];
    assert_eq!(machine["name"], "m");
    assert_eq!(machine["emulator"]["backend"], "minivmac");
    assert_eq!(machine["hardware"]["model"], "plus");
    assert_eq!(machine["disks"][0]["boot"], true);
}

#[test]
fn doctor_reports_each_check_and_keeps_its_exit_code() {
    let env = Env::new();
    env.register();
    env.create("m");
    let (code, report) = env.json(&["--json", "doctor"]);
    let failed = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["status"] == "error");
    assert_eq!(code, Some(i32::from(failed)), "{report}");
    assert_eq!(report["ok"], !failed);

    fs::remove_file(env.path().join("data/macman/machines/m/system.dsk")).unwrap();
    let (code, report) = env.json(&["--json", "doctor", "--verbose"]);
    assert_eq!(code, Some(1));
    assert_eq!(report["ok"], false);
    let checks = report["checks"].as_array().unwrap();
    for check in checks {
        let status = check["status"].as_str().unwrap();
        assert!(["ok", "warning", "error"].contains(&status), "{check}");
        assert!(check["message"].is_string(), "{check}");
        assert!(
            check["fix"].is_null() || check["fix"].is_string(),
            "{check}"
        );
    }
    let broken = checks
        .iter()
        .find(|check| check["message"] == "machine m cannot run")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(broken["status"], "error");
    assert_eq!(broken["fix"], "disk system.dsk is missing");
    assert!(checks.iter().any(|check| check["status"] == "ok"));
}

#[test]
fn a_failure_is_a_json_error_on_stdout_and_a_message_on_stderr() {
    let env = Env::new();
    let output = env.run(&["--json", "current"]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("no current machine"),
        "{value}"
    );
    assert!(stderr(&output).contains("error: no current machine"));
}

#[test]
fn commands_that_change_things_refuse_json_and_do_nothing() {
    let env = Env::new();
    env.register();
    let (code, value) = env.json(&[
        "--json",
        "create",
        "m",
        "--model",
        "plus",
        "--rom",
        "plus-v3",
        "--system",
        "system-6.0.8",
    ]);
    assert_eq!(code, Some(1));
    assert!(
        value["error"].as_str().unwrap().contains("macman create"),
        "{value}"
    );
    assert!(!env.path().join("data/macman/machines/m").exists());

    let (code, value) = env.json(&["rom", "add", "x.rom", "--name", "x", "--json"]);
    assert_eq!(code, Some(1));
    assert!(value["error"].as_str().unwrap().contains("macman rom add"));
}

#[test]
fn bad_arguments_are_a_json_error_too() {
    let env = Env::new();
    let (code, value) = env.json(&["--json", "no-such-command"]);
    assert_eq!(code, Some(2));
    assert!(value["error"].is_string(), "{value}");
}

#[test]
fn without_the_flag_nothing_changes() {
    let env = Env::new();
    env.register();
    let output = env.run(&["rom", "list"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("plus-v3  128K  512ke/plus"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err());
}
