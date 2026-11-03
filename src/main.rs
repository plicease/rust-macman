use anyhow::Result;
use clap::Parser;
use clap::error::ErrorKind;
use macman::app::{App, print_completions, print_names};
use macman::cli::{COMPLETE_NAMES, Cli, Command, NameKind};
use macman::default_backends;
use macman::paths::Paths;
use macman::report::error_json;
use std::process::ExitCode;

fn run(cli: Cli) -> Result<u8> {
    // Completion needs nothing from the environment, so it works anywhere.
    if let Command::Completions { shell } = cli.command {
        return print_completions(shell, cli.json);
    }

    let paths = Paths::from_env()?;
    let backends = default_backends(&paths)?;
    App::new(paths, backends).execute(cli)
}

/// Whether `--json` was asked for, before the arguments could be parsed.
fn wants_json() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
}

/// `macman __complete-names KIND [WORD]...`, for completion scripts: names on stdout,
/// and never an error, because no names is the worst completion should do.
fn complete_names() -> Option<ExitCode> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|first| first != COMPLETE_NAMES) {
        return None;
    }
    // Anything after the kind is the words typed before the one completed.
    if let [_, kind, words @ ..] = args.as_slice()
        && let Some(kind) = kind.to_str().and_then(NameKind::parse)
        && let Ok(paths) = Paths::from_env()
    {
        let words: Vec<String> = words
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        print_names(&paths, kind, &words);
    }
    Some(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    if let Some(code) = complete_names() {
        return code;
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and the version are asked for, so they print as they always do.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.exit()
        }
        Err(error) => {
            if wants_json() {
                let message = error.kind().as_str().unwrap_or("invalid arguments");
                println!("{}", error_json(message));
            }
            error.exit()
        }
    };
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if json {
                println!("{}", error_json(&format!("{error:#}")));
            }
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
