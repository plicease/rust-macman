//! Drawing the wizard, and its loop over a real terminal. Everything it
//! shows comes from [`Wizard`]; nothing here decides anything.

use super::rules::{Fit, Start};
use super::{Outcome, Row, STEPS, Step, Wizard};
use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

pub fn draw(wizard: &Wizard, frame: &mut Frame) {
    let [header, progress, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    frame.render_widget(
        Line::from(vec![
            Span::from(" macman wizard").bold(),
            Span::from(format!(
                "  step {} of {}: {}",
                wizard.step.number(),
                STEPS.len(),
                wizard.step.title()
            )),
        ]),
        header,
    );
    let steps: Vec<Span> = STEPS
        .iter()
        .flat_map(|step| {
            let span = Span::from(format!(" {} ", step.title()));
            let span = if *step == wizard.step {
                span.add_modifier(Modifier::REVERSED)
            } else if step.number() < wizard.step.number() {
                span
            } else {
                span.dim()
            };
            [span, Span::from("›").dim()]
        })
        .collect();
    frame.render_widget(Line::from(steps), progress);

    if wizard.step == Step::Settings
        && let Some(settings) = wizard.settings()
    {
        settings.draw_body(frame, body, footer);
        draw_message(wizard, frame, footer);
    } else {
        match wizard.step {
            Step::Source | Step::Backend | Step::Disk => draw_rows(wizard, frame, body),
            _ => draw_page(wizard, frame, body),
        }
        draw_footer(wizard, frame, footer);
    }
    if wizard.quitting {
        let area = body.inner(ratatui::layout::Margin {
            horizontal: 8,
            vertical: 3,
        });
        let area = Rect {
            height: area.height.min(5),
            ..area
        };
        let text = if wizard.created.is_some() {
            "Leave the wizard? The machine is made already.  y leave  any other key stay"
        } else {
            "Leave the wizard? Nothing has been made or changed.  y leave  any other key stay"
        };
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(Block::bordered().title(" Leave ")),
            area,
        );
    }
}

/// A step that's a list to pick from, with the selected line explained.
fn draw_rows(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let mut lines = Vec::new();
    let mut selected_line = 0;
    let rows = wizard.rows();
    for (index, row) in rows.iter().enumerate() {
        if let Some(heading) = heading(*row, index.checked_sub(1).map(|i| rows[i])) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(heading).bold().underlined());
        }
        let mut line = row_line(wizard, *row);
        if index == wizard.cursor {
            selected_line = lines.len();
            line = line.add_modifier(Modifier::REVERSED);
        }
        lines.push(line);
    }
    let height = list.height.saturating_sub(2) as usize;
    let scroll = selected_line.saturating_sub(height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(format!(" {} ", wizard.step.title())))
            .scroll((scroll as u16, 0)),
        list,
    );
    let mut text = Text::default();
    for line in detail_lines(wizard) {
        text.push_line(line);
    }
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" About this ")),
        detail,
    );
}

fn heading(row: Row, before: Option<Row>) -> Option<&'static str> {
    match (row, before) {
        (Row::Image, _) => Some("Image file"),
        (Row::Media(_), Some(Row::Image)) => Some("Or registered media"),
        (Row::Start(_), Some(Row::Image | Row::Media(_))) => Some("It is"),
        (Row::Register, _) => Some("Reuse"),
        (Row::Strategy(_), None) => Some("Boot disk"),
        (Row::DiskSize, _) => Some("Size"),
        (Row::Name, _) => Some("Machine name"),
        (Row::Route(_), None) => Some("Emulators, best first"),
        _ => None,
    }
}

fn row_line(wizard: &Wizard, row: Row) -> Line<'static> {
    let typed = |value: &str| match &wizard.typing {
        Some(text) if wizard.row() == Some(row) => format!("> {text}_"),
        _ if value.is_empty() => "(none: Enter to type)".to_string(),
        _ => value.to_string(),
    };
    match row {
        Row::Image => Line::from(typed(&wizard.image)),
        Row::Name => Line::from(typed(&wizard.name)),
        Row::DiskSize => Line::from(typed(&wizard.disk_size)),
        Row::RegisterName => Line::from(format!("  as media {}", typed(&wizard.register_name))),
        Row::Register => {
            let mark = if wizard.register { "[x]" } else { "[ ]" };
            Line::from(format!("{mark} Register the image as media, to use again"))
        }
        Row::Media(index) => {
            let medium = &wizard.registered[index];
            let mark = if wizard.media.as_deref() == Some(medium.name.as_str()) {
                "●"
            } else {
                " "
            };
            let version = medium
                .version
                .as_deref()
                .map(|v| format!(", {v}"))
                .unwrap_or_default();
            Line::from(format!(
                "{mark} {} ({}{version})",
                medium.name,
                medium.kind.as_str()
            ))
        }
        Row::Start(start) => {
            let mark = if start == wizard.start { "●" } else { " " };
            let note = match (start == wizard.start, wizard.start_chosen) {
                (true, true) => "  (your choice)",
                (true, false) if wizard.observation.is_some() => "  (suggested)",
                _ => "",
            };
            Line::from(format!("{mark} {}{note}", start.label()))
        }
        Row::Route(index) => {
            let route = &wizard.routes[index];
            let chosen = wizard.chosen_route() == Some(route);
            let mark = if chosen { "●" } else { " " };
            let (badge, dim) = match (&route.fit, route.recommended) {
                (Fit::Good, true) => ("★ recommended", false),
                (Fit::Good, false) => ("possible", false),
                (Fit::Unlikely(_), _) => ("unlikely", false),
                (Fit::Blocked(_), _) => ("✗ not here", true),
            };
            let line = Line::from(format!("{mark} {}  {badge}", route.label()));
            if dim { line.dim() } else { line }
        }
        Row::Strategy(index) => {
            let (strategy, problem) = &wizard.strategies[index];
            let mark = if wizard.strategy.as_ref() == Some(strategy) {
                "●"
            } else {
                " "
            };
            let line = Line::from(format!("{mark} {}", strategy.label()));
            if problem.is_some() { line.dim() } else { line }
        }
    }
}

fn detail_lines(wizard: &Wizard) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut push = |text: String| lines.push(Line::from(text));
    match wizard.row() {
        Some(Row::Image) => {
            push(
                "The disk or CD image to start from. Enter to type its path; spaces are \
                  fine, and so are quotes. It's only read."
                    .to_string(),
            );
            if let Some(observation) = &wizard.observation {
                push(String::new());
                for fact in observation.facts() {
                    push(fact);
                }
            }
        }
        Some(Row::Media(index)) => {
            let medium = &wizard.registered[index];
            push(
                "Media registered with `macman media add`. Enter picks it; its own copy is read, \
                 and attached read-only when the machine starts."
                    .to_string(),
            );
            push(String::new());
            push(format!(
                "{}: a {}, {}, {}",
                medium.name,
                medium.kind.as_str(),
                medium.format.describe(),
                if medium.bootable {
                    "with boot blocks"
                } else {
                    "no boot blocks"
                }
            ));
            if let Some(source) = &medium.source {
                push(format!("Registered from {source}"));
            }
            if let Some(version) = &medium.version {
                push(format!("Registered as version {version}"));
            }
        }
        Some(Row::Register) => push(
            "Copy the whole image, read-only, into macman's media registry once the machine \
             is made, so later runs can attach it with --media NAME even if the file moves. \
             The file itself isn't changed. Enter turns it on or off."
                .to_string(),
        ),
        Some(Row::RegisterName) => push(
            "The name to register the image under: letters, digits, '.', '_' and '-'. Enter \
             to type another."
                .to_string(),
        ),
        Some(Row::Start(start)) => push(match start {
            Start::SystemDisk => "The image already has a system installed. Its volume is \
                                  copied as the new machine's boot disk; the image isn't \
                                  changed."
                .to_string(),
            Start::InstallerCd => "The image is a CD to install from. The machine gets a \
                                   blank disk, and the whole CD, partition map and all, is \
                                   attached read-only when it starts."
                .to_string(),
            Start::InstallerFloppy => "The image is a floppy. macman can't yet install from a \
                                       series of floppies (#28), but a floppy that starts a \
                                       Mac can be copied as the boot disk."
                .to_string(),
            Start::Manual => "No image: pick the emulator, then a registered system image or \
                              a blank disk."
                .to_string(),
        }),
        Some(Row::Route(index)) => {
            let route = &wizard.routes[index];
            match &route.fit {
                Fit::Good => push(format!(
                    "Fits, with {} confidence.",
                    route.confidence.as_str()
                )),
                Fit::Unlikely(why) => push(format!("Unlikely to work: {why}.")),
                Fit::Blocked(why) => push(format!("Can't be had here: {why}.")),
            }
            for reason in &route.reasons {
                push(reason.clone());
            }
            if let Some(rom) = &route.rom {
                push(format!(
                    "ROM: {rom}, a registered one that fits; the next step can pick another."
                ));
            }
            push(String::new());
            push("Enter picks it. r looks again after registering a ROM.".to_string());
        }
        Some(Row::Strategy(index)) => {
            let (_, problem) = &wizard.strategies[index];
            match problem {
                Some(why) => push(format!("Not possible: {why}.")),
                None => push("Enter picks it.".to_string()),
            }
        }
        Some(Row::DiskSize) => push(
            "The blank boot disk's size, such as 500M or 2G. The file only takes up the \
             space the guest writes, where the filesystem allows."
                .to_string(),
        ),
        Some(Row::Name) => push(
            "What macman calls the machine: letters, digits, '.', '_' and '-'. Enter to type \
             another."
                .to_string(),
        ),
        None => {}
    }
    lines
}

/// A step that's text to read: the findings, the plan, or what's next.
fn draw_page(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let lines = page_lines(wizard);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((wizard.scroll, 0))
            .block(Block::bordered().title(format!(" {} ", wizard.step.title()))),
        area,
    );
}

pub fn page_lines(wizard: &Wizard) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match wizard.step {
        Step::Findings => {
            lines.push(Line::from("Observed").bold().underlined());
            match &wizard.observation {
                Some(observation) => {
                    lines.extend(observation.facts().into_iter().map(Line::from));
                    if let Some(medium) = wizard.used_media() {
                        let version = medium
                            .version
                            .as_deref()
                            .map(|v| format!(", given version {v}"))
                            .unwrap_or_default();
                        lines.push(Line::from(format!(
                            "Registered as media {}: a {}{version}",
                            medium.name,
                            medium.kind.as_str()
                        )));
                    }
                }
                None => lines.push(Line::from("No image.")),
            }
            lines.push(Line::default());
            lines.push(Line::from("Inferred").bold().underlined());
            match &wizard.guess {
                Some(guess) => {
                    lines.push(Line::from(format!(
                        "Probably {}{}, with {} confidence: {}.",
                        guess.label(),
                        if guess.installer {
                            ", an installer"
                        } else {
                            ""
                        },
                        guess.confidence.as_str(),
                        guess.evidence
                    )));
                }
                None => lines.push(Line::from(
                    "No system version could be read from the image, so none is assumed; \
                     every emulator stays on offer.",
                )),
            }
            lines.push(Line::default());
            lines.push(Line::from("Your choice").bold().underlined());
            let whose = if wizard.start_chosen {
                "as you chose"
            } else {
                "as suggested; b goes back to change it"
            };
            lines.push(Line::from(format!("{}, {whose}.", wizard.start.label())));
        }
        Step::Review => {
            lines.extend(wizard.plan().into_iter().map(Line::from));
        }
        Step::Done => {
            if let Some(created) = &wizard.created {
                lines.push(Line::from(format!("Made machine {}.", created.name)).bold());
                lines.push(Line::default());
                for line in &created.after {
                    lines.push(Line::from(format!("• {line}")));
                }
                if !created.guidance.is_empty() {
                    lines.push(Line::default());
                    lines.push(Line::from("Good to know").bold().underlined());
                    for line in &created.guidance {
                        lines.push(Line::from(format!("• {line}")));
                    }
                }
                lines.push(Line::default());
                lines.push(Line::from(
                    "All of this is printed again when the wizard closes.",
                ));
            }
        }
        _ => {}
    }
    lines
}

fn keys(wizard: &Wizard) -> &'static str {
    if wizard.typing.is_some() {
        return "Enter set  Backspace delete  Esc leave it";
    }
    match wizard.step {
        Step::Source => "↑/↓ move  Enter pick or type  n next  q quit",
        Step::Findings => "↑/↓ scroll  n next  b back  q quit",
        Step::Backend => "↑/↓ move  Enter pick  r look again  n next  b back  q quit",
        Step::Disk => "↑/↓ move  Enter pick or type  n next  b back  q quit",
        Step::Settings => "",
        Step::Review => "↑/↓ scroll  Enter or c make it  b back  q quit",
        Step::Done => "l start it now  Enter finish",
    }
}

fn draw_footer(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    frame.render_widget(Line::from(keys(wizard)).dim(), area);
    draw_message(wizard, frame, area);
}

fn draw_message(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    if let Some((message, problem)) = &wizard.message {
        let area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        let line = Line::from(message.clone());
        let line = if *problem { line.red() } else { line };
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
    }
}

/// Run the wizard on the terminal until it ends.
pub fn run(mut wizard: Wizard<'_>) -> Result<Outcome> {
    let mut terminal = ratatui::try_init().context("cannot start the terminal interface")?;
    let result = (|| -> Result<Outcome> {
        loop {
            terminal.draw(|frame| draw(&wizard, frame))?;
            if let Event::Key(key) = event::read()?
                && let Some(outcome) = wizard.handle(key)
            {
                return Ok(outcome);
            }
        }
    })();
    ratatui::restore();
    result
}
