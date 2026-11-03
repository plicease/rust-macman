//! `macman completions`, through the real binary: each script is all there is
//! on stdout, needs nothing from the host, and completes what the CLI has.

mod common;

use common::{PLUS_ROM_CHECKSUM, write_disk, write_rom};
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

/// With no `HOME` or XDG directories, every other command fails.
#[test]
fn generating_touches_nothing_and_needs_no_setup() {
    let root = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(["completions", "bash"])
        .current_dir(root.path())
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
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

/// A data directory holding ROM `plus-v3`, system `sys-608`, media
/// `disk-1`, and machines `alpha` and `beta`.
fn registered() -> TempDir {
    let root = TempDir::new().unwrap();
    let ok = |args: &[&str]| {
        let output = macman(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
    };
    let rom = write_rom(root.path(), "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
    let disk = write_disk(root.path(), "sys.dsk");
    ok(&["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"]);
    ok(&["system", "add", disk.to_str().unwrap(), "--name", "sys-608"]);
    ok(&["media", "add", disk.to_str().unwrap(), "--name", "disk-1"]);
    for name in ["beta", "alpha"] {
        ok(&[
            "create", name, "--model", "plus", "--rom", "plus-v3", "--system", "sys-608",
        ]);
    }
    root
}

/// `program` running `check` with `args`, with the same data directories as
/// [`macman`], and this macman first on `PATH`, as the scripts expect.
fn in_shell(root: &Path, program: &str, flags: &[&str], check: &str, args: &[&Path]) -> String {
    let bin = Path::new(env!("CARGO_BIN_EXE_macman")).parent().unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(program)
        .args(flags)
        .args(["-c", check, "check"])
        .args(args)
        .env("PATH", path)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn names_are_listed_for_scripts() {
    let root = registered();
    let names = |kind: &str| {
        let output = macman(root.path(), &["__complete-names", kind]);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(names("machines"), "alpha\nbeta\n");
    assert_eq!(names("roms"), "plus-v3\n");
    assert_eq!(names("systems"), "sys-608\n");
    assert_eq!(names("media"), "disk-1\n");
    assert_eq!(names("other"), "");
}

#[test]
fn listing_names_never_fails_or_changes_anything() {
    let root = TempDir::new().unwrap();
    for kind in ["machines", "roms", "systems", "media"] {
        let output = macman(root.path(), &["__complete-names", kind]);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);

    // With nowhere to look, there are no names.
    let output = Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(["__complete-names", "machines"])
        .env_remove("HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn the_name_lister_is_not_offered_as_a_command() {
    for shell in ["zsh", "bash", "fish", "powershell", "elvish"] {
        let text = script(shell);
        // Only the scripts' own calls to it mention it.
        for line in text
            .lines()
            .filter(|line| line.contains("__complete-names"))
        {
            assert!(line.contains("macman __complete-names"), "{shell}: {line}");
        }
    }
    let root = TempDir::new().unwrap();
    let help = macman(root.path(), &["--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("__complete"));
}

#[test]
fn bash_completes_names() {
    if !have("bash") {
        eprintln!("skipping: bash isn't installed");
        return;
    }
    let root = registered();
    let file = root.path().join("macman.bash");
    std::fs::write(&file, script("bash")).unwrap();
    // Complete each command line, whose last word is the one being typed.
    let check = r#"
        source "$1"
        complete_line() {
            read -ra COMP_WORDS <<< "$1"
            [[ $1 == *" " ]] && COMP_WORDS+=("")
            COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 ))
            COMPREPLY=()
            _macman_with_names macman "${COMP_WORDS[COMP_CWORD]}" "${COMP_WORDS[COMP_CWORD-1]}"
            echo "$1-> ${COMPREPLY[*]}"
        }
        complete_line "macman use "
        complete_line "macman run a"
        complete_line "macman -v run --disk extra.img "
        complete_line "macman export alpha "
        complete_line "macman clone alpha "
        complete_line "macman config show "
        complete_line "macman remove "
        complete_line "macman system remove "
        complete_line "macman create new --rom "
        complete_line "macman create new --system s"
        complete_line "macman create "
        complete_line "macman use --"
        complete_line "macman media info "
        complete_line "macman media remove d"
        complete_line "macman run alpha --media "
        complete_line "macman run --media disk-1 --media "
        complete_line "macman create new --media "
        complete_line "macman export --media "
    "#;
    let output = in_shell(root.path(), "bash", &[], check, &[&file]);
    let expected = "\
macman use -> alpha beta
macman run a-> alpha
macman -v run --disk extra.img -> alpha beta
macman export alpha -> alpha beta
macman clone alpha -> -v -h --verbose --json --help
macman config show -> alpha beta
macman remove -> alpha beta
macman system remove -> sys-608
macman create new --rom -> plus-v3
macman create new --system s-> sys-608
macman create -> -v -h --backend --model --rom --system --disk-size --memory --display --scale --fullscreen --cpu --no-fpu --no-sound --network --shared-folder --ephemeral --media --verbose --json --help
macman use ---> --verbose --json --help
macman media info -> disk-1
macman media remove d-> disk-1
macman run alpha --media -> disk-1
macman run --media disk-1 --media -> disk-1
macman create new --media -> disk-1
macman export --media -> disk-1
";
    assert_eq!(output, expected);

    // A new medium's file is a path, and its name a new one: neither is offered
    // a registered name.
    let check = format!(
        "{}\n{}",
        check.lines().take(10).collect::<Vec<_>>().join("\n"),
        r#"
        complete_line "macman media add d"
        complete_line "macman media add x.img --name d"
    "#
    );
    let output = in_shell(root.path(), "bash", &[], &check, &[&file]);
    assert_eq!(output.lines().count(), 2, "{output}");
    assert!(!output.contains("disk-1"), "{output}");
}

#[test]
fn zsh_completes_names() {
    if !have("zsh") {
        eprintln!("skipping: zsh isn't installed");
        return;
    }
    let text = script("zsh");
    for expected in [
        "':name:_macman_names machines'",
        ":ROM:_macman_names roms'",
        ":SYSTEM:_macman_names systems'",
        ":MEDIA:_macman_names media'",
        "'*::names:_macman_names machines'",
    ] {
        assert!(text.contains(expected), "zsh lacks {expected}");
    }

    let root = registered();
    let file = root.path().join("_macman");
    std::fs::write(&file, text).unwrap();
    // Load the script outside a completion, with stand-ins for the parts of
    // zsh's completion system it calls, and ask for each kind of name.
    let check = r#"
        compdef() { }
        compadd() { print -rl -- "${(@P)${@[-1]}}" }
        source "$1"
        _macman_names machines
        _macman_names roms
        _macman_names systems
        _macman_names media
        # As _arguments really calls it, with compadd's options first.
        _macman_names -J -default- systems
    "#;
    let output = in_shell(root.path(), "zsh", &["-f"], check, &[&file]);
    assert_eq!(output, "alpha\nbeta\nplus-v3\nsys-608\ndisk-1\nsys-608\n");
}

#[test]
fn fish_completes_names() {
    let text = script("fish");
    for expected in [
        r#"complete -c macman -n "__fish_macman_using_subcommand use" -f -a "(macman __complete-names machines)""#,
        r#"complete -c macman -n "__fish_macman_using_subcommand create" -l rom -r -f -a "(macman __complete-names roms)""#,
        r#"complete -c macman -n "__fish_macman_using_subcommand system; and __fish_seen_subcommand_from remove" -f -a "(macman __complete-names systems)""#,
        r#"complete -c macman -n "__fish_macman_using_subcommand media; and __fish_seen_subcommand_from info" -f -a "(macman __complete-names media)""#,
        r#"complete -c macman -n "__fish_macman_using_subcommand run" -l media -r -f -a "(macman __complete-names media)""#,
    ] {
        assert!(text.contains(expected), "fish lacks {expected}");
    }
    if have("fish") {
        let root = registered();
        let file = root.path().join("macman.fish");
        std::fs::write(&file, &text).unwrap();
        let check = r#"source $argv[1]; complete -C "macman use ""#;
        let output = in_shell(root.path(), "fish", &[], check, &[&file]);
        assert!(
            output.contains("alpha") && output.contains("beta"),
            "{output}"
        );
    }
}

#[test]
fn config_set_completes_machines_then_keys_then_values() {
    let root = registered();
    let offered = |words: &[&str]| {
        let mut args = vec!["__complete-names", "config-set"];
        args.extend_from_slice(words);
        let output = macman(root.path(), &args);
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{output:?}"
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(
        offered(&[]),
        "alpha\nbeta\n",
        "no current machine: names only"
    );
    let keys = offered(&["alpha"]);
    assert!(
        keys.starts_with("ephemeral\nhardware.rom\nhardware.memory\n"),
        "{keys}"
    );
    assert!(
        !keys.contains("alpha") && !keys.contains("basilisk2."),
        "{keys}"
    );
    assert_eq!(offered(&["alpha", "display.depth"]), "1\n");
    assert_eq!(offered(&["alpha", "ephemeral"]), "true\nfalse\n");
    assert!(!offered(&["alpha", "ephemeral", "true"]).contains("ephemeral"));
    assert_eq!(offered(&["alpha", "no.such"]), "");
    assert_eq!(offered(&["nobody", "ephemeral"]), "");

    let output = macman(root.path(), &["use", "beta"]);
    assert!(output.status.success());
    let first = offered(&[]);
    assert!(first.starts_with("alpha\nbeta\nephemeral\n"), "{first}");
    assert_eq!(offered(&["display.depth"]), "1\n", "the current machine's");
}

#[test]
fn every_shell_completes_config_set_by_position() {
    let zsh = script("zsh");
    assert!(zsh.contains(":_macman_names config-set'"), "zsh");
    assert!(
        zsh.contains("before+=(\"${(@)words[1,CURRENT-1]}\")"),
        "zsh"
    );
    assert!(
        zsh.contains("before=(--machine \"${opt_args[--machine]}\")"),
        "zsh"
    );
    assert!(
        zsh.contains(":MACHINE:_macman_names machines'"),
        "zsh: --machine takes names"
    );
    let fish = script("fish");
    assert!(
        fish.contains(r#"complete -c macman -n "__fish_macman_using_subcommand config; and __fish_seen_subcommand_from set" -f -a "(macman __complete-names config-set (__fish_macman_config_set_words))""#),
        "fish"
    );
    assert!(
        fish.contains("function __fish_macman_config_set_words"),
        "fish"
    );

    let root = registered();
    if have("bash") {
        let file = root.path().join("macman.bash");
        std::fs::write(&file, script("bash")).unwrap();
        let check = r#"
            source "$1"
            complete_line() {
                read -ra COMP_WORDS <<< "$1"
                [[ $1 == *" " ]] && COMP_WORDS+=("")
                COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 ))
                COMPREPLY=()
                _macman_with_names macman "${COMP_WORDS[COMP_CWORD]}" "${COMP_WORDS[COMP_CWORD-1]}"
                echo "$1-> ${COMPREPLY[*]}"
            }
            complete_line "macman config set "
            complete_line "macman config set alpha display.d"
            complete_line "macman config set alpha display.depth "
            complete_line "macman -v config set alpha ephemeral "
            complete_line "macman config set alpha ephemeral true display.f"
            complete_line "macman config set alpha display.scale 2 hardware.memory 4"
            complete_line "macman config set --machine "
            complete_line "macman config set --machine alpha "
            complete_line "macman config set --machine alpha display.depth "
            complete_line "macman config set ephemeral true --machine alpha display.d"
        "#;
        let output = in_shell(root.path(), "bash", &[], check, &[&file]);
        let expected = "\
macman config set -> alpha beta
macman config set alpha display.d-> display.depth
macman config set alpha display.depth -> 1
macman -v config set alpha ephemeral -> true false
macman config set alpha ephemeral true display.f-> display.fullscreen
macman config set alpha display.scale 2 hardware.memory 4-> 4M
macman config set --machine -> alpha beta
macman config set --machine alpha -> ephemeral hardware.rom hardware.memory display.width display.height display.depth display.scale display.fullscreen
macman config set --machine alpha display.depth -> 1
macman config set ephemeral true --machine alpha display.d-> display.depth
";
        assert_eq!(output, expected);
    }
    if have("zsh") {
        let file = root.path().join("_macman");
        std::fs::write(&file, &zsh).unwrap();
        // Within `config set`, zsh's $words are its own words, as a real
        // completion has them.
        let check = r#"
            compdef() { }
            compadd() { print -rl -- "${(@P)${@[-1]}}" }
            source "$1"
            words=(alpha display.depth ''); CURRENT=3; _macman_names -J -default- config-set
            words=(alpha ephemeral ''); CURRENT=3; _macman_names config-set
            words=(''); CURRENT=1; _macman_names config-set
            typeset -A opt_args; opt_args=(--machine alpha)
            words=(display.depth ''); CURRENT=2; _macman_names -J -default- config-set
            words=(''); CURRENT=1; _macman_names -J -default- config-set
        "#;
        let output = in_shell(root.path(), "zsh", &["-f"], check, &[&file]);
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(
            lines[..6],
            ["1", "true", "false", "alpha", "beta", "1"],
            "{output}"
        );
        // With --machine, the first word is a key: no machine names.
        assert_eq!(lines[6], "ephemeral", "{output}");
        assert!(!lines[6..].contains(&"alpha"), "{output}");
    }
    if have("fish") {
        let file = root.path().join("macman.fish");
        std::fs::write(&file, &fish).unwrap();
        let check = r#"source $argv[1]
            complete -C "macman config set alpha display.depth "
            echo --
            complete -C "macman config set alpha "
            echo --
            complete -C "macman -v config set alpha ephemeral "
            echo --
            complete -C "macman config set --machine alpha display.depth ""#;
        let output = in_shell(root.path(), "fish", &[], check, &[&file]);
        let parts: Vec<_> = output.split("--\n").collect();
        assert_eq!(parts[0].trim(), "1", "{output}");
        assert!(
            parts[1].contains("hardware.memory") && !parts[1].contains("alpha"),
            "{output}"
        );
        let values: Vec<_> = parts[2].lines().collect();
        assert_eq!(values, ["true", "false"], "{output}");
        assert_eq!(parts[3].trim(), "1", "{output}");
    }
}
