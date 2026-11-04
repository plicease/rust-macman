//! The wizard's recommendations and installer guidance, as plain rules over
//! what was observed, what was guessed, and what this host can do. Nothing
//! here reads files or draws anything, so every rule is tested on its own.

use super::inspect::{Confidence, Observation, OsGuess, Version};
use crate::disk::DiskFormat;
use crate::machine::QemuMachine;
use crate::media::MediaKind;
use crate::model::Model;

pub const MINIVMAC: &str = "minivmac";
pub const BASILISK2: &str = "basilisk2";
pub const SHEEPSHAVER: &str = "sheepshaver";
pub const QEMU: &str = "qemu-ppc";

/// A blank boot disk's size when nothing calls for more.
pub const DEFAULT_DISK_SIZE: &str = "1G";

/// Mac OS X's own disk space requirements, as Apple gave them, with the size
/// the wizard suggests for a blank disk: the requirement, with room for
/// applications and updates. Each is (from, least in megabytes, why, suggested).
const MAC_OS_X_DISKS: &[(&[u32], u64, &str, &str)] = &[
    (
        &[10, 5],
        9 * 1024,
        "Mac OS X 10.5 needs 9GB of disk space",
        "20G",
    ),
    (
        &[10, 4],
        4 * 1024,
        "Mac OS X 10.4 needs 3GB of disk space, and 4GB with the Xcode developer tools",
        "10G",
    ),
    (
        &[10, 3],
        1536,
        "Mac OS X 10.3 needs 1.5GB of disk space",
        "6G",
    ),
    (
        &[10, 2],
        3 * 1024,
        "Mac OS X 10.2 needs 3GB of disk space",
        "6G",
    ),
    (
        &[10, 0],
        1536,
        "Mac OS X 10.0 and 10.1 need 1.5GB of disk space",
        "4G",
    ),
];

fn mac_os_x_disk(
    guess: Option<&OsGuess>,
) -> Option<&'static (&'static [u32], u64, &'static str, &'static str)> {
    let guess = guess.filter(|g| g.family == "Mac OS X")?;
    MAC_OS_X_DISKS
        .iter()
        .find(|(from, ..)| guess.version.at_least(from))
}

/// The size to suggest for a blank boot disk, for the system `guess` names.
pub fn suggested_disk_size(guess: Option<&OsGuess>) -> &'static str {
    mac_os_x_disk(guess).map_or(DEFAULT_DISK_SIZE, |(.., suggested)| suggested)
}

/// Why a blank disk of `bytes` is too small for the system `guess` names, if
/// it is.
pub fn disk_too_small(guess: Option<&OsGuess>, bytes: u64) -> Option<String> {
    let (_, least, why, suggested) = mac_os_x_disk(guess)?;
    (bytes < least << 20).then(|| format!("{why}; try {suggested}"))
}

/// Which machine of a backend that has several: a Mini vMac model, or the
/// Power Mac QEMU emulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Model(Model),
    Qemu(QemuMachine),
}

impl Variant {
    /// What `create --model` takes for it.
    pub fn argument(self) -> String {
        match self {
            Self::Model(model) => model.to_string(),
            Self::Qemu(kind) => kind.to_string(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Model(model) => model_label(model),
            Self::Qemu(QemuMachine::Mac99) => "mac99, a New World Power Mac G4",
            Self::Qemu(QemuMachine::G3beige) => "g3beige, an Old World Power Mac G3",
        }
    }

    fn order(self) -> usize {
        match self {
            Self::Model(model) => MODEL_ORDER.iter().position(|o| *o == model).unwrap_or(99),
            Self::Qemu(QemuMachine::Mac99) => 0,
            Self::Qemu(QemuMachine::G3beige) => 1,
        }
    }

    fn model(self) -> Option<Model> {
        match self {
            Self::Model(model) => Some(model),
            Self::Qemu(_) => None,
        }
    }
}

/// What the user starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// A disk with a system already installed, to copy as the boot disk.
    SystemDisk,
    /// An installer CD, to attach to a machine with a blank disk.
    InstallerCd,
    /// An installer or startup floppy.
    InstallerFloppy,
    /// No image: pick everything by hand.
    Manual,
}

pub const STARTS: [Start; 4] = [
    Start::SystemDisk,
    Start::InstallerCd,
    Start::InstallerFloppy,
    Start::Manual,
];

impl Start {
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemDisk => "A disk with a system installed",
            Self::InstallerCd => "An installer CD image",
            Self::InstallerFloppy => "An installer or startup floppy image",
            Self::Manual => "Nothing: set up a machine by hand",
        }
    }

    pub fn needs_image(self) -> bool {
        self != Self::Manual
    }

    /// The start the image looks like, as a suggestion the user can change.
    pub fn suggested(observation: &Observation, guess: Option<&OsGuess>) -> Self {
        if observation.kind() == Some(MediaKind::Floppy) {
            Self::InstallerFloppy
        } else if guess.is_some_and(|g| g.installer) || observation.cd_layout() {
            Self::InstallerCd
        } else if observation.startable() || observation.cd.is_err() {
            Self::SystemDisk
        } else {
            Self::InstallerCd
        }
    }
}

/// One backend, and model for Mini vMac or QEMU, as this host can have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub backend: String,
    pub variant: Option<Variant>,
    /// A registered ROM it starts from, none for a backend with its own
    /// firmware, or why it can't be made here at all.
    pub rom: Result<Option<String>, String>,
    pub installs_from_cd: bool,
}

/// How well a route fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    Good,
    /// It can be made, but the guess says it won't work. The guess may be wrong.
    Unlikely(String),
    /// It can't be made here, for this reason.
    Blocked(String),
}

/// A backend, and model, the wizard offers, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub backend: String,
    pub variant: Option<Variant>,
    pub rom: Option<String>,
    pub fit: Fit,
    pub confidence: Confidence,
    pub reasons: Vec<String>,
    /// Among the best fits, which are all offered as equals.
    pub recommended: bool,
}

impl Route {
    pub fn label(&self) -> String {
        let backend = backend_label(&self.backend);
        match self.variant {
            Some(variant) => format!("{backend}: {}", variant.label()),
            None => backend.to_string(),
        }
    }

    pub fn usable(&self) -> bool {
        !matches!(self.fit, Fit::Blocked(_))
    }
}

pub fn backend_label(backend: &str) -> &str {
    match backend {
        MINIVMAC => "Mini vMac",
        BASILISK2 => "Basilisk II",
        SHEEPSHAVER => "SheepShaver",
        QEMU => "QEMU",
        other => other,
    }
}

fn model_label(model: Model) -> &'static str {
    match model {
        Model::Twig43 => "Twiggy prototype (4.3)",
        Model::Twiggy => "Twiggy prototype",
        Model::Mac128K => "Macintosh 128K",
        Model::Mac512Ke => "Macintosh 512Ke",
        Model::Plus => "Macintosh Plus",
        Model::Se => "Macintosh SE",
        Model::SeFdhd => "Macintosh SE FDHD",
        Model::Classic => "Macintosh Classic",
        Model::Pb100 => "PowerBook 100",
        Model::II => "Macintosh II",
        Model::IIx => "Macintosh IIx",
    }
}

/// Mini vMac models in the order they're offered: the ones most systems
/// suit first.
pub const MODEL_ORDER: [Model; 11] = [
    Model::Plus,
    Model::Se,
    Model::Classic,
    Model::SeFdhd,
    Model::II,
    Model::IIx,
    Model::Mac512Ke,
    Model::Pb100,
    Model::Mac128K,
    Model::Twiggy,
    Model::Twig43,
];

/// The systems each backend runs, as upstream documents them. For QEMU, the
/// releases sharing Mac OS 9.2.1's New World support, which started, and not
/// 9.0.4, which didn't, and Mac OS X 10.4, whose installer started.
fn runs(backend: &str, variant: Option<Variant>, version: &Version) -> Result<(), String> {
    let mac99 = matches!(variant, None | Some(Variant::Qemu(QemuMachine::Mac99)));
    if backend == QEMU && mac99 && version.at_least(&[10, 4]) && version.at_most(&[10, 4]) {
        return Ok(());
    }
    let (low, high, what): (&[u32], &[u32], &str) = match (backend, variant) {
        (QEMU, Some(Variant::Qemu(QemuMachine::G3beige))) => {
            return Err(
                "QEMU's g3beige starts from OpenBIOS, which can't start classic Mac OS: that \
                 needs an Old World ROM. It's for PowerPC Mac OS X, which hasn't been tried on it"
                    .to_string(),
            );
        }
        (QEMU, _) => (
            &[9, 1],
            &[9, 2, 2],
            "QEMU's mac99 starts Mac OS 9.1 to 9.2.2 from OpenBIOS, and Mac OS X 10.4's \
             installer; a Mac OS 9.0.4 CD didn't start on it when tried, and other Mac OS X \
             releases haven't been",
        ),
        _ => (&[], &[], ""),
    };
    if !what.is_empty() {
        return if version.at_least(low) && version.at_most(high) {
            Ok(())
        } else {
            Err(what.to_string())
        };
    }
    let (low, high, what): (&[u32], &[u32], &str) = match backend {
        MINIVMAC => (
            &[1],
            &[7, 5, 5],
            "Mini vMac emulates compact Macs and the Mac II, which run System 1 to 7.5.5",
        ),
        BASILISK2 => (
            &[7],
            &[8, 1],
            "Basilisk II emulates a 68k Mac, which runs System 7.0 to Mac OS 8.1",
        ),
        SHEEPSHAVER => (
            &[7, 5, 2],
            &[9, 0, 4],
            "SheepShaver emulates an Old World PowerPC Mac, which runs Mac OS 7.5.2 to 9.0.4",
        ),
        _ => return Ok(()),
    };
    if version.at_least(low) && version.at_most(high) {
        Ok(())
    } else {
        Err(what.to_string())
    }
}

/// Where each backend sits among equally good ones, for a system: the one
/// made for its era first.
fn preference(backend: &str, guess: Option<&OsGuess>, observation: Option<&Observation>) -> u32 {
    let small = observation.is_some_and(|o| {
        o.kind() == Some(MediaKind::Floppy)
            || o.volumes.iter().any(|v| v.format == DiskFormat::RawMfs)
    });
    let ppc_era = guess.is_some_and(|g| g.version.at_least(&[7, 6]));
    let order: [&str; 4] = if small {
        [MINIVMAC, BASILISK2, SHEEPSHAVER, QEMU]
    } else if ppc_era {
        [SHEEPSHAVER, QEMU, BASILISK2, MINIVMAC]
    } else {
        [BASILISK2, SHEEPSHAVER, QEMU, MINIVMAC]
    };
    order.iter().position(|b| *b == backend).unwrap_or(4) as u32
}

/// Every backend, and model, ranked for `start`, from what was seen and
/// guessed, and the `candidates` this host has. Several can be recommended:
/// when a system runs on two emulators, both are offered as equals.
pub fn routes(
    start: Start,
    observation: Option<&Observation>,
    guess: Option<&OsGuess>,
    candidates: &[Candidate],
) -> Vec<Route> {
    let mut routes: Vec<Route> = candidates
        .iter()
        .map(|candidate| route(start, observation, guess, candidate))
        .collect();
    let rank = |route: &Route| match route.fit {
        Fit::Good => 0,
        Fit::Unlikely(_) => 1,
        Fit::Blocked(_) => 2,
    };
    routes.sort_by_key(|route| {
        (
            rank(route),
            preference(&route.backend, guess, observation),
            route.variant.map_or(0, Variant::order),
        )
    });
    // One Mini vMac model stands for the rest: the others are as good, but
    // only one is the recommendation.
    let mut minivmac_seen = false;
    for route in &mut routes {
        route.recommended = route.fit == Fit::Good
            && route.confidence >= Confidence::Medium
            && !(route.backend == MINIVMAC && minivmac_seen);
        if route.backend == MINIVMAC && route.fit == Fit::Good {
            minivmac_seen = true;
        }
    }
    routes
}

fn route(
    start: Start,
    observation: Option<&Observation>,
    guess: Option<&OsGuess>,
    candidate: &Candidate,
) -> Route {
    let backend = candidate.backend.as_str();
    let mut reasons = Vec::new();
    let mut confidence = Confidence::Low;
    let mut fit = Fit::Good;
    match guess {
        Some(guess) => match runs(backend, candidate.variant, &guess.version) {
            Ok(()) => {
                reasons.push(format!(
                    "{} runs {}, which {}",
                    backend_label(backend),
                    guess.label(),
                    guess.evidence
                ));
                confidence = guess.confidence;
            }
            Err(why) => {
                fit = Fit::Unlikely(format!(
                    "{why}; this looks like {}, as {}",
                    guess.label(),
                    guess.evidence
                ))
            }
        },
        None if observation.is_some() => reasons.push(
            "Nothing on the image says which system it holds, so this is a guess from the \
             kind of media alone."
                .to_string(),
        ),
        None => {}
    }
    if let (Some(model), Some(observation)) =
        (candidate.variant.and_then(Variant::model), observation)
        && observation
            .volumes
            .iter()
            .all(|v| v.format == DiskFormat::RawHfs)
        && !observation.volumes.is_empty()
        && !model.reads_hfs()
    {
        fit = Fit::Unlikely(format!(
            "the {} can't read HFS, which this image is",
            model_label(model)
        ));
    }
    if let Some(observation) = observation
        && observation
            .volumes
            .iter()
            .any(|v| v.format == DiskFormat::RawMfs)
        && backend != MINIVMAC
        && fit == Fit::Good
    {
        fit = Fit::Unlikely(format!(
            "{} doesn't start from MFS volumes, which only the earliest systems used",
            backend_label(backend)
        ));
    }
    match start {
        Start::InstallerCd if backend == MINIVMAC => {
            fit = Fit::Blocked("Mini vMac has no CD drive".to_string())
        }
        Start::InstallerCd if !candidate.installs_from_cd => {
            fit = Fit::Blocked(format!(
                "{} can't install from a CD on this host",
                backend_label(backend)
            ))
        }
        Start::InstallerCd => {
            if let Some(Err(why)) = observation.map(|o| &o.cd) {
                fit = Fit::Blocked(format!("the image can't be attached as a CD: {why}"));
            } else {
                reasons.push("It installs from a CD onto a blank disk.".to_string());
            }
        }
        Start::SystemDisk | Start::InstallerFloppy => {
            if let Some(Err(why)) = observation.map(|o| &o.volume_import) {
                fit = Fit::Blocked(format!("the image has no volume to start from: {why}"));
            }
        }
        Start::Manual => {}
    }
    if let Err(why) = &candidate.rom {
        fit = Fit::Blocked(why.clone());
    }
    Route {
        backend: candidate.backend.clone(),
        variant: candidate.variant,
        rom: candidate.rom.clone().ok().flatten(),
        fit,
        confidence,
        reasons,
        recommended: false,
    }
}

/// How the boot disk is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// The volume in the chosen image, copied; the image isn't changed.
    CopyImage,
    /// A blank disk to install onto, with the chosen image as the CD.
    BlankWithCd,
    /// A blank disk, with no installer chosen yet.
    Blank,
    /// A copy of a registered system image.
    Registered(String),
}

/// The strategies `start` allows on `backend`, each with why it can't be
/// had, if it can't.
pub fn strategies(
    start: Start,
    observation: Option<&Observation>,
    installs_from_cd: bool,
    systems: &[String],
) -> Vec<(Strategy, Option<String>)> {
    let no_cd = (!installs_from_cd)
        .then(|| "this backend can't install from a CD on this host".to_string());
    match start {
        Start::SystemDisk => vec![(
            Strategy::CopyImage,
            observation.and_then(|o| o.volume_import.clone().err()),
        )],
        Start::InstallerCd => vec![(Strategy::BlankWithCd, no_cd)],
        Start::InstallerFloppy => {
            let mut problem = observation.and_then(|o| o.volume_import.clone().err());
            if problem.is_none() && observation.is_some_and(|o| !o.startable()) {
                problem = Some(
                    "the floppy can't start a Mac, and installing from floppies onto a blank \
                     disk isn't supported yet (#28)"
                        .to_string(),
                );
            }
            vec![(Strategy::CopyImage, problem)]
        }
        Start::Manual => {
            let mut list: Vec<(Strategy, Option<String>)> = systems
                .iter()
                .map(|name| (Strategy::Registered(name.clone()), None))
                .collect();
            list.push((Strategy::Blank, no_cd));
            list
        }
    }
}

impl Strategy {
    pub fn label(&self) -> String {
        match self {
            Self::CopyImage => "Copy the image's volume as the boot disk".to_string(),
            Self::BlankWithCd => "Install from the CD onto a new blank disk".to_string(),
            Self::Blank => "A new blank disk, to install onto from a CD later".to_string(),
            Self::Registered(name) => format!("A copy of registered system image {name}"),
        }
    }

    pub fn is_blank(&self) -> bool {
        matches!(self, Self::Blank | Self::BlankWithCd)
    }
}

/// What a piece of guidance is about.
pub struct Situation<'a> {
    pub start: Start,
    pub guess: Option<&'a OsGuess>,
    pub observation: Option<&'a Observation>,
    pub backend: &'a str,
    pub variant: Option<Variant>,
    pub strategy: &'a Strategy,
    pub ephemeral: bool,
    pub name: &'a str,
}

struct Rule {
    applies: fn(&Situation) -> bool,
    text: fn(&Situation) -> String,
}

fn mac_os_installer(situation: &Situation) -> bool {
    situation.strategy == &Strategy::BlankWithCd
        && situation
            .guess
            .is_some_and(|g| g.family == "Mac OS" && g.version.at_least(&[7, 6]))
}

const RULES: &[Rule] = &[
    Rule {
        applies: |s| mac_os_installer(s) && s.backend == SHEEPSHAVER,
        text: |s| {
            format!(
                "If {} hangs while installing, start again and, in the guest's installer, \
                 click Options and turn off \"Update Apple Hard Disk Drivers\". That's a \
                 setting of the Mac OS installer, not of macman: SheepShaver's disks have no \
                 Apple driver to update.",
                s.guess.map(OsGuess::label).unwrap_or_default()
            )
        },
    },
    Rule {
        applies: |s| mac_os_installer(s) && s.backend == BASILISK2,
        text: |_| {
            "Basilisk II's disks have no Apple driver to update either. If the installer offers \
             Options, turning off \"Update Apple Hard Disk Drivers\" there avoids a hang \
             while it tries; not every installer has that choice."
                .to_string()
        },
    },
    Rule {
        applies: |s| s.strategy == &Strategy::BlankWithCd && !mac_os_installer(s),
        text: |_| {
            "If the installer hangs while updating hard disk drivers, look in its options for \
             a way to skip that; emulated disks have no Apple driver to update."
                .to_string()
        },
    },
    Rule {
        applies: |s| s.strategy.is_blank(),
        text: |s| {
            let extended = if s.guess.is_some_and(|g| g.version.at_least(&[8, 1])) {
                "Mac OS Extended works for this system; "
            } else {
                ""
            };
            format!(
                "The new boot disk is blank. When the guest says it's unreadable, choose \
                 Initialize and give it a name, or use Drive Setup from the CD's Utilities. \
                 {extended}Mac OS Standard (HFS) works with every system."
            )
        },
    },
    Rule {
        applies: |s| s.strategy.is_blank() && s.ephemeral,
        text: |s| {
            format!(
                "This machine is ephemeral, so a plain `macman run {}` throws away what the \
                 guest writes to its disk. Install with `macman run {} --no-ephemeral --cdrom \
                 ...`, as the wizard does when it starts the installer.",
                s.name, s.name
            )
        },
    },
    Rule {
        applies: |s| s.start == Start::InstallerFloppy,
        text: |_| {
            "macman can't yet make a Mini vMac machine that installs from a series of floppies \
             (#28). The machine starts from a copy of this floppy instead; other floppies can \
             be registered with `macman media add` and attached with `run --media`."
                .to_string()
        },
    },
    Rule {
        applies: |s| {
            matches!(s.start, Start::SystemDisk | Start::InstallerFloppy)
                && s.observation.is_some_and(|o| !o.startable())
        },
        text: |_| {
            "The image's volume has no boot blocks, or no blessed System Folder, so it may not \
             start up. It's copied anyway; another startup disk can install a system onto it."
                .to_string()
        },
    },
    Rule {
        applies: |s| {
            s.guess
                .is_some_and(|g| runs(s.backend, s.variant, &g.version).is_err())
        },
        text: |s| {
            let guess = s.guess.expect("checked");
            format!(
                "{} isn't known to run on {}. The wizard thinks it's that because {}, which may \
                 be wrong.",
                guess.label(),
                backend_label(s.backend),
                guess.evidence
            )
        },
    },
    Rule {
        applies: |s| s.backend == QEMU,
        text: |_| {
            "QEMU's window keeps the mouse once you click in it; Ctrl-Alt-G lets it go.".to_string()
        },
    },
    Rule {
        applies: |s| s.backend == SHEEPSHAVER,
        text: |_| {
            "SheepShaver needs more of the host than the others; if it won't start, `macman \
             doctor` says why."
                .to_string()
        },
    },
];

/// Guidance for this machine and installer, most specific first.
pub fn guidance(situation: &Situation) -> Vec<String> {
    RULES
        .iter()
        .filter(|rule| (rule.applies)(situation))
        .map(|rule| (rule.text)(situation))
        .collect()
}

/// The installer a new machine starts from: a CD image by path, or
/// registered media by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    Cd(std::path::PathBuf),
    Media(String),
}

impl Install {
    /// How `macman run` is told to attach it.
    pub fn run_option(&self) -> String {
        match self {
            Self::Cd(path) => format!("--cdrom \"{}\"", path.display()),
            Self::Media(name) => format!("--media {name}"),
        }
    }
}

/// What to do once the machine's made, whatever it is.
pub fn after_creation(situation: &Situation, install: Option<&Install>) -> Vec<String> {
    let name = situation.name;
    let mut lines = Vec::new();
    if situation.strategy == &Strategy::BlankWithCd {
        let attach = install.map(Install::run_option).unwrap_or_default();
        lines.push(format!(
            "Install: `macman run {name} --no-ephemeral {attach}` starts the installer from \
             the CD. The installation is yours to run, in the guest."
        ));
        lines.push(
            "When it's done, quit the installer and choose Shut Down from the guest's Special \
             menu, so the disk is left clean."
                .to_string(),
        );
        lines.push(format!(
            "Then start it without the CD: `macman run {name}`. Starting with --cdrom again \
             boots the installer, not the new system."
        ));
    } else if situation.strategy == &Strategy::Blank {
        lines.push(format!(
            "Install: `macman run {name} --no-ephemeral --cdrom FILE` with an installer CD."
        ));
    } else {
        lines.push(format!("Start it: `macman run {name}`."));
        lines.push(
            "Shut down from the guest's Special menu rather than closing the window, so its \
             disk is left clean."
                .to_string(),
        );
    }
    lines.push(format!(
        "Settings: `macman config edit {name}`. Before trying anything risky, `macman \
         checkpoint create {name}` keeps a copy of the disk to roll back to."
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaFormat;
    use crate::wizard::inspect::Volume;
    use std::path::PathBuf;

    fn candidate(backend: &str, variant: Option<Variant>, cd: bool) -> Candidate {
        Candidate {
            backend: backend.to_string(),
            variant,
            rom: Ok((backend != QEMU).then(|| format!("{backend}-rom"))),
            installs_from_cd: cd,
        }
    }

    fn host() -> Vec<Candidate> {
        vec![
            candidate(MINIVMAC, Some(Variant::Model(Model::Plus)), false),
            candidate(MINIVMAC, Some(Variant::Model(Model::II)), false),
            candidate(BASILISK2, None, true),
            candidate(SHEEPSHAVER, None, true),
            candidate(QEMU, Some(Variant::Qemu(QemuMachine::Mac99)), true),
            candidate(QEMU, Some(Variant::Qemu(QemuMachine::G3beige)), true),
        ]
    }

    fn cd(name: &str) -> Observation {
        Observation {
            path: PathBuf::from("installer.toast"),
            size: 600 * 1024 * 1024,
            media: Ok((MediaKind::Cdrom, MediaFormat::Partitioned)),
            layout: "an Apple partition map with 4 partition(s)".to_string(),
            volumes: vec![Volume {
                partition: Some(3),
                format: DiskFormat::RawHfs,
                name: name.to_string(),
                boot_blocks: true,
                blessed: true,
            }],
            iso_label: None,
            partition_kinds: vec!["Apple_partition_map".into(), "Apple_Driver43_CD".into()],
            cd: Ok(()),
            volume_import: Ok(()),
        }
    }

    fn guess(text: &str) -> OsGuess {
        let (family, version) = super::super::inspect::version_in_name(text).unwrap();
        OsGuess {
            family,
            version,
            installer: true,
            confidence: Confidence::Medium,
            evidence: format!("the volume is named {text:?}"),
        }
    }

    fn recommended(routes: &[Route]) -> Vec<String> {
        routes
            .iter()
            .filter(|r| r.recommended)
            .map(Route::label)
            .collect()
    }

    #[test]
    fn a_mac_os_9_cd_suggests_sheepshaver_alone() {
        let observation = cd("Mac OS 9.0.4");
        let guess = guess("Mac OS 9.0.4");
        let routes = routes(
            Start::InstallerCd,
            Some(&observation),
            Some(&guess),
            &host(),
        );
        assert_eq!(recommended(&routes), ["SheepShaver"]);
        assert_eq!(routes[0].backend, SHEEPSHAVER);
        let basilisk2 = routes.iter().find(|r| r.backend == BASILISK2).unwrap();
        assert!(matches!(&basilisk2.fit, Fit::Unlikely(why) if why.contains("8.1")));
        // QEMU's mac99 was tried with a 9.0.4 CD, which didn't start.
        let mac99 = routes.iter().find(|r| r.backend == QEMU).unwrap();
        assert!(matches!(&mac99.fit, Fit::Unlikely(why) if why.contains("9.0.4 CD didn't")));
        assert!(
            routes
                .iter()
                .filter(|r| r.backend == MINIVMAC)
                .all(|r| r.fit == Fit::Blocked("Mini vMac has no CD drive".into()))
        );
    }

    #[test]
    fn mac_os_9_2_goes_to_qemus_mac99_and_never_g3beige() {
        let observation = cd("Mac OS 9.2.1");
        let guess = guess("Mac OS 9.2.1");
        let routes = routes(
            Start::InstallerCd,
            Some(&observation),
            Some(&guess),
            &host(),
        );
        assert_eq!(
            recommended(&routes),
            ["QEMU: mac99, a New World Power Mac G4"]
        );
        assert_eq!(routes[0].rom, None);
        assert_eq!(
            routes[0].variant.map(Variant::argument).as_deref(),
            Some("mac99")
        );
        let g3beige = routes
            .iter()
            .find(|r| r.variant == Some(Variant::Qemu(QemuMachine::G3beige)))
            .unwrap();
        assert!(matches!(&g3beige.fit, Fit::Unlikely(why) if why.contains("Old World ROM")));
        let sheepshaver = routes.iter().find(|r| r.backend == SHEEPSHAVER).unwrap();
        assert!(matches!(&sheepshaver.fit, Fit::Unlikely(why) if why.contains("9.0.4")));
    }

    #[test]
    fn mac_os_x_gets_a_disk_big_enough_for_it() {
        let size = |text: &str| suggested_disk_size(Some(&guess(text)));
        assert_eq!(suggested_disk_size(None), DEFAULT_DISK_SIZE);
        assert_eq!(size("Mac OS 9.2.1"), DEFAULT_DISK_SIZE);
        assert_eq!(size("Mac OS X 10.0"), "4G");
        assert_eq!(size("Mac OS X 10.1.5"), "4G");
        assert_eq!(size("Mac OS X 10.2"), "6G");
        assert_eq!(size("Mac OS X 10.3.9"), "6G");
        assert_eq!(size("Mac OS X 10.4"), "10G");
        assert_eq!(size("Mac OS X 10.5.8"), "20G");

        let tiger = guess("Mac OS X 10.4");
        let small = disk_too_small(Some(&tiger), 2 << 30).unwrap();
        assert!(small.contains("4GB") && small.contains("10G"), "{small}");
        assert_eq!(disk_too_small(Some(&tiger), 4 << 30), None);
        assert_eq!(disk_too_small(Some(&guess("Mac OS 9.2.1")), 1 << 20), None);
        assert!(disk_too_small(Some(&guess("Mac OS X 10.5")), 8 << 30).is_some());
    }

    #[test]
    fn mac_os_x_10_4_goes_to_qemus_mac99_alone() {
        let observation = cd("Mac OS X Install DVD");
        let tiger = guess("Mac OS X 10.4");
        let found = routes(
            Start::InstallerCd,
            Some(&observation),
            Some(&tiger),
            &host(),
        );
        assert_eq!(
            recommended(&found),
            ["QEMU: mac99, a New World Power Mac G4"]
        );
        for route in found.iter().filter(|r| !r.recommended) {
            assert!(!matches!(route.fit, Fit::Good), "{route:?}");
        }
        // Only 10.4 has been seen to start.
        let panther = guess("Mac OS X 10.3");
        let found = routes(
            Start::InstallerCd,
            Some(&observation),
            Some(&panther),
            &host(),
        );
        assert!(recommended(&found).is_empty());
    }

    #[test]
    fn a_system_both_emulators_run_offers_both() {
        let observation = cd("Mac OS 8.1");
        let guess = guess("Mac OS 8.1");
        let routes = routes(
            Start::InstallerCd,
            Some(&observation),
            Some(&guess),
            &host(),
        );
        assert_eq!(recommended(&routes), ["SheepShaver", "Basilisk II"]);
    }

    #[test]
    fn a_host_without_cd_installs_is_told_so() {
        let observation = cd("Mac OS 8.1");
        let guess = guess("Mac OS 8.1");
        let mut host = host();
        host[2].installs_from_cd = false;
        let routes = routes(Start::InstallerCd, Some(&observation), Some(&guess), &host);
        assert_eq!(recommended(&routes), ["SheepShaver"]);
        let basilisk2 = routes.iter().find(|r| r.backend == BASILISK2).unwrap();
        assert_eq!(
            basilisk2.fit,
            Fit::Blocked("Basilisk II can't install from a CD on this host".into())
        );
    }

    #[test]
    fn a_missing_rom_blocks_with_a_way_out() {
        let observation = cd("Mac OS 9.0.4");
        let guess = guess("Mac OS 9.0.4");
        let mut host = host();
        host[3].rom = Err("no registered ROM fits; register one with `macman rom add`".into());
        let routes = routes(Start::InstallerCd, Some(&observation), Some(&guess), &host);
        assert!(recommended(&routes).is_empty());
        let sheepshaver = routes.iter().find(|r| r.backend == SHEEPSHAVER).unwrap();
        assert!(matches!(&sheepshaver.fit, Fit::Blocked(why) if why.contains("rom add")));
    }

    #[test]
    fn an_unrecognized_image_recommends_nothing_but_offers_everything() {
        let mut observation = cd("Untitled");
        observation.volumes[0].partition = None;
        let routes = routes(Start::SystemDisk, Some(&observation), None, &host());
        assert!(recommended(&routes).is_empty());
        assert!(routes.iter().all(|r| r.fit == Fit::Good));
        assert!(routes.iter().all(|r| r.confidence == Confidence::Low));
    }

    #[test]
    fn a_68k_system_puts_mini_vmac_first_on_floppies() {
        let mut observation = cd("System 6.0.8");
        observation.media = Ok((MediaKind::Floppy, MediaFormat::Hfs));
        observation.size = 800 * 1024;
        observation.volumes[0].partition = None;
        let guess = guess("System 6.0.8");
        let routes = routes(
            Start::InstallerFloppy,
            Some(&observation),
            Some(&guess),
            &host(),
        );
        assert_eq!(recommended(&routes), ["Mini vMac: Macintosh Plus"]);
        assert_eq!(routes[1].label(), "Mini vMac: Macintosh II");
        assert!(
            routes[2..]
                .iter()
                .all(|r| !r.usable() || r.fit != Fit::Good)
        );
    }

    #[test]
    fn floppy_installs_are_never_offered() {
        let mut observation = cd("Install 1");
        observation.volumes[0].boot_blocks = false;
        let list = strategies(Start::InstallerFloppy, Some(&observation), false, &[]);
        assert_eq!(list.len(), 1);
        assert!(list[0].1.as_deref().unwrap().contains("#28"));
    }

    #[test]
    fn guidance_follows_the_installer_and_backend() {
        let guess = guess("Mac OS 9.0.4");
        let situation = |backend, strategy| Situation {
            start: Start::InstallerCd,
            guess: Some(&guess),
            observation: None,
            backend,
            variant: None,
            strategy,
            ephemeral: false,
            name: "nine",
        };
        let sheepshaver = guidance(&situation(SHEEPSHAVER, &Strategy::BlankWithCd));
        assert!(sheepshaver[0].contains("Update Apple Hard Disk Drivers"));
        assert!(sheepshaver[0].contains("Mac OS 9.0.4"));
        let basilisk2 = guidance(&situation(BASILISK2, &Strategy::BlankWithCd));
        assert!(basilisk2[0].contains("not every installer"));
        assert!(
            basilisk2
                .iter()
                .any(|l| l.contains("isn't known to run on Basilisk II"))
        );
        let copied = guidance(&situation(SHEEPSHAVER, &Strategy::CopyImage));
        assert!(!copied.iter().any(|l| l.contains("Hard Disk Drivers")));
        assert!(!copied.iter().any(|l| l.contains("blank")));
    }

    #[test]
    fn an_ephemeral_install_is_warned_about() {
        let situation = Situation {
            start: Start::InstallerCd,
            guess: None,
            observation: None,
            backend: SHEEPSHAVER,
            variant: None,
            strategy: &Strategy::BlankWithCd,
            ephemeral: true,
            name: "nine",
        };
        assert!(
            guidance(&situation)
                .iter()
                .any(|l| l.contains("--no-ephemeral"))
        );
        let cd = Install::Cd("/cds/OS 9.toast".into());
        let after = after_creation(&situation, Some(&cd));
        assert!(after[0].contains("--cdrom \"/cds/OS 9.toast\""));
        let media = Install::Media("os9".into());
        let after = after_creation(&situation, Some(&media));
        assert!(after[0].contains("--no-ephemeral --media os9`"));
        assert!(after[2].contains("`macman run nine`"));
    }
}
