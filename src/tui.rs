//! The terminal interface of `macman config edit`. All the rules live in
//! [`crate::edit`]; this only shows its fields and passes keys to it. Key
//! handling is a plain state machine, so it's tested without a terminal.

use crate::edit::{Editor, Field, Kind};
use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

/// How an editing session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Saved,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Browse,
    /// Picking from a field's choices; `at` is the highlighted one.
    Choosing {
        at: usize,
    },
    /// Typing a value for the selected field.
    Typing {
        text: String,
    },
    /// Looking over the changes before saving.
    Review,
    /// Asked whether to leave without saving.
    Leaving,
}

pub struct Ui<'a> {
    editor: Editor<'a>,
    /// Which field, of those [`Editor::fields`] returns.
    selected: usize,
    mode: Mode,
    /// The last thing to report, and whether it's a problem.
    message: Option<(String, bool)>,
}

impl<'a> Ui<'a> {
    pub fn new(editor: Editor<'a>) -> Self {
        let mut ui = Self {
            editor,
            selected: 0,
            mode: Mode::Browse,
            message: None,
        };
        // Start on the first field that can change.
        ui.selected = ui
            .editor
            .fields()
            .iter()
            .position(|field| field.id.is_some())
            .unwrap_or(0);
        ui
    }

    fn field(&self) -> Field {
        self.editor.fields().swap_remove(self.selected)
    }

    fn tell(&mut self, text: impl Into<String>, problem: bool) {
        self.message = Some((text.into(), problem));
    }

    fn report(&mut self, result: Result<()>) {
        match result {
            Ok(()) => self.message = None,
            Err(error) => self.tell(format!("{error:#}"), true),
        }
    }

    /// Act on one key, returning how editing ended, if it did.
    pub fn handle(&mut self, key: KeyEvent) -> Option<Ending> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Ending::Cancelled);
        }
        match self.mode.clone() {
            Mode::Browse => self.browse(key),
            Mode::Choosing { at } => {
                self.choose(key, at);
                None
            }
            Mode::Typing { text } => {
                self.typing(key, text);
                None
            }
            Mode::Review => self.review(key),
            Mode::Leaving => match key.code {
                KeyCode::Char('y') => Some(Ending::Cancelled),
                KeyCode::Char('s') => {
                    self.start_review();
                    None
                }
                _ => {
                    self.mode = Mode::Browse;
                    None
                }
            },
        }
    }

    fn browse(&mut self, key: KeyEvent) -> Option<Ending> {
        let count = self.editor.fields().len();
        let field = self.field();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count - 1)
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count - 1,
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(&field),
            KeyCode::Left | KeyCode::Char('h') => self.step(&field, false),
            KeyCode::Right | KeyCode::Char('l') => self.step(&field, true),
            KeyCode::Char('e') => match (&field.kind, field.id) {
                (Kind::Choice { typed: true, .. } | Kind::Path, Some(_)) => {
                    self.mode = Mode::Typing { text: field.value };
                }
                _ => self.tell("This field takes only the values listed.", true),
            },
            KeyCode::Char('s') => self.start_review(),
            KeyCode::Char('u') => {
                let id = field.id;
                if let Some(id) = id {
                    let value = crate::edit::value_of(&self.editor.original, id);
                    let result = self.editor.set(id, &value);
                    self.report(result);
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                if !self.editor.is_modified() {
                    return Some(Ending::Cancelled);
                }
                self.mode = Mode::Leaving;
            }
            _ => {}
        }
        None
    }

    fn activate(&mut self, field: &Field) {
        let Some(id) = field.id else {
            if let Kind::Fixed(reason) = &field.kind {
                self.tell(
                    format!("{} can't be changed here. {reason}", field.label),
                    true,
                );
            }
            return;
        };
        match &field.kind {
            Kind::Toggle => {
                let result = self.editor.toggle(id);
                self.report(result);
            }
            Kind::Choice { choices, .. } => {
                let at = choices
                    .iter()
                    .position(|choice| choice.value == field.value)
                    .unwrap_or(0);
                self.mode = Mode::Choosing { at };
            }
            Kind::Path => {
                self.mode = Mode::Typing {
                    text: field.value.clone(),
                }
            }
            Kind::Fixed(_) => {}
        }
    }

    /// Move to the next or previous choice that can be had.
    fn step(&mut self, field: &Field, forward: bool) {
        let (Some(id), Kind::Choice { choices, .. }) = (field.id, &field.kind) else {
            if let (Some(id), Kind::Toggle) = (field.id, &field.kind) {
                let result = self.editor.toggle(id);
                self.report(result);
            }
            return;
        };
        let current = choices.iter().position(|c| c.value == field.value);
        let order: Vec<usize> = if forward {
            (current.map_or(0, |at| at + 1)..choices.len()).collect()
        } else {
            (0..current.unwrap_or(choices.len())).rev().collect()
        };
        match order.into_iter().find(|&at| choices[at].problem.is_none()) {
            Some(at) => {
                let result = self.editor.set(id, &choices[at].value);
                self.report(result);
            }
            None => self.tell("No other value fits with the rest of this machine.", true),
        }
    }

    fn choose(&mut self, key: KeyEvent, at: usize) {
        let field = self.field();
        let (Some(id), Kind::Choice { choices, .. }) = (field.id, &field.kind) else {
            self.mode = Mode::Browse;
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Choosing {
                    at: at.saturating_sub(1),
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Choosing {
                    at: (at + 1).min(choices.len().saturating_sub(1)),
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(choice) = choices.get(at) else {
                    return;
                };
                if let Some(problem) = &choice.problem {
                    self.tell(
                        format!("{} can't be {}: {problem}", field.label, choice.value),
                        true,
                    );
                    return;
                }
                let result = self.editor.set(id, &choice.value);
                self.report(result);
                self.mode = Mode::Browse;
            }
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Browse,
            _ => {}
        }
    }

    fn typing(&mut self, key: KeyEvent, mut text: String) {
        let Some(id) = self.field().id else {
            self.mode = Mode::Browse;
            return;
        };
        match key.code {
            KeyCode::Enter => match self.editor.set(id, &text) {
                Ok(()) => {
                    self.message = None;
                    self.mode = Mode::Browse;
                }
                Err(error) => {
                    self.tell(format!("{error:#}"), true);
                    self.mode = Mode::Typing { text };
                }
            },
            KeyCode::Esc => self.mode = Mode::Browse,
            KeyCode::Backspace => {
                text.pop();
                self.mode = Mode::Typing { text };
            }
            KeyCode::Char(c) => {
                text.push(c);
                self.mode = Mode::Typing { text };
            }
            _ => {}
        }
    }

    fn start_review(&mut self) {
        if !self.editor.is_modified() {
            self.mode = Mode::Browse;
            self.tell("Nothing has changed, so there's nothing to save.", false);
            return;
        }
        self.mode = Mode::Review;
    }

    fn review(&mut self, key: KeyEvent) -> Option<Ending> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => match self.editor.save() {
                Ok(()) => Some(Ending::Saved),
                Err(error) => {
                    self.tell(format!("{error:#}"), true);
                    self.mode = Mode::Browse;
                    None
                }
            },
            _ => {
                self.mode = Mode::Browse;
                None
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .areas(frame.area());
        let modified = if self.editor.is_modified() {
            "  [unsaved changes]"
        } else {
            ""
        };
        frame.render_widget(
            Line::from(vec![
                Span::from(format!(" macman config edit {}", self.editor.original.name)).bold(),
                Span::from(modified).yellow(),
            ]),
            header,
        );
        let [list, detail] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
        let fields = self.editor.fields();
        self.draw_fields(frame, list, &fields);
        self.draw_detail(frame, detail, &fields[self.selected]);
        self.draw_footer(frame, footer);
        if self.mode == Mode::Review {
            self.draw_review(frame, body);
        }
    }

    fn draw_fields(&self, frame: &mut Frame, area: Rect, fields: &[Field]) {
        let mut lines = Vec::new();
        let mut group = "";
        let mut selected_line = 0;
        for (index, field) in fields.iter().enumerate() {
            if field.group != group {
                if !group.is_empty() {
                    lines.push(Line::default());
                }
                group = field.group;
                lines.push(Line::from(group).bold().underlined());
            }
            let marker = if field.changed { "*" } else { " " };
            let value = if field.value.is_empty() {
                "none"
            } else {
                &field.value
            };
            let mut line = Line::from(vec![
                Span::from(format!("{marker} {:<14}", field.label)),
                Span::from(value.to_string()),
            ]);
            if field.id.is_none() {
                line = line.dim();
            }
            if field.changed {
                line = line.yellow();
            }
            if index == self.selected {
                selected_line = lines.len();
                line = line.add_modifier(Modifier::REVERSED);
            }
            lines.push(line);
        }
        let height = area.height.saturating_sub(2) as usize;
        let scroll = selected_line.saturating_sub(height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(" Settings "))
                .scroll((scroll as u16, 0)),
            area,
        );
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect, field: &Field) {
        let mut text = Text::default();
        text.push_line(Line::from(field.help));
        text.push_line(Line::default());
        match (&field.kind, &self.mode) {
            (Kind::Fixed(_), _) => text.push_line(Line::from("Read-only here.").dim()),
            (Kind::Toggle, _) => {
                text.push_line(Line::from("Enter, Space, or ←/→ turns it on or off."))
            }
            (Kind::Path, Mode::Typing { text: typed })
            | (Kind::Choice { .. }, Mode::Typing { text: typed }) => {
                text.push_line(Line::from(format!("> {typed}_")).bold());
                text.push_line(Line::from("Enter to set it, Esc to leave it.").dim());
            }
            (Kind::Path, _) => text.push_line(Line::from("Enter or e to type a path.")),
            (Kind::Choice { choices, .. }, mode) => {
                let at = match mode {
                    Mode::Choosing { at } => Some(*at),
                    _ => None,
                };
                for (index, choice) in choices.iter().enumerate() {
                    let current = choice.value == field.value;
                    let mark = if current { "●" } else { " " };
                    let mut line = match &choice.problem {
                        None => Line::from(format!("{mark} {}", choice.value)),
                        Some(problem) if at == Some(index) => {
                            Line::from(format!("✗ {} — {problem}", choice.value)).dim()
                        }
                        Some(_) => Line::from(format!("✗ {}", choice.value)).dim(),
                    };
                    if at == Some(index) {
                        line = line.add_modifier(Modifier::REVERSED);
                    }
                    text.push_line(line);
                }
            }
        }
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(format!(" {} ", field.label))),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let keys = match self.mode {
            Mode::Browse => {
                "↑/↓ move  Enter change  ←/→ previous/next  e type  u undo field  s save  q quit"
            }
            Mode::Choosing { .. } => "↑/↓ move  Enter pick  Esc back",
            Mode::Typing { .. } => "Enter set  Backspace delete  Esc back",
            Mode::Review => "y or Enter save  any other key back",
            Mode::Leaving => {
                "Leave without saving?  y leave  s review and save  any other key stay"
            }
        };
        let mut lines = vec![Line::from(keys).dim()];
        if let Some((message, problem)) = &self.message {
            let line = Line::from(message.as_str());
            lines.push(if *problem { line.red() } else { line });
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
    }

    fn draw_review(&self, frame: &mut Frame, area: Rect) {
        let area = area.inner(ratatui::layout::Margin {
            horizontal: 4,
            vertical: 1,
        });
        let mut text = Text::default();
        for change in self.editor.changes() {
            text.push_line(Line::from(format!(
                "{}: {} → {}",
                change.label, change.before, change.after
            )));
        }
        let notes = self.editor.notes();
        if !notes.is_empty() {
            text.push_line(Line::default());
            for note in notes {
                text.push_line(Line::from(format!("Note: {note}")).yellow());
            }
        }
        if let Err(error) = self.editor.check(&self.editor.draft) {
            text.push_line(Line::default());
            text.push_line(Line::from(format!("Can't be saved: {error:#}")).red());
        }
        text.push_line(Line::default());
        text.push_line(
            Line::from("Save these changes to machine.toml? y saves; any other key goes back.")
                .bold(),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Review ")),
            area,
        );
    }
}

/// Run the editor on this terminal until it's saved or cancelled.
pub fn run(editor: Editor<'_>) -> Result<Ending> {
    let mut ui = Ui::new(editor);
    let mut terminal = ratatui::try_init().context("cannot start the terminal interface")?;
    let result = (|| -> Result<Ending> {
        loop {
            terminal.draw(|frame| ui.draw(frame))?;
            if let Event::Key(key) = event::read()?
                && let Some(ending) = ui.handle(key)
            {
                return Ok(ending);
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::FieldId;
    use crate::paths::Paths;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Where the field `id` is, so tests can select it.
    fn position(ui: &Ui<'_>, id: FieldId) -> usize {
        ui.editor
            .fields()
            .iter()
            .position(|field| field.id == Some(id))
            .unwrap()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn plus(root: &std::path::Path) -> (Paths, crate::backend::Backends) {
        let paths = Paths::under(root);
        let backends = crate::default_backends(&paths).unwrap();
        crate::edit::tests::make_plus(&paths);
        (paths, backends)
    }

    fn screen(ui: &Ui<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
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

    #[test]
    fn keys_pick_a_memory_size_review_and_save() {
        let root = tempfile::tempdir().unwrap();
        let (paths, backends) = plus(root.path());
        let editor = Editor::open(&paths, &backends, "plus").unwrap();
        let mut ui = Ui::new(editor);
        assert!(screen(&ui).contains("ROM"));

        ui.selected = position(&ui, FieldId::Memory);
        ui.handle(key(KeyCode::Enter));
        assert!(matches!(ui.mode, Mode::Choosing { .. }));
        // From 4M down past 5M to 8M, which a Plus can't have: it's
        // refused, with the reason.
        for _ in 0..2 {
            ui.handle(key(KeyCode::Down));
        }
        assert!(screen(&ui).contains("8M"));
        ui.handle(key(KeyCode::Enter));
        let (message, problem) = ui.message.clone().unwrap();
        assert!(problem && message.contains("cannot have 8M"), "{message}");
        assert_eq!(ui.editor.draft.hardware.memory, "4M");

        // Up past 5M, 4M, and 2.5M to 2M, which it can.
        for _ in 0..4 {
            ui.handle(key(KeyCode::Up));
        }
        ui.handle(key(KeyCode::Enter));
        assert_eq!(ui.mode, Mode::Browse);
        assert_eq!(ui.editor.draft.hardware.memory, "2M");
        assert!(screen(&ui).contains("[unsaved changes]"));

        ui.handle(key(KeyCode::Char('s')));
        assert_eq!(ui.mode, Mode::Review);
        let review = screen(&ui);
        assert!(review.contains("Memory: 4M → 2M"), "{review}");
        assert!(review.contains("so the next run"), "{review}");
        assert_eq!(ui.handle(key(KeyCode::Char('y'))), Some(Ending::Saved));
        assert_eq!(
            crate::machine::Machine::load(&paths, "plus")
                .unwrap()
                .hardware
                .memory,
            "2M"
        );
    }

    #[test]
    fn leaving_with_changes_asks_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (paths, backends) = plus(root.path());
        let file = paths.machine("plus").join("machine.toml");
        let before = std::fs::read(&file).unwrap();
        let mut ui = Ui::new(Editor::open(&paths, &backends, "plus").unwrap());

        ui.selected = position(&ui, FieldId::Fullscreen);
        ui.handle(key(KeyCode::Char(' ')));
        assert!(ui.editor.is_modified());
        assert_eq!(ui.handle(key(KeyCode::Char('q'))), None);
        assert_eq!(ui.mode, Mode::Leaving);
        assert_eq!(ui.handle(key(KeyCode::Char('n'))), None);
        assert_eq!(ui.mode, Mode::Browse);
        ui.handle(key(KeyCode::Char('q')));
        assert_eq!(ui.handle(key(KeyCode::Char('y'))), Some(Ending::Cancelled));
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    #[test]
    fn fixed_fields_explain_themselves_and_arrows_skip_what_cannot_be_had() {
        let root = tempfile::tempdir().unwrap();
        let (paths, backends) = plus(root.path());
        let mut ui = Ui::new(Editor::open(&paths, &backends, "plus").unwrap());
        ui.selected = 0;
        ui.handle(key(KeyCode::Enter));
        let (message, _) = ui.message.clone().unwrap();
        assert!(message.contains("can't be changed here"), "{message}");

        // A Plus is monochrome, so there's no deeper display to step to.
        ui.selected = position(&ui, FieldId::Depth);
        ui.handle(key(KeyCode::Right));
        assert_eq!(ui.editor.draft.display.depth, 1);
        assert!(ui.message.clone().unwrap().1);

        // Typing goes through the same checks.
        ui.selected = position(&ui, FieldId::Memory);
        ui.handle(key(KeyCode::Char('e')));
        for _ in 0..2 {
            ui.handle(key(KeyCode::Backspace));
        }
        for c in "2G".chars() {
            ui.handle(key(KeyCode::Char(c)));
        }
        ui.handle(key(KeyCode::Enter));
        assert!(
            matches!(ui.mode, Mode::Typing { .. }),
            "kept for another try"
        );
        assert_eq!(ui.editor.draft.hardware.memory, "4M");
        ui.handle(key(KeyCode::Esc));
        assert_eq!(ui.mode, Mode::Browse);
        assert_eq!(ui.handle(key(KeyCode::Char('q'))), Some(Ending::Cancelled));
    }
}
