//! `macman export` and `macman import`, through the real binary, with two
//! separate data directories standing in for two hosts.

mod common;

use clap::Parser;
use common::{II_ROM_CHECKSUM, IIX_ROM_CHECKSUM, PLUS_ROM_CHECKSUM, write_disk, write_rom};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use macman::app::App;
use macman::archive;
use macman::backend::Backends;
use macman::backend::minivmac::{MiniVmac, SourcePin};
use macman::cli::Cli;
use macman::disk::{LockMode, lock_image};
use macman::fsutil::{sha256_bytes, sha256_file};
use macman::machine::Machine;
use macman::paths::Paths;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Host {
    root: TempDir,
}

impl Host {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn data(&self) -> PathBuf {
        self.path().join("data/macman")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_macman"));
        command
            .args(args)
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("cfg"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            stdout(&output),
            stderr(&output)
        );
        stdout(&output)
    }

    /// Exit status 1, with output on stdout and stderr together.
    fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        stdout(&output) + &stderr(&output)
    }

    fn add_plus_rom(&self, name: &str) {
        let rom = write_rom(self.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        self.ok(&["rom", "add", rom.to_str().unwrap(), "--name", name]);
    }

    fn add_ii_rom(&self, name: &str) {
        self.add_large_rom(name, II_ROM_CHECKSUM);
    }

    fn add_large_rom(&self, name: &str, checksum: u32) {
        let rom = write_rom(self.path(), "large.bin", checksum, 256 * 1024);
        self.ok(&["rom", "add", rom.to_str().unwrap(), "--name", name]);
    }

    fn add_system(&self) {
        let disk = write_disk(self.path(), "sys.dsk");
        self.ok(&["system", "add", disk.to_str().unwrap(), "--name", "sys"]);
    }

    fn create(&self, name: &str, model: &str, rom: &str) {
        self.ok(&[
            "create", name, "--model", model, "--rom", rom, "--system", "sys",
        ]);
    }

    /// A host with machines `plus-a` and `plus-b` on ROM `plus-v3`.
    fn with_two_machines() -> Self {
        let host = Self::new();
        host.add_plus_rom("plus-v3");
        host.add_system();
        host.create("plus-a", "plus", "plus-v3");
        host.create("plus-b", "plus", "plus-v3");
        host
    }

    fn machine_dir(&self, name: &str) -> PathBuf {
        self.data().join("machines").join(name)
    }

    fn definition(&self, name: &str) -> String {
        fs::read_to_string(self.machine_dir(name).join("machine.toml")).unwrap()
    }

    fn write_definition(&self, name: &str, text: &str) {
        fs::write(self.machine_dir(name).join("machine.toml"), text).unwrap();
    }

    fn scribble(&self, name: &str, disk: &str, byte: u8) {
        let path = self.machine_dir(name).join(disk);
        let mut bytes = fs::read(&path).unwrap();
        bytes[8192] = byte;
        fs::write(path, bytes).unwrap();
    }

    fn archive(&self, name: &str) -> String {
        self.path().join(name).to_str().unwrap().to_string()
    }

    fn export(&self, names: &[&str], file: &str) -> String {
        let archive = self.archive(file);
        let mut args = vec!["export"];
        args.extend_from_slice(names);
        args.extend(["-o", &archive]);
        self.ok(&args);
        archive
    }

    /// Every file in the data directory with its digest, leaving out the lock
    /// bookkeeping that doesn't hold any user data.
    fn snapshot(&self) -> BTreeMap<String, String> {
        let mut found = BTreeMap::new();
        walk(&self.data(), &self.data(), &mut found, true);
        found
    }

    /// Everything macman keeps, lock files and all, and whether each of its
    /// directories exists.
    fn exact_snapshot(&self) -> BTreeMap<String, String> {
        let mut found = BTreeMap::new();
        for dir in ["data", "cfg", "cache"] {
            let dir = self.path().join(dir);
            if dir.exists() {
                found.insert(dir.display().to_string(), "dir".to_string());
            }
            walk(&dir, &dir, &mut found, false);
        }
        found
    }

    /// `import --dry-run ARGS`, checking that it changed nothing and left
    /// nothing behind in the temporary directory. Returns the exit status,
    /// stdout, and stderr.
    fn preview(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let tmp = self.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        let before = self.exact_snapshot();
        let output = self
            .command(&[&["import", "--dry-run"], args].concat())
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp)
            .output()
            .unwrap();
        assert_eq!(self.exact_snapshot(), before, "{args:?}: {output:?}");
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0, "{args:?}");
        (output.status.code(), stdout(&output), stderr(&output))
    }
}

fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, String>, skip_locks: bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let relative = path.strip_prefix(root).unwrap().display().to_string();
        let relative = relative.replace('\\', "/");
        if skip_locks && (relative == "import.lock" || relative == "locks") {
            continue;
        }
        if path.is_dir() {
            found.insert(relative, "dir".to_string());
            walk(root, &path, found, skip_locks);
        } else {
            found.insert(relative, sha256_file(&path).unwrap());
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn id_of(definition: &str) -> &str {
    definition
        .lines()
        .find_map(|line| line.strip_prefix("id = "))
        .expect("machine has an id")
}

fn digest(path: impl AsRef<Path>) -> String {
    sha256_file(path.as_ref()).unwrap()
}

/// The archive's entries as (name, contents).
fn read_entries(archive: &str) -> Vec<(String, Vec<u8>)> {
    let file = fs::File::open(archive).unwrap();
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    tar.entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let name = String::from_utf8(entry.path_bytes().into_owned()).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

fn raw_header(name: &[u8], size: u64, kind: tar::EntryType) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_entry_type(kind);
    header.set_cksum();
    header
}

/// Write entries exactly as given, which `tar::Builder` wouldn't always allow.
fn write_entries(path: &Path, entries: &[(tar::Header, Vec<u8>)]) {
    let file = fs::File::create(path).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
    for (header, bytes) in entries {
        builder.append(header, bytes.as_slice()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

/// The archive's entries as headers, for tests to tamper with.
fn entries_of(archive: &str) -> Vec<(tar::Header, Vec<u8>)> {
    read_entries(archive)
        .into_iter()
        .map(|(name, bytes)| {
            let header = raw_header(name.as_bytes(), bytes.len() as u64, tar::EntryType::Regular);
            (header, bytes)
        })
        .collect()
}

fn with_manifest(
    archive: &str,
    edit: impl FnOnce(String) -> String,
) -> Vec<(tar::Header, Vec<u8>)> {
    let mut entries = entries_of(archive);
    let text = String::from_utf8(entries[0].1.clone()).unwrap();
    let edited = edit(text).into_bytes();
    entries[0] = (
        raw_header(
            b"manifest.toml",
            edited.len() as u64,
            tar::EntryType::Regular,
        ),
        edited,
    );
    entries
}

#[test]
fn round_trip_keeps_private_disk_changes_and_identity() {
    let source = Host::with_two_machines();
    source.scribble("plus-a", "system.dsk", 0x5A);
    let disk = digest(source.machine_dir("plus-a").join("system.dsk"));
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    let output = target.ok(&["import", &archive]);
    assert!(output.contains("imported rom plus-v3"), "{output}");
    assert!(output.contains("imported machine plus-a"), "{output}");
    assert!(
        output.contains("1 imported, 0 unchanged, 0 unresolved"),
        "{output}"
    );

    assert_eq!(
        digest(target.machine_dir("plus-a").join("system.dsk")),
        disk
    );
    assert_eq!(target.definition("plus-a"), source.definition("plus-a"));
    assert!(target.ok(&["list"]).contains("plus-a"));
    assert!(target.ok(&["rom", "list"]).contains("plus-v3"));

    let manifest = String::from_utf8(read_entries(&archive)[0].1.clone()).unwrap();
    assert!(!manifest.contains(source.path().to_str().unwrap()));
    assert!(!target.data().join("staging").exists());
}

#[test]
fn round_trip_keeps_every_disk_of_a_multi_disk_machine() {
    let source = Host::new();
    source.add_large_rom("iix", IIX_ROM_CHECKSUM);
    source.add_system();
    source.create("iix-7", "iix", "iix");
    let dir = source.machine_dir("iix-7");
    write_disk(&dir, "data.dsk");
    write_disk(&dir, "extra.dsk");
    source.scribble("iix-7", "data.dsk", 1);
    source.scribble("iix-7", "extra.dsk", 2);
    source.scribble("iix-7", "system.dsk", 3);
    let definition = source.definition("iix-7")
        + "\n[[disks]]\npath = \"data.dsk\"\n\n[[disks]]\npath = \"extra.dsk\"\nreadonly = true\n";
    source.write_definition("iix-7", &definition);
    let archive = source.export(&["iix-7"], "iix.tar.gz");

    let target = Host::new();
    target.ok(&["import", &archive]);
    for disk in ["system.dsk", "data.dsk", "extra.dsk"] {
        assert_eq!(
            digest(target.machine_dir("iix-7").join(disk)),
            digest(dir.join(disk)),
            "{disk}"
        );
    }
    let parse = |host: &Host| -> Machine { toml::from_str(&host.definition("iix-7")).unwrap() };
    assert_eq!(parse(&target), parse(&source));
    assert_eq!(parse(&target).disks.len(), 3);
    assert!(
        target
            .ok(&["import", &archive])
            .contains("unchanged machine")
    );
}

#[test]
fn only_what_the_machines_need_is_archived_and_shared_data_once() {
    let source = Host::with_two_machines();
    source.add_ii_rom("unused-rom");
    source.scribble("plus-b", "system.dsk", 9);
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");

    let names: Vec<_> = read_entries(&archive)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let rom = digest(source.data().join("roms/plus-v3.rom"));
    assert_eq!(names[0], "manifest.toml");
    // One ROM and two disks, whose contents differ.
    assert_eq!(names.len(), 4, "{names:?}");
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == format!("blobs/{rom}"))
            .count(),
        1
    );
    assert!(names[1..].iter().all(|name| name.starts_with("blobs/")));
}

#[test]
fn importing_again_changes_nothing() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);
    let before = target.snapshot();

    let output = target.ok(&["import", &archive]);
    assert!(output.contains("unchanged rom plus-v3"), "{output}");
    assert!(output.contains("unchanged machine plus-a"), "{output}");
    assert!(output.contains("unchanged machine plus-b"), "{output}");
    assert!(
        output.contains("0 imported, 2 unchanged, 0 unresolved"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[test]
fn a_later_archive_brings_in_only_what_is_missing() {
    let source = Host::with_two_machines();
    let target = Host::new();
    target.ok(&["import", &source.export(&["plus-a"], "one.tar.gz")]);
    let archive = source.export(&["plus-a", "plus-b"], "two.tar.gz");

    let output = target.ok(&["import", &archive]);
    assert!(output.contains("unchanged machine plus-a"), "{output}");
    assert!(output.contains("imported machine plus-b"), "{output}");
    assert!(
        output.contains("1 imported, 1 unchanged, 0 unresolved"),
        "{output}"
    );
}

#[test]
fn an_identical_rom_under_another_name_is_reused() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    target.add_plus_rom("my-plus");
    let output = target.ok(&["import", &archive]);
    assert!(output.contains("reused rom plus-v3 as my-plus"), "{output}");
    assert!(!target.data().join("roms/plus-v3.rom").exists());
    assert!(target.definition("plus-a").contains("rom = \"my-plus\""));

    // The alias doesn't count as a difference afterwards.
    let output = target.ok(&["import", &archive]);
    assert!(output.contains("unchanged machine plus-a"), "{output}");
}

#[test]
fn an_unrelated_machine_with_the_same_name_is_a_conflict() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::with_two_machines();
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(output.contains("conflict machine plus-a"), "{output}");
    assert!(output.contains("--rename"), "{output}");
    assert!(
        output.contains("0 imported, 0 unchanged, 1 unresolved"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[test]
fn a_diverged_copy_is_a_conflict_whichever_side_changed() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    // The disk changed after the import.
    let target = Host::new();
    target.ok(&["import", &archive]);
    target.scribble("plus-a", "system.dsk", 7);
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(output.contains("different disk"), "{output}");
    assert_eq!(target.snapshot(), before);

    // The configuration changed after the import.
    let target = Host::new();
    target.ok(&["import", &archive]);
    let edited = target
        .definition("plus-a")
        .replace("scale = 2", "scale = 3");
    target.write_definition("plus-a", &edited);
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(output.contains("different configuration"), "{output}");
    assert_eq!(target.snapshot(), before);

    // The source moved on.
    let target = Host::new();
    target.ok(&["import", &archive]);
    source.scribble("plus-a", "system.dsk", 8);
    let newer = source.export(&["plus-a"], "newer.tar.gz");
    let before = target.snapshot();
    let output = target.fails(&["import", &newer]);
    assert!(output.contains("conflict machine plus-a"), "{output}");
    assert_eq!(target.snapshot(), before);
}

#[test]
fn a_different_rom_with_the_same_name_is_a_conflict() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    target.add_ii_rom("plus-v3");
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(
        output.contains("different ROM is already named"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[test]
fn rename_makes_an_independent_copy() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    target.ok(&["import", &archive]);
    let original = target.snapshot();
    let output = target.ok(&["import", &archive, "--rename", "copy"]);
    assert!(output.contains("imported machine copy"), "{output}");

    let mut current = target.snapshot();
    current.retain(|path, _| !path.starts_with("machines/copy"));
    assert_eq!(current, original);
    let (copy, first) = (target.definition("copy"), target.definition("plus-a"));
    assert_ne!(id_of(&copy), id_of(&first));
    assert!(copy.contains("name = \"copy\""));
    assert_eq!(
        digest(target.machine_dir("copy").join("system.dsk")),
        digest(target.machine_dir("plus-a").join("system.dsk"))
    );

    // A second import under that name would not be idempotent, so it's refused.
    let output = target.fails(&["import", &archive, "--rename", "copy"]);
    assert!(output.contains("already exists"), "{output}");
}

#[test]
fn rename_refuses_an_archive_of_several_machines() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let target = Host::new();
    let output = target.fails(&["import", &archive, "--rename", "copy"]);
    assert!(output.contains("one machine"), "{output}");
    assert!(!target.machine_dir("copy").exists());
    assert!(!target.data().join("roms/plus-v3.rom").exists());
}

#[test]
fn rename_refuses_a_name_that_is_not_safe() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.fails(&["import", &archive, "--rename", "../escape"]);
    assert!(!target.data().join("escape").exists());
}

#[test]
fn a_conflict_does_not_stop_the_other_machines() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");

    let target = Host::new();
    target.add_plus_rom("plus-v3");
    target.add_system();
    target.create("plus-a", "plus", "plus-v3");
    let local = target.definition("plus-a");

    let output = target.fails(&["import", &archive]);
    assert!(output.contains("conflict machine plus-a"), "{output}");
    assert!(output.contains("imported machine plus-b"), "{output}");
    assert!(
        output.contains("1 imported, 0 unchanged, 1 unresolved"),
        "{output}"
    );
    assert_eq!(target.definition("plus-a"), local);
}

#[test]
fn an_incompatible_machine_fails_with_advice_and_others_continue() {
    let source = Host::with_two_machines();
    let revision = format!("revision = \"{}\"", SourcePin::default().revision);
    let definition = source
        .definition("plus-b")
        .replace(&revision, "revision = \"99.0\"");
    source.write_definition("plus-b", &definition);
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");

    let target = Host::new();
    let output = target.fails(&["import", &archive]);
    assert!(output.contains("failed machine plus-b"), "{output}");
    assert!(output.contains("pinned to Mini vMac 99.0"), "{output}");
    assert!(output.contains("imported machine plus-a"), "{output}");
    assert!(!target.machine_dir("plus-b").exists());
}

/// A host whose backend builds `revision`, whatever this test really runs on.
struct Pinned {
    root: TempDir,
    app: App,
    paths: Paths,
}

impl Pinned {
    fn new(revision: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let pin = SourcePin {
            revision: revision.into(),
            ..SourcePin::default()
        };
        let backend = MiniVmac::new(paths.clone(), pin, None);
        let app = App::new(paths.clone(), Backends::new(vec![Box::new(backend)]));
        Self { root, app, paths }
    }

    fn cmd(&self, args: &[&str]) {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        assert_eq!(
            self.app.execute(Cli::parse_from(argv)).unwrap(),
            0,
            "{args:?}"
        );
    }

    fn create_machine(&self) {
        let dir = self.root.path();
        let rom = write_rom(dir, "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        let disk = write_disk(dir, "sys.dsk");
        self.cmd(&["rom", "add", rom.to_str().unwrap(), "--name", "plus"]);
        self.cmd(&["system", "add", disk.to_str().unwrap(), "--name", "sys"]);
        self.cmd(&[
            "create", "plus-a", "--model", "plus", "--rom", "plus", "--system", "sys",
        ]);
    }

    fn export(&self) -> PathBuf {
        let archive = self.root.path().join("out.tar.gz");
        let _ = fs::remove_file(&archive);
        self.cmd(&["export", "plus-a", "-o", archive.to_str().unwrap()]);
        archive
    }

    fn import(&self, archive: &Path) -> (usize, usize) {
        let report =
            archive::import(&self.paths, &self.app.backends, archive, None, false).unwrap();
        (report.imported(), report.unchanged())
    }

    fn recorded_revision(&self) -> String {
        let machine = Machine::load(&self.paths, "plus-a").unwrap();
        machine.emulator.revision
    }

    fn runnable(&self) -> bool {
        self.app
            .doctor_checks(false)
            .unwrap()
            .iter()
            .any(|check| check.message == "machine plus-a is runnable")
    }
}

/// 36.04 to 37.03, as from Linux or Windows to macOS, and the reverse.
#[test]
fn machines_move_between_hosts_on_different_revisions() {
    for (from, to) in [("36.04", "37.03"), ("37.03", "36.04")] {
        let source = Pinned::new(from);
        source.create_machine();
        assert_eq!(source.recorded_revision(), from);
        let archive = source.export();

        let target = Pinned::new(to);
        assert_eq!(target.import(&archive), (1, 0), "{from} to {to}");
        assert!(target.runnable(), "{from} to {to}");
        assert_eq!(
            target.recorded_revision(),
            from,
            "the definition arrives as it left"
        );

        // Importing again changes nothing, and neither does coming home.
        assert_eq!(target.import(&archive), (0, 1), "{from} to {to}");
        assert_eq!(source.import(&target.export()), (0, 1), "{from} to {to}");
        assert_eq!(source.recorded_revision(), from);
    }
}

/// Two hosts that made the same machine's copies with different revisions.
#[test]
fn copies_recording_different_revisions_are_the_same_machine() {
    let source = Pinned::new("36.04");
    source.create_machine();
    let archive = source.export();
    let target = Pinned::new("37.03");
    assert_eq!(target.import(&archive), (1, 0));

    let file = target.paths.machine("plus-a").join("machine.toml");
    let text = fs::read_to_string(&file).unwrap();
    fs::write(&file, text.replace("\"36.04\"", "\"37.03\"")).unwrap();
    assert_eq!(target.recorded_revision(), "37.03");
    assert_eq!(target.import(&archive), (0, 1));

    // One that can't run here isn't the same, though everything else matches.
    let text = fs::read_to_string(&file).unwrap();
    fs::write(&file, text.replace("\"37.03\"", "\"99.0\"")).unwrap();
    let report =
        archive::import(&target.paths, &target.app.backends, &archive, None, false).unwrap();
    assert_eq!((report.imported(), report.unchanged()), (0, 0));
}

#[test]
fn import_does_not_build_anything() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);
    assert!(!target.path().join("cache").exists());
}

#[test]
fn export_all_takes_every_machine() {
    let source = Host::with_two_machines();
    let archive = source.archive("all.tar.gz");
    let output = source.ok(&["export", "--all", "-o", &archive]);
    assert!(output.contains("Exported 2 machine(s)"), "{output}");

    let target = Host::new();
    target.ok(&["import", &archive]);
    let list = target.ok(&["list"]);
    assert!(list.contains("plus-a") && list.contains("plus-b"), "{list}");
}

#[test]
fn export_all_needs_machines_and_excludes_names() {
    let empty = Host::new();
    let archive = empty.archive("all.tar.gz");
    let output = empty.fails(&["export", "--all", "-o", &archive]);
    assert!(output.contains("no machines"), "{output}");
    assert!(!Path::new(&archive).exists());

    let both = empty.run(&["export", "--all", "plus-a", "-o", &archive]);
    assert_eq!(both.status.code(), Some(2), "{both:?}");
    let neither = empty.run(&["export", "-o", &archive]);
    assert_eq!(neither.status.code(), Some(2), "{neither:?}");
}

#[test]
fn export_refuses_a_disk_in_use() {
    let source = Host::with_two_machines();
    let paths = Paths {
        config_dir: source.path().join("cfg/macman"),
        data_dir: source.data(),
        cache_dir: source.path().join("cache/macman"),
    };
    // The second machine's disk: a problem anywhere stops the whole export.
    let disk = source.machine_dir("plus-b").join("system.dsk");
    let lock = lock_image(&paths, &disk, LockMode::Exclusive).unwrap();

    let archive = source.archive("out.tar.gz");
    let output = source.fails(&["export", "plus-a", "plus-b", "-o", &archive]);
    assert!(output.contains("in use"), "{output}");
    assert!(!Path::new(&archive).exists());

    drop(lock);
    source.ok(&["export", "plus-a", "plus-b", "-o", &archive]);
}

#[test]
fn export_never_replaces_a_file() {
    let source = Host::with_two_machines();
    let archive = source.archive("exists.tar.gz");
    fs::write(&archive, "precious").unwrap();
    let output = source.fails(&["export", "plus-a", "-o", &archive]);
    assert!(output.contains("already exists"), "{output}");
    assert_eq!(fs::read_to_string(&archive).unwrap(), "precious");
}

#[test]
fn export_leaves_nothing_behind_on_failure() {
    let source = Host::with_two_machines();
    fs::remove_file(source.data().join("roms/plus-v3.rom")).unwrap();
    let archive = source.archive("out.tar.gz");
    source.fails(&["export", "plus-a", "-o", &archive]);
    let leftovers: Vec<_> = fs::read_dir(source.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("out.tar.gz"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
}

#[test]
fn export_notices_a_damaged_rom() {
    let source = Host::with_two_machines();
    fs::write(source.data().join("roms/plus-v3.rom"), "damaged").unwrap();
    let archive = source.archive("out.tar.gz");
    let output = source.fails(&["export", "plus-a", "-o", &archive]);
    assert!(
        output.contains("changed since it was registered"),
        "{output}"
    );
    assert!(!Path::new(&archive).exists());
}

#[test]
fn a_machine_from_before_ids_gets_one_that_lasts() {
    let source = Host::with_two_machines();
    let legacy: String = source
        .definition("plus-a")
        .lines()
        .filter(|line| !line.starts_with("id = "))
        .map(|line| format!("{line}\n"))
        .collect();
    source.write_definition("plus-a", &legacy);
    assert!(source.ok(&["list"]).contains("plus-a"));

    let first = source.export(&["plus-a"], "first.tar.gz");
    let assigned = source.definition("plus-a");
    let id = id_of(&assigned).to_string();
    let second = source.export(&["plus-a"], "second.tar.gz");
    assert_eq!(id_of(&source.definition("plus-a")), id);

    // Both exports are the same machine to whoever imports them.
    let target = Host::new();
    target.ok(&["import", &first]);
    assert_eq!(id_of(&target.definition("plus-a")), id);
    let output = target.ok(&["import", &second]);
    assert!(output.contains("unchanged machine plus-a"), "{output}");
}

#[test]
fn a_clone_is_a_different_machine() {
    let source = Host::with_two_machines();
    source.ok(&["clone", "plus-a", "plus-c"]);
    assert_ne!(
        id_of(&source.definition("plus-a")),
        id_of(&source.definition("plus-c"))
    );
}

/// A host where registering `plus-v3` stopped after the ROM file, holding
/// `bytes` where the ROM should be.
fn with_interrupted_rom(bytes: &[u8]) -> Host {
    let host = Host::new();
    let roms = host.data().join("roms");
    fs::create_dir_all(&roms).unwrap();
    fs::write(roms.join("plus-v3.rom"), bytes).unwrap();
    host
}

#[test]
fn a_rom_registration_interrupted_before_its_metadata_is_recovered() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let rom = fs::read(source.data().join("roms/plus-v3.rom")).unwrap();

    let target = with_interrupted_rom(&rom);
    let output = target.ok(&["import", &archive]);
    assert!(output.contains("recovered rom plus-v3"), "{output}");
    assert!(
        output.contains("2 imported, 0 unchanged, 0 unresolved"),
        "{output}"
    );
    assert_eq!(
        fs::read(target.data().join("roms/plus-v3.rom")).unwrap(),
        rom
    );

    // The result is what an uninterrupted import would have left.
    let clean = Host::new();
    clean.ok(&["import", &archive]);
    let metadata = |host: &Host| fs::read(host.data().join("roms/plus-v3.toml")).unwrap();
    assert_eq!(metadata(&target), metadata(&clean));
    assert_eq!(target.ok(&["rom", "list"]), clean.ok(&["rom", "list"]));

    let output = target.ok(&["import", &archive]);
    assert!(output.contains("unchanged rom plus-v3"), "{output}");
    assert!(
        output.contains("0 imported, 2 unchanged, 0 unresolved"),
        "{output}"
    );
}

#[test]
fn an_incomplete_rom_with_other_contents_is_left_alone() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = with_interrupted_rom(b"something else");
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(
        output.contains("different ROM is already named"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[test]
fn rom_metadata_without_its_file_is_left_alone() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    let roms = target.data().join("roms");
    fs::create_dir_all(&roms).unwrap();
    let metadata = roms.join("plus-v3.toml");
    fs::copy(source.data().join("roms/plus-v3.toml"), &metadata).unwrap();
    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(
        output.contains("different ROM is already named"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[test]
fn rom_metadata_that_disagrees_with_its_file_is_left_alone() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let rom = fs::read(source.data().join("roms/plus-v3.rom")).unwrap();

    let target = with_interrupted_rom(&rom);
    let metadata = fs::read_to_string(source.data().join("roms/plus-v3.toml")).unwrap();
    let digest = sha256_bytes(&rom);
    let metadata = metadata.replace(&digest, &sha256_bytes(b"another ROM"));
    fs::write(target.data().join("roms/plus-v3.toml"), metadata).unwrap();

    let before = target.snapshot();
    let output = target.fails(&["import", &archive]);
    assert!(
        output.contains("different ROM is already named"),
        "{output}"
    );
    assert_eq!(target.snapshot(), before);
}

#[cfg(unix)]
#[test]
fn a_rom_reached_through_a_link_is_not_recovered() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    let roms = target.data().join("roms");
    fs::create_dir_all(&roms).unwrap();
    let elsewhere = source.data().join("roms/plus-v3.rom");
    std::os::unix::fs::symlink(&elsewhere, roms.join("plus-v3.rom")).unwrap();
    let output = target.fails(&["import", &archive]);
    assert!(
        output.contains("different ROM is already named"),
        "{output}"
    );
    assert!(!roms.join("plus-v3.toml").exists());
}

#[test]
fn a_recovered_rom_stays_when_its_machine_cannot_be_published() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let rom = fs::read(source.data().join("roms/plus-v3.rom")).unwrap();

    let target = with_interrupted_rom(&rom);
    // Publication needs the locks directory, which this blocks.
    fs::write(target.data().join("locks"), "in the way").unwrap();
    let output = target.fails(&["import", &archive]);
    assert!(output.contains("failed machine plus-a"), "{output}");
    assert!(target.data().join("roms/plus-v3.rom").exists());
    assert!(target.data().join("roms/plus-v3.toml").exists());
}

/// Waits behind `File::lock`, so the competing machine lands exactly between
/// the import's own check and its publication.
#[test]
fn a_machine_appearing_during_an_import_is_not_replaced() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    fs::create_dir_all(target.data().join("locks")).unwrap();
    let lock = fs::File::create(target.data().join("locks/machines.lock")).unwrap();
    lock.lock().unwrap();

    let child = target
        .command(&["import", &archive])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The ROM goes in just before the machine does.
    let rom = target.data().join("roms/plus-v3.toml");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !rom.exists() {
        assert!(
            Instant::now() < deadline,
            "the import never reached the lock"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let competitor = target.machine_dir("plus-a");
    fs::create_dir_all(&competitor).unwrap();
    drop(lock);
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stdout(&output).contains("appeared during the import"),
        "{output:?}"
    );
    assert_eq!(fs::read_dir(&competitor).unwrap().count(), 0);
    assert!(!rom.exists(), "the ROM added for it was rolled back");
    assert!(!target.data().join("staging").exists());
}

#[test]
fn staging_abandoned_by_a_killed_import_is_cleared() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");

    let target = Host::new();
    let staging = target.data().join("staging");
    let rom = digest(source.data().join("roms/plus-v3.rom"));
    fs::create_dir_all(staging.join("blobs")).unwrap();
    fs::write(staging.join("blobs").join(rom), "half a ROM").unwrap();

    let output = target.ok(&["import", &archive]);
    assert!(output.contains("imported machine plus-a"), "{output}");
    assert!(!staging.exists());
}

#[test]
fn a_failed_import_leaves_no_staging() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let mut entries = entries_of(&archive);
    entries.last_mut().unwrap().1[100] ^= 1;
    let damaged = source.path().join("damaged.tar.gz");
    write_entries(&damaged, &entries);

    let target = Host::new();
    target.fails(&["import", damaged.to_str().unwrap()]);
    assert!(!target.data().join("staging").exists());
    assert!(!target.data().join("roms/plus-v3.rom").exists());
}

/// A bad archive must be refused whole, with nothing else changed, and it
/// must write nothing outside the data directory.
fn assert_refused(entries: &[(tar::Header, Vec<u8>)], expected: &str) {
    let target = Host::with_two_machines();
    let archive = target.path().join("bad.tar.gz");
    write_entries(&archive, entries);
    let (code, _, preview) = target.preview(&[archive.to_str().unwrap()]);
    assert_eq!(code, Some(1), "{preview}");
    assert!(preview.contains(expected), "wanted {expected:?}: {preview}");
    let before = target.snapshot();
    let output = target.fails(&["import", archive.to_str().unwrap()]);
    assert!(output.contains(expected), "wanted {expected:?}: {output}");
    assert_eq!(target.snapshot(), before);
    assert!(!target.path().join("data/escape").exists());
}

fn honest() -> (Host, String) {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    (source, archive)
}

fn regular(name: &str, bytes: &[u8]) -> (tar::Header, Vec<u8>) {
    let header = raw_header(name.as_bytes(), bytes.len() as u64, tar::EntryType::Regular);
    (header, bytes.to_vec())
}

#[test]
fn refuses_entries_with_unexpected_names() {
    let (_source, archive) = honest();
    for name in [
        "../escape",
        "../../../../escape",
        "/tmp/macman-escape",
        "blobs/../../escape",
        "blobs/short",
        "machines/plus-a/machine.toml",
    ] {
        let mut entries = entries_of(&archive);
        entries.push(regular(name, b"evil"));
        assert_refused(&entries, "unexpected entry");
    }
}

#[test]
fn refuses_links_and_other_entry_types() {
    let (_source, archive) = honest();
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Directory,
        tar::EntryType::Fifo,
        tar::EntryType::Char,
    ] {
        let mut entries = entries_of(&archive);
        let name = format!("blobs/{}", "ab".repeat(32));
        entries.push((raw_header(name.as_bytes(), 0, kind), Vec::new()));
        assert_refused(&entries, "unexpected entry of type");
    }
}

#[test]
fn refuses_duplicate_and_unlisted_blobs() {
    let (_source, archive) = honest();
    let mut entries = entries_of(&archive);
    let (name, bytes) = (read_entries(&archive)[1].0.clone(), entries[1].1.clone());
    entries.push(regular(&name, &bytes));
    assert_refused(&entries, "duplicate entry");

    let mut entries = entries_of(&archive);
    let unlisted = sha256_bytes(b"evil");
    entries.push(regular(&format!("blobs/{unlisted}"), b"evil"));
    assert_refused(&entries, "unlisted entry");
}

#[test]
fn refuses_blobs_that_do_not_match_the_manifest() {
    let (_source, archive) = honest();

    let mut entries = entries_of(&archive);
    entries[1].1[10] ^= 1;
    assert_refused(&entries, "digest");

    let mut entries = entries_of(&archive);
    let bytes = entries[1].1[1..].to_vec();
    let name = read_entries(&archive)[1].0.clone();
    entries[1] = regular(&name, &bytes);
    assert_refused(&entries, "the manifest says");

    let mut entries = entries_of(&archive);
    entries.pop();
    assert_refused(&entries, "incomplete");
}

#[test]
fn refuses_sizes_beyond_the_limits_before_reading_the_data() {
    let (_source, archive) = honest();
    let edit = |text: String| text.replace("size = 131072", "size = 5000000000");
    assert_refused(
        &with_manifest(&archive, edit),
        "too large to import (5000000000",
    );

    // Each one is allowed, but not all of them together.
    let edit = |mut text: String| {
        for index in 0..17 {
            let sha256 = sha256_bytes(&[index]);
            let size = 4u64 << 30;
            text +=
                &format!("\n[[roms]]\nname = \"r{index}\"\nsha256 = \"{sha256}\"\nsize = {size}\n");
        }
        text
    };
    assert_refused(&with_manifest(&archive, edit), "archive is too large");

    // Claimed in the tar header but not the manifest.
    let mut entries = entries_of(&archive);
    let name = read_entries(&archive)[1].0.clone();
    let bytes = vec![0u8; 64];
    entries[1] = regular(&name, &bytes);
    assert_refused(&entries, "the manifest says");

    let huge = format!(
        "{}\n# {}",
        "format_version = 1",
        "x".repeat(17 * 1024 * 1024)
    );
    let (header, bytes) = regular("manifest.toml", huge.as_bytes());
    assert_refused(&[(header, bytes)], "too large");
}

#[test]
fn refuses_hostile_manifests() {
    let (_source, archive) = honest();
    for path in ["CON.dsk", "sub/nul", "system.dsk.", "system.dsk "] {
        let edit =
            |text: String| text.replace("path = \"system.dsk\"", &format!("path = \"{path}\""));
        assert_refused(&with_manifest(&archive, edit), "unsafe disk path");
    }
    let cases: &[(&str, &str, &str)] = &[
        (
            "path = \"system.dsk\"",
            "path = \"../../escape\"",
            "unsafe disk path",
        ),
        (
            "path = \"system.dsk\"",
            "path = \"/tmp/macman-escape\"",
            "unsafe disk path",
        ),
        (
            "path = \"system.dsk\"",
            "path = \"..\\\\\\\\escape\"",
            "unsafe disk path",
        ),
        (
            "path = \"system.dsk\"",
            "path = \"C:escape\"",
            "unsafe disk path",
        ),
        (
            "path = \"system.dsk\"",
            "path = \"machine.toml\"",
            "unsafe disk path",
        ),
        (
            "name = \"plus-a\"",
            "name = \"../escape\"",
            "invalid machine name",
        ),
        (
            "name = \"plus-v3\"",
            "name = \"../escape\"",
            "invalid ROM name",
        ),
        ("format_version = 1", "format_version = 2", "upgrade macman"),
        ("sha256 = \"", "sha256 = \"../", "invalid digest"),
    ];
    for (from, to, expected) in cases {
        let edit = |text: String| {
            assert!(text.contains(from), "{from}");
            text.replace(from, to)
        };
        assert_refused(&with_manifest(&archive, edit), expected);
    }
}

#[test]
fn refuses_things_that_are_not_archives() {
    let target = Host::new();
    let file = target.path().join("not.tar.gz");
    fs::write(&file, "plain text").unwrap();
    target.fails(&["import", file.to_str().unwrap()]);

    let (_source, archive) = honest();
    let mut bytes = fs::read(&archive).unwrap();
    bytes.truncate(bytes.len() / 2);
    fs::write(&file, bytes).unwrap();
    target.fails(&["import", file.to_str().unwrap()]);

    write_entries(&file, &[regular("readme.txt", b"hello")]);
    let output = target.fails(&["import", file.to_str().unwrap()]);
    assert!(
        output.contains("doesn't start with manifest.toml"),
        "{output}"
    );

    write_entries(&file, &[]);
    target.fails(&["import", file.to_str().unwrap()]);
    let missing = target.path().join("missing.tar.gz");
    target.fails(&["import", missing.to_str().unwrap()]);
    assert!(!target.data().join("machines").exists());
}

/// What `import` prints, given what `import --dry-run` printed.
fn as_import(preview: &str) -> String {
    preview
        .lines()
        .filter(|line| !line.starts_with("(this is how an import would go"))
        .map(|line| {
            let line = match line.strip_prefix("dry run: ") {
                Some(summary) => summary
                    .strip_suffix("; nothing was changed")
                    .expect("the summary says nothing changed")
                    .replace(" would be imported", " imported"),
                None => line
                    .replacen("would import ", "imported ", 1)
                    .replacen("would recover ", "recovered ", 1)
                    .replacen("would reuse ", "reused ", 1),
            };
            line + "\n"
        })
        .collect()
}

/// Preview importing with `args`, twice, then import, and check that the
/// preview foretold the import exactly. Returns the preview.
fn assert_preview_matches(target: &Host, args: &[&str]) -> String {
    let (code, preview, errors) = target.preview(args);
    assert!(
        preview.is_empty() || preview.contains("; nothing was changed"),
        "{preview}"
    );
    assert_eq!(
        target.preview(args),
        (code, preview.clone(), errors.clone())
    );

    let output = target.run(&[&["import"], args].concat());
    assert_eq!(output.status.code(), code, "{preview}{errors}: {output:?}");
    assert_eq!(stdout(&output), as_import(&preview), "{errors}");
    assert_eq!(stderr(&output), errors);
    preview
}

/// `plus-a` and `plus-b` on a Plus ROM, and `ii-a` on a Mac II ROM.
fn with_three_machines() -> Host {
    let host = Host::with_two_machines();
    host.add_ii_rom("ii-rom");
    host.create("ii-a", "ii", "ii-rom");
    host
}

#[test]
fn a_dry_run_of_a_clean_import_changes_nothing_and_foretells_it() {
    let source = with_three_machines();
    let archive = source.export(&["plus-a", "plus-b", "ii-a"], "three.tar.gz");
    let target = Host::new();
    let preview = assert_preview_matches(&target, &[&archive]);
    for line in [
        "would import rom ii-rom",
        "would import rom plus-v3",
        "would import machine ii-a",
        "would import machine plus-a",
        "would import machine plus-b",
        "dry run: 3 would be imported, 0 unchanged, 0 unresolved; nothing was changed",
    ] {
        assert!(preview.contains(line), "{line}: {preview}");
    }
    assert!(target.machine_dir("plus-b").exists());
}

#[test]
fn a_dry_run_reports_unchanged_and_reused_data() {
    let source = with_three_machines();
    let archive = source.export(&["plus-a", "plus-b", "ii-a"], "three.tar.gz");

    let target = Host::new();
    target.ok(&["import", &source.export(&["plus-a"], "one.tar.gz")]);
    target.add_ii_rom("my-ii");
    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(preview.contains("unchanged rom plus-v3"), "{preview}");
    assert!(
        preview.contains("would reuse rom ii-rom as my-ii"),
        "{preview}"
    );
    assert!(preview.contains("unchanged machine plus-a"), "{preview}");
    assert!(preview.contains("would import machine plus-b"), "{preview}");
    assert!(
        preview.contains("dry run: 2 would be imported, 1 unchanged, 0 unresolved"),
        "{preview}"
    );

    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(
        preview.contains("dry run: 0 would be imported, 3 unchanged, 0 unresolved"),
        "{preview}"
    );
}

#[test]
fn a_dry_run_reports_every_conflict_and_failure() {
    let source = with_three_machines();
    let revision = format!("revision = \"{}\"", SourcePin::default().revision);
    let definition = source
        .definition("plus-b")
        .replace(&revision, "revision = \"99.0\"");
    source.write_definition("plus-b", &definition);
    let archive = source.export(&["plus-a", "plus-b", "ii-a"], "three.tar.gz");

    // An unrelated plus-a, and an unrelated ROM named ii-rom.
    let target = Host::new();
    target.add_plus_rom("plus-v3");
    target.add_system();
    target.create("plus-a", "plus", "plus-v3");
    target.add_large_rom("ii-rom", IIX_ROM_CHECKSUM);
    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(preview.contains("conflict machine plus-a"), "{preview}");
    assert!(preview.contains("--rename"), "{preview}");
    assert!(preview.contains("failed machine plus-b"), "{preview}");
    assert!(preview.contains("pinned to Mini vMac 99.0"), "{preview}");
    assert!(preview.contains("conflict machine ii-a"), "{preview}");
    assert!(
        preview.contains("different ROM is already named \"ii-rom\""),
        "{preview}"
    );
    assert!(
        preview.contains("dry run: 0 would be imported, 0 unchanged, 3 unresolved"),
        "{preview}"
    );
}

#[test]
fn a_dry_run_reports_diverged_copies() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);
    target.scribble("plus-a", "system.dsk", 7);
    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(preview.contains("different disk"), "{preview}");
}

#[test]
fn a_dry_run_foresees_recovering_an_interrupted_rom() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let rom = fs::read(source.data().join("roms/plus-v3.rom")).unwrap();
    let target = with_interrupted_rom(&rom);
    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(preview.contains("would recover rom plus-v3"), "{preview}");
    assert!(
        preview.contains("dry run: 2 would be imported"),
        "{preview}"
    );

    let target = with_interrupted_rom(b"something else");
    let preview = assert_preview_matches(&target, &[&archive]);
    assert!(
        preview.contains("different ROM is already named"),
        "{preview}"
    );
}

#[test]
fn a_dry_run_follows_rename() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);

    let preview = assert_preview_matches(&target, &[&archive, "--rename", "copy"]);
    assert!(preview.contains("would import machine copy"), "{preview}");
    let preview = assert_preview_matches(&target, &[&archive, "--rename", "copy"]);
    assert!(preview.contains("already exists"), "{preview}");

    let both = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let (code, _, errors) = target.preview(&[&both, "--rename", "other"]);
    assert_eq!(code, Some(1));
    assert!(errors.contains("one machine"), "{errors}");
    let (code, _, errors) = target.preview(&[&archive, "--rename", "../escape"]);
    assert_eq!(code, Some(1), "{errors}");
}

#[test]
fn a_dry_run_leaves_abandoned_staging_and_takes_no_new_locks() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);
    // Comparing with the local copy locks its disks; a preview mustn't leave
    // lock files for that, or anything else.
    fs::remove_dir_all(target.data().join("locks")).unwrap();
    let staging = target.data().join("staging/blobs");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("leftover"), "half a ROM").unwrap();

    let (code, preview, _) = target.preview(&[&archive]);
    assert_eq!(code, Some(0));
    assert!(preview.contains("unchanged machine plus-a"), "{preview}");
    assert!(!target.data().join("locks").exists());
    assert!(staging.join("leftover").exists());
}

#[test]
fn a_dry_run_waits_for_no_import_and_respects_disks_in_use() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a"], "plus.tar.gz");
    let target = Host::new();
    target.ok(&["import", &archive]);

    let lock = fs::File::open(target.data().join("import.lock")).unwrap();
    lock.lock().unwrap();
    let (code, _, errors) = target.preview(&[&archive]);
    assert_eq!(code, Some(1));
    assert!(
        errors.contains("another macman import is running"),
        "{errors}"
    );
    drop(lock);

    let disk = target.machine_dir("plus-a").join("system.dsk");
    let held = lock_image(&Paths::under(target.path()), &disk, LockMode::Exclusive).unwrap();
    let (code, preview, _) = target.preview(&[&archive]);
    assert_eq!(code, Some(1));
    assert!(preview.contains("is in use"), "{preview}");
    drop(held);
}

#[test]
fn a_dry_run_reports_json() {
    let source = Host::with_two_machines();
    let archive = source.export(&["plus-a", "plus-b"], "both.tar.gz");
    let target = Host::new();
    target.add_plus_rom("plus-v3");
    target.add_system();
    target.create("plus-a", "plus", "plus-v3");
    let before = target.exact_snapshot();

    let output = target.run(&["--json", "import", "--dry-run", &archive]);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(
        report["roms"],
        serde_json::json!([{"name": "plus-v3", "outcome": "unchanged", "local": "plus-v3"}])
    );
    let machines = report["machines"].as_array().unwrap();
    assert_eq!(machines[0]["name"], "plus-a");
    assert_eq!(machines[0]["outcome"], "conflict");
    assert!(machines[0]["reason"].as_str().unwrap().contains("--rename"));
    assert_eq!(
        machines[1],
        serde_json::json!({"name": "plus-b", "outcome": "imported", "reason": null})
    );
    assert_eq!(
        (
            &report["imported"],
            &report["unchanged"],
            &report["unresolved"]
        ),
        (
            &serde_json::json!(1),
            &serde_json::json!(0),
            &serde_json::json!(1)
        )
    );
    assert_eq!(target.exact_snapshot(), before);

    // A real import changes things, so it has no JSON report.
    let output = target.run(&["--json", "import", &archive]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("--dry-run"), "{output:?}");
    assert_eq!(target.exact_snapshot(), before);
}
