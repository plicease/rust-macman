//! MacMan: a manager for named classic Macintosh emulation environments.

pub mod app;
pub mod archive;
pub mod backend;
pub mod checkpoint;
pub mod cli;
pub mod completion;
pub mod disk;
pub mod edit;
pub mod fsutil;
pub mod machine;
pub mod media;
pub mod model;
pub mod paths;
pub mod registry;
pub mod report;
pub mod settings;
pub mod tui;
pub mod usage;

use anyhow::Result;
use backend::Backends;
use backend::basilisk2::Basilisk2;
use backend::macemu;
use backend::macemu_windows;
use backend::minivmac::{MiniVmac, SourcePin};
use backend::sheepshaver::SheepShaver;
use machine::Config;
use paths::Paths;

/// The production backends; the first is the default for new machines.
pub fn default_backends(paths: &Paths) -> Result<Backends> {
    let config = Config::load(paths)?;
    Ok(Backends::new(vec![
        Box::new(MiniVmac::new(
            paths.clone(),
            SourcePin::default(),
            config.minivmac.source_archive,
        )),
        Box::new(
            Basilisk2::new(
                paths.clone(),
                macemu::pin(),
                config.basilisk2.source_archive,
            )
            .with_sdl(macemu_windows::sdl_pin(), config.sdl.source_archive.clone()),
        ),
        Box::new(
            SheepShaver::new(
                paths.clone(),
                macemu::pin(),
                config.sheepshaver.source_archive,
            )
            .with_sdl(macemu_windows::sdl_pin(), config.sdl.source_archive),
        ),
    ]))
}
