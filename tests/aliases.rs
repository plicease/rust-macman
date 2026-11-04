//! `macman alias` and `unalias`, through the real binary: other names for a
//! machine, accepted wherever its name is.

mod common;

use common::{PLUS_ROM_CHECKSUM, write_disk, write_rom};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Host {
    root: TempDir,
}

impl Host {
    /// A host with Plus machines `wombat-6` and `wombat-7`.
    fn new() -> Self {
        let host = Self::empty();
        let rom = write_rom(host.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        let disk = write_disk(host.path(), "sys6.dsk");
        host.ok(&["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"]);
        host.ok(&["system", "add", disk.to_str().unwrap(), "--name", "sys"]);
        host.create("wombat-6");
        host.create("wombat-7");
        host
    }

    fn empty() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn create(&self, name: &str) {
        self.ok(&[
            "create", name, "--model", "plus", "--rom", "plus-v3", "--system", "sys",
        ]);
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
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        String::from_utf8(output.stderr).unwrap()
    }

    fn machine(&self, name: &str) -> PathBuf {
        self.path().join("data/macman/machines").join(name)
    }

    fn definition(&self, name: &str) -> String {
        fs::read_to_string(self.machine(name).join("machine.toml")).unwrap()
    }

    /// The `list` line for `name`, without its padding.
    fn listed(&self, name: &str) -> Vec<String> {
        let list = self.ok(&["list"]);
        let line = list
            .lines()
            .find(|line| line[2..].starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} isn't listed: {list}"));
        let mut cells: Vec<String> = line
            .split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .map(str::to_string)
            .collect();
        if line.starts_with('*') {
            cells[0] = format!("* {}", cells[0].trim_start_matches("* "));
        }
        cells
    }
}

#[test]
fn two_aliases_name_one_machine_everywhere() {
    let host = Host::new();
    assert_eq!(
        host.ok(&["alias", "wombat-6", "wombat"]),
        "wombat is now an alias of machine wombat-6\n"
    );
    // The target may be an alias, but the new one names the machine.
    host.ok(&["alias", "wombat", "plus"]);
    assert_eq!(
        host.ok(&["alias", "wombat-6", "plus"]),
        "plus was already an alias of machine wombat-6\n"
    );
    assert_eq!(
        host.listed("wombat-6").last().unwrap(),
        "aliases plus, wombat"
    );
    assert_eq!(host.listed("wombat-7").last().unwrap(), "aliases -");
    let json: serde_json::Value = serde_json::from_str(&host.ok(&["--json", "list"])).unwrap();
    let machines = json["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 2, "aliases aren't machines of their own");
    assert_eq!(
        machines[0]["aliases"],
        serde_json::json!(["plus", "wombat"])
    );
    assert_eq!(machines[1]["aliases"], serde_json::json!([]));

    // Representative machine arguments, each by an alias.
    assert!(
        host.ok(&["config", "show", "plus"])
            .contains("name = \"wombat-6\"")
    );
    host.ok(&["config", "set", "wombat", "display.scale", "3"]);
    host.ok(&[
        "config",
        "set",
        "--machine",
        "plus",
        "display.fullscreen",
        "true",
    ]);
    let definition = host.definition("wombat-6");
    assert!(definition.contains("scale = 3") && definition.contains("fullscreen = true"));
    assert!(!host.definition("wombat-7").contains("scale = 3"));
    assert!(
        host.ok(&["config", "keys", "wombat"])
            .contains("display.scale")
    );

    host.ok(&["use", "plus"]);
    assert_eq!(
        host.ok(&["current"]),
        "wombat-6\n",
        "the machine, not the alias"
    );
    assert_eq!(host.listed("wombat-6")[0], "* wombat-6");

    host.ok(&["checkpoint", "create", "wombat", "--name", "before"]);
    assert!(
        host.ok(&["checkpoint", "list", "plus"])
            .starts_with("before")
    );
    host.ok(&["rollback", "wombat", "--checkpoint", "before", "--yes"]);
    host.ok(&["checkpoint", "delete", "plus", "--name", "before", "--yes"]);

    // A clone is a machine of its own, and the aliases stay with the original.
    host.ok(&["clone", "wombat", "copy"]);
    assert!(host.definition("copy").contains("scale = 3"));
    assert_eq!(host.listed("copy").last().unwrap(), "aliases -");
    assert_eq!(
        host.listed("wombat-6").last().unwrap(),
        "aliases plus, wombat"
    );

    // An export names the machine, and aliases stay on this host.
    let archive = host.path().join("w.tar.gz");
    host.ok(&["export", "wombat", "plus", "-o", archive.to_str().unwrap()]);
    let other = Host::empty();
    let output = other.ok(&["import", archive.to_str().unwrap()]);
    assert!(output.contains("imported machine wombat-6"), "{output}");
    assert_eq!(other.ok(&["--json", "list"]).matches("\"name\"").count(), 1);
    assert!(other.ok(&["list"]).contains("aliases -"));

    // Persisted: each command above was a process of its own, and the
    // registry is plain TOML.
    let registry = fs::read_to_string(host.path().join("data/macman/aliases.toml")).unwrap();
    assert!(registry.contains("[aliases.wombat]"), "{registry}");
    assert!(registry.contains("machine = \"wombat-6\""), "{registry}");
}

#[test]
fn unalias_removes_only_the_alias() {
    let host = Host::new();
    host.ok(&["alias", "wombat-6", "wombat"]);
    host.ok(&["alias", "wombat-6", "plus"]);
    let definition = host.definition("wombat-6");
    let disk = fs::read(host.machine("wombat-6").join("system.dsk")).unwrap();
    assert_eq!(
        host.ok(&["unalias", "wombat"]),
        "Removed alias wombat of machine wombat-6\n"
    );
    assert_eq!(host.definition("wombat-6"), definition);
    assert_eq!(
        fs::read(host.machine("wombat-6").join("system.dsk")).unwrap(),
        disk
    );
    assert_eq!(host.listed("wombat-6").last().unwrap(), "aliases plus");
    assert!(
        host.fails(&["run", "wombat"])
            .contains("no machine named \"wombat\"")
    );

    let error = host.fails(&["unalias", "wombat-6"]);
    assert!(error.contains("is a machine, not an alias"), "{error}");
    let error = host.fails(&["unalias", "wombat"]);
    assert!(error.contains("no alias named"), "{error}");
    for args in [&["alias", "wombat-6", "x"][..], &["unalias", "plus"]] {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        assert!(host.run(&all).status.code() == Some(1), "{args:?}");
    }
    assert!(
        host.ok(&["list"]).contains("aliases plus"),
        "--json changed nothing"
    );
}

#[test]
fn names_are_one_namespace() {
    let host = Host::new();
    host.ok(&["alias", "wombat-6", "wombat"]);
    let registry = host.path().join("data/macman/aliases.toml");
    let before = fs::read_to_string(&registry).unwrap();
    for (args, says) in [
        (
            &["alias", "wombat-6", "wombat-7"][..],
            "is the name of a machine",
        ),
        (
            &["alias", "wombat-7", "wombat"],
            "already an alias of machine \"wombat-6\"",
        ),
        (
            &["alias", "wombat-6", "wombat-6"],
            "already the machine's own name",
        ),
        (
            &["alias", "wombat", "wombat-6"],
            "already the machine's own name",
        ),
        (&["alias", "wombat-6", ".hidden"], "invalid alias name"),
        (&["alias", "wombat-6", "a b"], "invalid alias name"),
        (&["alias", "wombat-6", "a/b"], "invalid alias name"),
        (&["alias", "nobody", "x"], "no machine named \"nobody\""),
        // A new machine can't take an alias's name.
        (
            &[
                "create", "wombat", "--model", "plus", "--rom", "plus-v3", "--system", "sys",
            ],
            "is an alias of machine \"wombat-6\"",
        ),
        (
            &["clone", "wombat-7", "wombat"],
            "is an alias of machine \"wombat-6\"",
        ),
    ] {
        let error = host.fails(args);
        assert!(error.contains(says), "{args:?}: {error}");
    }
    assert_eq!(fs::read_to_string(&registry).unwrap(), before);
    assert!(!host.machine("wombat").exists());

    // Nor can an imported one, under its own name or by --rename.
    let source = Host::new();
    source.create("wombat");
    let archive = source.path().join("w.tar.gz");
    source.ok(&["export", "wombat", "-o", archive.to_str().unwrap()]);
    let output = host.run(&["import", archive.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("is an alias of machine"), "{text}");
    let archive7 = source.path().join("w7.tar.gz");
    source.ok(&["export", "wombat-7", "-o", archive7.to_str().unwrap()]);
    let output = host.run(&["import", archive7.to_str().unwrap(), "--rename", "wombat"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("is an alias"));
    assert!(!host.machine("wombat").exists());
}

#[test]
fn removing_a_machine_removes_its_aliases() {
    let host = Host::new();
    host.ok(&["alias", "wombat-6", "wombat"]);
    host.ok(&["alias", "wombat-7", "seven"]);
    let output = host.ok(&["remove", "wombat", "--delete-disks"]);
    assert_eq!(
        output,
        "Removed machine wombat-6 and its alias(es) wombat\n"
    );
    assert!(!host.machine("wombat-6").exists());
    assert!(
        host.fails(&["run", "wombat"])
            .contains("no machine named \"wombat\"")
    );
    assert_eq!(host.listed("wombat-7").last().unwrap(), "aliases seven");
    // The name is free again, for a machine or an alias.
    host.create("wombat");
    host.ok(&["remove", "wombat", "--delete-disks"]);
    host.ok(&["alias", "seven", "wombat"]);
}

#[test]
fn an_alias_follows_its_machine_and_says_when_it_is_gone() {
    let host = Host::new();
    host.ok(&["alias", "wombat-6", "wombat"]);
    // Renamed by hand, directory and definition: the id still says which.
    fs::rename(host.machine("wombat-6"), host.machine("renamed")).unwrap();
    let file = host.machine("renamed").join("machine.toml");
    let text = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        text.replace("name = \"wombat-6\"", "name = \"renamed\""),
    )
    .unwrap();
    assert!(
        host.ok(&["config", "show", "wombat"])
            .contains("name = \"renamed\"")
    );
    assert_eq!(host.listed("renamed").last().unwrap(), "aliases wombat");

    // Deleted by hand: the alias says so, and doctor too, until it's removed.
    fs::remove_dir_all(host.machine("renamed")).unwrap();
    let error = host.fails(&["run", "wombat"]);
    assert!(
        error.contains("no longer exists") && error.contains("macman unalias wombat"),
        "{error}"
    );
    let doctor = String::from_utf8(host.run(&["doctor"]).stdout).unwrap();
    assert!(
        doctor.contains("alias wombat names machine wombat-6"),
        "{doctor}"
    );
    host.ok(&["unalias", "wombat"]);
    let doctor = String::from_utf8(host.run(&["doctor"]).stdout).unwrap();
    assert!(!doctor.contains("alias wombat"), "{doctor}");
}

#[test]
fn a_machine_from_before_ids_gets_one_for_its_alias() {
    let host = Host::new();
    let file = host.machine("wombat-6").join("machine.toml");
    let text = fs::read_to_string(&file).unwrap();
    let without: String = text
        .lines()
        .filter(|line| !line.starts_with("id = "))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&file, &without).unwrap();
    host.ok(&["alias", "wombat-6", "wombat"]);
    assert!(host.definition("wombat-6").contains("id = "));
    assert!(host.ok(&["config", "show", "wombat"]).contains("wombat-6"));
}

#[test]
fn concurrent_aliases_are_all_kept() {
    let host = Host::new();
    let names: Vec<String> = (0..8).map(|i| format!("w{i}")).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let host = &host;
                let machine = if i % 2 == 0 { "wombat-6" } else { "wombat-7" };
                scope.spawn(move || host.run(&["alias", machine, name]))
            })
            .collect();
        for handle in handles {
            let output = handle.join().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    });
    assert_eq!(
        host.listed("wombat-6").last().unwrap(),
        "aliases w0, w2, w4, w6"
    );
    assert_eq!(
        host.listed("wombat-7").last().unwrap(),
        "aliases w1, w3, w5, w7"
    );
}

#[test]
fn completion_offers_aliases_where_machines_go_and_only_aliases_to_unalias() {
    let host = Host::new();
    host.ok(&["alias", "wombat-6", "wombat"]);
    let names = |kind: &str| host.ok(&["__complete-names", kind]);
    assert_eq!(names("machines"), "wombat\nwombat-6\nwombat-7\n");
    assert_eq!(names("aliases"), "wombat\n");
    let keys = host.ok(&["__complete-names", "config-set", "wombat"]);
    assert!(keys.contains("display.scale"), "an alias's keys: {keys}");
}
