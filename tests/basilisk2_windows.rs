//! The Basilisk II adapter on Windows, fed fixture copies of macemu and SDL.
//! Every source file exists but is empty, so the real build steps run, quickly.
//! `main_windows.cpp` stands in for the emulator, and acts out what it would do.
//! See `tests/basilisk2.rs` for Linux.
#![cfg(all(windows, target_arch = "x86_64"))]

mod common;

use anyhow::Result;
use clap::Parser;
use common::write_disk;
use macman::app::App;
use macman::backend::Backends;
use macman::backend::basilisk2::{Basilisk2, pin};
use macman::backend::macemu_windows::{CROSS_PLATFORM_SOURCES, SLIRP_SOURCES, SOURCES, sdl_pin};
use macman::backend::source::SourcePin;
use macman::cli::Cli;
use macman::fsutil::sha256_file;
use macman::paths::Paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const CONFIG: &str = "#ifndef _DEBUG\n#define ENABLE_VOSF 1\n#endif\n\
                      #define SIZEOF_LONG_DOUBLE 8\n#define SIZEOF_VOID_P 4\n";
const ALLOCATOR: &str = "/* \tif ((addr = VirtualAlloc(NULL, size, alloc_type, \
                         PAGE_EXECUTE_READWRITE)) == NULL)\n\t\treturn VM_MAP_FAILED;\n */\n";
const BUILD68K: &str = r#"
#include <stdio.h>
int main(int argc, char **argv) {
    FILE *log = fopen("LOG", "a");
    fprintf(log, "build\n");
    fclose(log);
    puts("/* from build68k */");
    return 0;
}
"#;
const GENCPU: &str = r#"
#include <stdio.h>
int main(void) {
    fclose(fopen("cpuemu.cpp", "w"));
    fclose(fopen("cpustbl.cpp", "w"));
    fclose(fopen("cputbl.h", "w"));
    return 0;
}
"#;
/// The two silent alerts macman changes are kept in a comment. It records how
/// it was called, then acts: it writes to writable disks, as the real one
/// would, which is how the tests see which file it reached.
const EMULATOR: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <direct.h>
/*
	if (PrefsFindBool("nogui"))
		return;

	VideoQuitFullScreen();

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
    if (!strcmp(mode, "refuse")) { puts("ERROR: Unsupported ROM type."); return 0; }
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
    sdl: PathBuf,
    sdl_pin: SourcePin,
    pin: SourcePin,
    archive: PathBuf,
}

fn runs(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

fn tar(root: &Path, archive: &str, tree: &str) -> PathBuf {
    let status = Command::new("tar")
        .args(["czf", archive, "-C", "fixture", tree])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    root.join(archive)
}

fn macemu_archive(root: &Path, log: &Path) -> PathBuf {
    let log = log.to_str().unwrap().replace('\\', "/");
    let mode = root.join("mode").to_str().unwrap().replace('\\', "/");
    let src = root.join("fixture/macemu/BasiliskII/src");
    let dir = src.join("Windows");
    let touch = |path: PathBuf| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    };
    for source in SOURCES {
        touch(dir.join(source));
    }
    for name in SLIRP_SOURCES {
        touch(src.join("slirp").join(format!("{name}.c")));
    }
    for name in CROSS_PLATFORM_SOURCES {
        touch(src.join("CrossPlatform").join(name));
    }
    touch(src.join("uae_cpu/table68k"));
    fs::write(src.join("CrossPlatform/vm_alloc.cpp"), ALLOCATOR).unwrap();
    fs::write(dir.join("config.h"), CONFIG).unwrap();
    fs::write(
        dir.join("BasiliskII.rc"),
        "STRINGTABLE\nBEGIN\n1 \"fixture\"\nEND\n",
    )
    .unwrap();
    fs::write(
        dir.join("main_windows.cpp"),
        EMULATOR.replace("LOG", &log).replace("MODE", &mode),
    )
    .unwrap();
    fs::write(
        src.join("uae_cpu/build68k.c"),
        BUILD68K.replace("LOG", &log),
    )
    .unwrap();
    fs::write(src.join("uae_cpu/gencpu.c"), GENCPU).unwrap();
    // A link is what real macemu has, and what the build has to leave out.
    fs::create_dir_all(root.join("fixture/macemu/SheepShaver")).unwrap();
    fs::write(root.join("fixture/macemu/SheepShaver/skipped"), "").unwrap();
    tar(root, "macemu.tgz", "macemu")
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
    tar(root, "sdl.tgz", "sdl2")
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
        let app = App::new(
            paths.clone(),
            backends(&paths, &pin, &archive, &sdl_pin, &sdl),
        );
        let harness = Self {
            root,
            paths,
            app,
            log,
            sdl,
            sdl_pin,
            pin,
            archive,
        };
        let quadra = harness.root.path().join("quadra.bin");
        let mut rom = vec![0u8; 1024 * 1024];
        rom[8..10].copy_from_slice(&[0x06, 0x7C]);
        fs::write(&quadra, rom).unwrap();
        let system = write_disk(harness.root.path(), "system.dsk");
        harness
            .cmd(&["rom", "add", quadra.to_str().unwrap(), "--name", "quadra"])
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
        self.cmd(&[
            "create",
            name,
            "--backend",
            "basilisk2",
            "--rom",
            "quadra",
            "--system",
            "sys",
        ])
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

fn backends(
    paths: &Paths,
    pin: &SourcePin,
    archive: &Path,
    sdl_pin: &SourcePin,
    sdl: &Path,
) -> Backends {
    let backend = Basilisk2::new(paths.clone(), pin.clone(), Some(archive.to_path_buf()))
        .with_sdl(sdl_pin.clone(), Some(sdl.to_path_buf()));
    Backends::new(vec![Box::new(backend)])
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

    // The first run builds, from a directory make can only use by short name.
    let disk = h.paths.machine("a").join("system.dsk");
    let private = sha256_file(&disk).unwrap();
    let template = sha256_file(&h.paths.systems().join("sys.dsk")).unwrap();
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert_eq!(h.count("build"), 1);

    // The session's files go by relative name, and the disk is written.
    let run = h.last_run();
    assert!(run.starts_with("run [--config] [prefs] [--rom] [rom] [--disk] ["));
    assert!(run.ends_with(r"\system.dsk]"), "{run}");
    assert!(!run.contains(r"\\?\"), "{run}");
    assert_eq!(h.count("rom 1"), 1, "{}", h.log());
    assert_eq!(h.count("disk 1"), 1, "{}", h.log());
    assert_eq!(h.count("enableextfs false"), 1, "{}", h.log());
    assert_eq!(h.count("pollmedia false"), 1, "{}", h.log());
    assert_ne!(sha256_file(&disk).unwrap(), private);
    assert_eq!(
        sha256_file(&h.paths.systems().join("sys.dsk")).unwrap(),
        template
    );
    assert_eq!(h.sessions(), 0, "a clean exit leaves no session");

    // Another machine shares the build, as nothing of it is in the key.
    assert_eq!(h.cmd(&["run", "b"]).unwrap(), 0);
    assert_eq!(h.count("build"), 1);

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

    // A refusal exits 0, like everything else from `QuitEmulator`.
    h.act("refuse");
    let error = h.cmd(&["run", "a"]).unwrap_err();
    assert!(
        error.to_string().contains("Unsupported ROM type"),
        "{error}"
    );
    assert!(h.doctor_says("the emulator failed to start"));

    h.act("crash");
    assert_ne!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert!(h.doctor_says("crashed on the host"));
    h.act("fail");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 3);
    assert!(h.doctor_says("exited with code 3"));
    h.act("");
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert!(!h.doctor_says("ended abnormally"), "the next run clears it");

    // A damaged build is found, and rebuilt.
    let build = fs::read_dir(h.paths.builds())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let manifest = fs::read_to_string(build.join("manifest.toml")).unwrap();
    assert!(
        manifest.contains("executable = \"BasiliskII.exe\""),
        "{manifest}"
    );
    assert!(manifest.contains("x86_64-pc-windows-gnu"), "{manifest}");
    fs::write(build.join("BasiliskII.exe"), b"damaged").unwrap();
    assert!(h.doctor_says("is damaged"));
    assert_eq!(h.cmd(&["run", "a"]).unwrap(), 0);
    assert_eq!(h.count("build"), 2);

    // SDL is verified like any other source, before anything is compiled.
    let wrong = SourcePin {
        sha256: "0".repeat(64),
        ..h.sdl_pin.clone()
    };
    let other = tempfile::tempdir().unwrap();
    let paths = Paths::under(other.path());
    let backend = backends(&paths, &h.pin, &h.archive, &wrong, &h.sdl);
    let build = backend.get("basilisk2").unwrap();
    let machine = macman::machine::Machine::load(&h.paths, "a").unwrap();
    let error = build.prepare(&machine, false).unwrap_err();
    assert!(format!("{error:#}").contains("pinned SHA-256"), "{error:#}");

    // A shared folder would have to be a whole drive here.
    let shared = h.root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let error = h
        .cmd(&[
            "create",
            "s",
            "--backend",
            "basilisk2",
            "--rom",
            "quadra",
            "--system",
            "sys",
            "--shared-folder",
            shared.to_str().unwrap(),
        ])
        .unwrap_err();
    assert!(error.to_string().contains("whole drives"), "{error}");
    assert!(!h.paths.machine("s").exists());
}
