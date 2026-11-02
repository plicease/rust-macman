//! Small filesystem helpers shared across the crate.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Write `contents` so readers see the old file or the new one, never a mix.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = temp_sibling(path)?;
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot write {}", path.display()))
}

/// Copy `src` to `dst`, publishing `dst` only once the copy is complete.
/// Refuses to replace an existing file.
pub fn atomic_copy(src: &Path, dst: &Path) -> Result<()> {
    let length = fs::metadata(src)
        .with_context(|| format!("cannot read {}", src.display()))?
        .len();
    atomic_copy_range(src, 0, length, dst)
}

/// As [`atomic_copy`], for `length` bytes of `src` starting at `offset`.
pub fn atomic_copy_range(src: &Path, offset: u64, length: u64, dst: &Path) -> Result<()> {
    atomic_copy_range_checked(src, offset, length, dst, |_| Ok(()))
}

/// As [`atomic_copy_range`], with `check` deciding, from the finished temporary
/// file, whether to publish it. A rejected copy never appears at `dst`.
pub fn atomic_copy_range_checked(
    src: &Path,
    offset: u64,
    length: u64,
    dst: &Path,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    if dst.symlink_metadata().is_ok() {
        bail!("{} already exists", dst.display());
    }
    let tmp = temp_sibling(dst)?;
    let result = (|| -> Result<()> {
        let mut input = File::open(src)?;
        input.seek(SeekFrom::Start(offset))?;
        let mut output = File::create(&tmp)?;
        if io::copy(&mut input.take(length), &mut output)? != length {
            bail!("source ended early");
        }
        output.sync_all()?;
        drop(output);
        check(&tmp)?;
        publish_new(&tmp, dst)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot copy {} to {}", src.display(), dst.display()))
}

/// Move `tmp` to `dst`, failing if `dst` appeared since it was last checked.
pub fn publish_new(tmp: &Path, dst: &Path) -> Result<()> {
    publish_new_with(tmp, dst, |from, to| fs::hard_link(from, to))
}

/// A hard link can't replace a file, where a rename would, so `link` is the
/// only way in: with no hard links available, this fails rather than risk it.
fn publish_new_with(
    tmp: &Path,
    dst: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    match link(tmp, dst) {
        Ok(()) => {
            fs::remove_file(tmp)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            bail!("{} already exists", dst.display())
        }
        Err(error) => bail!(
            "cannot safely create {}: {error}. macman needs hard links there so it can't \
             replace an existing file, which some filesystems (such as FAT and exFAT) lack; \
             use another location",
            dst.display()
        ),
    }
}

pub fn temp_sibling(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create directory {}", parent.display()))?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.tmp-{}", std::process::id())))
}

/// Undo the `\\?\` form that `fs::canonicalize` gives on Windows, which
/// neither people nor Mini vMac need to see.
pub fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if count == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..count]);
    }
}

pub fn hex_digest(hasher: Sha256) -> String {
    hex(&hasher.finalize())
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

pub fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value)
        .with_context(|| format!("cannot serialize {}", path.display()))?;
    atomic_write(path, text.as_bytes())
}

/// Names become file names, so restrict them to a safe alphabet.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !valid_chars || !valid_start || name.len() > 64 {
        bail!(
            "invalid {kind} name {name:?}: use up to 64 letters, digits, '.', '_' or '-', \
             starting with a letter or digit"
        );
    }
    if cfg!(windows) && !windows_can_name(name) {
        bail!("invalid {kind} name {name:?}: Windows reserves it for a device or trims it");
    }
    Ok(())
}

/// Device names are reserved whatever follows the first dot, and a trailing
/// dot is silently dropped, which would make two names one directory.
fn windows_can_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    !device && !name.ends_with('.')
}

/// Sorted names of the non-hidden entries of `dir`; empty if it doesn't exist.
pub fn list_dir(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot list {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_their_directory() {
        for bad in ["", "..", ".hidden", "a/b", "../x", "a b", "-flag"] {
            assert!(validate_name("machine", bad).is_err(), "{bad:?}");
        }
        for good in ["wombat-6", "system-6.0.8", "plus_v3", "6"] {
            validate_name("machine", good).unwrap();
        }
    }

    #[test]
    fn verbatim_prefixes_are_removed() {
        for (path, expected) in [
            (r"\\?\C:\Users\Zoë Q\a.dsk", r"C:\Users\Zoë Q\a.dsk"),
            (r"\\?\UNC\server\share\a.dsk", r"\\server\share\a.dsk"),
            (r"C:\plain.dsk", r"C:\plain.dsk"),
        ] {
            assert_eq!(
                without_verbatim_prefix(Path::new(path)),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn windows_reserved_names_are_recognized() {
        for bad in [
            "con",
            "CON",
            "nul.dsk",
            "aux.1",
            "com1",
            "LPT9.rom",
            "trailing.",
        ] {
            assert!(!windows_can_name(bad), "{bad:?}");
        }
        for good in ["console", "com", "com10", "lpt", "null", "mac.ii", "comx"] {
            assert!(windows_can_name(good), "{good:?}");
        }
    }

    #[test]
    fn atomic_copy_makes_read_only_sources_writable_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, b"data").unwrap();
        let mut perms = fs::metadata(&src).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&src, perms).unwrap();

        atomic_copy(&src, &dst).unwrap();
        assert!(!fs::metadata(&dst).unwrap().permissions().readonly());
        assert!(atomic_copy(&src, &dst).is_err());
    }

    #[test]
    fn checked_copy_publishes_only_what_passes_and_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"data").unwrap();

        let dst = dir.path().join("rejected");
        let rejected = atomic_copy_range_checked(&src, 0, 4, &dst, |tmp| {
            assert_eq!(fs::read(tmp).unwrap(), b"data");
            bail!("no thanks")
        });
        assert!(rejected.is_err());
        assert!(!dst.exists());

        // A file appearing after the first check must survive.
        let dst = dir.path().join("raced");
        let raced = atomic_copy_range_checked(&src, 0, 4, &dst, |_| {
            fs::write(&dst, b"someone else's").unwrap();
            Ok(())
        });
        assert!(format!("{:#}", raced.unwrap_err()).contains("already exists"));
        assert_eq!(fs::read(&dst).unwrap(), b"someone else's");

        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 2, "no temporary files left: {names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"data").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("target"), &link).unwrap();

        assert!(atomic_copy_range(&src, 0, 4, &link).is_err());
        assert!(!dir.path().join("target").exists());
    }

    #[test]
    fn without_hard_links_publishing_fails_instead_of_renaming() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        fs::write(&tmp, b"new").unwrap();
        let no_links = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::Unsupported));

        // Nothing there: still refused, and nothing is created.
        let dst = dir.path().join("dst");
        let err = publish_new_with(&tmp, &dst, no_links)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hard links"), "{err}");
        assert!(!dst.exists());

        // A file that appeared after the first check, the case a rename would destroy.
        fs::write(&dst, b"someone else's").unwrap();
        assert!(publish_new_with(&tmp, &dst, no_links).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"someone else's");
        assert!(tmp.exists(), "the caller decides what to do with it");
    }

    #[test]
    fn atomic_copy_range_copies_exactly_the_range_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"0123456789").unwrap();

        let dst = dir.path().join("dst");
        atomic_copy_range(&src, 2, 5, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"23456");

        let short = dir.path().join("short");
        assert!(atomic_copy_range(&src, 8, 5, &short).is_err());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "nothing left behind: {names:?}");
    }
}
