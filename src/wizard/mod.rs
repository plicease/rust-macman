//! `macman wizard`: a guided way to make a machine, from a system disk, an
//! installer CD, a floppy, or nothing. It recommends; the user decides, and
//! nothing is written until they confirm the plan. The machine is made by
//! [`App::prepare_create`] and [`App::publish_prepared`], as `create` makes
//! one, and its settings are edited with `config edit`'s own editor.
//!
//! This is the state and the keys; [`ui`] draws it. Inspection is in
//! [`inspect`] and every recommendation is in [`rules`], apart from both.

pub mod inspect;
pub mod rules;
pub mod ui;

use crate::app::{App, BootSource, Prepared};
use crate::cli::CreateArgs;
use crate::edit::Editor;
use crate::fsutil::{parse_disk_size, validate_name};
use crate::machine::QemuMachine;
use crate::media::{self, Media};
use crate::registry;
use crate::tui::{self, Leave};
use inspect::{Observation, OsGuess};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rules::{Candidate, Install, Route, Situation, Start, Strategy, Variant};
use std::path::{Path, PathBuf};

/// Where new blank disks start, unless the user picks another size.
pub use rules::DEFAULT_DISK_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Source,
    Findings,
    Backend,
    Disk,
    Settings,
    Review,
    Done,
}

pub const STEPS: [Step; 7] = [
    Step::Source,
    Step::Findings,
    Step::Backend,
    Step::Disk,
    Step::Settings,
    Step::Review,
    Step::Done,
];

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Self::Source => "Start from",
            Self::Findings => "What the image holds",
            Self::Backend => "Emulator",
            Self::Disk => "Boot disk and name",
            Self::Settings => "Settings",
            Self::Review => "Review the plan",
            Self::Done => "Made",
        }
    }

    pub fn number(self) -> usize {
        STEPS.iter().position(|s| *s == self).unwrap_or(0) + 1
    }
}

/// A line of a step that the cursor can be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Image,
    /// Registered media, by its place in [`Wizard::registered`].
    Media(usize),
    Start(Start),
    Route(usize),
    Strategy(usize),
    DiskSize,
    Name,
    /// Whether to register a new image as media, for reuse.
    Register,
    RegisterName,
}

/// A machine the wizard made, and what to tell the user about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    /// The installer to start it with, if it's to install from one.
    pub install: Option<Install>,
    /// The media the image was registered as, if the user asked.
    pub registered: Option<String>,
    pub guidance: Vec<String>,
    pub after: Vec<String>,
}

/// How the wizard ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Left before anything was made, so nothing was.
    Cancelled,
    /// Made, and to be started now if `launch`.
    Created { created: Created, launch: bool },
}

pub struct Wizard<'a> {
    app: &'a App,
    pub step: Step,
    pub start: Start,
    /// Whether the user picked the start, rather than taking the suggestion.
    pub start_chosen: bool,
    /// The image's path, as given.
    pub image: String,
    /// Registered media, which the image can be picked from.
    pub registered: Vec<Media>,
    /// The registered media the image is, if it was picked from them.
    pub media: Option<String>,
    /// Whether to register a new image as media when the machine is made.
    pub register: bool,
    pub register_name: String,
    pub observation: Option<Observation>,
    pub guess: Option<OsGuess>,
    candidates: Option<Vec<Candidate>>,
    pub routes: Vec<Route>,
    /// The chosen backend and model.
    pub route: Option<(String, Option<Variant>)>,
    pub strategies: Vec<(Strategy, Option<String>)>,
    pub strategy: Option<Strategy>,
    pub disk_size: String,
    /// Whether the user typed the disk size, so it isn't suggested over.
    disk_size_typed: bool,
    pub name: String,
    /// Whether the user typed the name, so it isn't suggested over.
    name_typed: bool,
    settings: Option<tui::Ui<'a>>,
    pub prepared: Option<Prepared>,
    /// Which [`Row`] of the step the cursor is on.
    pub cursor: usize,
    /// Text being typed into the row under the cursor.
    pub typing: Option<String>,
    /// How far a long page is scrolled.
    pub scroll: u16,
    pub message: Option<(String, bool)>,
    /// Asked whether to leave.
    pub quitting: bool,
    pub created: Option<Created>,
}

impl<'a> Wizard<'a> {
    /// A wizard, starting from `image`, or registered media `media`, if
    /// either is given.
    pub fn new(app: &'a App, image: Option<&Path>, media: Option<&str>) -> Self {
        let mut registered = media::list_media(&app.paths).unwrap_or_default();
        registered.sort_by(|a, b| a.name.cmp(&b.name));
        let mut wizard = Self {
            app,
            step: Step::Source,
            start: Start::Manual,
            start_chosen: false,
            image: String::new(),
            registered,
            media: None,
            register: false,
            register_name: String::new(),
            observation: None,
            guess: None,
            candidates: None,
            routes: Vec::new(),
            route: None,
            strategies: Vec::new(),
            strategy: None,
            disk_size: DEFAULT_DISK_SIZE.to_string(),
            disk_size_typed: false,
            name: String::new(),
            name_typed: false,
            settings: None,
            prepared: None,
            cursor: 0,
            typing: None,
            scroll: 0,
            message: None,
            quitting: false,
            created: None,
        };
        if let Some(image) = image {
            wizard.set_image(&image.to_string_lossy());
        }
        if let Some(name) = media {
            match wizard.registered.iter().position(|m| m.name == name) {
                Some(index) => wizard.set_media(index),
                None => wizard.tell(
                    format!("no media named {name:?} is registered; see `macman media list`"),
                    true,
                ),
            }
        }
        wizard
    }

    pub fn tell(&mut self, text: impl Into<String>, problem: bool) {
        self.message = Some((text.into(), problem));
    }

    /// The rows of this step, in order.
    pub fn rows(&self) -> Vec<Row> {
        match self.step {
            Step::Source => std::iter::once(Row::Image)
                .chain((0..self.registered.len()).map(Row::Media))
                .chain(rules::STARTS.iter().map(|s| Row::Start(*s)))
                .collect(),
            Step::Backend => (0..self.routes.len()).map(Row::Route).collect(),
            Step::Disk => {
                let mut rows: Vec<Row> = (0..self.strategies.len()).map(Row::Strategy).collect();
                if self.strategy.as_ref().is_some_and(Strategy::is_blank) {
                    rows.push(Row::DiskSize);
                }
                rows.push(Row::Name);
                if self.can_register() {
                    rows.push(Row::Register);
                    if self.register {
                        rows.push(Row::RegisterName);
                    }
                }
                rows
            }
            _ => Vec::new(),
        }
    }

    pub fn row(&self) -> Option<Row> {
        self.rows().get(self.cursor).copied()
    }

    /// The image, when the start uses one and it's been read.
    pub fn used_observation(&self) -> Option<&Observation> {
        self.start
            .needs_image()
            .then_some(self.observation.as_ref())
            .flatten()
    }

    /// Whether the image is a new file that could be registered as media.
    pub fn can_register(&self) -> bool {
        self.media.is_none()
            && self.used_observation().is_some_and(|o| o.media.is_ok())
            && matches!(self.start, Start::InstallerCd | Start::InstallerFloppy)
    }

    /// The registered media picked, if one was and the start uses it.
    pub fn used_media(&self) -> Option<&Media> {
        let name = self.media.as_ref()?;
        self.used_observation()?;
        self.registered.iter().find(|m| &m.name == name)
    }

    fn used_guess(&self) -> Option<&OsGuess> {
        self.used_observation().and(self.guess.as_ref())
    }

    pub fn image_path(&self) -> Option<PathBuf> {
        let path = self.used_observation()?.path.clone();
        Some(std::fs::canonicalize(&path).unwrap_or(path))
    }

    /// Start from registered media: its own copy is read, as any image is,
    /// and what it was registered as counts too.
    fn set_media(&mut self, index: usize) {
        let medium = self.registered[index].clone();
        let Ok((_, file)) = media::load_media(&self.app.paths, &medium.name) else {
            return self.tell(format!("media {:?} can't be read", medium.name), true);
        };
        self.set_image(&file.to_string_lossy());
        if self.observation.is_none() {
            return;
        }
        self.media = Some(medium.name.clone());
        if self.guess.is_none() {
            self.guess = medium.version.as_deref().and_then(|version| {
                let observation = self.observation.as_ref()?;
                inspect::from_registered_version(version, observation)
            });
        }
        let looks = match &self.guess {
            Some(guess) => format!("It looks like {}.", guess.label()),
            None => "Nothing on it says which system it holds.".to_string(),
        };
        self.tell(
            format!(
                "Read registered media {}. {looks} Check how to start, then press n.",
                medium.name
            ),
            false,
        );
    }

    /// Take a new image, read it, and work out what it suggests.
    fn set_image(&mut self, text: &str) {
        let text = unquote(text.trim());
        self.image = text.clone();
        self.observation = None;
        self.guess = None;
        self.media = None;
        self.register = false;
        if text.is_empty() {
            return;
        }
        match inspect::observe(Path::new(&text)) {
            Ok(observation) => {
                self.guess = inspect::identify(&observation);
                if !self.start_chosen || self.start == Start::Manual {
                    self.start = Start::suggested(&observation, self.guess.as_ref());
                    self.start_chosen = false;
                }
                let looks = match &self.guess {
                    Some(guess) => format!("It looks like {}.", guess.label()),
                    None => "Nothing on it says which system it holds.".to_string(),
                };
                self.tell(
                    format!("Read {}. {looks} Check how to start, then press n.", text),
                    false,
                );
                self.observation = Some(observation);
            }
            Err(error) => self.tell(format!("{error:#}"), true),
        }
        if !self.name_typed {
            self.name.clear();
        }
    }

    /// Act on one key, returning how the wizard ended, if it did.
    pub fn handle(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(self.leave(false));
        }
        if self.quitting {
            self.quitting = false;
            return (key.code == KeyCode::Char('y')).then(|| self.leave(false));
        }
        if self.step == Step::Settings
            && let Some(settings) = &mut self.settings
        {
            match settings.handle_embedded(key)? {
                Leave::Next => self.next(),
                Leave::Back => self.back(),
                Leave::Quit => self.quitting = true,
            }
            return None;
        }
        if let Some(text) = self.typing.take() {
            self.type_key(key, text);
            return None;
        }
        let count = self.rows().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                self.cursor = self.cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                self.cursor = (self.cursor + 1).min(count - 1)
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(1)
            }
            KeyCode::Enter if self.step == Step::Review => self.create(),
            KeyCode::Char('c') if self.step == Step::Review => self.create(),
            KeyCode::Char('l') if self.step == Step::Done => return Some(self.leave(true)),
            KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc if self.step == Step::Done => {
                return Some(self.leave(false));
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('n') | KeyCode::Tab => self.next(),
            KeyCode::Char('b') | KeyCode::BackTab => self.back(),
            KeyCode::Esc if self.step == Step::Source => self.quitting = true,
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') if self.step == Step::Backend => {
                self.candidates = None;
                self.recommend();
                self.tell("Looked again at this host's emulators and ROMs.", false);
            }
            KeyCode::Char('q') => self.quitting = true,
            _ => {}
        }
        None
    }

    fn leave(&mut self, launch: bool) -> Outcome {
        match self.created.clone() {
            Some(created) => Outcome::Created { created, launch },
            None => Outcome::Cancelled,
        }
    }

    fn activate(&mut self) {
        match self.row() {
            Some(Row::Image) => self.typing = Some(self.image.clone()),
            Some(Row::Name) => self.typing = Some(self.name.clone()),
            Some(Row::DiskSize) => self.typing = Some(self.disk_size.clone()),
            Some(Row::RegisterName) => self.typing = Some(self.register_name.clone()),
            Some(Row::Media(index)) => self.set_media(index),
            Some(Row::Register) => {
                self.register = !self.register;
                if self.register && self.register_name.is_empty() {
                    self.register_name = self.suggested_media_name();
                }
            }
            Some(Row::Start(start)) => {
                self.start = start;
                self.start_chosen = true;
                self.message = None;
            }
            Some(Row::Route(index)) => {
                let route = &self.routes[index];
                match &route.fit {
                    rules::Fit::Blocked(why) => {
                        let text = format!("{} can't be had here: {why}", route.label());
                        self.tell(text, true);
                    }
                    _ => {
                        self.route = Some((route.backend.clone(), route.variant));
                        self.message = None;
                    }
                }
            }
            Some(Row::Strategy(index)) => {
                let (strategy, problem) = self.strategies[index].clone();
                match problem {
                    Some(why) => self.tell(format!("Not possible: {why}"), true),
                    None => {
                        self.strategy = Some(strategy);
                        self.message = None;
                    }
                }
            }
            None => {}
        }
    }

    fn type_key(&mut self, key: KeyEvent, mut text: String) {
        match key.code {
            KeyCode::Enter => {
                let row = self.row();
                let result = match row {
                    Some(Row::Image) => {
                        self.set_image(&text);
                        Ok(())
                    }
                    Some(Row::Name) => self.set_name(text.trim()),
                    Some(Row::DiskSize) => parse_disk_size(text.trim()).map(|_| {
                        self.disk_size = text.trim().to_string();
                        self.disk_size_typed = true;
                    }),
                    Some(Row::RegisterName) => self.set_register_name(text.trim()),
                    _ => Ok(()),
                };
                let sized = row == Some(Row::DiskSize) && result.is_ok();
                if let Err(error) = result {
                    self.tell(format!("{error:#}"), true);
                    self.typing = Some(text);
                } else if row != Some(Row::Image) {
                    self.message = None;
                }
                if sized
                    && let Some(why) = parse_disk_size(&self.disk_size)
                        .ok()
                        .and_then(|bytes| rules::disk_too_small(self.used_guess(), bytes))
                {
                    self.tell(format!("That may be too small: {why}."), true);
                }
            }
            KeyCode::Esc => {}
            KeyCode::Backspace => {
                text.pop();
                self.typing = Some(text);
            }
            KeyCode::Char(c) => {
                text.push(c);
                self.typing = Some(text);
            }
            _ => self.typing = Some(text),
        }
    }

    fn set_name(&mut self, name: &str) -> anyhow::Result<()> {
        validate_name("machine", name)?;
        if self.app.paths.machine(name).exists() {
            anyhow::bail!("machine {name:?} already exists; pick another name");
        }
        crate::aliases::check_new_name(&self.app.paths, name)?;
        self.name = name.to_string();
        self.name_typed = true;
        Ok(())
    }

    fn set_register_name(&mut self, name: &str) -> anyhow::Result<()> {
        validate_name("media", name)?;
        if media::media_exists(&self.app.paths, name) {
            anyhow::bail!("media named {name:?} is already registered; pick another name");
        }
        self.register_name = name.to_string();
        Ok(())
    }

    /// The machine's suggested name, as a media name not yet taken.
    fn suggested_media_name(&self) -> String {
        let base = self.suggested_name();
        let mut candidate = base.clone();
        let mut number = 2;
        while media::media_exists(&self.app.paths, &candidate) {
            candidate = format!("{base}-{number}");
            number += 1;
        }
        candidate
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        self.scroll = 0;
        self.cursor = match step {
            Step::Source => 0,
            Step::Backend => self
                .routes
                .iter()
                .position(|r| Some((r.backend.clone(), r.variant)) == self.route)
                .unwrap_or(0),
            Step::Disk => self
                .strategies
                .iter()
                .position(|(s, _)| Some(s) == self.strategy.as_ref())
                .unwrap_or(0),
            _ => 0,
        };
    }

    pub fn next(&mut self) {
        self.message = None;
        match self.step {
            Step::Source => {
                if self.start.needs_image() {
                    if self.image.is_empty() {
                        return self.tell(
                            "Give the image's path first: Enter on the Image line to type it.",
                            true,
                        );
                    }
                    if self.observation.is_none() {
                        // Read it again, to show why it couldn't be.
                        let image = self.image.clone();
                        self.set_image(&image);
                        if self.observation.is_none() {
                            return;
                        }
                        self.message = None;
                    }
                    self.go(Step::Findings);
                } else {
                    self.recommend();
                    self.go(Step::Backend);
                }
            }
            Step::Findings => {
                self.recommend();
                self.go(Step::Backend);
            }
            Step::Backend => {
                let Some(route) = self.chosen_route() else {
                    return self.tell("Pick an emulator first: Enter on its line.", true);
                };
                if !route.usable() {
                    return self.tell("That emulator can't be had here; pick another.", true);
                }
                self.plan_disk();
                self.go(Step::Disk);
            }
            Step::Disk => {
                if self.register && self.can_register() {
                    let name = self.register_name.clone();
                    if let Err(error) = self.set_register_name(&name) {
                        return self.tell(format!("{error:#}"), true);
                    }
                }
                if let Err(error) = self.prepare() {
                    return self.tell(format!("{error:#}"), true);
                }
                self.go(Step::Settings);
            }
            Step::Settings => {
                let Some(settings) = &self.settings else {
                    return;
                };
                let editor = settings.editor();
                if let Err(error) = editor.check(&editor.draft) {
                    return self.tell(format!("{error:#}"), true);
                }
                let draft = editor.draft.clone();
                if let Some(prepared) = &mut self.prepared {
                    prepared.machine = draft;
                }
                self.go(Step::Review);
            }
            Step::Review => self.tell("Press Enter or c to make it, or b to go back.", false),
            Step::Done => {}
        }
    }

    pub fn back(&mut self) {
        self.message = None;
        self.typing = None;
        let previous = match self.step {
            Step::Source => return self.tell("This is the first step; q leaves.", false),
            Step::Findings => Step::Source,
            Step::Backend if self.start.needs_image() => Step::Findings,
            Step::Backend => Step::Source,
            Step::Disk => Step::Backend,
            Step::Settings => Step::Disk,
            Step::Review => Step::Settings,
            Step::Done => {
                return self.tell("The machine is made; Enter finishes, l starts it.", false);
            }
        };
        self.go(previous);
    }

    pub fn chosen_route(&self) -> Option<&Route> {
        let (backend, model) = self.route.as_ref()?;
        self.routes
            .iter()
            .find(|r| &r.backend == backend && r.variant == *model)
    }

    /// Rank the emulators again, keeping the user's pick if it still works.
    fn recommend(&mut self) {
        if self.candidates.is_none() {
            self.candidates = Some(candidates(self.app));
        }
        let observation = self.used_observation().cloned();
        let guess = self.used_guess().cloned();
        self.routes = rules::routes(
            self.start,
            observation.as_ref(),
            guess.as_ref(),
            self.candidates.as_deref().unwrap_or_default(),
        );
        if !self.chosen_route().is_some_and(Route::usable) {
            self.route = self
                .routes
                .iter()
                .find(|r| r.recommended)
                // One that may well not work is offered, but not picked.
                .or_else(|| self.routes.iter().find(|r| r.fit == rules::Fit::Good))
                .map(|r| (r.backend.clone(), r.variant));
        }
    }

    /// Work out the boot disk choices for the chosen emulator.
    fn plan_disk(&mut self) {
        let installs_from_cd = self
            .route
            .as_ref()
            .and_then(|(backend, _)| self.app.backends.get(backend).ok())
            .is_some_and(|backend| backend.installs_from_cd());
        let systems: Vec<String> = registry::list_systems(&self.app.paths)
            .map(|list| list.into_iter().map(|s| s.name).collect())
            .unwrap_or_default();
        self.strategies = rules::strategies(
            self.start,
            self.used_observation(),
            installs_from_cd,
            &systems,
        );
        let still = self
            .strategies
            .iter()
            .any(|(s, problem)| Some(s) == self.strategy.as_ref() && problem.is_none());
        if !still {
            self.strategy = self
                .strategies
                .iter()
                .find(|(_, problem)| problem.is_none())
                .map(|(s, _)| s.clone());
        }
        if self.name.is_empty() {
            self.name = self.suggested_name();
        }
        if !self.disk_size_typed {
            self.disk_size = rules::suggested_disk_size(self.used_guess()).to_string();
        }
    }

    /// A name from what the image holds, or its volume, not yet taken.
    fn suggested_name(&self) -> String {
        let base = match (self.used_guess(), self.used_observation()) {
            (Some(guess), _) => guess.label(),
            (None, Some(observation)) => observation
                .volumes
                .iter()
                .map(|v| v.name.clone())
                .find(|n| !n.is_empty())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let mut name: String = base
            .to_ascii_lowercase()
            .replace(".x", "")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        while name.contains("--") {
            name = name.replace("--", "-");
        }
        let name = name
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_string();
        let name = if validate_name("machine", &name).is_ok() {
            name
        } else {
            "mac".to_string()
        };
        let mut candidate = name.clone();
        let mut number = 2;
        while self.app.paths.machine(&candidate).exists()
            || crate::aliases::check_new_name(&self.app.paths, &candidate).is_err()
        {
            candidate = format!("{name}-{number}");
            number += 1;
        }
        candidate
    }

    /// The `create` arguments the choices so far come to.
    pub fn create_args(&self) -> anyhow::Result<(CreateArgs, Option<PathBuf>)> {
        let route = self
            .chosen_route()
            .ok_or_else(|| anyhow::anyhow!("no emulator is chosen"))?;
        let strategy = self
            .strategy
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no boot disk is chosen"))?;
        let uses_rom = self
            .app
            .backends
            .get(&route.backend)
            .is_ok_and(|backend| backend.uses_rom());
        let rom = match route.rom.clone() {
            None if uses_rom => anyhow::bail!("no registered ROM fits {}", route.label()),
            rom => rom,
        };
        let (system, disk_size, image) = match strategy {
            Strategy::CopyImage => (None, None, self.image_path()),
            Strategy::Registered(name) => (Some(name), None, None),
            Strategy::Blank | Strategy::BlankWithCd => (None, Some(self.disk_size.clone()), None),
        };
        let args = CreateArgs {
            name: self.name.clone(),
            backend: Some(route.backend.clone()),
            model: route.variant.map(Variant::argument),
            rom,
            system,
            disk_size,
            memory: None,
            display: None,
            scale: None,
            fullscreen: false,
            cpu: None,
            no_fpu: false,
            no_sound: false,
            network: None,
            shared_folder: None,
            ephemeral: false,
            // Registered media is checked against the machine, as `create
            // --media` checks it.
            media: match (&self.strategy, self.used_media()) {
                (Some(Strategy::BlankWithCd), Some(medium)) => vec![medium.name.clone()],
                _ => Vec::new(),
            },
        };
        Ok((args, image))
    }

    /// Check the plan as `create` would, and open its settings, keeping any
    /// the user already changed for the same emulator.
    fn prepare(&mut self) -> anyhow::Result<()> {
        validate_name("machine", &self.name)?;
        let (args, image) = self.create_args()?;
        let source = match &image {
            Some(image) => BootSource::Image(image),
            None => BootSource::Args,
        };
        let prepared = self.app.prepare_create(&args, source)?;
        let mut editor = Editor::for_new(
            &self.app.paths,
            &self.app.backends,
            prepared.machine.clone(),
        )?;
        if let Some(old) = &self.settings {
            let old = &old.editor().draft;
            let same = old.emulator.backend == prepared.machine.emulator.backend
                && old.hardware.model == prepared.machine.hardware.model
                && old.qemu.as_ref().map(|q| q.machine)
                    == prepared.machine.qemu.as_ref().map(|q| q.machine);
            if same {
                let draft = &mut editor.draft;
                draft.ephemeral = old.ephemeral;
                draft.hardware.rom = old.hardware.rom.clone();
                draft.hardware.memory = old.hardware.memory.clone();
                draft.display = old.display.clone();
                draft.basilisk2 = old.basilisk2.clone();
                draft.sheepshaver = old.sheepshaver.clone();
                draft.qemu = old.qemu.clone();
            }
        }
        self.settings = Some(tui::Ui::embedded(editor));
        self.prepared = Some(prepared);
        Ok(())
    }

    /// The settings step's editor, once the plan has got that far.
    pub fn settings(&self) -> Option<&tui::Ui<'a>> {
        self.settings.as_ref()
    }

    fn situation<'s>(&'s self, backend: &'s str, strategy: &'s Strategy) -> Situation<'s> {
        Situation {
            start: self.start,
            guess: self.used_guess(),
            observation: self.used_observation(),
            backend,
            variant: self.chosen_route().and_then(|r| r.variant),
            strategy,
            ephemeral: self.prepared.as_ref().is_some_and(|p| p.machine.ephemeral),
            name: &self.name,
        }
    }

    /// Guidance for what's chosen so far.
    pub fn guidance(&self) -> Vec<String> {
        let (Some(route), Some(strategy)) = (self.chosen_route(), &self.strategy) else {
            return Vec::new();
        };
        rules::guidance(&self.situation(&route.backend, strategy))
    }

    /// The plan, as the review step shows it.
    pub fn plan(&self) -> Vec<String> {
        let Some(prepared) = &self.prepared else {
            return Vec::new();
        };
        let machine = &prepared.machine;
        let mut lines = vec![format!("Machine {}", machine.name)];
        if let Some(settings) = &self.settings {
            for field in settings.editor().fields() {
                if field.label == "Name" {
                    continue;
                }
                let value = if field.value.is_empty() {
                    "none"
                } else {
                    &field.value
                };
                let mark = if field.changed { "  (your choice)" } else { "" };
                lines.push(format!("  {}: {value}{mark}", field.label));
            }
        }
        lines.push(String::new());
        let image = self
            .image_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let destination = self.app.paths.machine(&machine.name);
        match &self.strategy {
            Some(Strategy::CopyImage) => {
                lines.push(format!(
                    "Boot disk: the volume in {image}, copied into {}",
                    destination.display()
                ));
                lines.push("The image itself is only read, never changed.".to_string());
            }
            Some(Strategy::Registered(name)) => lines.push(format!(
                "Boot disk: a copy of registered system image {name}, in {}",
                destination.display()
            )),
            Some(Strategy::BlankWithCd) => {
                lines.push(format!(
                    "Boot disk: a new blank {} disk, in {}",
                    self.disk_size,
                    destination.display()
                ));
                match self.used_media() {
                    Some(medium) => lines.push(format!(
                        "Installer CD: registered media {}, attached whole and read-only, \
                         partition map and all, each time it's started with --media.",
                        medium.name
                    )),
                    None => lines.push(format!(
                        "Installer CD: {image}, attached whole and read-only, partition map \
                         and all, each time it's started with --cdrom."
                    )),
                }
            }
            Some(Strategy::Blank) => lines.push(format!(
                "Boot disk: a new blank {} disk, in {}",
                self.disk_size,
                destination.display()
            )),
            None => {}
        }
        let backend = crate::wizard::rules::backend_label(&machine.emulator.backend);
        lines.push(format!(
            "Emulator: {backend} {}. The first start builds it, fetching its source unless a \
             source archive is set, if this configuration isn't built yet.",
            machine.emulator.revision
        ));
        if self.register && self.can_register() {
            let size = self.used_observation().map_or(0, |o| o.size);
            lines.push(format!(
                "Registers {image} as media {}: a read-only copy of the whole image, {}, in {}. \
                 The file itself isn't changed.",
                self.register_name,
                inspect::size_text(size),
                self.app.paths.media().display()
            ));
            lines.push(
                "Nothing else changes: no system images are registered, and no other machine \
                 is touched."
                    .to_string(),
            );
        } else {
            lines.push(
                "Nothing else changes: no media or system images are registered, and no other \
                 machine is touched."
                    .to_string(),
            );
        }
        let mut caveats = prepared.warnings.clone();
        caveats.extend(self.guidance());
        if !caveats.is_empty() {
            lines.push(String::new());
            lines.push("Before starting:".to_string());
            lines.extend(caveats.into_iter().map(|c| format!("  • {c}")));
        }
        lines
    }

    /// Make the machine, as `create` would.
    fn create(&mut self) {
        let Some(prepared) = &self.prepared else {
            return;
        };
        if let Err(error) = self.app.publish_prepared(prepared) {
            return self.tell(
                format!("{error:#}. Nothing was made; go back to change the plan."),
                true,
            );
        }
        let strategy = self.strategy.clone().unwrap_or(Strategy::Blank);
        let backend = prepared.machine.emulator.backend.clone();
        let image = self.image_path();
        // Registered after the machine is made, so a plan that fails leaves
        // the registry as it was. The source is only read.
        let mut registered = None;
        let mut note = None;
        if self.register
            && self.can_register()
            && let Some(image) = &image
        {
            let options = media::AddOptions {
                kind: match self.start {
                    Start::InstallerCd => Some(media::MediaKind::Cdrom),
                    Start::InstallerFloppy => Some(media::MediaKind::Floppy),
                    _ => None,
                },
                version: None,
                any_format: false,
            };
            match media::add_media(&self.app.paths, image, &self.register_name, &options) {
                Ok(medium) => registered = Some(medium.name),
                Err(error) => {
                    note = Some(format!(
                        "The machine is made, but the image wasn't registered: {error:#}"
                    ))
                }
            }
        }
        let install = (strategy == Strategy::BlankWithCd)
            .then(|| match (registered.clone(), self.used_media()) {
                (Some(name), _) => Some(Install::Media(name)),
                (None, Some(medium)) => Some(Install::Media(medium.name.clone())),
                (None, None) => image.clone().map(Install::Cd),
            })
            .flatten();
        let situation = self.situation(&backend, &strategy);
        self.created = Some(Created {
            name: self.name.clone(),
            install: install.clone(),
            registered,
            guidance: rules::guidance(&situation),
            after: rules::after_creation(&situation, install.as_ref()),
        });
        self.message = note.map(|note| (note, true));
        self.go(Step::Done);
    }
}

/// `"a b"` or `'a b'` as `a b`, as a path pasted from a shell may be quoted.
fn unquote(text: &str) -> String {
    let quoted = text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')));
    let text = if quoted {
        &text[1..text.len() - 1]
    } else {
        text
    };
    match (text.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).to_string_lossy().into_owned(),
        _ => text.to_string(),
    }
}

/// Every backend, and Mini vMac model or QEMU machine, this host has, each
/// with a ROM that fits, none if it needs none, or why it can't be had,
/// checked as `create` checks them.
pub fn candidates(app: &App) -> Vec<Candidate> {
    let roms: Vec<String> = {
        let mut roms: Vec<String> = registry::list_roms(&app.paths)
            .map(|list| list.into_iter().map(|rom| rom.name).collect())
            .unwrap_or_default();
        roms.sort();
        roms
    };
    let probe = format!("wizard-probe-{}", std::process::id());
    let mut list = Vec::new();
    for backend in app.backends.iter() {
        let name = backend.name().to_string();
        let label = rules::backend_label(&name).to_string();
        let installs_from_cd = backend.installs_from_cd();
        let blocked = |why: String| Candidate {
            backend: name.clone(),
            variant: None,
            rom: Err(why),
            installs_from_cd,
        };
        if let Err(error) = backend.check_host() {
            list.push(blocked(format!("{error:#}")));
            continue;
        }
        if !backend.uses_rom() {
            for kind in [QemuMachine::Mac99, QemuMachine::G3beige] {
                let variant = Some(Variant::Qemu(kind));
                let args = probe_args(&probe, &name, variant, None);
                list.push(Candidate {
                    backend: name.clone(),
                    variant,
                    rom: app
                        .prepare_create(&args, BootSource::Unchosen)
                        .map(|_| None)
                        .map_err(|error| format!("{error:#}")),
                    installs_from_cd,
                });
            }
            continue;
        }
        if roms.is_empty() {
            list.push(blocked(format!(
                "no ROM is registered; register one {label} can start from with `macman rom \
                 add FILE --name NAME`, then press r here"
            )));
            continue;
        }
        let models: Vec<Option<Variant>> = if name == rules::MINIVMAC {
            rules::MODEL_ORDER
                .iter()
                .map(|model| Some(Variant::Model(*model)))
                .collect()
        } else {
            vec![None]
        };
        let mut found = false;
        let mut first_problem = None;
        for model in models {
            let fits = roms.iter().find(|rom| {
                let args = probe_args(&probe, &name, model, Some(rom));
                match app.prepare_create(&args, BootSource::Unchosen) {
                    Ok(_) => true,
                    Err(error) => {
                        first_problem.get_or_insert_with(|| format!("{error:#}"));
                        false
                    }
                }
            });
            if let Some(rom) = fits {
                found = true;
                list.push(Candidate {
                    backend: name.clone(),
                    variant: model,
                    rom: Ok(Some(rom.clone())),
                    installs_from_cd,
                });
            }
        }
        if !found {
            list.push(blocked(format!(
                "no registered ROM fits {label} ({}); register one with `macman rom add FILE \
                 --name NAME`, then press r here",
                first_problem.unwrap_or_default()
            )));
        }
    }
    list
}

fn probe_args(
    name: &str,
    backend: &str,
    variant: Option<Variant>,
    rom: Option<&str>,
) -> CreateArgs {
    CreateArgs {
        name: name.to_string(),
        backend: Some(backend.to_string()),
        model: variant.map(Variant::argument),
        rom: rom.map(str::to_string),
        system: None,
        disk_size: None,
        memory: None,
        display: None,
        scale: None,
        fullscreen: false,
        cpu: None,
        no_fpu: false,
        no_sound: false,
        network: None,
        shared_folder: None,
        ephemeral: false,
        media: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::tests::{add_rom, clean_rom, plus_rom, powerpc_rom};
    use crate::paths::Paths;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(wizard: &mut Wizard, keys: &str) -> Option<Outcome> {
        let mut outcome = None;
        for c in keys.chars() {
            outcome = wizard.handle(key(KeyCode::Char(c)));
        }
        outcome
    }

    fn type_text(wizard: &mut Wizard, text: &str) {
        for c in text.chars() {
            wizard.handle(key(KeyCode::Char(c)));
        }
        wizard.handle(key(KeyCode::Enter));
    }

    fn screen(wizard: &Wizard) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| ui::draw(wizard, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn app(root: &Path) -> App {
        let paths = Paths::under(root);
        let backends = crate::default_backends(&paths).unwrap();
        App::new(paths, backends)
    }

    /// Boot blocks, then an HFS master directory block naming the volume
    /// and, if `blessed`, a System Folder. No Apple code: headers only.
    fn hfs_volume(bytes: &mut [u8], at: usize, name: &str, blessed: bool) {
        bytes[at..at + 2].copy_from_slice(b"LK");
        let mdb = at + 1024;
        bytes[mdb..mdb + 2].copy_from_slice(b"BD");
        bytes[mdb + 10] = 0x01;
        bytes[mdb + 0x24] = name.len() as u8;
        bytes[mdb + 0x25..mdb + 0x25 + name.len()].copy_from_slice(name.as_bytes());
        if blessed {
            bytes[mdb + 0x5C..mdb + 0x60].copy_from_slice(&123u32.to_be_bytes());
        }
    }

    /// A synthetic installer CD: a partition map with a CD driver, and a
    /// startable HFS volume called `name`.
    fn installer_cd(dir: &Path, name: &str) -> PathBuf {
        let blocks = 4096u32;
        let mut bytes = vec![0u8; blocks as usize * 512];
        bytes[..2].copy_from_slice(b"ER");
        bytes[2..4].copy_from_slice(&512u16.to_be_bytes());
        let parts = [
            (1u32, 63u32, "Apple_partition_map"),
            (64, 32, "Apple_Driver43_CD"),
            (96, blocks - 96, "Apple_HFS"),
        ];
        for (index, (start, length, kind)) in parts.iter().enumerate() {
            let at = (index + 1) * 512;
            bytes[at..at + 2].copy_from_slice(b"PM");
            bytes[at + 4..at + 8].copy_from_slice(&3u32.to_be_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&start.to_be_bytes());
            bytes[at + 12..at + 16].copy_from_slice(&length.to_be_bytes());
            bytes[at + 48..at + 48 + kind.len()].copy_from_slice(kind.as_bytes());
        }
        hfs_volume(&mut bytes, 96 * 512, name, true);
        let path = dir.join(format!("{name}.toast"));
        fs::write(&path, bytes).unwrap();
        path
    }

    /// A bare HFS volume of `size` bytes.
    fn hfs_image(dir: &Path, file: &str, name: &str, size: usize, blessed: bool) -> PathBuf {
        let mut bytes = vec![0u8; size];
        hfs_volume(&mut bytes, 0, name, blessed);
        if !blessed {
            bytes[..2].copy_from_slice(b"\0\0");
        }
        let path = dir.join(file);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn cd_installs(app: &App, backend: &str) -> bool {
        let backend = app.backends.get(backend).unwrap();
        backend.check_host().is_ok() && backend.installs_from_cd()
    }

    fn machines(app: &App) -> Vec<String> {
        crate::fsutil::list_dir(&app.paths.machines()).unwrap_or_default()
    }

    #[test]
    fn a_mac_os_9_installer_cd_becomes_a_sheepshaver_install() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "tnt", powerpc_rom);
        add_rom(&app.paths, "quadra", clean_rom);
        let dir = root.path().join("my cds");
        fs::create_dir_all(&dir).unwrap();
        let cd = installer_cd(&dir, "Mac OS 9.0.4");
        let before = fs::read(&cd).unwrap();

        let mut wizard = Wizard::new(&app, Some(&cd), None);
        assert_eq!(wizard.start, Start::InstallerCd);
        assert!(!wizard.start_chosen);
        assert!(screen(&wizard).contains("(suggested)"));
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Findings);
        let findings = screen(&wizard);
        assert!(findings.contains("Apple_Driver43_CD"), "{findings}");
        assert!(
            findings.contains("Probably Mac OS 9.0.4, an installer, with medium confidence"),
            "{findings}"
        );
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Backend);
        let sheepshaver = wizard
            .routes
            .iter()
            .find(|r| r.backend == "sheepshaver")
            .unwrap();
        if !cd_installs(&app, "sheepshaver") {
            // This host can't install from CDs, and the wizard says so
            // rather than offer it.
            assert!(!sheepshaver.usable());
            assert_eq!(wizard.route, None);
            press(&mut wizard, "n");
            assert_eq!(wizard.step, Step::Backend);
            return;
        }
        assert!(sheepshaver.recommended);
        assert_eq!(wizard.routes[0].backend, "sheepshaver");
        assert_eq!(wizard.route, Some(("sheepshaver".to_string(), None)));
        assert!(screen(&wizard).contains("★ recommended"));

        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Disk);
        assert_eq!(wizard.strategy, Some(Strategy::BlankWithCd));
        assert_eq!(wizard.name, "mac-os-9.0.4");
        assert_eq!(wizard.disk_size, DEFAULT_DISK_SIZE);

        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Settings, "{:?}", wizard.message);
        let settings = screen(&wizard);
        assert!(settings.contains("tnt"), "{settings}");
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Review);
        let plan = wizard.plan().join("\n");
        assert!(plan.contains("Installer CD:"), "{plan}");
        assert!(
            plan.contains("no media or system images are registered"),
            "{plan}"
        );
        assert!(plan.contains("Update Apple Hard Disk Drivers"), "{plan}");
        assert!(plan.contains("ROM: tnt"), "{plan}");
        // Nothing is written before the plan is confirmed.
        assert!(machines(&app).is_empty());

        wizard.handle(key(KeyCode::Enter));
        assert_eq!(wizard.step, Step::Done, "{:?}", wizard.message);
        let disk = app.paths.machine("mac-os-9.0.4").join("system.dsk");
        assert_eq!(fs::metadata(&disk).unwrap().len(), 1 << 30);
        assert_eq!(fs::read(&cd).unwrap(), before);
        assert!(screen(&wizard).contains("Made machine mac-os-9.0.4"));
        let Some(Outcome::Created { created, launch }) = press(&mut wizard, "l") else {
            panic!("expected a made machine");
        };
        assert!(launch);
        assert_eq!(
            created.install,
            Some(Install::Cd(fs::canonicalize(&cd).unwrap()))
        );
        assert_eq!(created.registered, None);
        assert!(created.after[0].contains("--no-ephemeral --cdrom"));
        assert!(created.after[0].contains("my cds"));
    }

    #[test]
    fn a_system_disk_is_copied_and_offers_every_emulator_it_suits() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "tnt", powerpc_rom);
        add_rom(&app.paths, "quadra", clean_rom);
        let image = hfs_image(root.path(), "hd.img", "Mac OS 8.1 HD", 4 << 20, true);

        let mut wizard = Wizard::new(&app, Some(&image), None);
        assert_eq!(wizard.start, Start::SystemDisk);
        press(&mut wizard, "nn");
        let good: Vec<&str> = wizard
            .routes
            .iter()
            .filter(|r| r.recommended)
            .map(|r| r.backend.as_str())
            .collect();
        assert_eq!(good, ["sheepshaver", "basilisk2"]);

        // Pick Basilisk II, the second, over the first recommendation.
        wizard.handle(key(KeyCode::Down));
        wizard.handle(key(KeyCode::Enter));
        assert_eq!(wizard.route, Some(("basilisk2".to_string(), None)));
        press(&mut wizard, "n");
        assert_eq!(wizard.strategy, Some(Strategy::CopyImage));
        press(&mut wizard, "nn");
        assert_eq!(wizard.step, Step::Review, "{:?}", wizard.message);
        assert!(
            wizard
                .plan()
                .join("\n")
                .contains("is only read, never changed")
        );
        press(&mut wizard, "c");
        assert_eq!(wizard.step, Step::Done, "{:?}", wizard.message);
        let copy = app.paths.machine("mac-os-8.1").join("system.dsk");
        assert_eq!(fs::read(copy).unwrap(), fs::read(&image).unwrap());
        let machine = crate::machine::Machine::load(&app.paths, "mac-os-8.1").unwrap();
        assert_eq!(machine.emulator.backend, "basilisk2");
        assert_eq!(machine.hardware.rom.as_deref(), Some("quadra"));
        assert_eq!(
            press(&mut wizard, "q"),
            Some(Outcome::Created {
                created: wizard.created.clone().unwrap(),
                launch: false
            })
        );
    }

    #[test]
    fn a_mac_os_x_installer_gets_a_bigger_blank_disk_unless_one_is_typed() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        let cd = installer_cd(root.path(), "Mac OS X 10.4");
        let mut wizard = Wizard::new(&app, Some(&cd), None);
        assert_eq!(wizard.start, Start::InstallerCd);
        wizard.route = Some((rules::QEMU.to_string(), None));
        wizard.step = Step::Disk;
        wizard.plan_disk();
        assert_eq!(wizard.strategy, Some(Strategy::BlankWithCd));
        assert_eq!(wizard.disk_size, "10G");

        wizard.cursor = wizard
            .rows()
            .iter()
            .position(|r| *r == Row::DiskSize)
            .unwrap();
        wizard.handle(key(KeyCode::Enter));
        for _ in 0..wizard.disk_size.len() {
            wizard.handle(key(KeyCode::Backspace));
        }
        type_text(&mut wizard, "2G");
        assert_eq!(wizard.disk_size, "2G");
        let (message, _) = wizard.message.clone().unwrap();
        assert!(
            message.contains("4GB") && message.contains("10G"),
            "{message}"
        );

        // What was typed stays when the disk is worked out again.
        wizard.plan_disk();
        assert_eq!(wizard.disk_size, "2G");
    }

    #[test]
    fn going_back_keeps_what_still_applies() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "tnt", powerpc_rom);
        add_rom(&app.paths, "quadra", clean_rom);
        let image = hfs_image(root.path(), "hd.img", "Mac OS 8.1 HD", 4 << 20, true);
        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnn");
        assert_eq!(wizard.step, Step::Settings);
        assert_eq!(wizard.route, Some(("sheepshaver".to_string(), None)));

        // Change the memory in the settings, as `config edit` would.
        let fields = wizard.settings().unwrap().editor().fields();
        let memory = fields.iter().position(|f| f.label == "Memory").unwrap();
        wizard.handle(key(KeyCode::Home));
        for _ in 0..memory {
            wizard.handle(key(KeyCode::Down));
        }
        wizard.handle(key(KeyCode::Right));
        let changed = wizard
            .settings()
            .unwrap()
            .editor()
            .draft
            .hardware
            .memory
            .clone();
        assert_ne!(changed, crate::backend::sheepshaver::DEFAULT_MEMORY);
        // s doesn't save anything here.
        press(&mut wizard, "s");
        assert!(machines(&app).is_empty());

        // Back to the name, which changes; the memory stays.
        press(&mut wizard, "b");
        assert_eq!(wizard.step, Step::Disk);
        wizard.cursor = wizard.rows().iter().position(|r| *r == Row::Name).unwrap();
        wizard.handle(key(KeyCode::Enter));
        for _ in 0..wizard.name.len() {
            wizard.handle(key(KeyCode::Backspace));
        }
        type_text(&mut wizard, "eight one");
        assert!(
            wizard
                .message
                .as_ref()
                .unwrap()
                .0
                .contains("invalid machine name")
        );
        wizard.handle(key(KeyCode::Esc));
        wizard.handle(key(KeyCode::Enter));
        for _ in 0..20 {
            wizard.handle(key(KeyCode::Backspace));
        }
        type_text(&mut wizard, "eight");
        assert_eq!(wizard.name, "eight");
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Settings);
        let draft = &wizard.settings().unwrap().editor().draft;
        assert_eq!(draft.name, "eight");
        assert_eq!(draft.hardware.memory, changed);
        assert!(screen(&wizard).contains("* Memory"));

        // Back to the start: change the image, and the recommendations
        // follow it, while the typed name stays.
        press(&mut wizard, "bbbb");
        assert_eq!(wizard.step, Step::Source);
        let floppy = hfs_image(root.path(), "boot.dsk", "System 6.0.8", 800 * 1024, true);
        add_rom(&app.paths, "plus", plus_rom);
        wizard.cursor = 0;
        wizard.handle(key(KeyCode::Enter));
        for _ in 0..wizard.image.len() {
            wizard.handle(key(KeyCode::Backspace));
        }
        type_text(&mut wizard, &format!("\"{}\"", floppy.display()));
        assert_eq!(wizard.start, Start::InstallerFloppy);
        press(&mut wizard, "r");
        press(&mut wizard, "nn");
        assert_eq!(wizard.step, Step::Backend);
        wizard.candidates = None;
        wizard.recommend();
        assert_eq!(wizard.routes[0].label(), "Mini vMac: Macintosh Plus");
        assert_eq!(wizard.route, Some(("sheepshaver".to_string(), None)));
        assert!(wizard.chosen_route().unwrap().usable());
        wizard.cursor = 0;
        wizard.handle(key(KeyCode::Enter));
        press(&mut wizard, "n");
        assert_eq!(wizard.name, "eight");
        assert_eq!(wizard.strategy, Some(Strategy::CopyImage));
        assert!(wizard.guidance().iter().any(|g| g.contains("#28")));
        press(&mut wizard, "n");
        let draft = &wizard.settings().unwrap().editor().draft;
        assert_eq!(draft.emulator.backend, "minivmac");
        assert_eq!(draft.hardware.memory, "4M");
    }

    #[test]
    fn an_unrecognized_image_guesses_nothing_and_allows_manual_setup() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "quadra", clean_rom);
        let image = root.path().join("mystery.img");
        fs::write(&image, vec![0x5Au8; 1 << 20]).unwrap();
        let mut wizard = Wizard::new(&app, Some(&image), None);
        assert_eq!(wizard.guess, None);
        press(&mut wizard, "n");
        assert!(screen(&wizard).contains("No system version could be read"));
        press(&mut wizard, "n");
        assert!(wizard.routes.iter().all(|r| !r.usable()));
        assert_eq!(wizard.route, None);

        // Set it up by hand instead.
        press(&mut wizard, "bb");
        wizard.cursor = wizard
            .rows()
            .iter()
            .position(|r| *r == Row::Start(Start::Manual))
            .unwrap();
        wizard.handle(key(KeyCode::Enter));
        assert!(wizard.start_chosen);
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Backend);
        let basilisk2 = wizard
            .routes
            .iter()
            .find(|r| r.backend == "basilisk2")
            .unwrap();
        assert!(basilisk2.usable());
        assert!(!basilisk2.recommended);
        assert_eq!(wizard.name, "");
        if !cd_installs(&app, "basilisk2") {
            press(&mut wizard, "n");
            assert_eq!(wizard.strategy, None);
            return;
        }
        assert_eq!(wizard.route, Some(("basilisk2".to_string(), None)));
        press(&mut wizard, "nnn");
        assert_eq!(wizard.step, Step::Review, "{:?}", wizard.message);
        assert_eq!(wizard.name, "mac");
        let plan = wizard.plan().join("\n");
        // No system is named: none was identified.
        assert!(
            !plan.contains("Mac OS 8") && !plan.contains("Mac OS 9"),
            "{plan}"
        );
        assert!(!plan.contains("System 7"), "{plan}");
        assert!(plan.contains("a new blank 1G disk"), "{plan}");
    }

    #[test]
    fn cancelling_or_failing_makes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "quadra", clean_rom);
        add_rom(&app.paths, "tnt", powerpc_rom);
        let image = hfs_image(root.path(), "hd.img", "Mac OS 8.1 HD", 4 << 20, true);
        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnnn");
        assert_eq!(wizard.step, Step::Review);
        assert_eq!(press(&mut wizard, "q"), None);
        assert!(screen(&wizard).contains("Nothing has been made or changed"));
        assert_eq!(press(&mut wizard, "x"), None);
        assert!(!wizard.quitting);
        press(&mut wizard, "q");
        assert_eq!(press(&mut wizard, "y"), Some(Outcome::Cancelled));
        assert!(machines(&app).is_empty());

        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnnn");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(wizard.handle(ctrl_c), Some(Outcome::Cancelled));

        // Something else takes the name before the plan is confirmed.
        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnnn");
        fs::create_dir_all(app.paths.machine("mac-os-8.1")).unwrap();
        press(&mut wizard, "c");
        assert_eq!(wizard.step, Step::Review);
        let (message, problem) = wizard.message.clone().unwrap();
        assert!(problem && message.contains("already exists"), "{message}");
        assert_eq!(machines(&app), ["mac-os-8.1"]);
        assert!(
            fs::read_dir(app.paths.machine("mac-os-8.1"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn a_missing_rom_says_how_to_add_one_and_looks_again() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        let floppy = hfs_image(root.path(), "boot.dsk", "System 6.0.8", 800 * 1024, true);
        let mut wizard = Wizard::new(&app, Some(&floppy), None);
        press(&mut wizard, "nn");
        // QEMU needs no ROM, but doesn't run System 6, so isn't picked.
        let needing_roms = wizard.routes.iter().filter(|r| r.backend != rules::QEMU);
        assert!(needing_roms.clone().all(|r| !r.usable()));
        assert_eq!(wizard.route, None);
        let backend = screen(&wizard);
        assert!(backend.contains("✗ not here"), "{backend}");
        wizard.cursor = wizard
            .routes
            .iter()
            .position(|r| r.backend == rules::MINIVMAC)
            .unwrap();
        wizard.handle(key(KeyCode::Enter));
        let (message, _) = wizard.message.clone().unwrap();
        assert!(message.contains("macman rom add"), "{message}");

        add_rom(&app.paths, "plus", plus_rom);
        press(&mut wizard, "r");
        assert_eq!(
            wizard.chosen_route().map(Route::label).as_deref(),
            Some("Mini vMac: Macintosh Plus")
        );
    }

    #[test]
    fn a_floppy_that_cant_start_a_mac_isnt_offered_as_an_install() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "plus", plus_rom);
        let floppy = hfs_image(root.path(), "disk2.dsk", "Install 2", 800 * 1024, false);
        let mut wizard = Wizard::new(&app, Some(&floppy), None);
        assert_eq!(wizard.start, Start::InstallerFloppy);
        press(&mut wizard, "nnn");
        assert_eq!(wizard.step, Step::Disk);
        assert_eq!(wizard.strategy, None);
        wizard.handle(key(KeyCode::Enter));
        assert!(wizard.message.clone().unwrap().0.contains("#28"));
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Disk);
    }

    #[test]
    fn the_settings_step_is_config_edits_editor() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "quadra", clean_rom);
        add_rom(&app.paths, "tnt", powerpc_rom);
        let image = hfs_image(root.path(), "hd.img", "Mac OS 8.1 HD", 4 << 20, true);
        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnn");
        let text = screen(&wizard);
        assert!(text.contains("n next  b back"), "{text}");
        // The name was chosen a step back, and says so.
        wizard.handle(key(KeyCode::Home));
        wizard.handle(key(KeyCode::Enter));
        let text = screen(&wizard);
        assert!(text.contains("Chosen in an earlier step"), "{text}");
        // A ROM that can't start SheepShaver is offered, refused, and why.
        let fields = wizard.settings().unwrap().editor().fields();
        let rom = fields.iter().find(|f| f.label == "ROM").unwrap();
        let crate::edit::Kind::Choice { choices, .. } = &rom.kind else {
            panic!("ROM is a choice");
        };
        assert!(
            choices
                .iter()
                .any(|c| c.value == "quadra" && c.problem.is_some())
        );
    }

    #[test]
    fn paths_can_be_quoted_or_start_at_home() {
        assert_eq!(unquote("'/a b/c.toast'"), "/a b/c.toast");
        assert_eq!(unquote("\"/a b\""), "/a b");
        assert_eq!(unquote("/plain"), "/plain");
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                unquote("~/x.iso"),
                Path::new(&home).join("x.iso").to_string_lossy()
            );
        }
    }

    #[test]
    fn registered_media_is_offered_and_its_version_counts() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "tnt", powerpc_rom);
        let dir = root.path().join("cds");
        fs::create_dir_all(&dir).unwrap();
        // The volume name says nothing; the version given at registration does.
        let cd = installer_cd(&dir, "Untitled CD");
        let options = media::AddOptions {
            kind: None,
            version: Some("9.0.4"),
            any_format: false,
        };
        media::add_media(&app.paths, &cd, "os9", &options).unwrap();

        let mut wizard = Wizard::new(&app, None, None);
        assert_eq!(wizard.rows()[1], Row::Media(0));
        let source = screen(&wizard);
        assert!(source.contains("Or registered media"), "{source}");
        assert!(source.contains("os9 (cdrom, 9.0.4)"), "{source}");
        wizard.cursor = 1;
        wizard.handle(key(KeyCode::Enter));
        assert_eq!(wizard.media.as_deref(), Some("os9"));
        assert_eq!(wizard.start, Start::InstallerCd);
        let guess = wizard.guess.clone().unwrap();
        assert_eq!(guess.label(), "Mac OS 9.0.4");
        assert_eq!(guess.evidence, "it was registered as version \"9.0.4\"");
        press(&mut wizard, "n");
        assert!(screen(&wizard).contains("Registered as media os9: a cdrom, given version 9.0.4"));
        press(&mut wizard, "n");
        if !cd_installs(&app, "sheepshaver") {
            return;
        }
        assert_eq!(wizard.route, Some(("sheepshaver".to_string(), None)));
        press(&mut wizard, "n");
        // It's registered already, so there's nothing to offer to register.
        assert!(!wizard.rows().contains(&Row::Register));
        press(&mut wizard, "nn");
        assert_eq!(wizard.step, Step::Review, "{:?}", wizard.message);
        assert!(wizard.plan().join("\n").contains("registered media os9"));
        press(&mut wizard, "c");
        let created = wizard.created.clone().unwrap();
        assert_eq!(created.install, Some(Install::Media("os9".into())));
        assert!(created.after[0].contains("--media os9"));
    }

    #[test]
    fn a_new_image_is_registered_only_when_asked_and_after_the_machine() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "tnt", powerpc_rom);
        let cd = installer_cd(root.path(), "Mac OS 9.0.4");
        let before = fs::read(&cd).unwrap();
        let mut wizard = Wizard::new(&app, Some(&cd), None);
        press(&mut wizard, "nn");
        if !cd_installs(&app, "sheepshaver") {
            return;
        }
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Disk);
        let register = wizard
            .rows()
            .iter()
            .position(|r| *r == Row::Register)
            .unwrap();
        wizard.cursor = register;
        wizard.handle(key(KeyCode::Enter));
        assert!(wizard.register);
        assert_eq!(wizard.register_name, "mac-os-9.0.4");
        assert_eq!(wizard.rows()[register + 1], Row::RegisterName);

        // A name already taken is refused when it's typed, and when going on.
        media::add_media(&app.paths, &cd, "taken", &media::AddOptions::default()).unwrap();
        wizard.cursor = register + 1;
        wizard.handle(key(KeyCode::Enter));
        for _ in 0..wizard.register_name.len() {
            wizard.handle(key(KeyCode::Backspace));
        }
        type_text(&mut wizard, "taken");
        assert!(
            wizard
                .message
                .clone()
                .unwrap()
                .0
                .contains("already registered")
        );
        wizard.handle(key(KeyCode::Esc));
        wizard.register_name = "taken".into();
        press(&mut wizard, "n");
        assert_eq!(wizard.step, Step::Disk);
        wizard.register_name = "os9-cd".into();

        press(&mut wizard, "nn");
        assert_eq!(wizard.step, Step::Review, "{:?}", wizard.message);
        assert!(wizard.plan().join("\n").contains("as media os9-cd"));
        // Still nothing registered before the plan is confirmed.
        assert!(!media::media_exists(&app.paths, "os9-cd"));
        press(&mut wizard, "c");
        assert_eq!(wizard.step, Step::Done, "{:?}", wizard.message);
        let (medium, file) = media::load_media(&app.paths, "os9-cd").unwrap();
        assert_eq!(medium.kind, media::MediaKind::Cdrom);
        assert_eq!(fs::read(file).unwrap(), before);
        assert_eq!(fs::read(&cd).unwrap(), before);
        let created = wizard.created.clone().unwrap();
        assert_eq!(created.registered.as_deref(), Some("os9-cd"));
        assert_eq!(created.install, Some(Install::Media("os9-cd".into())));
    }

    #[test]
    fn names_taken_by_aliases_are_neither_suggested_nor_accepted() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        add_rom(&app.paths, "quadra", clean_rom);
        add_rom(&app.paths, "tnt", powerpc_rom);
        let image = hfs_image(root.path(), "hd.img", "Mac OS 8.1 HD", 4 << 20, true);
        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnnnn");
        press(&mut wizard, "c");
        assert_eq!(wizard.step, Step::Done, "{:?}", wizard.message);
        crate::aliases::add(&app.paths, "mac-os-8.1", "mac-os-8.1-2").unwrap();

        let mut wizard = Wizard::new(&app, Some(&image), None);
        press(&mut wizard, "nnn");
        assert_eq!(wizard.name, "mac-os-8.1-3");
        assert!(wizard.set_name("mac-os-8.1-2").is_err());
    }

    #[test]
    fn media_named_at_the_start_is_picked_or_reported() {
        let root = tempfile::tempdir().unwrap();
        let app = app(root.path());
        let floppy = hfs_image(root.path(), "boot.dsk", "System 6.0.8", 800 * 1024, true);
        media::add_media(&app.paths, &floppy, "six", &media::AddOptions::default()).unwrap();
        let wizard = Wizard::new(&app, None, Some("six"));
        assert_eq!(wizard.media.as_deref(), Some("six"));
        assert_eq!(wizard.start, Start::InstallerFloppy);
        let wizard = Wizard::new(&app, None, Some("seven"));
        assert!(wizard.message.clone().unwrap().0.contains("no media named"));
        assert_eq!(wizard.media, None);
    }
}
