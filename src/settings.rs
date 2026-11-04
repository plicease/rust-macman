//! `macman config set` and `config keys`: a machine's settings by their names
//! in `machine.toml`, such as `hardware.memory`, over the model `config edit`
//! uses. Which keys a machine has, and what each may be, come from
//! [`Editor::fields`]; values are applied as the editor applies them, and the
//! result is checked once, whole, by [`Editor::check`], as the editor checks
//! before saving. So the two accept and refuse the same configurations, and
//! this module has no rules of its own beyond spelling.

use crate::backend::Backends;
use crate::edit::{self, Editor, FieldId, Kind};
use crate::machine::Machine;
use crate::paths::Paths;
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

/// A key `config set` can change, where its machine's backend has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    id: FieldId,
    /// The backend whose table it's in, for a backend's own settings.
    backend: Option<&'static str>,
    /// The type, as `config keys` reports it.
    pub kind: &'static str,
}

const fn key(
    name: &'static str,
    id: FieldId,
    backend: Option<&'static str>,
    kind: &'static str,
) -> Key {
    Key {
        name,
        id,
        backend,
        kind,
    }
}

const BASILISK2: Option<&str> = Some("basilisk2");
const SHEEPSHAVER: Option<&str> = Some("sheepshaver");
const QEMU: Option<&str> = Some(crate::backend::qemu::BACKEND_NAME);

/// Every key, in the order `config edit` shows their fields.
pub const KEYS: &[Key] = &[
    key("ephemeral", FieldId::Ephemeral, None, "boolean"),
    key("hardware.rom", FieldId::Rom, None, "string"),
    key("hardware.memory", FieldId::Memory, None, "string"),
    key("basilisk2.cpu", FieldId::Cpu, BASILISK2, "string"),
    key("basilisk2.fpu", FieldId::Fpu, BASILISK2, "boolean"),
    key("display.width", FieldId::Size, None, "integer"),
    key("display.height", FieldId::Size, None, "integer"),
    key("display.depth", FieldId::Depth, None, "integer"),
    key("display.scale", FieldId::Scale, None, "integer"),
    key("display.fullscreen", FieldId::Fullscreen, None, "boolean"),
    key("basilisk2.sound", FieldId::Sound, BASILISK2, "boolean"),
    key("basilisk2.network", FieldId::Network, BASILISK2, "string"),
    key(
        "basilisk2.shared_folder",
        FieldId::SharedFolder,
        BASILISK2,
        "path",
    ),
    key("sheepshaver.sound", FieldId::Sound, SHEEPSHAVER, "boolean"),
    key(
        "sheepshaver.network",
        FieldId::Network,
        SHEEPSHAVER,
        "string",
    ),
    key(
        "sheepshaver.shared_folder",
        FieldId::SharedFolder,
        SHEEPSHAVER,
        "path",
    ),
    key("qemu.sound", FieldId::Sound, QEMU, "boolean"),
    key("qemu.network", FieldId::Network, QEMU, "string"),
];

/// Keys in `machine.toml` that are never set this way, and why.
pub const FIXED: &[(&str, &str)] = &[
    (
        "schema_version",
        "It's the version of machine.toml's format, which macman keeps.",
    ),
    ("name", edit::RENAME),
    (
        "id",
        "It's the machine's identity, which import uses to recognize it on another host.",
    ),
    ("emulator.backend", edit::BACKEND_FIXED),
    (
        "emulator.revision",
        "It's the emulator source the machine was made with, which macman keeps.",
    ),
    ("hardware.model", edit::MODEL_FIXED),
    ("qemu.machine", edit::MODEL_FIXED),
    (
        "system.version",
        "It records the system the machine was created from, and nothing uses it to run.",
    ),
    ("disks", edit::DISKS_FIXED),
];

/// A key and the value to give it.
pub type Pair = (String, String);

/// One change `config set` made, with values typed as in `machine.toml`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub key: &'static str,
    pub before: Value,
    pub after: Value,
}

/// A key a machine has, for `config keys`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyInfo {
    pub key: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub value: Value,
    /// Values it may take as the machine is now, if they're a list.
    pub choices: Option<Vec<Value>>,
    /// Whether values beyond `choices` may be given.
    pub other_values: bool,
}

/// The fixed key `name` falls under, if it's one.
fn fixed(name: &str) -> Option<&'static str> {
    FIXED.iter().find_map(|(key, reason)| {
        (name == *key
            || name
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(['.', '['])))
        .then_some(*reason)
    })
}

/// Whether `name` is a key of any machine, editable or not.
pub fn is_key(name: &str) -> bool {
    KEYS.iter().any(|key| key.name == name) || fixed(name).is_some()
}

/// The keys `editor`'s machine has, which `config edit` offers it.
pub fn keys(editor: &Editor) -> Vec<Key> {
    let fields = editor.fields();
    let backend = editor.draft.emulator.backend.as_str();
    KEYS.iter()
        .filter(|key| key.backend.is_none_or(|b| b == backend))
        .filter(|key| fields.iter().any(|field| field.id == Some(key.id)))
        .copied()
        .collect()
}

/// Find `name` among `editor`'s keys, or explain why it can't be set.
fn find(editor: &Editor, name: &str) -> Result<Key> {
    let machine = &editor.draft;
    if let Some(key) = keys(editor).into_iter().find(|key| key.name == name) {
        return Ok(key);
    }
    if let Some(reason) = fixed(name) {
        bail!("{name} can't be set: {reason}");
    }
    if let Some(key) = KEYS.iter().find(|key| key.name == name) {
        // Only a backend's own settings, or a field the backend refuses, end here.
        let reason = editor
            .fields()
            .into_iter()
            .find(|field| {
                field.id.is_none()
                    && field.label == "Shared folder"
                    && key.id == FieldId::SharedFolder
            })
            .and_then(|field| match field.kind {
                Kind::Fixed(reason) => Some(reason),
                _ => None,
            });
        match (reason, key.backend) {
            (Some(reason), _) => bail!("{name} can't be set for {:?}: {reason}", machine.name),
            (None, Some(backend)) => bail!(
                "{name} is a {backend} setting, and {:?} is a {} machine",
                machine.name,
                machine.emulator.backend
            ),
            (None, None) => bail!("{name} doesn't apply to {:?}", machine.name),
        }
    }
    let names: Vec<_> = keys(editor).iter().map(|key| key.name).collect();
    bail!(
        "unknown key {name:?}; {:?} has {}; see `macman config keys {}`",
        machine.name,
        names.join(", "),
        machine.name
    )
}

/// `key`'s value in `machine`, typed as in `machine.toml`; a shared folder
/// that isn't set is null.
pub fn value(machine: &Machine, key: &Key) -> Value {
    let shown = edit::value_of(machine, key.id);
    match (key.kind, key.name) {
        ("boolean", _) => Value::Bool(shown == "on"),
        (_, "display.width") => machine.display.width.into(),
        (_, "display.height") => machine.display.height.into(),
        ("integer", _) => shown
            .parse::<u64>()
            .map_or(Value::String(shown), Value::from),
        ("path", _) if shown.is_empty() => Value::Null,
        _ => Value::String(shown),
    }
}

/// How a value reads in text: a string without its quotes, and null as none.
pub fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "none".to_string(),
        other => other.to_string(),
    }
}

fn parse_bool(name: &str, value: &str) -> Result<&'static str> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" => "on",
        "false" | "off" | "no" => "off",
        _ => bail!("{name} takes true or false, not {value:?}"),
    })
}

/// Apply `value` to `key` in the draft, checking only that it reads as the
/// key's type: whether the machine can have it is for [`Editor::check`].
fn assign(editor: &mut Editor, key: &Key, value: &str) -> Result<()> {
    let draft = &editor.draft;
    let shown = match (key.kind, key.name) {
        ("boolean", _) => parse_bool(key.name, value)?.to_string(),
        (_, "display.width" | "display.height") => {
            let number: u32 = value.trim().parse().map_err(|_| {
                anyhow::anyhow!("{} takes a whole number of pixels, not {value:?}", key.name)
            })?;
            if key.name == "display.width" {
                format!("{number}x{}", draft.display.height)
            } else {
                format!("{}x{number}", draft.display.width)
            }
        }
        ("integer", _) => value
            .trim()
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("{} takes a whole number, not {value:?}", key.name))?
            .to_string(),
        _ => value.to_string(),
    };
    // Where `config edit` offers only a list, the value has to be on it, so
    // neither can reach a machine the other can't.
    if matches!(key.id, FieldId::Rom | FieldId::Depth | FieldId::Scale) {
        let listed = choice_values(editor, key, false);
        if !listed.iter().any(|choice| text(choice) == shown) {
            let values: Vec<_> = listed.iter().map(text).collect();
            bail!("{} takes {}, not {value:?}", key.name, one_of(&values));
        }
    }
    let draft = &mut editor.draft;
    if let Err(error) = edit::apply(draft, key.id, &shown) {
        if matches!(key.id, FieldId::Cpu | FieldId::Network) {
            let choices = choice_values(editor, key, false);
            bail!(
                "{} takes {}, not {value:?}",
                key.name,
                one_of(&choices.iter().map(text).collect::<Vec<_>>())
            );
        }
        bail!("{}: {error:#}", key.name);
    }
    Ok(())
}

fn one_of(values: &[String]) -> String {
    match values {
        [] => "nothing here".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("one of {}, or {last}", rest.join(", ")),
    }
}

/// The values `key` lists, as `config edit` offers them for the draft; with
/// `usable`, only those that leave nothing wrong.
fn choice_values(editor: &Editor, key: &Key, usable: bool) -> Vec<Value> {
    if key.kind == "boolean" {
        return vec![Value::Bool(true), Value::Bool(false)];
    }
    let Some(field) = editor.fields().into_iter().find(|f| f.id == Some(key.id)) else {
        return Vec::new();
    };
    let Kind::Choice { choices, .. } = field.kind else {
        return Vec::new();
    };
    let mut values: Vec<Value> = Vec::new();
    for choice in choices {
        if usable && choice.problem.is_some() {
            continue;
        }
        let value = match key.name {
            "display.width" | "display.height" => {
                let Some((width, height)) = choice.value.split_once('x') else {
                    continue;
                };
                let part = if key.name == "display.width" {
                    width
                } else {
                    height
                };
                part.parse::<u64>().map(Value::from).ok()
            }
            _ if key.kind == "integer" => choice.value.parse::<u64>().map(Value::from).ok(),
            _ => Some(Value::String(choice.value)),
        };
        if let Some(value) = value
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    // Halves of the sizes, which are in no order of their own.
    if matches!(key.name, "display.width" | "display.height") {
        values.sort_by_key(|value| value.as_u64());
    }
    values
}

/// What `config keys` says of each of `editor`'s keys.
pub fn describe(editor: &Editor) -> Vec<KeyInfo> {
    keys(editor)
        .iter()
        .map(|key| {
            let typed = editor.fields().into_iter().any(|field| {
                field.id == Some(key.id) && matches!(field.kind, Kind::Choice { typed: true, .. })
            });
            let choices = (key.kind != "path").then(|| choice_values(editor, key, true));
            KeyInfo {
                key: key.name,
                kind: key.kind,
                value: value(&editor.draft, key),
                choices,
                other_values: typed || key.kind == "path",
            }
        })
        .collect()
}

/// Apply every `(key, value)` pair to the draft, then check the machine they
/// make, once and whole. On any error the draft is left as it was.
pub fn set(editor: &mut Editor, pairs: &[(String, String)]) -> Result<Vec<Change>> {
    let mut found: Vec<Key> = Vec::new();
    for (name, _) in pairs {
        let key = find(editor, name)?;
        if found.iter().any(|seen| seen.name == key.name) {
            bail!("{name} is given more than once");
        }
        found.push(key);
    }
    // What a single key could be instead, from the machine as it is.
    let alternatives = match found.as_slice() {
        [key] if key.kind != "boolean" => choice_values(editor, key, true),
        _ => Vec::new(),
    };
    let before = editor.draft.clone();
    let result = (|| {
        for (key, (_, value)) in found.iter().zip(pairs) {
            assign(editor, key, value)?;
        }
        if editor.is_modified()
            && let Err(error) = editor.check(&editor.draft)
        {
            let mut message = format!("{error:#}");
            if let ([key], false) = (found.as_slice(), alternatives.is_empty()) {
                let values: Vec<_> = alternatives.iter().map(text).collect();
                message += &format!("; {} can be {} here", key.name, one_of(&values));
            }
            bail!("{message}");
        }
        Ok(())
    })();
    if let Err(error) = result {
        editor.draft = before;
        bail!("{error:#}; nothing was changed");
    }
    Ok(found
        .iter()
        .map(|key| Change {
            key: key.name,
            before: value(&editor.original, key),
            after: value(&editor.draft, key),
        })
        .filter(|change| change.before != change.after)
        .collect())
}

/// Split `config set`'s words into the machine, if named, and pairs. The
/// machine is `option`, from `--machine`, or else, given an odd number of
/// words, the first; with neither, it's the current one. `is_machine` says
/// whether a word names a machine, to explain a value left out rather than
/// read the words some other way.
pub fn split<'a>(
    words: &'a [String],
    option: Option<&'a str>,
    is_machine: &dyn Fn(&str) -> bool,
) -> Result<(Option<&'a str>, Vec<Pair>)> {
    let first = words.first().map_or("", String::as_str);
    let looks_like_machine = !first.is_empty() && !is_key(first) && is_machine(first);
    let missing = |rest: &[String]| -> Result<()> {
        match rest.last() {
            Some(last) if rest.len() % 2 == 1 => bail!(
                "{last:?} has no value; give KEY VALUE pairs, such as `hardware.memory 4M`; \
                 nothing was changed"
            ),
            _ => Ok(()),
        }
    };
    let (machine, rest) = match option {
        Some(named) => {
            if looks_like_machine {
                bail!(
                    "the machine is given twice, as --machine {named} and as {first:?}; name it \
                     one way or the other; nothing was changed"
                );
            }
            if words.len() % 2 == 1 && !is_key(first) {
                bail!(
                    "{first:?} isn't a key, and with --machine every word is a KEY or its VALUE; \
                     see `macman config keys {named}`; nothing was changed"
                );
            }
            missing(words)?;
            (Some(named), words)
        }
        // A key first means a value is missing, not that a machine is named.
        None if words.len() % 2 == 1 && is_key(first) => {
            missing(words)?;
            unreachable!("an odd number of words has a last one")
        }
        None if words.len() % 2 == 1 => (Some(first), &words[1..]),
        None => {
            if looks_like_machine {
                missing(&words[1..])?;
            }
            (None, words)
        }
    };
    if rest.is_empty() {
        bail!("give a key and a value to set, such as `hardware.memory 4M`");
    }
    let pairs = rest
        .chunks(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect();
    Ok((machine, pairs))
}

/// What to offer for the next word of `config set`, after `words`. The first
/// may be a machine or, with a current machine, a key; after that keys and
/// values alternate. Values are those that suit the machine with the pairs
/// before them applied. Best effort, and never an error: only `set` decides.
pub fn complete(
    paths: &Paths,
    backends: &Backends,
    current: Option<&str>,
    machines: &[String],
    words: &[String],
) -> Vec<String> {
    // `--machine NAME` names the machine; other options are passed over.
    let mut option: Option<String> = None;
    let mut plain: Vec<String> = Vec::new();
    let mut given = words.iter();
    while let Some(word) = given.next() {
        if word == "--machine" {
            option = given.next().cloned();
        } else if let Some(name) = word.strip_prefix("--machine=") {
            option = Some(name.to_string());
        } else if !word.starts_with('-') {
            plain.push(word.clone());
        }
    }
    let words = plain;
    let key_names = |name: &str| -> Vec<String> {
        Editor::inspect(paths, backends, name)
            .map(|editor| {
                keys(&editor)
                    .iter()
                    .map(|key| key.name.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let (machine, rest) = match (option.as_deref(), words.first()) {
        (Some(named), _) => (named, &words[..]),
        (None, None) => {
            let mut offered = machines.to_vec();
            if let Some(current) = current {
                offered.extend(key_names(current));
            }
            return offered;
        }
        (None, Some(first)) if machines.contains(first) && !is_key(first) => {
            (first.as_str(), &words[1..])
        }
        (None, Some(_)) => match current {
            Some(current) => (current, &words[..]),
            None => return Vec::new(),
        },
    };
    let machine = crate::aliases::resolve(paths, machine).unwrap_or_else(|_| machine.to_string());
    let Ok(mut editor) = Editor::inspect(paths, backends, &machine) else {
        return Vec::new();
    };
    let complete_pairs = rest.len() / 2;
    let mut given = Vec::new();
    for pair in rest[..complete_pairs * 2].chunks(2) {
        if let Ok(key) = find(&editor, &pair[0]) {
            let _ = assign(&mut editor, &key, &pair[1]);
            given.push(key.name);
        }
    }
    if rest.len() % 2 == 0 {
        return keys(&editor)
            .iter()
            .filter(|key| !given.contains(&key.name))
            .map(|key| key.name.to_string())
            .collect();
    }
    match find(&editor, &rest[rest.len() - 1]) {
        Ok(key) if key.kind != "path" => choice_values(&editor, &key, true)
            .iter()
            .map(text)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::tests::{add_rom, clean_rom, make_basilisk, make_plus, make_qemu, make_sheep};
    use std::fs;

    fn pairs(text: &str) -> Vec<(String, String)> {
        split(&words(text), Some("test"), &|_| false).unwrap().1
    }

    fn saved(paths: &Paths, name: &str) -> String {
        fs::read_to_string(paths.machine(name).join("machine.toml")).unwrap()
    }

    /// Set `text`'s pairs on `name` and save, as `config set` does.
    fn set_and_save(paths: &Paths, name: &str, text: &str) -> Result<Vec<Change>> {
        let backends = crate::default_backends(paths).unwrap();
        let mut editor = Editor::open_locked(paths, &backends, name)?;
        let changes = set(&mut editor, &pairs(text))?;
        if editor.is_modified() {
            editor.save()?;
        }
        Ok(changes)
    }

    #[test]
    fn a_plus_takes_its_keys_and_refuses_what_it_cant_have() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_plus(&paths);
        add_rom(&paths, "quadra", clean_rom);
        let untouched = saved(&paths, "plus");

        let error = set_and_save(&paths, "plus", "hardware.memory 2G").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("nothing was changed"), "{message}");
        assert!(
            message.contains("hardware.memory can be one of"),
            "{message}"
        );
        assert!(message.contains("4M"), "{message}");
        // Less obvious: a depth, a size, and a ROM a Plus can't have.
        for bad in [
            "display.depth 8",
            "display.width 650",
            "hardware.rom quadra",
            "hardware.rom missing",
        ] {
            assert!(set_and_save(&paths, "plus", bad).is_err(), "{bad}");
        }
        for (bad, says) in [
            ("basilisk2.cpu 68040", "is a basilisk2 setting"),
            ("hardware.model se", "can't be set"),
            ("id 1234", "machine's identity"),
            ("disks[0].path other.dsk", "can't be set"),
            ("emulator.backend basilisk2", "can't be set"),
            ("hardware.speed fast", "unknown key"),
            ("display.fullscreen maybe", "true or false"),
            ("display.scale two", "whole number"),
            ("display.scale 7", "takes one of 1, 2, 3, or 4"),
            ("hardware.rom missing", "takes one of"),
            ("ephemeral true ephemeral false", "more than once"),
        ] {
            let error = format!("{:#}", set_and_save(&paths, "plus", bad).unwrap_err());
            assert!(error.contains(says), "{bad}: {error}");
        }
        assert_eq!(
            saved(&paths, "plus"),
            untouched,
            "nothing refused was written"
        );

        let changes = set_and_save(
            &paths,
            "plus",
            "hardware.memory 1m display.scale 3 ephemeral yes",
        )
        .unwrap();
        let keys: Vec<_> = changes.iter().map(|c| c.key).collect();
        assert_eq!(keys, ["hardware.memory", "display.scale", "ephemeral"]);
        assert_eq!(changes[0].after, "1M");
        assert_eq!(changes[1].after, 3);
        assert_eq!(changes[2].after, true);
        let machine = Machine::load(&paths, "plus").unwrap();
        assert_eq!(machine.hardware.memory, "1M");
        assert_eq!(machine.display.scale, 3);
        assert!(machine.ephemeral);

        // Setting what's there already changes and writes nothing.
        let before = saved(&paths, "plus");
        assert!(
            set_and_save(&paths, "plus", "display.scale 3")
                .unwrap()
                .is_empty()
        );
        assert_eq!(saved(&paths, "plus"), before);
    }

    #[test]
    fn related_settings_change_together_or_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_basilisk(&paths);
        let untouched = saved(&paths, "quadra");

        // A 68040 has its FPU, so turning it off fails, and with it the CPU.
        let error =
            set_and_save(&paths, "quadra", "basilisk2.cpu 68040 basilisk2.fpu false").unwrap_err();
        assert!(
            format!("{error:#}").contains("nothing was changed"),
            "{error:#}"
        );
        assert_eq!(saved(&paths, "quadra"), untouched);
        let error = set_and_save(&paths, "quadra", "basilisk2.cpu 68010").unwrap_err();
        assert!(
            format!("{error:#}").contains("one of 68020, 68030, or 68040"),
            "{error:#}"
        );

        // A size both of whose halves change, each perhaps invalid alone.
        set_and_save(&paths, "quadra", "display.width 1024 display.height 768").unwrap();
        let machine = Machine::load(&paths, "quadra").unwrap();
        assert_eq!((machine.display.width, machine.display.height), (1024, 768));
        set_and_save(&paths, "quadra", "basilisk2.cpu 68030 basilisk2.fpu off").unwrap();
        let machine = Machine::load(&paths, "quadra").unwrap();
        let settings = machine.basilisk2.unwrap();
        assert!(!settings.fpu);
        assert_eq!(
            edit::value_of(&Machine::load(&paths, "quadra").unwrap(), FieldId::Cpu),
            "68030"
        );
    }

    #[test]
    fn config_set_and_config_edit_accept_the_same_machines() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_basilisk(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        for memory in ["128K", "1M", "8M", "40M", "512M", "1024M", "2G", "lots"] {
            for depth in ["1", "8", "32"] {
                let text = format!("hardware.memory {memory} display.depth {depth}");
                let mut setter = Editor::inspect(&paths, &backends, "quadra").unwrap();
                let by_set = set(&mut setter, &pairs(&text)).is_ok();
                // The editor's way: each field set on its draft, then saving's check.
                let mut editor = Editor::inspect(&paths, &backends, "quadra").unwrap();
                let by_edit = edit::apply(&mut editor.draft, FieldId::Memory, memory).is_ok()
                    && edit::apply(&mut editor.draft, FieldId::Depth, depth).is_ok()
                    && editor.check(&editor.draft).is_ok();
                assert_eq!(by_set, by_edit, "{text}");
            }
        }
    }

    #[test]
    fn sheepshaver_has_its_own_keys() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_sheep(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let editor = Editor::inspect(&paths, &backends, "ppc").unwrap();
        let names: Vec<_> = keys(&editor).iter().map(|key| key.name).collect();
        assert!(names.contains(&"sheepshaver.network"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.starts_with("basilisk2.")),
            "{names:?}"
        );
        let described = describe(&editor);
        let depth = described.iter().find(|k| k.key == "display.depth").unwrap();
        assert_eq!(depth.choices, Some(vec![Value::from(32)]));
        let network = described
            .iter()
            .find(|k| k.key == "sheepshaver.network")
            .unwrap();
        assert_eq!(network.value, "none");

        set_and_save(
            &paths,
            "ppc",
            "sheepshaver.network slirp sheepshaver.sound off",
        )
        .unwrap();
        let machine = Machine::load(&paths, "ppc").unwrap();
        let settings = machine.sheepshaver.unwrap();
        assert!(!settings.sound);
        assert_eq!(
            edit::value_of(&Machine::load(&paths, "ppc").unwrap(), FieldId::Network),
            "slirp"
        );
        let error = format!(
            "{:#}",
            set_and_save(&paths, "ppc", "basilisk2.fpu off").unwrap_err()
        );
        assert!(error.contains("is a basilisk2 setting"), "{error}");
    }

    #[test]
    fn qemu_has_its_own_keys_and_no_rom() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_qemu(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let editor = Editor::inspect(&paths, &backends, "g4").unwrap();
        let names: Vec<_> = keys(&editor).iter().map(|key| key.name).collect();
        assert!(names.contains(&"qemu.network"), "{names:?}");
        assert!(names.contains(&"qemu.sound"), "{names:?}");
        assert!(!names.contains(&"hardware.rom"), "{names:?}");

        set_and_save(&paths, "g4", "qemu.network slirp qemu.sound off").unwrap();
        let settings = Machine::load(&paths, "g4").unwrap().qemu.unwrap();
        assert!(!settings.sound);
        assert_eq!(settings.machine, crate::machine::QemuMachine::Mac99);
        assert_eq!(settings.network, crate::machine::Network::Slirp);

        for (text, expected) in [
            ("qemu.machine g3beige", "can't be set"),
            ("hardware.rom quadra", "doesn't apply"),
            ("sheepshaver.sound off", "is a sheepshaver setting"),
        ] {
            let error = format!("{:#}", set_and_save(&paths, "g4", text).unwrap_err());
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn completion_follows_the_words_before_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        make_plus(&paths);
        let backends = crate::default_backends(&paths).unwrap();
        let machines = vec!["plus".to_string()];
        let offer = |current: Option<&str>, text: &str| {
            complete(&paths, &backends, current, &machines, &words(text))
        };
        assert_eq!(offer(None, ""), ["plus"]);
        let first = offer(Some("plus"), "");
        assert_eq!(first[0], "plus");
        assert!(first.contains(&"hardware.memory".to_string()), "{first:?}");
        let keys = offer(None, "plus");
        assert!(keys.contains(&"display.scale".to_string()), "{keys:?}");
        assert!(
            !keys.contains(&"plus".to_string()),
            "never a machine for a key"
        );
        let memory = offer(None, "plus hardware.memory");
        assert!(memory.contains(&"4M".to_string()), "{memory:?}");
        assert!(!memory.contains(&"8M".to_string()), "{memory:?}");
        assert_eq!(offer(None, "plus display.depth"), ["1"]);
        assert_eq!(offer(None, "plus ephemeral"), ["true", "false"]);
        // A key already given isn't offered again.
        let rest = offer(None, "plus ephemeral true");
        assert!(!rest.contains(&"ephemeral".to_string()), "{rest:?}");
        assert!(rest.contains(&"hardware.memory".to_string()), "{rest:?}");
        // The current machine's keys and values without a name.
        assert_eq!(offer(Some("plus"), "display.depth"), ["1"]);
        assert!(offer(None, "display.depth").is_empty(), "no machine to ask");
        assert!(offer(None, "plus no.such.key").is_empty());
        // With --machine, the first word is a key, and no machine is offered.
        let keys = offer(None, "--machine plus");
        assert!(
            keys.contains(&"display.depth".to_string()) && !keys.contains(&"plus".to_string()),
            "{keys:?}"
        );
        assert_eq!(offer(None, "--machine plus display.depth"), ["1"]);
        assert_eq!(offer(None, "--machine=plus ephemeral"), ["true", "false"]);
        assert_eq!(
            offer(None, "-v --machine plus ephemeral true display.depth"),
            ["1"]
        );
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn the_machine_is_named_once_and_a_missing_value_is_said() {
        let is_machine = |name: &str| name == "wombat";
        let split_words = |text: &str, option: Option<&str>| -> Result<(Option<String>, usize)> {
            let w = words(text);
            split(&w, option, &is_machine)
                .map(|(machine, pairs)| (machine.map(str::to_string), pairs.len()))
        };
        let error = |text: &str, option: Option<&str>| {
            format!("{:#}", split_words(text, option).unwrap_err())
        };

        // By position, as before.
        let w = words("wombat hardware.memory 4M");
        let (machine, pairs) = split(&w, None, &is_machine).unwrap();
        assert_eq!(machine, Some("wombat"));
        assert_eq!(pairs, [("hardware.memory".into(), "4M".into())]);
        assert_eq!(
            split_words("hardware.memory 4M display.scale 2", None).unwrap(),
            (None, 2)
        );
        // A machine that doesn't exist is still taken as one, for load to report.
        assert_eq!(
            split_words("nobody ephemeral true", None).unwrap(),
            (Some("nobody".into()), 1)
        );

        // By --machine, every word is a key or a value.
        assert_eq!(
            split_words("display.width 640 display.height 480", Some("wombat")).unwrap(),
            (Some("wombat".into()), 2)
        );

        // A value left out is said, wherever the machine came from.
        for (text, option) in [
            ("wombat hardware.memory", None),
            ("wombat ephemeral true display.scale", None),
            ("ephemeral true display.scale", None),
            ("hardware.memory", Some("wombat")),
            ("ephemeral true display.scale", Some("wombat")),
        ] {
            let message = error(text, option);
            assert!(message.contains("has no value"), "{text}: {message}");
        }
        assert!(
            error("ephemeral true display.scale", None).contains("\"display.scale\" has no value")
        );

        // Both ways at once is refused, not read as a key.
        let message = error("wombat ephemeral true", Some("wombat"));
        assert!(message.contains("given twice"), "{message}");
        let message = error("wombat ephemeral true", Some("other"));
        assert!(message.contains("given twice"), "{message}");
        let message = error("typo ephemeral true", Some("wombat"));
        assert!(message.contains("isn't a key"), "{message}");

        assert!(split_words("wombat", None).is_err());
        assert!(split(&[], None, &is_machine).is_err());
    }

    #[test]
    fn fixed_keys_cover_their_tables() {
        assert!(fixed("disks").is_some());
        assert!(fixed("disks[0].path").is_some());
        assert!(fixed("disks.0.path").is_some());
        assert!(fixed("diskspace").is_none());
        assert!(fixed("name").is_some());
        assert!(is_key("hardware.memory") && is_key("id"));
        assert!(!is_key("wombat-6"));
    }

    #[test]
    fn booleans_read_every_usual_way() {
        for yes in ["true", "TRUE", "on", "yes"] {
            assert_eq!(parse_bool("k", yes).unwrap(), "on");
        }
        for no in ["false", "off", "No"] {
            assert_eq!(parse_bool("k", no).unwrap(), "off");
        }
        for bad in ["1", "0", "", "maybe"] {
            assert!(parse_bool("k", bad).is_err(), "{bad}");
        }
    }
}
