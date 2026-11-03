use anyhow::Result;
use clap::Parser;
use clap::error::ErrorKind;
use macman::app::App;
use macman::cli::Cli;
use macman::default_backends;
use macman::paths::Paths;
use macman::report::error_json;
use std::process::ExitCode;

fn run(cli: Cli) -> Result<u8> {
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

fn main() -> ExitCode {
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
