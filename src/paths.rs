//! XDG directory layout (design §6).

use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Resolve directories from `XDG_*` variables, falling back to `$HOME`.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            config_dir: xdg_dir("XDG_CONFIG_HOME", ".config")?,
            data_dir: xdg_dir("XDG_DATA_HOME", ".local/share")?,
            cache_dir: xdg_dir("XDG_CACHE_HOME", ".cache")?,
        })
    }

    /// Use one root for all three directories; handy for tests.
    pub fn under(root: &Path) -> Self {
        Self {
            config_dir: root.join("config/macman"),
            data_dir: root.join("data/macman"),
            cache_dir: root.join("cache/macman"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn roms(&self) -> PathBuf {
        self.data_dir.join("roms")
    }
    pub fn systems(&self) -> PathBuf {
        self.data_dir.join("systems")
    }
    pub fn machines(&self) -> PathBuf {
        self.data_dir.join("machines")
    }
    pub fn machine(&self, name: &str) -> PathBuf {
        self.machines().join(name)
    }
    /// Per-image advisory lock files.
    pub fn locks(&self) -> PathBuf {
        self.data_dir.join("locks")
    }
    /// One directory per live `run`; holds throwaway disk copies.
    pub fn sessions(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    pub fn builds(&self) -> PathBuf {
        self.cache_dir.join("builds")
    }
    pub fn sources(&self) -> PathBuf {
        self.cache_dir.join("sources")
    }
    pub fn build_work(&self) -> PathBuf {
        self.cache_dir.join("work")
    }
    pub fn build_logs(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }
}

fn xdg_dir(var: &str, home_relative: &str) -> Result<PathBuf> {
    // The XDG spec says relative values must be ignored.
    if let Some(value) = env::var_os(var) {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path.join("macman"));
        }
    }
    let Some(home) = env::var_os("HOME").filter(|h| !h.is_empty()) else {
        bail!("neither {var} nor HOME is set; set one so macman can find its files");
    };
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(anyhow::anyhow!("HOME is not an absolute path"))
            .context("cannot locate macman directories");
    }
    Ok(home.join(home_relative).join("macman"))
}
