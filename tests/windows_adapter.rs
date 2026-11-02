//! The real Mini vMac adapter on Windows, fed a small fixture source tree,
//! which needs only MinGW gcc, make, and tar. See `tests/run.rs` for Unix.
#![cfg(windows)]

mod common;

use clap::Parser;
use common::{PLUS_ROM_CHECKSUM, write_disk, write_rom};
use macman::app::App;
use macman::backend::Backends;
use macman::backend::minivmac::{MiniVmac, SourcePin};
use macman::cli::Cli;
use macman::fsutil::sha256_file;
use macman::paths::Paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Stand-in for upstream's configuration tool. Its generated script's last
/// line ends in a digit, as upstream's do, so it needs the redirect fix.
const FIXTURE_TOOL: &str = r#"
#include <stdio.h>
int main(int argc, char **argv) {
    int i;
    printf("@echo off\n");
    printf("copy /y fixture.mk %%my_project_d%%\\Makefile >nul\n");
    printf("set DestFile=LOG\n");
    printf("echo options:");
    for (i = 1; i < argc; i++) printf(" %s", argv[i]);
    printf(">>\"%%DestFile%%\"\n");
    return 0;
}
"#;

/// Names the options macman edits, and links only with `-municode` in place.
const FIXTURE_MAKEFILE: &str = "mk_COptionsCommon = -c -Wall\n\n\
                                minivmac.exe:\n\
                                \techo build>>\"LOG\"\n\
                                \tgcc.exe fake-emulator.c -o minivmac.exe -mwindows -lkernel32\n";

/// Records its raw command line, and looks for the ROM as Mini vMac does.
const FIXTURE_EMULATOR: &str = r#"
#include <windows.h>
#include <stdio.h>
int WINAPI wWinMain(HINSTANCE a, HINSTANCE b, PWSTR c, int d) {
    char line[4096];
    wchar_t module[MAX_PATH];
    wchar_t *slash;
    FILE *log = fopen("LOG", "a");
    WideCharToMultiByte(CP_UTF8, 0, GetCommandLineW(), -1, line, sizeof line, 0, 0);
    fprintf(log, "run %s\n", line);
    GetModuleFileNameW(NULL, module, MAX_PATH);
    slash = wcsrchr(module, L'\\');
    wcscpy(slash + 1, L"vMac.ROM");
    fprintf(log, "rom %d\n", GetFileAttributesW(module) != INVALID_FILE_ATTRIBUTES);
    fclose(log);
    return 0;
}
"#;

struct Harness {
    root: TempDir,
    paths: Paths,
    app: App,
    log: PathBuf,
}

impl Harness {
    fn new() -> Option<Self> {
        let tools = ["gcc", "tar"].iter().all(|tool| runs(tool));
        if !tools || !(runs("mingw32-make") || runs("make")) {
            eprintln!("skipping: MinGW build tools not installed");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("emulator.log");
        let archive = write_archive(root.path(), &log);
        let pin = SourcePin {
            revision: "36.04".into(),
            url: "http://invalid.example/not-used".into(),
            sha256: sha256_file(&archive).unwrap(),
        };
        let paths = Paths::under(&root.path().join("Zoë Q 日本"));
        let backend = MiniVmac::new(paths.clone(), pin, Some(archive));
        let app = App::new(paths.clone(), Backends::new(vec![Box::new(backend)]));
        let harness = Self {
            root,
            paths,
            app,
            log,
        };
        let rom = write_rom(harness.dir(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
        let system = write_disk(harness.dir(), "system.dsk");
        harness.cmd(&["rom", "add", rom.to_str().unwrap(), "--name", "plus"]);
        harness.cmd(&["system", "add", system.to_str().unwrap(), "--name", "sys"]);
        harness.cmd(&[
            "create", "plus-a", "--model", "plus", "--rom", "plus", "--system", "sys",
        ]);
        Some(harness)
    }

    fn dir(&self) -> &Path {
        self.root.path()
    }

    fn cmd(&self, args: &[&str]) -> u8 {
        let argv = std::iter::once("macman").chain(args.iter().copied());
        self.app.execute(Cli::parse_from(argv)).unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn builds(&self) -> usize {
        self.log().lines().filter(|l| l.trim() == "build").count()
    }

    fn last(&self, prefix: &str) -> String {
        let log = self.log();
        log.lines().rfind(|l| l.starts_with(prefix)).unwrap().into()
    }
}

fn runs(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

fn write_archive(root: &Path, log: &Path) -> PathBuf {
    let log = log.to_str().unwrap().replace('\\', "/");
    let tree = root.join("fixture/minivmac");
    fs::create_dir_all(tree.join("setup")).unwrap();
    for (file, text) in [
        ("setup/tool.c", FIXTURE_TOOL),
        ("fixture.mk", FIXTURE_MAKEFILE),
        ("fake-emulator.c", FIXTURE_EMULATOR),
    ] {
        fs::write(tree.join(file), text.replace("LOG", &log)).unwrap();
    }
    let status = Command::new("tar")
        .args(["czf", "fixture.tgz", "-C", "fixture", "minivmac"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    root.join("fixture.tgz")
}

#[test]
fn builds_once_and_launches_with_rom_beside_the_private_executable() {
    let Some(h) = Harness::new() else {
        return;
    };

    // First run compiles with translated options, whole to the last digit.
    assert_eq!(h.cmd(&["run", "plus-a"]), 0);
    assert_eq!(h.builds(), 1);
    assert!(
        h.log().contains(
            "options: -t wx64 -m Plus -mem 4M -hres 512 -vres 342 -depth 0 -fullscreen 0 \
             -mf 2 -magnify 1"
        ),
        "{}",
        h.log()
    );

    // No flags, the ROM beside the executable, and a plain path to the disk.
    let run = h.last("run ");
    assert_eq!(h.last("rom "), "rom 1");
    assert!(!run.contains(" -r ") && !run.contains(" -d "), "{run}");
    assert!(!run.contains(r"\\?\"), "{run}");
    assert!(run.ends_with(r#"\machines\plus-a\system.dsk""#), "{run}");
    assert!(run.contains("Zoë Q 日本"), "{run}");
    assert_eq!(fs::read_dir(h.paths.sessions()).unwrap().count(), 0);

    // A cached build is reused, and read-only copies go by relative name.
    let extra = write_disk(h.dir(), "extra disk.dsk");
    let path = extra.to_str().unwrap();
    assert_eq!(h.cmd(&["run", "plus-a", "--disk", path, "--readonly"]), 0);
    assert_eq!(h.builds(), 1);
    assert!(
        h.last("run ").ends_with(r#" "1-extra disk.dsk""#),
        "{}",
        h.log()
    );

    // A damaged artifact fails validation and is rebuilt.
    let build = fs::read_dir(h.paths.builds())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        fs::read_to_string(build.join("manifest.toml"))
            .unwrap()
            .contains("executable = \"minivmac.exe\"")
    );
    fs::write(build.join("minivmac.exe"), b"damaged").unwrap();
    assert_eq!(h.cmd(&["run", "plus-a"]), 0);
    assert_eq!(h.builds(), 2);
    assert_eq!(h.cmd(&["list", "--builds"]), 0);
    assert_eq!(h.cmd(&["doctor"]), 0);
}
