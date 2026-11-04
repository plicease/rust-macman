//! Machine aliases: other names for a machine, usable wherever a machine's
//! name is. They live in `aliases.toml` in the data directory, each naming
//! its machine and that machine's stable id, never another alias, so there are
//! no chains. Machine names and aliases are one namespace, guarded by the
//! machine lock that publishing a machine takes, so neither can take a name
//! the other has. The file is replaced atomically, so it's read without a lock.

use crate::edit::lock_definition;
use crate::fsutil::{read_toml, validate_name, write_toml};
use crate::machine::{Machine, lock_machines, new_id};
use crate::paths::Paths;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    aliases: BTreeMap<String, Target>,
}

/// The machine an alias names: its name when the alias was made, and its id,
/// which outlasts the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Target {
    machine: String,
    id: String,
}

/// What `alias` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    Added,
    /// The alias already named that machine, which is fine.
    Already,
}

fn file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("aliases.toml")
}

fn load(paths: &Paths) -> Result<Registry> {
    let file = file(paths);
    if !file.exists() {
        return Ok(Registry::default());
    }
    read_toml(&file)
}

fn save(paths: &Paths, registry: &Registry) -> Result<()> {
    write_toml(&file(paths), registry)
}

/// Whether `name` is a machine, rather than an alias or nothing.
fn is_machine(paths: &Paths, name: &str) -> bool {
    validate_name("machine", name).is_ok() && paths.machine(name).join("machine.toml").is_file()
}

/// The machine with `id`, by name, if there is one.
fn machine_with_id(paths: &Paths, target: &Target) -> Option<String> {
    let id = Some(target.id.as_str());
    if let Ok(machine) = Machine::load(paths, &target.machine)
        && machine.id.as_deref() == id
    {
        return Some(machine.name);
    }
    // Moved under another name: the id still says which it is.
    crate::fsutil::list_dir(&paths.machines())
        .ok()?
        .into_iter()
        .find(|name| Machine::load(paths, name).is_ok_and(|m| m.id.as_deref() == id))
}

/// The machine `name` names: itself, if it's a machine, or the one it's an
/// alias of. Anything else comes back as it is, for loading it to report.
pub fn resolve(paths: &Paths, name: &str) -> Result<String> {
    if is_machine(paths, name) {
        return Ok(name.to_string());
    }
    let registry = load(paths)?;
    let Some(target) = registry.aliases.get(name) else {
        return Ok(name.to_string());
    };
    match machine_with_id(paths, target) {
        Some(machine) => Ok(machine),
        None => bail!(
            "alias {name:?} names machine {:?}, which no longer exists; remove it with `macman \
             unalias {name}`",
            target.machine
        ),
    }
}

/// `machine`'s aliases, sorted.
pub fn of(paths: &Paths, machine: &Machine) -> Result<Vec<String>> {
    let Some(id) = &machine.id else {
        return Ok(Vec::new());
    };
    Ok(load(paths)?
        .aliases
        .into_iter()
        .filter(|(_, target)| &target.id == id)
        .map(|(alias, _)| alias)
        .collect())
}

/// Every alias, sorted, for completion.
pub fn all(paths: &Paths) -> Vec<String> {
    load(paths)
        .map(|registry| registry.aliases.into_keys().collect())
        .unwrap_or_default()
}

/// Aliases whose machine is gone, with the name each last had.
pub fn stale(paths: &Paths) -> Result<Vec<(String, String)>> {
    Ok(load(paths)?
        .aliases
        .into_iter()
        .filter(|(_, target)| machine_with_id(paths, target).is_none())
        .map(|(alias, target)| (alias, target.machine))
        .collect())
}

/// Refuse `name` for a new machine if an alias has it. The caller holds the
/// machine lock.
pub fn check_new_name(paths: &Paths, name: &str) -> Result<()> {
    if let Some(target) = load(paths)?.aliases.get(name) {
        bail!(
            "{name:?} is an alias of machine {:?}; pick another name, or free it with `macman \
             unalias {name}`",
            target.machine
        );
    }
    Ok(())
}

/// Make `alias` another name for the machine `target` names, which may be an
/// alias itself; the new alias names the machine, not it.
pub fn add(paths: &Paths, target: &str, alias: &str) -> Result<(String, Added)> {
    validate_name("alias", alias)?;
    let _lock = lock_machines(paths)?;
    let canonical = resolve(paths, target)?;
    let mut machine = Machine::load(paths, &canonical)?;
    if alias == canonical {
        bail!("{alias:?} is already the machine's own name");
    }
    if is_machine(paths, alias) || paths.machine(alias).symlink_metadata().is_ok() {
        bail!("{alias:?} is the name of a machine, so it can't be an alias too");
    }
    let mut registry = load(paths)?;
    if let Some(existing) = registry.aliases.get(alias) {
        if machine.id.as_deref() == Some(existing.id.as_str()) {
            return Ok((canonical, Added::Already));
        }
        bail!(
            "{alias:?} is already an alias of machine {:?}; remove it first with `macman \
             unalias {alias}`",
            existing.machine
        );
    }
    let id = match &machine.id {
        Some(id) => id.clone(),
        None => {
            // An alias follows the id, so a machine from before ids gets one.
            let _definition = lock_definition(paths, &canonical)?;
            machine = Machine::load(paths, &canonical)?;
            let id = machine.id.clone().unwrap_or_else(new_id);
            if machine.id.is_none() {
                machine.id = Some(id.clone());
                machine.save_to(&paths.machine(&canonical))?;
            }
            id
        }
    };
    registry.aliases.insert(
        alias.to_string(),
        Target {
            machine: canonical.clone(),
            id,
        },
    );
    save(paths, &registry)?;
    Ok((canonical, Added::Added))
}

/// Remove `alias`, and nothing else, returning the machine it named.
pub fn remove(paths: &Paths, alias: &str) -> Result<String> {
    validate_name("alias", alias)?;
    let _lock = lock_machines(paths)?;
    let mut registry = load(paths)?;
    let Some(target) = registry.aliases.remove(alias) else {
        if is_machine(paths, alias) {
            bail!("{alias:?} is a machine, not an alias; `macman remove {alias}` deletes machines");
        }
        bail!("there's no alias named {alias:?}; `macman list` shows each machine's aliases");
    };
    save(paths, &registry)?;
    Ok(machine_with_id(paths, &target).unwrap_or(target.machine))
}

/// Remove every alias of `machine`, as it's deleted, returning them.
pub fn forget(paths: &Paths, machine: &Machine) -> Result<Vec<String>> {
    let Some(id) = &machine.id else {
        return Ok(Vec::new());
    };
    let _lock = lock_machines(paths)?;
    let mut registry = load(paths)?;
    let gone: Vec<String> = registry
        .aliases
        .iter()
        .filter(|(_, target)| &target.id == id)
        .map(|(alias, _)| alias.clone())
        .collect();
    if !gone.is_empty() {
        for alias in &gone {
            registry.aliases.remove(alias);
        }
        save(paths, &registry)?;
    }
    Ok(gone)
}
