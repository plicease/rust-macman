//! The SheepShaver adapter on Windows, fed fixture copies of macemu and SDL.
//! Every source file exists but is empty, so the real build steps run, quickly,
//! and some are links into Basilisk II's tree, as upstream's are.
//! `main_windows.cpp` stands in for the emulator, and acts out what it would do.
//! See `tests/sheepshaver.rs` for Linux.
#![cfg(all(windows, target_arch = "x86_64"))]

mod common;

use anyhow::Result;
use clap::Parser;
use common::write_disk;
use flate2::Compression;
use flate2::write::GzEncoder;
use macman::app::App;
use macman::backend::Backends;
use macman::backend::macemu::pin;
use macman::backend::macemu_windows::{CROSS_PLATFORM_SOURCES, SLIRP_SOURCES, sdl_pin};
use macman::backend::sheepshaver::SheepShaver;
use macman::backend::sheepshaver_windows::SOURCES;
use macman::backend::source::SourcePin;
use macman::cli::Cli;
use macman::fsutil::sha256_file;
use macman::paths::Paths;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// `genexec.pl` reads this, preprocessed, for the handlers it names.
const DECODER: &str =
    "#ifdef GENEXEC\nvoid table() { &powerpc_cpu::execute_fixture<1, 2>; }\n#endif\n";
/// The silent alert macman changes is kept in a comment. It records how it was
/// called, then acts: it writes to writable disks, as the real one would, which
/// is how the tests see which file it reached.
const EMULATOR: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <direct.h>
/*
	if (PrefsFindBool("nogui"))
		return;

	VideoQuitFullScreen();
*/
// The C runtime would expand a read-only `*name`; SDL's main reads the command line itself.
extern "C" { int _dowildcard = 0; }
int main(int argc, char **argv) {
    char line[4096], cwd[1024], mode[32] = "";
    FILE *log = fopen("LOG", "a"), *file = fopen("MODE", "r");
    int i;
    if (file) { fgets(mode, sizeof mode, file); fclose(file); }
    fputs("run", log);
    for (i = 1; i < argc; i++) fprintf(log, " [%s]", argv[i]);
    fprintf(log, "\ncwd %s\n", _getcwd(cwd, sizeof cwd));
    file = fopen(argv[2], "r");
    while (file && fgets(line, sizeof line, file)) fputs(line, log);
    if (file) fclose(file);
    file = fopen(argv[4], "rb");
    fprintf(log, "rom %d\n", file != NULL);
    if (file) fclose(file);
    for (i = 5; i + 1 < argc; i += 2) {
        int writable = argv[i + 1][0] != '*';
        file = fopen(argv[i + 1] + !writable, writable ? "r+b" : "rb");
        fprintf(log, "disk %d\n", file != NULL);
        if (file && writable) { fseek(file, 4096, SEEK_SET); fputs("written", file); }
        if (file) fclose(file);
    }
    fclose(log);
    if (!strcmp(mode, "refuse")) { puts("ERROR: Invalid ROM file size."); return 0; }
    if (!strcmp(mode, "crash")) *(volatile int *)0 = 1;
    if (!strcmp(mode, "fail")) return 3;
    return 0;
}
"#;

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
    log: PathBuf,
}

fn runs(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

/// Builds a GitHub-style archive: one top directory, holding `files` and
/// `links`, each by its path from there.
struct Archive {
    builder: tar::Builder<GzEncoder<File>>,
}

impl Archive {
    fn new(path: &Path) -> Self {
        let file = File::create(path).unwrap();
        Self {
            builder: tar::Builder::new(GzEncoder::new(file, Compression::default())),
        }
    }

    fn file(&mut self, path: &str, text: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        let path = normalize(&format!("macemu-1/{path}"));
        self.builder
            .append_data(&mut header, path, text.as_bytes())
            .unwrap();
    }

    fn link(&mut self, path: &str, target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        let path = normalize(&format!("macemu-1/{path}"));
        self.builder.append_link(&mut header, path, target).unwrap();
    }

    fn finish(self) {
        self.builder.into_inner().unwrap().finish().unwrap();
    }
}

/// `a/b/../c` as `a/c`, as the sources are listed from the build directory.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part == ".." {
            parts.pop();
        } else {
            parts.push(part);
        }
    }
    parts.join("/")
}

fn macemu_archive(root: &Path, log: &Path) -> PathBuf {
    let log = log.to_str().unwrap().replace('\\', "/");
    let mode = root.join("mode").to_str().unwrap().replace('\\', "/");
    let path = root.join("macemu.tgz");
    let mut archive = Archive::new(&path);
    let windows = "SheepShaver/src/Windows";
    for source in SOURCES {
        let text = match *source {
            "main_windows.cpp" => EMULATOR
                .replace("LOG", &log)
                .replace("MODE", &mode)
                .replace('\n', "\r\n"),
            "../kpx_cpu/src/cpu/ppc/ppc-decode.cpp" => DECODER.into(),
            "sys_windows.cpp" => continue,
            _ => String::new(),
        };
        archive.file(&format!("{windows}/{source}"), &text);
    }
    archive.file(
        &format!("{windows}/SheepShaver.rc"),
        "STRINGTABLE\nBEGIN\n1 \"fixture\"\nEND\n",
    );
    // Links to a file, to a directory, and to a file in another directory.
    archive.file("BasiliskII/src/Windows/sys_windows.cpp", "");
    archive.link(
        &format!("{windows}/sys_windows.cpp"),
        "../../../BasiliskII/src/Windows/sys_windows.cpp",
    );
    for name in SLIRP_SOURCES {
        archive.file(&format!("BasiliskII/src/slirp/{name}.c"), "");
    }
    archive.link("SheepShaver/src/slirp", "../../BasiliskII/src/slirp");
    for name in CROSS_PLATFORM_SOURCES {
        archive.file(&format!("BasiliskII/src/CrossPlatform/{name}"), "");
        archive.link(
            &format!("SheepShaver/src/CrossPlatform/{name}"),
            &format!("../../../BasiliskII/src/CrossPlatform/{name}"),
        );
    }
    archive.file("cxmon/src/mon.cpp", "not needed");
    archive.finish();
    path
}

fn sdl_archive(root: &Path) -> PathBuf {
    let tree = root.join("fixture/sdl2");
    for target in ["x86_64-w64-mingw32", "i686-w64-mingw32"] {
        fs::create_dir_all(tree.join(target).join("include/SDL2")).unwrap();
        fs::create_dir_all(tree.join(target).join("lib")).unwrap();
        for library in ["libSDL2.a", "libSDL2main.a"] {
            let status = Command::new("ar")
                .arg("rcs")
                .arg(tree.join(target).join("lib").join(library))
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
    let status = Command::new("tar")
        .args(["czf", "sdl.tgz", "-C", "fixture", "sdl2"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    root.join("sdl.tgz")
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
        let tools = ["gcc", "g++", "windres", "ar", "tar"];
        if !tools.iter().all(|tool| runs(tool)) || !(runs("mingw32-make") || runs("make")) {
            eprintln!("skipping: MinGW build tools not installed");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("emulator.log");
        let archive = macemu_archive(root.path(), &log);
        let sdl = sdl_archive(root.path());
        let not_used = String::from("http://invalid.example/not-used");
        let pin = SourcePin {
            url: not_used.clone(),
            sha256: sha256_file(&archive).unwrap(),
            ..pin()
        };
        let sdl_pin = SourcePin {
            url: not_used,
            sha256: sha256_file(&sdl).unwrap(),
            ..sdl_pin()
        };
        let paths = Paths::under(&root.path().join("Zoë Q 日本"));
        let backend =
            SheepShaver::new(paths.clone(), pin, Some(archive)).with_sdl(sdl_pin, Some(sdl));
        let app = App::new(paths.clone(), Backends::new(vec![Box::new(backend)]));
        let harness = Self {
            root,
            paths,
            app,
            log,
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

    fn cmd(&self, args: &[&str]) -> Result<u8> {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        self.app.execute(Cli::parse_from(argv))
    }

    fn create(&self, name: &str) -> Result<u8> {
        self.cmd(&["create", name, "--rom", "beige", "--system", "sys"])
    }

    fn log(&self) -> String {
        // The stand-in logs its working directory in the system code page.
        String::from_utf8_lossy(&fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    fn count(&self, needle: &str) -> usize {
        self.log().lines().filter(|line| *line == needle).count()
    }

    fn last_run(&self) -> String {
        let log = self.log();
        log.lines().rfind(|l| l.starts_with("run ")).unwrap().into()
    }

    fn act(&self, mode: &str) {
        fs::write(self.root.path().join("mode"), mode).unwrap();
    }

    fn doctor_says(&self, text: &str) -> bool {
        self.app
            .doctor_checks(false)
            .unwrap()
            .iter()
            .any(|check| check.message.contains(text))
    }

    fn sessions(&self) -> usize {
        fs::read_dir(self.paths.sessions()).unwrap().count()
    }
}

/// One test, as each harness compiles a hundred files.
#[test]
fn builds_once_and_runs_on_what_windows_needs() {
    let Some(h) = Harness::new() else {
        return;
    };
    h.create("a").unwrap();
    h.create("b").unwrap();
    assert!(h.doctor_says("SDL 2.32.10 source available from source_archive"));

    // The first run builds, from links unpacked as copies, under a path make
    // can only use by short name.
    let disk = h.paths.machine("a").join("system.dsk");
    let private = sha256_file(&disk).unwrap();
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert_eq!(h.sessions(), 0, "a clean exit leaves no session");

    // The session's files go by relative name, and the disk is written.
    let run = h.last_run();
    assert!(run.starts_with("run [--config] [prefs] [--rom] [rom] [--disk] ["));
    assert!(run.ends_with(r"\system.dsk]"), "{run}");
    assert_eq!(h.count("rom 1"), 1, "{}", h.log());
    assert_eq!(h.count("disk 1"), 1, "{}", h.log());
    for line in ["jit false", "enableextfs false", "pollmedia false"] {
        assert_eq!(h.count(line), 1, "{line}: {}", h.log());
    }
    assert_ne!(sha256_file(&disk).unwrap(), private);

    // Another machine shares the build, as nothing of it is in the key.
    let builds = || fs::read_dir(h.paths.builds()).unwrap().count();
    assert_eq!(h.cmd(&["run", "b"]).unwrap(), 0);
    assert_eq!(builds(), 1);

    // A read-only disk with a Unicode name reaches the emulator, unchanged.
    let extra = write_disk(h.root.path(), "extra 日本.dsk");
    let before = sha256_file(&extra).unwrap();
    let path = extra.to_str().unwrap();
    assert_eq!(
        h.cmd(&["run", "a", "--disk", path, "--readonly"]).unwrap(),
        0
    );
    let run = h.last_run();
    assert!(run.ends_with(" [--disk] [*1-extra 日本.dsk]"), "{run}");
    assert_eq!(h.count("disk 1"), 4, "{}", h.log());
    assert_eq!(sha256_file(&extra).unwrap(), before);

    // A refusal exits 0, and the patched alert is what reports it.
    h.act("refuse");
    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(
        error.to_string().contains("Invalid ROM file size"),
        "{error}"
    );
    h.act("crash");
    assert_ne!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert!(h.doctor_says("crashed on the host"));
    h.act("fail");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 3);
    h.act("");

    // A damaged build is found, and rebuilt.
    let build = fs::read_dir(h.paths.builds())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let manifest = fs::read_to_string(build.join("manifest.toml")).unwrap();
    assert!(
        manifest.contains("executable = \"SheepShaver.exe\""),
        "{manifest}"
    );
    assert!(manifest.contains("x86_64-pc-windows-gnu"), "{manifest}");
    assert!(manifest.contains("-DMEM_BULK"), "{manifest}");
    fs::write(build.join("SheepShaver.exe"), b"damaged").unwrap();
    assert!(h.doctor_says("is damaged"));
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);

    // A shared folder would have to be a whole drive here.
    let shared = h.root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let error = h
        .cmd(&[
            "create",
            "s",
            "--rom",
            "beige",
            "--system",
            "sys",
            "--shared-folder",
            shared.to_str().unwrap(),
        ])
        .unwrap_err();
    assert!(error.to_string().contains("whole drives"), "{error}");
    assert!(!h.paths.machine("s").exists());
}
