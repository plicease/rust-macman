//! Command-line interface (design §5).

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "macman",
    version,
    about = "Classic Macintosh environment manager"
)]
pub struct Cli {
    /// Show build and launch logs, and full paths in diagnostics
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the host, registered files, cache, and machines
    Doctor,
    /// List machines, or cached emulator builds
    List {
        /// List cached emulator builds instead of machines
        #[arg(long)]
        builds: bool,
    },
    /// Manage imported ROMs
    #[command(subcommand)]
    Rom(RomCommand),
    /// Manage registered bootable system images
    #[command(subcommand)]
    System(SystemCommand),
    /// Inspect disk images and extract volumes from them
    #[command(subcommand)]
    Disk(DiskCommand),
    /// Create a machine from a ROM and a private copy of a system image
    Create(CreateArgs),
    /// Set the default machine
    Use { name: String },
    /// Show the default machine
    Current,
    /// Launch a machine (the current one by default)
    Run(RunArgs),
    /// Inspect configuration
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Copy a machine and its private disks
    Clone { source: String, dest: String },
    /// Remove a machine; registered ROMs and system images are kept
    Remove {
        name: String,
        /// Delete the machine's private disks without prompting
        #[arg(long)]
        delete_disks: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RomCommand {
    /// Import a ROM file you legally own
    Add {
        file: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// List imported ROMs
    List,
}

#[derive(Debug, Subcommand)]
pub enum SystemCommand {
    /// Register an existing bootable disk image
    Add {
        file: PathBuf,
        #[arg(long)]
        name: String,
        /// System Software version, for display only
        #[arg(long)]
        version: Option<String>,
        /// Partition to extract from a partitioned disk image, if it has several volumes
        #[arg(long, value_name = "N")]
        partition: Option<u32>,
    },
    /// List registered system images
    List,
}

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// Show an image's format and partitions
    Info { file: PathBuf },
    /// Copy a volume out of a partitioned or Disk Copy image; the source is unchanged
    Extract {
        source: PathBuf,
        dest: PathBuf,
        /// Partition to extract, if the image has several volumes
        #[arg(long, value_name = "N")]
        partition: Option<u32>,
        /// Extract the partition whatever it holds, without checking it's a volume
        #[arg(long, requires = "partition")]
        raw: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    pub name: String,
    /// Hardware model, such as plus, se, classic, ii, or iix
    #[arg(long)]
    pub model: String,
    /// Name of an imported ROM
    #[arg(long)]
    pub rom: String,
    /// Name of a registered system image, cloned into the machine
    #[arg(long)]
    pub system: String,
    /// Memory size, such as 4M (default depends on the model)
    #[arg(long)]
    pub memory: Option<String>,
    /// Display as WIDTHxHEIGHTxDEPTH (default depends on the model)
    #[arg(long)]
    pub display: Option<String>,
    /// Window magnification
    #[arg(long, default_value_t = 2)]
    pub scale: u32,
    /// Start in full screen
    #[arg(long)]
    pub fullscreen: bool,
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    pub name: Option<String>,
    /// Attach an extra disk for this run only (repeatable)
    #[arg(long = "disk", value_name = "FILE")]
    pub disks: Vec<PathBuf>,
    /// Give the emulator read-only throwaway copies of the --disk files
    #[arg(long, requires = "disks")]
    pub readonly: bool,
    /// Attach --disk files whose format macman does not recognize
    #[arg(long, requires = "disks")]
    pub any_format: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a machine's configuration (the current one by default)
    Show { name: Option<String> },
}
