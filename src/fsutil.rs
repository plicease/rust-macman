//! Small filesystem helpers shared across the crate.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
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
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    let tmp = temp_sibling(dst)?;
    let result = (|| -> Result<()> {
        fs::copy(src, &tmp)?;
        // A read-only source must still yield a writable private copy.
        let mut perms = fs::metadata(&tmp)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&tmp, perms)?;
        fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot copy {} to {}", src.display(), dst.display()))
}

fn temp_sibling(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create directory {}", parent.display()))?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.tmp-{}", std::process::id())))
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
    Ok(())
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
}
