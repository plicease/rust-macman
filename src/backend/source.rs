//! What every backend does the same way: fetching a pinned source archive, and
//! publishing one build per key in the cache.

use super::minivmac::{MANIFEST, Manifest};
use super::{Check, Executable};
use crate::fsutil::{atomic_copy, read_toml, sha256_file};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An exact upstream source archive.
#[derive(Debug, Clone)]
pub struct SourcePin {
    pub revision: String,
    pub url: String,
    pub sha256: String,
}

/// One backend's source: its pin, and where a user may have put a local copy.
pub struct Source<'a> {
    pub paths: &'a Paths,
    pub pin: &'a SourcePin,
    /// Optional local copy of the pinned archive.
    pub local: Option<&'a Path>,
    /// The emulator's name in messages.
    pub label: &'a str,
    /// The table in `config.toml` that holds `source_archive`.
    pub config_table: &'a str,
}

impl Source<'_> {
    pub fn cached_archive(&self) -> PathBuf {
        self.paths
            .sources()
            .join(&self.pin.revision)
            .join("source.tgz")
    }

    /// Put the verified source archive in the cache, downloading if needed.
    pub fn fetch(&self) -> Result<PathBuf> {
        let (label, table) = (self.label, self.config_table);
        let cached = self.cached_archive();
        if cached.exists() {
            if sha256_file(&cached)? == self.pin.sha256 {
                return Ok(cached);
            }
            eprintln!("warning: cached {label} source is damaged; fetching it again");
            fs::remove_file(&cached)?;
        }

        if let Some(local) = self.local {
            atomic_copy(local, &cached).context("cannot use configured source_archive")?;
        } else {
            eprintln!("Downloading {label} {}...", self.pin.revision);
            let partial = cached.with_extension("partial");
            let directory = cached.parent().context("no source directory")?;
            fs::create_dir_all(directory)?;
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                // Relative, so curl needn't cope with the cache's real path.
                .arg(partial.file_name().context("no source file name")?)
                .arg(&self.pin.url)
                .current_dir(directory)
                .status()
                .with_context(|| {
                    format!(
                        "cannot run curl; install it, or set {table}.source_archive in config.toml"
                    )
                })?;
            if !status.success() {
                let _ = fs::remove_file(&partial);
                bail!(
                    "download of {} failed; check the network, or download it yourself and set \
                     {table}.source_archive in config.toml",
                    self.pin.url
                );
            }
            fs::rename(&partial, &cached)?;
        }

        if sha256_file(&cached)? != self.pin.sha256 {
            fs::remove_file(&cached)?;
            bail!(
                "{label} source archive does not match the pinned SHA-256 {}; refusing to build \
                 from it",
                self.pin.sha256
            );
        }
        Ok(cached)
    }

    /// The `macman doctor` line for this source.
    pub fn check(&self) -> Check {
        let (label, table, revision) = (self.label, self.config_table, &self.pin.revision);
        match sha256_file(&self.cached_archive()) {
            Ok(sha) if sha == self.pin.sha256 => {
                Check::ok(format!("{label} {revision} source cached"))
            }
            Ok(_) => Check::warn(
                format!("cached {label} source is damaged"),
                "it will be fetched again on the next build",
            ),
            Err(_) => match self.local {
                Some(local) if local.is_file() => Check::ok(format!(
                    "{label} {revision} source available from source_archive"
                )),
                Some(_) => Check::fail(
                    format!("configured {table}.source_archive is missing"),
                    "fix or remove it in config.toml",
                ),
                None if tool_exists("curl") => Check::warn(
                    format!("{label} {revision} source not downloaded yet"),
                    format!(
                        "it will be downloaded on the first build; offline, set \
                         {table}.source_archive in config.toml"
                    ),
                ),
                None => Check::fail(
                    format!("{label} source not cached, and curl not found"),
                    format!("install curl, or set {table}.source_archive in config.toml"),
                ),
            },
        }
    }
}

/// The executable in a published build, provided it matches its manifest.
pub fn cached_build(paths: &Paths, key: &str) -> Option<PathBuf> {
    let dir = paths.builds().join(key);
    let manifest: Manifest = read_toml(&dir.join(MANIFEST)).ok()?;
    manifest
        .is_intact(&dir)
        .then(|| dir.join(&manifest.executable))
}

/// Reuse the published build `key`, or run `build` to publish it. Two runs of
/// one configuration wait for each other rather than building it twice.
pub fn cached_or_built(
    paths: &Paths,
    key: String,
    label: &str,
    build: impl FnOnce(&str) -> Result<()>,
) -> Result<Executable> {
    let reuse = |path, build_key| Executable {
        path,
        build_key,
        built: false,
    };
    if let Some(path) = cached_build(paths, &key) {
        return Ok(reuse(path, key));
    }

    fs::create_dir_all(paths.build_work())?;
    let lock_path = paths.build_work().join(format!(".{key}.lock"));
    let lock = File::create(&lock_path).context("cannot create build lock")?;
    lock.lock().context("cannot lock build")?;
    if let Some(path) = cached_build(paths, &key) {
        return Ok(reuse(path, key));
    }
    build(&key)?;
    let path = cached_build(paths, &key)
        .with_context(|| format!("published {label} build failed validation"))?;
    Ok(Executable {
        path,
        build_key: key,
        built: true,
    })
}

pub fn tool_exists(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}
