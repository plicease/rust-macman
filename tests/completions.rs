//! `macman completions`, through the real binary: each script is all there is
//! on stdout, needs nothing from the host, and completes what the CLI has.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// Run macman with its directories pointing into `root`.
fn macman(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The script for `shell`, checking that it's the only output.
fn script(shell: &str) -> String {
    let root = TempDir::new().unwrap();
    let output = macman(root.path(), &["completions", shell]);
    assert!(output.status.success(), "{shell}: {output:?}");
    assert!(output.stderr.is_empty(), "{shell}: {output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\x1b'), "{shell}");
    text
}

/// Whether `program` can be run here, for the checks that need a real shell.
fn have(program: &str) -> bool {
    Command::new(program)
        .arg("-c")
        .arg("true")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn every_shell_gets_a_script() {
    for shell in ["zsh", "bash", "fish", "powershell", "elvish"] {
        let text = script(shell);
        // Option names without their dashes, which fish writes as `-l json`.
        for word in [
            "doctor",
            "import",
            "completions",
            "dry-run",
            "verbose",
            "json",
        ] {
            assert!(text.contains(word), "{shell} lacks {word}");
        }
    }
}

#[test]
fn zsh_completes_choices_and_paths() {
    let text = script("zsh");
    assert!(text.starts_with("#compdef macman"), "{text}");
    for expected in [
        ":BACKEND:(minivmac basilisk2 sheepshaver)",
        "(twig43 twiggy 128k 512ke plus se sefdhd classic pb100 ii iix)",
        "68040",
        "slirp",
        "(bash elvish fish powershell zsh)",
        ":file:_files",
        ":DIR:_files -/",
    ] {
        assert!(text.contains(expected), "zsh lacks {expected}");
    }
}

#[test]
fn an_unknown_shell_fails_with_no_script() {
    let root = TempDir::new().unwrap();
    let output = macman(root.path(), &["completions", "tcsh"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("possible values"), "{errors}");
}

#[test]
fn json_does_not_apply() {
    let root = TempDir::new().unwrap();
    let output = macman(root.path(), &["--json", "completions", "zsh"]);
    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["error"].as_str().unwrap().contains("shell script"));
}

#[test]
fn generating_touches_nothing_and_needs_no_setup() {
    let root = TempDir::new().unwrap();
    // A relative XDG directory is an error for every other command.
    let output = Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(["completions", "bash"])
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", "relative")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn the_scripts_are_valid_shell() {
    for (shell, program) in [("zsh", "zsh"), ("bash", "bash")] {
        if !have(program) {
            eprintln!("skipping {shell}: {program} isn't installed");
            continue;
        }
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("script");
        std::fs::write(&file, script(shell)).unwrap();
        let output = Command::new(program).arg("-n").arg(&file).output().unwrap();
        assert!(output.status.success(), "{shell}: {output:?}");
    }
}

#[test]
fn bash_completes_commands_and_backends() {
    if !have("bash") {
        eprintln!("skipping: bash isn't installed");
        return;
    }
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("macman.bash");
    std::fs::write(&file, script("bash")).unwrap();
    // Ask the script to complete `macman imp` and `macman create x --backend `.
    let check = r#"
        source "$1"
        COMP_WORDS=(macman imp); COMP_CWORD=1
        _macman macman imp macman
        echo "${COMPREPLY[*]}"
        COMP_WORDS=(macman create x --backend ""); COMP_CWORD=4
        _macman macman "" --backend
        echo "${COMPREPLY[*]}"
    "#;
    let output = Command::new("bash")
        .args(["-c", check, "check"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text, "import\nminivmac basilisk2 sheepshaver\n");
}

#[test]
fn zsh_picks_up_the_script_from_fpath() {
    if !have("zsh") {
        eprintln!("skipping: zsh isn't installed");
        return;
    }
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("_macman"), script("zsh")).unwrap();
    // As in the README: on fpath before compinit.
    let check = r#"
        fpath=($1 $fpath)
        autoload -Uz compinit
        compinit -D -u
        print -- $_comps[macman]
    "#;
    let output = Command::new("zsh")
        .args(["-f", "-c", check, "check"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "_macman\n");
}
