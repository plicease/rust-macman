//! Checkpoints of a machine's boot disk: `macman checkpoint` and `rollback`.
//!
//! A checkpoint is a copy of the boot disk taken while the machine is shut
//! down, kept in the machine's own `checkpoints/` directory as `NAME.dsk`, read-
//! only, with a `NAME.toml` sidecar. It is a disk checkpoint, not a snapshot of
//! a running guest: no memory or emulator state, and no other disk.
//!
//! The image is published before its metadata, and only a checkpoint with
//! metadata exists as far as anything else is concerned, so an interrupted
//! `checkpoint create` never leaves one that looks usable. Rollback copies a
//! checkpoint beside the live disk, verifies it, and renames it into place, so
//! the live disk is untouched until the copy is known good.

use crate::disk::{self, ImageLock, LockMode};
use crate::fsutil::{
    independent_copy, list_dir, read_toml, sha256_file, validate_name, write_toml_new,
};
use crate::machine::Machine;
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where a machine's checkpoints live, inside its directory.
const DIR: &str = "checkpoints";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub name: String,
    /// When it was taken, in UTC, as RFC 3339.
    pub created: String,
    /// The boot disk it was taken of, as `machine.toml` names it.
    pub disk: String,
    pub size: u64,
    pub sha256: String,
}

pub fn dir(paths: &Paths, machine: &str) -> PathBuf {
    paths.machine(machine).join(DIR)
}

/// Where a checkpoint's image is stored.
pub fn image_file(paths: &Paths, machine: &str, name: &str) -> PathBuf {
    dir(paths, machine).join(format!("{name}.dsk"))
}

fn sidecar(image: &Path) -> PathBuf {
    image.with_extension("toml")
}

/// A machine's checkpoints, oldest first. An image without metadata is an
/// interrupted `create`, and isn't one.
pub fn list(paths: &Paths, machine: &str) -> Result<Vec<Checkpoint>> {
    let dir = dir(paths, machine);
    let mut found: Vec<Checkpoint> = list_dir(&dir)?
        .iter()
        .filter(|file| file.ends_with(".toml"))
        .map(|file| read_toml(&dir.join(file)))
        .collect::<Result<_>>()?;
    found.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

pub fn load(paths: &Paths, machine: &str, name: &str) -> Result<(Checkpoint, PathBuf)> {
    validate_name("checkpoint", name)?;
    let image = image_file(paths, machine, name);
    let metadata = sidecar(&image);
    if metadata.symlink_metadata().is_err() {
        bail!(
            "machine {machine:?} has no checkpoint named {name:?}; see `macman checkpoint list \
             {machine}`"
        );
    }
    Ok((read_toml(&metadata)?, image))
}

/// Proof that no emulator, and nothing else of macman's, has any of the
/// machine's disks: an exclusive lock on each, as `run` would need, held as
/// long as this lives.
struct ShutDown {
    _locks: Vec<ImageLock>,
}

fn shut_down(paths: &Paths, machine: &Machine) -> Result<ShutDown> {
    let mut locks = Vec::new();
    for (path, _) in machine.disk_paths(paths)? {
        if path.exists() {
            let lock = disk::lock_image(paths, &path, LockMode::Exclusive).with_context(|| {
                format!(
                    "machine {:?} is running, or one of its disks is in use; shut it down and \
                     quit the emulator first",
                    machine.name
                )
            })?;
            locks.push(lock);
        }
    }
    Ok(ShutDown { _locks: locks })
}

/// The boot disk, which must be one the guest can change.
fn boot_disk(paths: &Paths, machine: &Machine) -> Result<(PathBuf, String)> {
    let disks = machine.disk_paths(paths)?;
    let Some((path, disk)) = disks.into_iter().find(|(_, disk)| disk.boot) else {
        bail!(
            "machine {:?} has no boot disk, so there's nothing to checkpoint",
            machine.name
        );
    };
    if disk.readonly {
        bail!(
            "machine {:?} boots from a read-only disk, which the guest can't change, so there's \
             nothing to checkpoint or roll back",
            machine.name
        );
    }
    if Path::new(&disk.path).starts_with(DIR) {
        bail!(
            "machine {:?} keeps its boot disk under {DIR}/, where its checkpoints go",
            machine.name
        );
    }
    Ok((path, disk.path.clone()))
}

/// A name from the current time, such as `2026-11-03T21-04-05Z`, for a
/// checkpoint given none.
pub fn default_name() -> String {
    timestamp().replace(':', "-")
}

/// Now, in UTC, as RFC 3339 to the second.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    format_utc(seconds)
}

fn format_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Days to a civil date, after Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Save the machine's boot disk as checkpoint `name`, or one named for the
/// time. The machine must be shut down.
pub fn create(paths: &Paths, machine: &Machine, name: Option<&str>) -> Result<Checkpoint> {
    let name = name.map_or_else(default_name, str::to_string);
    validate_name("checkpoint", &name)?;
    let _shut = shut_down(paths, machine)?;
    let (live, disk) = boot_disk(paths, machine)?;
    if !live.exists() {
        bail!(
            "machine {:?} is missing its boot disk {}",
            machine.name,
            live.display()
        );
    }
    if all_zero(&live)? {
        bail!(
            "the boot disk of {:?} is blank, so there's nothing on it to keep yet; install a \
             system first",
            machine.name
        );
    }
    capture(paths, machine, &live, &disk, &name)
}

/// Whether every byte of the image at `path` is zero: a disk nothing has been
/// written to, not merely one whose first blocks are empty.
fn all_zero(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if read == 0 {
            return Ok(true);
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
    }
}

/// Copy `live` to a new checkpoint, the caller holding the machine shut down.
fn capture(
    paths: &Paths,
    machine: &Machine,
    live: &Path,
    disk: &str,
    name: &str,
) -> Result<Checkpoint> {
    let image = image_file(paths, &machine.name, name);
    let metadata = sidecar(&image);
    if metadata.symlink_metadata().is_ok() {
        bail!(
            "machine {:?} already has a checkpoint named {name:?}; pick another --name, or \
             delete that one first",
            machine.name
        );
    }
    // An image alone is what an interrupted `create` leaves; nothing uses it.
    if image.symlink_metadata().is_ok() {
        remove_image(&image)
            .with_context(|| format!("cannot clear an unfinished checkpoint {name:?}"))?;
    }
    fs::create_dir_all(dir(paths, &machine.name))?;
    let sha256 = sha256_file(live)?;
    // Never a hard link, so the guest's later writes can't reach it.
    independent_copy(live, &image).with_context(|| format!("cannot save checkpoint {name:?}"))?;
    let result = (|| -> Result<Checkpoint> {
        if sha256_file(&image)? != sha256 {
            bail!("checkpoint {name:?} doesn't match the disk it was copied from");
        }
        set_readonly(&image, true)?;
        let checkpoint = Checkpoint {
            name: name.to_string(),
            created: timestamp(),
            disk: disk.to_string(),
            size: fs::metadata(&image)?.len(),
            sha256,
        };
        write_toml_new(&metadata, &checkpoint)?;
        Ok(checkpoint)
    })();
    if result.is_err() {
        let _ = remove_image(&image);
    }
    result
}

/// Delete a checkpoint. Like everything else here, it waits for the machine
/// to be shut down, so a rollback reading it isn't undercut.
pub fn delete(paths: &Paths, machine: &Machine, name: &str) -> Result<()> {
    let (_, image) = load(paths, &machine.name, name)?;
    let _shut = shut_down(paths, machine)?;
    // The metadata goes first, so a checkpoint is never listed without its image.
    fs::remove_file(sidecar(&image))
        .with_context(|| format!("cannot delete checkpoint {name:?}"))?;
    match remove_image(&image) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("cannot delete {}", image.display()))
        }
        _ => Ok(()),
    }
}

/// Replace the machine's boot disk with checkpoint `name`, which is left as
/// it is. With `save`, first keep the disk as it stands as a new checkpoint,
/// so this can be undone; it's returned.
/// What a rollback did, beyond restoring the checkpoint.
#[derive(Debug)]
pub struct RolledBack {
    /// The checkpoint `--save` made of the boot disk first.
    pub saved: Option<Checkpoint>,
    /// Whether the boot disk was missing, and so recreated, not replaced.
    pub restored_missing: bool,
}

/// Replace the boot disk of `machine` with checkpoint `name`, keeping the disk
/// first as checkpoint `save`, if given. A missing boot disk is restored, which
/// is how to recover one deleted by mistake, but then there's nothing to save,
/// so `save` is refused rather than skipped.
pub fn rollback(
    paths: &Paths,
    machine: &Machine,
    name: &str,
    save: Option<&str>,
) -> Result<RolledBack> {
    let (checkpoint, image) = load(paths, &machine.name, name)?;
    if let Some(save) = save {
        validate_name("checkpoint", save)?;
    }
    let _shut = shut_down(paths, machine)?;
    let (live, disk) = boot_disk(paths, machine)?;
    let missing = !live.exists();
    if missing && save.is_some() {
        bail!(
            "machine {:?} is missing its boot disk {}, so there's nothing for --save to keep; \
             roll back without --save to restore the disk from the checkpoint; nothing was \
             changed",
            machine.name,
            live.display()
        );
    }
    if checkpoint.disk != disk {
        bail!(
            "checkpoint {name:?} is of disk {:?}, but machine {:?} now boots from {disk:?}",
            checkpoint.disk,
            machine.name
        );
    }
    let size = fs::metadata(&image)
        .with_context(|| format!("checkpoint {name:?} is missing its image"))?
        .len();
    if size != checkpoint.size || sha256_file(&image)? != checkpoint.sha256 {
        bail!(
            "checkpoint {name:?} is damaged: it no longer matches its digest; nothing was changed"
        );
    }
    let saved = match save {
        Some(save) => Some(capture(paths, machine, &live, &disk, save)?),
        None => None,
    };

    let staged = live.with_file_name(format!(
        ".rollback-{}-{}",
        std::process::id(),
        live.file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = fs::remove_file(&staged);
    let result = (|| -> Result<()> {
        // A writable, independent copy: guest writes can't reach the checkpoint.
        independent_copy(&image, &staged)?;
        if sha256_file(&staged)? != checkpoint.sha256 {
            bail!("the restored copy doesn't match checkpoint {name:?}");
        }
        // One rename, so the live disk is either the old one or the restored one.
        fs::rename(&staged, &live).with_context(|| format!("cannot replace {}", live.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result.with_context(|| {
        format!(
            "cannot roll machine {:?} back; its boot disk is as it was",
            machine.name
        )
    })?;
    Ok(RolledBack {
        saved,
        restored_missing: missing,
    })
}

fn set_readonly(path: &Path, readonly: bool) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms).with_context(|| format!("cannot protect {}", path.display()))
}

/// Windows won't delete a read-only file, so it's made writable first.
fn remove_image(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        let _ = set_readonly(path, false);
    }
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_rfc_3339() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_793_739_845), "2026-11-03T21:04:05Z");
        let name = default_name();
        assert!(!name.contains(':'), "{name}");
        validate_name("checkpoint", &name).unwrap();
    }
}
