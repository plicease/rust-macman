//! MacMan: a manager for named classic Macintosh emulation environments.

pub mod app;
pub mod archive;
pub mod backend;
pub mod cli;
pub mod disk;
pub mod fsutil;
pub mod machine;
pub mod model;
pub mod paths;
pub mod registry;

use anyhow::Result;
use backend::Backends;
use backend::minivmac::{MiniVmac, SourcePin};
use machine::Config;
use paths::Paths;

/// The production backends; the first is the default for new machines.
pub fn default_backends(paths: &Paths) -> Result<Backends> {
    let config = Config::load(paths)?;
    Ok(Backends::new(vec![Box::new(MiniVmac::new(
        paths.clone(),
        SourcePin::default(),
        config.minivmac.source_archive,
    ))]))
}
