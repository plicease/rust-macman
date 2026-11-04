//! `macman completions`: shell scripts generated from the clap definition by
//! `clap_complete`, plus completion of machine, ROM, system, and media names.
//!
//! The names are only known at run time, so the scripts ask for them: the zsh,
//! bash, and fish scripts run `macman __complete-names KIND` where an argument
//! takes a name. The arguments that do are marked in `crate::cli` with an
//! otherwise unused value hint ([`NameKind::hint`]). That mark is found two
//! ways: zsh's generated script carries it, so it's swapped in place; bash's
//! and fish's don't, so code for them is written from [`name_args`].
//! PowerShell and elvish get the generated scripts as they are.

use crate::cli::{COMPLETE_NAMES, Cli, NameKind};
use clap::CommandFactory;
use clap_complete::Shell;

/// The completion script for `shell`.
pub fn script(shell: Shell) -> Vec<u8> {
    let mut command = Cli::command();
    command.build();
    let mut generated = Vec::new();
    clap_complete::generate(shell, &mut command, "macman", &mut generated);
    let generated = String::from_utf8(generated).expect("clap_complete writes UTF-8");
    let args = name_args(&command);
    match shell {
        Shell::Zsh => zsh(&generated),
        Shell::Bash => bash(&generated, &command, &args),
        Shell::Fish => fish(&generated, &command, &args),
        _ => generated,
    }
    .into_bytes()
}

/// An argument that takes a registered name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameArg {
    /// The subcommands that lead to it, such as `["system", "remove"]`.
    pub path: Vec<String>,
    pub place: Place,
    pub kind: NameKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// `--long VALUE`.
    Option { long: String },
    /// The positional argument at `index`, counting from 0, and every one
    /// after it if it `repeats`.
    Positional { index: usize, repeats: bool },
}

/// Every argument in `command` that takes a registered name.
pub fn name_args(command: &clap::Command) -> Vec<NameArg> {
    let mut found = Vec::new();
    collect(command, &mut Vec::new(), &mut found);
    found
}

fn collect(command: &clap::Command, path: &mut Vec<String>, found: &mut Vec<NameArg>) {
    for (index, arg) in command.get_positionals().enumerate() {
        if let Some(kind) = NameKind::of(arg.get_value_hint()) {
            let repeats = arg.get_num_args().is_some_and(|n| n.max_values() > 1);
            found.push(NameArg {
                path: path.clone(),
                place: Place::Positional { index, repeats },
                kind,
            });
        }
    }
    for arg in command.get_opts() {
        if let (Some(kind), Some(long)) = (NameKind::of(arg.get_value_hint()), arg.get_long()) {
            found.push(NameArg {
                path: path.clone(),
                place: Place::Option {
                    long: long.to_string(),
                },
                kind,
            });
        }
    }
    for sub in command.get_subcommands() {
        path.push(sub.get_name().to_string());
        collect(sub, path, found);
        path.pop();
    }
}

/// Replace `before` in `text`, which must be there, with `after`.
fn splice(text: &str, before: &str, after: &str) -> String {
    assert!(
        text.contains(before),
        "the generated script lacks {before:?}"
    );
    text.replace(before, after)
}

/// zsh keeps each marker hint as its completion function, such as `_users`.
fn zsh(generated: &str) -> String {
    let mut script = generated.to_string();
    for (kind, function) in [
        (NameKind::Machines, "_users"),
        (NameKind::Roms, "_hosts"),
        (NameKind::Systems, "_email_addresses"),
        (NameKind::Media, "_urls"),
        (NameKind::Settings, "_command_names -e"),
        (NameKind::Aliases, "_absolute_command_paths"),
    ] {
        script = splice(
            &script,
            &format!(":{function}'"),
            &format!(":_macman_names {}'", kind.as_str()),
        );
    }
    let helper = format!(
        r#"(( $+functions[_macman_names] )) ||
_macman_names() {{
    # _arguments puts its own options, such as -J and a group, before the
    # kind, which is last; they're compadd's.
    local kind=${{@[-1]}}
    local -a names before
    # `config set` is completed knowing the words before this one: those of
    # $words there, which are its positionals, and its --machine.
    if [[ $kind == config-set ]]; then
        [[ -n ${{opt_args[--machine]}} ]] && before=(--machine "${{opt_args[--machine]}}")
        before+=("${{(@)words[1,CURRENT-1]}}")
    fi
    names=(${{(f)"$(macman {COMPLETE_NAMES} $kind "${{before[@]}}" 2>/dev/null)"}})
    compadd "${{(@)@[1,-2]}}" -a names
}}

if [ "$funcstack[1]" = "_macman" ]; then"#
    );
    splice(
        &script,
        r#"if [ "$funcstack[1]" = "_macman" ]; then"#,
        &helper,
    )
}

/// Bash's generated script completes no positional arguments, so a wrapper
/// handles names, working out where the cursor is, and leaves the rest to it.
fn bash(generated: &str, command: &clap::Command, args: &[NameArg]) -> String {
    // Subcommands that have subcommands, so take another word for the path.
    let groups: Vec<_> = command
        .get_subcommands()
        .filter(|sub| sub.has_subcommands())
        .map(|sub| sub.get_name())
        .collect();
    let takes_value = value_options(command);

    let mut options = String::new();
    let mut positionals = String::new();
    for arg in args {
        let path = arg.path.join(" ");
        let kind = arg.kind.as_str();
        match &arg.place {
            Place::Option { long } => {
                options += &format!("        \"{path}|--{long}\") kind={kind} ;;\n");
            }
            Place::Positional {
                index,
                repeats: false,
            } => {
                positionals += &format!("            \"{path}|{index}\") kind={kind} ;;\n");
            }
            Place::Positional {
                index,
                repeats: true,
            } => {
                positionals += &format!(
                    "            \"{path}|\"*) (( positional >= {index} )) && kind={kind} ;;\n"
                );
            }
        }
    }

    let wrapper = format!(
        r#"# Machine, ROM, and system names, which only macman knows.
_macman_with_names() {{
    local cur="${{COMP_WORDS[COMP_CWORD]}}" prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    local path="" word i positional=0 value_next=0
    for (( i = 1; i < COMP_CWORD; i++ )); do
        word="${{COMP_WORDS[i]}}"
        if (( value_next )); then
            value_next=0
            continue
        fi
        case "$word" in
            {values}) value_next=1 ;;
            -*) ;;
            *)
                case "$path" in
                    "") path="$word" ;;
                    {groups}) path="$path $word" ;;
                    *) positional=$(( positional + 1 )) ;;
                esac
                ;;
        esac
    done

    local kind=""
    case "$path|$prev" in
{options}    esac
    if [[ -z $kind && $cur != -* ]] && (( ! value_next )); then
        case "$path|$positional" in
{positionals}        esac
    fi

    if [[ -n $kind ]]; then
        local IFS=$'\n'
        # `config set` is completed knowing every word after it, options too.
        local -a extra=()
        if [[ $kind == config-set ]]; then
            local seen=0
            for (( i = 1; i < COMP_CWORD; i++ )); do
                word="${{COMP_WORDS[i]}}"
                case $seen in
                    2) extra+=("$word") ;;
                    1) [[ $word == set ]] && seen=2 ;;
                    0) [[ $word == config ]] && seen=1 ;;
                esac
            done
        fi
        COMPREPLY=( $(compgen -W "$(macman {COMPLETE_NAMES} "$kind" "${{extra[@]}}" 2>/dev/null)" -- "$cur") )
        return 0
    fi
    _macman "$@"
}}

if [[ "${{BASH_VERSINFO[0]}}" -eq 4"#,
        values = takes_value.join("|"),
        groups = groups.join("|"),
    );
    let script = splice(generated, r#"if [[ "${BASH_VERSINFO[0]}" -eq 4"#, &wrapper);
    splice(
        &script,
        "complete -F _macman -o",
        "complete -F _macman_with_names -o",
    )
}

/// Options anywhere in `command` that take their value as the next word.
fn value_options(command: &clap::Command) -> Vec<String> {
    let mut found = Vec::new();
    collect_value_options(command, &mut found);
    found.sort();
    found.dedup();
    found
}

fn collect_value_options(command: &clap::Command, found: &mut Vec<String>) {
    for arg in command.get_opts() {
        if arg.get_action().takes_values() {
            found.extend(arg.get_long().map(|long| format!("--{long}")));
            found.extend(arg.get_short().map(|short| format!("-{short}")));
        }
    }
    for sub in command.get_subcommands() {
        collect_value_options(sub, found);
    }
}

/// The words after `config set` on fish's command line, before the cursor,
/// options too, which macman sorts out.
const FISH_CONFIG_SET_WORDS: &str = r#"function __fish_macman_config_set_words
    set -l seen 0
    for token in (commandline -opc)
        switch $seen
            case 2
                printf '%s\n' $token
            case 1
                string match -q -- set $token; and set seen 2
            case 0
                string match -q -- config $token; and set seen 1
        end
    end
end
"#;

/// Whether the word being completed is positional argument INDEX, counting
/// from 0, of the subcommand DEPTH words deep, or any after it with a third
/// argument, `+`. As bash's wrapper does, it skips options, and the values of
/// those that take the next word, and is false where the word is such a value.
fn fish_at_positional(value_options: &[String]) -> String {
    format!(
        r#"function __fish_macman_at_positional
    set -l depth $argv[1]
    set -l index $argv[2]
    set -l path 0
    set -l positional 0
    set -l value_next 0
    for word in (commandline -opc)[2..-1]
        if test $value_next = 1
            set value_next 0
            continue
        end
        switch $word
            case {values}
                set value_next 1
            case '-*'
            case '*'
                if test $path -lt $depth
                    set path (math $path + 1)
                else
                    set positional (math $positional + 1)
                end
        end
    end
    test $value_next = 0; or return 1
    if test "$argv[3]" = +
        test $positional -ge $index
    else
        test $positional -eq $index
    end
end
"#,
        values = value_options.join(" "),
    )
}

/// Fish's generated script has no marks to replace, so this adds completions
/// for each name argument. A positional one is offered only at its own
/// position, worked out as bash's wrapper does.
fn fish(generated: &str, command: &clap::Command, args: &[NameArg]) -> String {
    let mut script = generated.to_string();
    script += "\n# Machine, ROM, and system names, which only macman knows.\n";
    script += FISH_CONFIG_SET_WORDS;
    script += &fish_at_positional(&value_options(command));
    for arg in args {
        let condition = match arg.path.as_slice() {
            [command] => format!("__fish_macman_using_subcommand {command}"),
            [command, sub] => {
                format!(
                    "__fish_macman_using_subcommand {command}; and __fish_seen_subcommand_from {sub}"
                )
            }
            path => unreachable!("macman has no commands nested as deep as {path:?}"),
        };
        let names = match arg.kind {
            // Keyed by the words before the cursor, not by its position alone.
            NameKind::Settings => {
                format!("(macman {COMPLETE_NAMES} config-set (__fish_macman_config_set_words))")
            }
            kind => format!("(macman {COMPLETE_NAMES} {})", kind.as_str()),
        };
        script += &match &arg.place {
            Place::Option { long } => {
                format!("complete -c macman -n \"{condition}\" -l {long} -r -f -a \"{names}\"\n")
            }
            Place::Positional { index, repeats } => {
                let depth = arg.path.len();
                let more = if *repeats { " +" } else { "" };
                format!(
                    "complete -c macman -n \"{condition}; and __fish_macman_at_positional {depth} {index}{more}\" -f -a \"{names}\"\n"
                )
            }
        };
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(path: &[&str], place: Place, kind: NameKind) -> NameArg {
        NameArg {
            path: path.iter().map(|part| part.to_string()).collect(),
            place,
            kind,
        }
    }

    fn at(index: usize) -> Place {
        Place::Positional {
            index,
            repeats: false,
        }
    }

    fn option(long: &str) -> Place {
        Place::Option {
            long: long.to_string(),
        }
    }

    #[test]
    fn every_name_argument_is_found() {
        let mut command = Cli::command();
        command.build();
        let expected = vec![
            arg(&["system", "remove"], at(0), NameKind::Systems),
            arg(&["media", "info"], at(0), NameKind::Media),
            arg(&["media", "remove"], at(0), NameKind::Media),
            arg(&["create"], option("rom"), NameKind::Roms),
            arg(&["create"], option("system"), NameKind::Systems),
            arg(&["create"], option("media"), NameKind::Media),
            arg(&["wizard"], option("media"), NameKind::Media),
            arg(&["use"], at(0), NameKind::Machines),
            arg(&["run"], at(0), NameKind::Machines),
            arg(&["run"], option("media"), NameKind::Media),
            arg(&["config", "show"], at(0), NameKind::Machines),
            arg(&["config", "edit"], at(0), NameKind::Machines),
            arg(
                &["config", "set"],
                Place::Positional {
                    index: 0,
                    repeats: true,
                },
                NameKind::Settings,
            ),
            arg(&["config", "set"], option("machine"), NameKind::Machines),
            arg(&["config", "keys"], at(0), NameKind::Machines),
            arg(&["checkpoint", "create"], at(0), NameKind::Machines),
            arg(&["checkpoint", "list"], at(0), NameKind::Machines),
            arg(&["checkpoint", "delete"], at(0), NameKind::Machines),
            arg(&["rollback"], at(0), NameKind::Machines),
            arg(&["alias"], at(0), NameKind::Machines),
            arg(&["unalias"], at(0), NameKind::Aliases),
            arg(&["clone"], at(0), NameKind::Machines),
            arg(
                &["export"],
                Place::Positional {
                    index: 0,
                    repeats: true,
                },
                NameKind::Machines,
            ),
            arg(&["export"], option("rom"), NameKind::Roms),
            arg(&["export"], option("system"), NameKind::Systems),
            arg(&["export"], option("media"), NameKind::Media),
            arg(&["remove"], at(0), NameKind::Machines),
        ];
        assert_eq!(name_args(&command), expected);
    }

    #[test]
    fn every_shell_gets_a_script() {
        for shell in [
            Shell::Zsh,
            Shell::Bash,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            assert!(!script(shell).is_empty(), "{shell}");
        }
    }

    #[test]
    fn zsh_keeps_no_marker() {
        let text = String::from_utf8(script(Shell::Zsh)).unwrap();
        for marker in ["_users", "_hosts", "_email_addresses", "_urls"] {
            assert!(!text.contains(marker), "{marker}");
        }
    }
}
