use anyhow::Result;
use clap::Parser;
use macman::app::App;
use macman::cli::Cli;
use macman::default_backends;
use macman::paths::Paths;
use std::process::ExitCode;

fn run(cli: Cli) -> Result<u8> {
    let paths = Paths::from_env()?;
    let backends = default_backends(&paths)?;
    App::new(paths, backends).execute(cli)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
