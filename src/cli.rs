//! Command-line interface (design §5).

use crate::machine::{Cpu, Network};
use crate::model::ALL_MODELS;
use clap::builder::{PossibleValue, TypedValueParser};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use std::ffi::OsStr;
use std::path::PathBuf;

/// Mark arguments that take a registered name. No argument needs these hints
/// for what they really mean, so `crate::completion` can find them, in the
/// definition and in generated scripts, and complete names there instead.
pub(crate) const MACHINE: ValueHint = ValueHint::Username;
pub(crate) const ROM: ValueHint = ValueHint::Hostname;
pub(crate) const SYSTEM: ValueHint = ValueHint::EmailAddress;

/// The command completion scripts run for names. It isn't a subcommand,
/// because every shell's script would then offer it.
pub const COMPLETE_NAMES: &str = "__complete-names";

/// What `macman __complete-names` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Machines,
    Roms,
    Systems,
}

impl NameKind {
    pub const ALL: [Self; 3] = [Self::Machines, Self::Roms, Self::Systems];

    /// The kind an argument marked with `hint` takes, if any.
    pub fn of(hint: ValueHint) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.hint() == hint)
    }

    pub fn hint(self) -> ValueHint {
        match self {
            Self::Machines => MACHINE,
            Self::Roms => ROM,
            Self::Systems => SYSTEM,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Machines => "machines",
            Self::Roms => "roms",
            Self::Systems => "systems",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "machines" => Some(Self::Machines),
            "roms" => Some(Self::Roms),
            "systems" => Some(Self::Systems),
            _ => None,
        }
    }
}

/// The backends a machine can name, for completion. A test checks it against
/// what this macman really has.
pub const BACKEND_NAMES: [&str; 3] = ["minivmac", "basilisk2", "sheepshaver"];

/// Accepts any string, as `String` does, so macman itself explains a bad
/// value, but offers `choices` to shell completion and `--help`.
#[derive(Clone)]
struct Suggest(fn() -> Vec<&'static str>);

impl TypedValueParser for Suggest {
    type Value = String;

    fn parse_ref(
        &self,
        command: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<String, clap::Error> {
        clap::builder::StringValueParser::new().parse_ref(command, arg, value)
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new((self.0)().into_iter().map(PossibleValue::new)))
    }
}

fn backends() -> Suggest {
    Suggest(|| BACKEND_NAMES.to_vec())
}

fn models() -> Suggest {
    Suggest(|| ALL_MODELS.iter().map(|model| model.name()).collect())
}

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

    /// Print the result as JSON, for commands that report data (see docs/JSON.md)
    #[arg(long, global = true)]
    pub json: bool,

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
    Use {
        #[arg(value_name = "MACHINE", value_hint = MACHINE)]
        name: String,
    },
    /// Show the default machine
    Current,
    /// Launch a machine (the current one by default)
    Run(RunArgs),
    /// Inspect configuration
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Copy a machine and its private disks
    Clone {
        #[arg(value_name = "MACHINE", value_hint = MACHINE)]
        source: String,
        dest: String,
    },
    /// Write machines, with their ROMs and disks, to an archive for another host
    Export {
        #[arg(required_unless_present = "all", value_name = "MACHINE", value_hint = MACHINE)]
        names: Vec<String>,
        /// Export every machine
        #[arg(long, conflicts_with = "names")]
        all: bool,
        /// Archive to create, such as machines.tar.gz
        #[arg(short, long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        output: PathBuf,
    },
    /// Bring in machines from an archive; existing data is never replaced
    Import {
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Import an independent copy under this name (one-machine archives only)
        #[arg(long, value_name = "NAME")]
        rename: Option<String>,
        /// Check the archive and show what an import would do, changing nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a machine; registered ROMs and system images are kept
    Remove {
        #[arg(value_name = "MACHINE", value_hint = MACHINE)]
        name: String,
        /// Delete the machine's private disks without prompting
        #[arg(long)]
        delete_disks: bool,
    },
    /// Print a shell completion script, such as for `macman completions zsh > ~/.zfunc/_macman`
    Completions {
        /// The shell to complete for
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum RomCommand {
    /// Import a ROM file you legally own
    Add {
        #[arg(value_hint = ValueHint::FilePath)]
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
        #[arg(value_hint = ValueHint::FilePath)]
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
    /// Delete a registered system image; machines made from it keep their own disks
    Remove {
        #[arg(value_name = "SYSTEM", value_hint = SYSTEM)]
        name: String,
        /// Delete it without prompting
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// Show an image's format and partitions
    Info {
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
    },
    /// Copy a volume out of a partitioned or Disk Copy image; the source is unchanged
    Extract {
        #[arg(value_hint = ValueHint::FilePath)]
        source: PathBuf,
        #[arg(value_hint = ValueHint::FilePath)]
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
    /// Emulator: minivmac (the default), basilisk2, or sheepshaver
    #[arg(long, value_parser = backends(), hide_possible_values = true)]
    pub backend: Option<String>,
    /// Hardware model, such as plus, se, classic, ii, or iix (Mini vMac only)
    #[arg(long, value_parser = models(), hide_possible_values = true)]
    pub model: Option<String>,
    /// Name of an imported ROM
    #[arg(long, value_name = "ROM", value_hint = ROM)]
    pub rom: String,
    /// Name of a registered system image, cloned into the machine
    #[arg(
        long,
        value_name = "SYSTEM",
        value_hint = SYSTEM,
        required_unless_present = "disk_size",
        conflicts_with = "disk_size"
    )]
    pub system: Option<String>,
    /// Start with a blank boot disk of this size, such as 1G, to install onto from
    /// `macman run --cdrom` (SheepShaver, and Basilisk II on Linux)
    #[arg(long, value_name = "SIZE")]
    pub disk_size: Option<String>,
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
    /// Emulated CPU (Basilisk II only; default 68040)
    #[arg(long, value_enum)]
    pub cpu: Option<Cpu>,
    /// Leave out the FPU (Basilisk II only, and not with a 68040)
    #[arg(long)]
    pub no_fpu: bool,
    /// Turn off sound (Basilisk II and SheepShaver only)
    #[arg(long)]
    pub no_sound: bool,
    /// Guest networking (Basilisk II and SheepShaver only; default none)
    #[arg(long, value_enum)]
    pub network: Option<Network>,
    /// Host folder to show the guest as a volume (Basilisk II and SheepShaver only)
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub shared_folder: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[arg(value_name = "MACHINE", value_hint = MACHINE)]
    pub name: Option<String>,
    /// Attach an extra disk for this run only (repeatable)
    #[arg(long = "disk", value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub disks: Vec<PathBuf>,
    /// Give the emulator read-only throwaway copies of the --disk files
    #[arg(long, requires = "disks")]
    pub readonly: bool,
    /// Attach --disk files whose format macman does not recognize
    #[arg(long, requires = "disks")]
    pub any_format: bool,
    /// Attach a CD image, read-only, for this run only, and start from it (repeatable;
    /// SheepShaver, and Basilisk II on Linux)
    #[arg(long = "cdrom", value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub cdroms: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a machine's configuration (the current one by default)
    Show {
        #[arg(value_name = "MACHINE", value_hint = MACHINE)]
        name: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use clap::CommandFactory;

    #[test]
    fn completion_offers_the_backends_this_macman_has() {
        let root = tempfile::tempdir().unwrap();
        let backends = crate::default_backends(&Paths::under(root.path())).unwrap();
        assert_eq!(backends.names(), BACKEND_NAMES);
    }

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
