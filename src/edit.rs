//! What `macman config edit` offers, and the rules for changing it, apart
//! from any user interface. Every check is the one `create` and `run` make:
//! the backend's own `validate`, `validate_disks`, and `check_rom`. Choices
//! are found by trying each candidate on a copy of the machine against those,
//! so the editor has no rules of its own to drift from theirs.

use crate::backend::Backends;
use crate::disk::{self, LockMode};
use crate::fsutil::validate_name;
use crate::machine::{Basilisk2Settings, Cpu, Machine, Network, SheepShaverSettings};
use crate::paths::Paths;
use crate::registry;
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::fs;
use std::path::PathBuf;

/// Memory sizes offered. Each backend accepts only some of them, and the
/// editor also takes any other size typed in.
const MEMORY: &[&str] = &[
    "128K", "512K", "1M", "2M", "2.5M", "4M", "5M", "8M", "16M", "24M", "32M", "48M", "64M", "96M",
    "128M", "192M", "256M", "384M", "512M", "768M", "1023M", "1024M",
];
/// Display sizes offered, as for [`MEMORY`].
const SIZES: &[(u32, u32)] = &[
    (512, 342),
    (512, 384),
    (640, 400),
    (640, 480),
    (800, 600),
    (832, 624),
    (1024, 768),
    (1152, 870),
    (1280, 960),
    (1280, 1024),
    (1600, 1200),
    (1920, 1080),
];
const DEPTHS: &[u32] = &[1, 2, 4, 8, 16, 32];
const SCALES: &[u32] = &[1, 2, 3, 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    Ephemeral,
    Rom,
    Memory,
    Cpu,
    Fpu,
    Size,
    Depth,
    Scale,
    Fullscreen,
    Sound,
    Network,
    SharedFolder,
}

/// How a field is changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// One of these, or another value typed in where `typed` allows it.
    Choice {
        choices: Vec<Choice>,
        typed: bool,
    },
    Toggle,
    /// A host path, or nothing.
    Path,
    /// Shown, but not editable here, for this reason.
    Fixed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    /// Why it can't be chosen, if it can't.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// `None` for fixed fields, which nothing can change.
    pub id: Option<FieldId>,
    pub group: &'static str,
    pub label: &'static str,
    pub value: String,
    pub help: &'static str,
    pub kind: Kind,
    /// Whether it differs from the saved machine.
    pub changed: bool,
}

/// One pending change, for review before saving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub label: &'static str,
    pub before: String,
    pub after: String,
}

/// An open edit of one machine.
pub struct Editor<'a> {
    paths: &'a Paths,
    backends: &'a Backends,
    pub original: Machine,
    pub draft: Machine,
    /// The definition as it was read, to notice anyone else changing it.
    loaded: String,
    roms: Vec<String>,
}

impl<'a> Editor<'a> {
    /// Open `name` for editing, refusing while it's running.
    pub fn open(paths: &'a Paths, backends: &'a Backends, name: &str) -> Result<Self> {
        validate_name("machine", name)?;
        let original = Machine::load(paths, name)?;
        backends.get(&original.emulator.backend)?;
        let loaded = fs::read_to_string(definition(paths, name))?;
        let mut roms: Vec<String> = registry::list_roms(paths)?
            .into_iter()
            .map(|rom| rom.name)
            .collect();
        roms.sort();
        let editor = Self {
            paths,
            backends,
            draft: original.clone(),
            original,
            loaded,
            roms,
        };
        editor.check_idle()?;
        Ok(editor)
    }

    /// Refuse while an emulator has the machine's disks, as `remove` does.
    fn check_idle(&self) -> Result<()> {
        for (path, _) in self.original.disk_paths(self.paths)? {
            if path.exists() {
                drop(
                    disk::lock_image(self.paths, &path, LockMode::Exclusive).with_context(
                        || {
                            format!(
                                "machine {:?} is running, so it can't be edited; quit it first",
                                self.original.name
                            )
                        },
                    )?,
                );
            }
        }
        Ok(())
    }

    /// Whether `machine` would be accepted by `create` and `run`, as far as
    /// can be told without starting it.
    pub fn check(&self, machine: &Machine) -> Result<()> {
        let backend = self.backends.get(&machine.emulator.backend)?;
        backend.validate(machine)?;
        backend.validate_disks(machine, machine.disks.len())?;
        let (rom, file) = registry::load_rom(self.paths, &machine.hardware.rom)?;
        backend.check_rom(machine, &rom, &file)?;
        if let Some(folder) = shared_folder(machine)
            && !folder.is_dir()
        {
            bail!(
                "shared folder {} isn't a folder on this host; pick another, or none",
                folder.display()
            );
        }
        Ok(())
    }

    /// Whether saving would change anything. A missing `[basilisk2]` or
    /// `[sheepshaver]` table means the defaults, so it's the same as one
    /// that holds them.
    pub fn is_modified(&self) -> bool {
        normalized(&self.draft) != normalized(&self.original)
    }

    /// What `value` would leave wrong that isn't wrong already, if anything.
    /// A machine that was already invalid can still be fixed one field at a time.
    fn problem_with(&self, id: FieldId, value: &str) -> Option<String> {
        let mut candidate = self.draft.clone();
        if let Err(error) = apply(&mut candidate, id, value) {
            return Some(format!("{error:#}"));
        }
        let after = self.check(&candidate).err()?;
        let after = format!("{after:#}");
        let before = self.check(&self.draft).err().map(|e| format!("{e:#}"));
        (before.as_deref() != Some(after.as_str())).then_some(after)
    }

    /// Change a field, unless that would make the machine unusable.
    pub fn set(&mut self, id: FieldId, value: &str) -> Result<()> {
        if let Some(problem) = self.problem_with(id, value) {
            bail!("{problem}");
        }
        apply(&mut self.draft, id, value)
    }

    /// Flip a toggle.
    pub fn toggle(&mut self, id: FieldId) -> Result<()> {
        let value = if value_of(&self.draft, id) == "on" {
            "off"
        } else {
            "on"
        };
        self.set(id, value)
    }

    /// The fields, in their groups, as this machine's backend has them.
    pub fn fields(&self) -> Vec<Field> {
        let machine = &self.draft;
        let backend = machine.emulator.backend.as_str();
        let macemu = matches!(backend, "basilisk2" | "sheepshaver");
        let mut fields = vec![
            self.fixed(
                "Machine",
                "Name",
                machine.name.clone(),
                "Rename a machine by cloning it with `macman clone`, then removing the old one.",
            ),
            self.fixed(
                "Machine",
                "Backend",
                machine.emulator.backend.clone(),
                "A machine's disks were made for its emulator; create a new machine for another.",
            ),
        ];
        if let Some(model) = machine.hardware.model {
            fields.push(self.fixed(
                "Machine",
                "Model",
                model.to_string(),
                "The model decides the ROM, disks, and build; create a new machine for another.",
            ));
        }
        fields.push(self.field(
            FieldId::Ephemeral,
            "Machine",
            "Ephemeral",
            "On: a plain `run` boots a temporary copy of the boot disk and discards the \
             guest's changes to it. `run --no-ephemeral` keeps them for one run.",
            Kind::Toggle,
        ));
        let disks = machine
            .disks
            .iter()
            .map(|disk| {
                let mut text = disk.path.clone();
                if disk.boot {
                    text += " (boot)";
                }
                if disk.readonly {
                    text += " (read-only)";
                }
                text
            })
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(self.fixed(
            "Machine",
            "Disks",
            disks,
            "The disk files and their order aren't edited here, so nothing can lose track \
             of an installed system. Attach others for one run with `run --disk`.",
        ));

        let roms = self.choices(FieldId::Rom, self.roms.iter().cloned());
        fields.push(self.field(
            FieldId::Rom,
            "Hardware",
            "ROM",
            "Registered ROMs this machine's backend and model can start from. A different \
             ROM may not start the system already installed.",
            Kind::Choice {
                choices: roms,
                typed: false,
            },
        ));
        let memory = self.choices(FieldId::Memory, MEMORY.iter().map(|m| m.to_string()));
        fields.push(self.field(
            FieldId::Memory,
            "Hardware",
            "Memory",
            "Sizes this model or emulator can have. Press e to type another, such as 40M.",
            Kind::Choice {
                choices: memory,
                typed: true,
            },
        ));
        if backend == "basilisk2" {
            let cpus = self.choices(FieldId::Cpu, Cpu::value_variants().iter().map(value_name));
            fields.push(self.field(
                FieldId::Cpu,
                "Hardware",
                "CPU",
                "The emulated 68k processor.",
                Kind::Choice {
                    choices: cpus,
                    typed: false,
                },
            ));
            fields.push(self.field(
                FieldId::Fpu,
                "Hardware",
                "FPU",
                "A floating-point unit. A 68040 always has one.",
                Kind::Toggle,
            ));
        }

        let sizes = self.choices(
            FieldId::Size,
            SIZES
                .iter()
                .map(|(width, height)| format!("{width}x{height}")),
        );
        fields.push(self.field(
            FieldId::Size,
            "Display",
            "Size",
            "Width and height in pixels, as this model or emulator allows with the depth. \
             Press e to type another, such as 1440x900.",
            Kind::Choice {
                choices: sizes,
                typed: true,
            },
        ));
        let depths = self.choices(FieldId::Depth, DEPTHS.iter().map(u32::to_string));
        fields.push(self.field(
            FieldId::Depth,
            "Display",
            "Depth",
            "Bits per pixel.",
            Kind::Choice {
                choices: depths,
                typed: false,
            },
        ));
        let scales = self.choices(FieldId::Scale, SCALES.iter().map(u32::to_string));
        fields.push(self.field(
            FieldId::Scale,
            "Display",
            "Scale",
            "How many times larger the window is than the Mac's screen.",
            Kind::Choice {
                choices: scales,
                typed: false,
            },
        ));
        fields.push(self.field(
            FieldId::Fullscreen,
            "Display",
            "Full screen",
            "Start in full screen.",
            Kind::Toggle,
        ));

        if macemu {
            fields.push(self.field(
                FieldId::Sound,
                "Emulator",
                "Sound",
                "Sound output from the guest.",
                Kind::Toggle,
            ));
            let networks = self.choices(
                FieldId::Network,
                Network::value_variants().iter().map(value_name),
            );
            fields.push(self.field(
                FieldId::Network,
                "Emulator",
                "Network",
                "none, or slirp: networking through the host, with no setup.",
                Kind::Choice {
                    choices: networks,
                    typed: false,
                },
            ));
            fields.push(self.field(
                FieldId::SharedFolder,
                "Emulator",
                "Shared folder",
                "A host folder the guest sees as a volume. Leave it empty for none. It isn't \
                 exported, as it belongs to this host.",
                Kind::Path,
            ));
        }
        fields
    }

    fn fixed(
        &self,
        group: &'static str,
        label: &'static str,
        value: String,
        reason: &'static str,
    ) -> Field {
        Field {
            id: None,
            group,
            label,
            value,
            help: reason,
            kind: Kind::Fixed(reason.to_string()),
            changed: false,
        }
    }

    fn field(
        &self,
        id: FieldId,
        group: &'static str,
        label: &'static str,
        help: &'static str,
        kind: Kind,
    ) -> Field {
        let value = value_of(&self.draft, id);
        Field {
            id: Some(id),
            group,
            label,
            changed: value != value_of(&self.original, id),
            value,
            help,
            kind,
        }
    }

    /// `candidates`, and the current value if it isn't one, each with what
    /// would be wrong with it.
    fn choices(&self, id: FieldId, candidates: impl Iterator<Item = String>) -> Vec<Choice> {
        let current = value_of(&self.draft, id);
        let mut values: Vec<String> = candidates.collect();
        if !values.contains(&current) && !current.is_empty() {
            values.push(current.clone());
        }
        values
            .into_iter()
            .map(|value| Choice {
                problem: if value == current {
                    None
                } else {
                    self.problem_with(id, &value)
                },
                value,
            })
            .collect()
    }

    /// The pending changes, in the order the fields are shown.
    pub fn changes(&self) -> Vec<Change> {
        self.fields()
            .into_iter()
            .filter(|field| field.changed)
            .filter_map(|field| {
                let id = field.id?;
                Some(Change {
                    label: field.label,
                    before: shown(&value_of(&self.original, id)),
                    after: shown(&field.value),
                })
            })
            .collect()
    }

    /// What the pending changes mean beyond the definition, worth knowing
    /// before saving. Static checks can't promise a guest will cope.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let (before, after) = (&self.original, &self.draft);
        let backend = self.backends.get(&after.emulator.backend);
        if backend.is_ok_and(|backend| backend.builds_differently(before, after)) {
            notes.push(
                "Mini vMac builds memory and the display into the emulator, so the next run \
                 compiles one for this configuration, unless it's been built before."
                    .to_string(),
            );
        }
        if before.hardware.rom != after.hardware.rom {
            notes.push(
                "A different ROM may not start the system installed on this machine's disk."
                    .to_string(),
            );
        }
        if before.hardware.memory != after.hardware.memory {
            notes.push(
                "macman can't tell whether the installed system needs more memory than this, \
                 or can use it all."
                    .to_string(),
            );
        }
        if !before.ephemeral && after.ephemeral {
            notes.push(
                "A plain `run` will discard the guest's changes to the boot disk; use `run \
                 --no-ephemeral` to keep them."
                    .to_string(),
            );
        }
        notes
    }

    /// Write the draft, if it's valid, the machine isn't running, and nobody
    /// else changed it meanwhile. The write is atomic.
    pub fn save(&self) -> Result<()> {
        self.check(&self.draft)
            .context("this configuration can't be saved")?;
        self.check_idle()?;
        let file = definition(self.paths, &self.original.name);
        let now =
            fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
        if now != self.loaded {
            bail!(
                "machine {:?} was changed by something else while it was being edited; nothing \
                 was saved, so start again",
                self.original.name
            );
        }
        self.draft.save_to(&self.paths.machine(&self.original.name))
    }
}

fn normalized(machine: &Machine) -> Machine {
    let mut machine = machine.clone();
    match machine.emulator.backend.as_str() {
        "basilisk2" => {
            machine.basilisk2.get_or_insert_with(Default::default);
        }
        "sheepshaver" => {
            machine.sheepshaver.get_or_insert_with(Default::default);
        }
        _ => {}
    }
    machine
}

fn definition(paths: &Paths, name: &str) -> PathBuf {
    paths.machine(name).join("machine.toml")
}

/// How a value reads in a review: an empty path as "none".
fn shown(value: &str) -> String {
    if value.is_empty() {
        "none".to_string()
    } else {
        value.to_string()
    }
}

fn value_name<T: ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .map(|value| value.get_name().to_string())
        .unwrap_or_default()
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

fn shared_folder(machine: &Machine) -> Option<&PathBuf> {
    match (&machine.basilisk2, &machine.sheepshaver) {
        (Some(settings), _) => settings.shared_folder.as_ref(),
        (_, Some(settings)) => settings.shared_folder.as_ref(),
        _ => None,
    }
}

/// A field's value, as the editor shows and takes it.
pub fn value_of(machine: &Machine, id: FieldId) -> String {
    let basilisk2 = machine.basilisk2.clone().unwrap_or_default();
    let sheepshaver = machine.sheepshaver.clone().unwrap_or_default();
    let is_sheepshaver = machine.emulator.backend == "sheepshaver";
    let display = &machine.display;
    match id {
        FieldId::Ephemeral => on_off(machine.ephemeral),
        FieldId::Rom => machine.hardware.rom.clone(),
        FieldId::Memory => machine.hardware.memory.clone(),
        FieldId::Cpu => value_name(&basilisk2.cpu),
        FieldId::Fpu => on_off(basilisk2.fpu),
        FieldId::Size => format!("{}x{}", display.width, display.height),
        FieldId::Depth => display.depth.to_string(),
        FieldId::Scale => display.scale.to_string(),
        FieldId::Fullscreen => on_off(display.fullscreen),
        FieldId::Sound if is_sheepshaver => on_off(sheepshaver.sound),
        FieldId::Sound => on_off(basilisk2.sound),
        FieldId::Network if is_sheepshaver => value_name(&sheepshaver.network),
        FieldId::Network => value_name(&basilisk2.network),
        FieldId::SharedFolder => shared_folder(machine)
            .map(|folder| folder.display().to_string())
            .unwrap_or_default(),
    }
}

fn parse_toggle(value: &str) -> Result<bool> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("expected on or off, not {value:?}"),
    }
}

fn parse_number(what: &str, value: &str) -> Result<u32> {
    value
        .trim()
        .parse()
        .with_context(|| format!("{what} must be a whole number, not {value:?}"))
}

/// Set a field from its shown form, checking only that the value parses.
fn apply(machine: &mut Machine, id: FieldId, value: &str) -> Result<()> {
    let is_sheepshaver = machine.emulator.backend == "sheepshaver";
    let basilisk2: Basilisk2Settings = machine.basilisk2.clone().unwrap_or_default();
    let sheepshaver: SheepShaverSettings = machine.sheepshaver.clone().unwrap_or_default();
    match id {
        FieldId::Ephemeral => machine.ephemeral = parse_toggle(value)?,
        FieldId::Rom => {
            validate_name("ROM", value)?;
            machine.hardware.rom = value.to_string();
        }
        FieldId::Memory => {
            let memory = value.trim().to_ascii_uppercase();
            if memory.is_empty() {
                bail!("give a memory size, such as 4M");
            }
            machine.hardware.memory = memory;
        }
        FieldId::Cpu => {
            let mut settings = basilisk2.clone();
            settings.cpu = Cpu::from_str(value, true).map_err(anyhow::Error::msg)?;
            machine.basilisk2 = Some(settings);
        }
        FieldId::Fpu => {
            let mut settings = basilisk2.clone();
            settings.fpu = parse_toggle(value)?;
            machine.basilisk2 = Some(settings);
        }
        FieldId::Size => {
            let (width, height) = value
                .trim()
                .split_once(['x', 'X'])
                .context("give a size as WIDTHxHEIGHT, such as 640x480")?;
            machine.display.width = parse_number("the width", width)?;
            machine.display.height = parse_number("the height", height)?;
        }
        FieldId::Depth => machine.display.depth = parse_number("the depth", value)?,
        FieldId::Scale => machine.display.scale = parse_number("the scale", value)?,
        FieldId::Fullscreen => machine.display.fullscreen = parse_toggle(value)?,
        FieldId::Sound if is_sheepshaver => {
            let mut settings = sheepshaver.clone();
            settings.sound = parse_toggle(value)?;
            machine.sheepshaver = Some(settings);
        }
        FieldId::Sound => {
            let mut settings = basilisk2.clone();
            settings.sound = parse_toggle(value)?;
            machine.basilisk2 = Some(settings);
        }
        FieldId::Network => {
            let network = Network::from_str(value, true).map_err(anyhow::Error::msg)?;
            if is_sheepshaver {
                let mut settings = sheepshaver.clone();
                settings.network = network;
                machine.sheepshaver = Some(settings);
            } else {
                let mut settings = basilisk2.clone();
                settings.network = network;
                machine.basilisk2 = Some(settings);
            }
        }
        FieldId::SharedFolder => {
            let folder = match value.trim() {
                "" => None,
                path => Some(
                    fs::canonicalize(path)
                        .with_context(|| format!("cannot use shared folder {path}"))?,
                ),
            };
            if is_sheepshaver {
                let mut settings = sheepshaver.clone();
                settings.shared_folder = folder;
                machine.sheepshaver = Some(settings);
            } else {
                let mut settings = basilisk2.clone();
                settings.shared_folder = folder;
                machine.basilisk2 = Some(settings);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::App;
    use crate::cli::Cli;
    use clap::Parser;
    use std::path::Path;

    const PLUS_ROM_CHECKSUM: u32 = 0x4D1F_8172;

    /// A synthetic Plus ROM that passes the real checksum test, with no Apple code.
    fn plus_rom(path: &Path) {
        let mut bytes = vec![0u8; 128 * 1024];
        bytes[..4].copy_from_slice(&PLUS_ROM_CHECKSUM.to_be_bytes());
        let mut remaining = PLUS_ROM_CHECKSUM;
        for word in bytes[4..].as_chunks_mut::<2>().0 {
            let value = remaining.min(0xFFFF) as u16;
            *word = value.to_be_bytes();
            remaining -= u32::from(value);
        }
        fs::write(path, bytes).unwrap();
    }

    /// A synthetic 32-bit clean ROM, for Basilisk II.
    fn clean_rom(path: &Path) {
        let mut bytes = vec![0u8; 1024 * 1024];
        bytes[8..10].copy_from_slice(&0x067Cu16.to_be_bytes());
        fs::write(path, bytes).unwrap();
    }

    /// A synthetic old-world PowerPC ROM, for SheepShaver.
    fn powerpc_rom(path: &Path) {
        let mut bytes = vec![0u8; 0x40_0000];
        bytes[0x30_D064..0x30_D064 + 8].copy_from_slice(b"Boot TNT");
        fs::write(path, bytes).unwrap();
    }

    fn macman(paths: &Paths, args: &[&str]) {
        let app = App::new(paths.clone(), crate::default_backends(paths).unwrap());
        let cli =
            Cli::try_parse_from(std::iter::once("macman").chain(args.iter().copied())).unwrap();
        assert_eq!(app.execute(cli).unwrap(), 0, "{args:?}");
    }

    /// A blank-but-valid-looking HFS volume, registered as system `sys`.
    fn add_system(paths: &Paths) {
        let dir = paths.cache_dir.join("sources");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sys.dsk");
        let mut bytes = vec![0u8; 800 * 1024];
        bytes[1024..1026].copy_from_slice(b"BD");
        bytes[1034] = 0x01;
        fs::write(&file, bytes).unwrap();
        macman(
            paths,
            &["system", "add", file.to_str().unwrap(), "--name", "sys"],
        );
    }

    fn add_rom(paths: &Paths, name: &str, write: fn(&Path)) {
        let dir = paths.cache_dir.join("sources");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{name}.rom"));
        write(&file);
        registry::add_rom(paths, &file, name).unwrap();
    }

    /// A Mac Plus named `plus` with 4M, under Mini vMac.
    pub fn make_plus(paths: &Paths) {
        add_rom(paths, "plus", plus_rom);
        add_system(paths);
        macman(
            paths,
            &[
                "create",
                "plus",
                "--system",
                "sys",
                "--backend",
                "minivmac",
                "--model",
                "plus",
                "--rom",
                "plus",
                "--memory",
                "4M",
            ],
        );
    }

    fn make_basilisk(paths: &Paths) {
        add_rom(paths, "quadra", clean_rom);
        add_rom(paths, "plus", plus_rom);
        add_system(paths);
        macman(
            paths,
            &[
                "create",
                "quadra",
                "--system",
                "sys",
                "--backend",
                "basilisk2",
                "--rom",
                "quadra",
            ],
        );
    }

    fn make_sheep(paths: &Paths) {
        add_rom(paths, "tnt", powerpc_rom);
        add_rom(paths, "quadra", clean_rom);
        add_system(paths);
        macman(
            paths,
            &[
                "create",
                "ppc",
                "--system",
                "sys",
                "--backend",
                "sheepshaver",
                "--rom",
                "tnt",
            ],
        );
    }

    fn choices(editor: &Editor, id: FieldId) -> Vec<(String, bool)> {
        let field = editor
            .fields()
            .into_iter()
            .find(|field| field.id == Some(id))
            .unwrap_or_else(|| panic!("no {id:?} field"));
        match field.kind {
            Kind::Choice { choices, .. } => choices
                .into_iter()
                .map(|choice| (choice.value, choice.problem.is_none()))
                .collect(),
            other => panic!("{id:?} is {other:?}"),
        }
    }

    fn allowed(editor: &Editor, id: FieldId) -> Vec<String> {
        choices(editor, id)
            .into_iter()
            .filter(|(_, ok)| *ok)
            .map(|(value, _)| value)
            .collect()
    }

    #[test]
    fn a_plus_is_offered_only_what_a_plus_can_have() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_plus(&paths);
        add_rom(&paths, "quadra", clean_rom);
        let backends = crate::default_backends(&paths).unwrap();
        let mut editor = Editor::open(&paths, &backends, "plus").unwrap();

        let memory = allowed(&editor, FieldId::Memory);
        assert!(memory.contains(&"4M".to_string()), "{memory:?}");
        for too_much in ["8M", "2G"] {
            assert!(!memory.contains(&too_much.to_string()), "{memory:?}");
            assert!(editor.set(FieldId::Memory, too_much).is_err());
        }
        assert_eq!(allowed(&editor, FieldId::Rom), vec!["plus"]);
        assert!(editor.set(FieldId::Rom, "quadra").is_err());
        assert_eq!(allowed(&editor, FieldId::Depth), vec!["1"]);
        assert!(editor.set(FieldId::Memory, "lots").is_err());
        assert!(!editor.is_modified(), "refused values change nothing");

        let ids: Vec<_> = editor.fields().iter().filter_map(|f| f.id).collect();
        for absent in [
            FieldId::Cpu,
            FieldId::Fpu,
            FieldId::Sound,
            FieldId::SharedFolder,
        ] {
            assert!(!ids.contains(&absent), "{absent:?}");
        }
        let model = editor
            .fields()
            .into_iter()
            .find(|f| f.label == "Model")
            .unwrap();
        assert!(matches!(model.kind, Kind::Fixed(_)));

        editor.set(FieldId::Memory, "1M").unwrap();
        assert!(
            editor.notes().iter().any(|note| note.contains("compiles")),
            "{:?}",
            editor.notes()
        );
    }

    #[test]
    fn basilisk_offers_its_cpus_depths_and_options() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_basilisk(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let mut editor = Editor::open(&paths, &backends, "quadra").unwrap();

        assert_eq!(allowed(&editor, FieldId::Rom), vec!["quadra"]);
        assert_eq!(allowed(&editor, FieldId::Depth), vec!["8", "16", "32"]);
        assert!(allowed(&editor, FieldId::Memory).contains(&"512M".to_string()));
        let ids: Vec<_> = editor.fields().iter().filter_map(|f| f.id).collect();
        for present in [
            FieldId::Cpu,
            FieldId::Fpu,
            FieldId::Sound,
            FieldId::Network,
            FieldId::SharedFolder,
        ] {
            assert!(ids.contains(&present), "{present:?}");
        }

        // A 68040 has its FPU built in, so it can't be turned off.
        editor.set(FieldId::Cpu, "68040").unwrap();
        assert!(editor.toggle(FieldId::Fpu).is_err());
        assert_eq!(value_of(&editor.draft, FieldId::Fpu), "on");

        editor.toggle(FieldId::Sound).unwrap();
        assert!(editor.set(FieldId::SharedFolder, "/no/such/dir").is_err());
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        editor
            .set(FieldId::SharedFolder, shared.to_str().unwrap())
            .unwrap();
        editor.save().unwrap();

        let saved = Machine::load(&paths, "quadra").unwrap();
        assert_eq!(value_of(&saved, FieldId::Sound), "off");
        assert_eq!(value_of(&saved, FieldId::Cpu), "68040");
        assert_eq!(saved.id, editor.original.id);
        assert_eq!(saved.disks, editor.original.disks);
    }

    #[test]
    fn sheepshaver_has_only_its_own_settings() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_sheep(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let mut editor = Editor::open(&paths, &backends, "ppc").unwrap();

        assert_eq!(allowed(&editor, FieldId::Rom), vec!["tnt"]);
        assert_eq!(allowed(&editor, FieldId::Depth), vec!["32"]);
        let ids: Vec<_> = editor.fields().iter().filter_map(|f| f.id).collect();
        assert!(!ids.contains(&FieldId::Cpu));
        assert!(ids.contains(&FieldId::Sound));
        editor.toggle(FieldId::Ephemeral).unwrap();
        assert!(
            editor.notes().iter().any(|n| n.contains("discard")),
            "{:?}",
            editor.notes()
        );
        editor.save().unwrap();
        assert!(Machine::load(&paths, "ppc").unwrap().ephemeral);
    }

    #[test]
    fn unsaved_edits_write_nothing_and_a_concurrent_edit_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_plus(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let file = paths.machine("plus").join("machine.toml");
        let before = fs::read_to_string(&file).unwrap();

        let mut editor = Editor::open(&paths, &backends, "plus").unwrap();
        editor.set(FieldId::Memory, "1M").unwrap();
        assert_eq!(editor.changes().len(), 1);
        drop(editor);
        assert_eq!(fs::read_to_string(&file).unwrap(), before);

        let mut editor = Editor::open(&paths, &backends, "plus").unwrap();
        editor.set(FieldId::Memory, "1M").unwrap();
        fs::write(&file, before.replace("4M", "2M")).unwrap();
        let error = editor.save().unwrap_err().to_string();
        assert!(error.contains("changed by something else"), "{error}");
        assert!(fs::read_to_string(&file).unwrap().contains("2M"));
    }

    #[test]
    fn a_running_machine_cannot_be_edited() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_plus(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let machine = Machine::load(&paths, "plus").unwrap();
        let (disk, _) = machine.disk_paths(&paths).unwrap().remove(0);
        let _running = disk::lock_image(&paths, &disk, LockMode::Exclusive).unwrap();
        let error = Editor::open(&paths, &backends, "plus")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("is running"), "{error}");
    }
}
