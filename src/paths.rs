//! Per-user directory layout (design §6): XDG directories on Unix, macOS
//! included, and the roaming and local application data folders on Windows.

#[cfg(not(windows))]
use anyhow::Context;
use anyhow::{Result, bail};
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
    #[cfg(not(windows))]
    pub fn from_env() -> Result<Self> {
        Self::from_unix_env(|var| env::var_os(var))
    }

    #[cfg(not(windows))]
    fn from_unix_env(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Self> {
        Ok(Self {
            config_dir: xdg_dir(&var, "XDG_CONFIG_HOME", ".config")?,
            data_dir: xdg_dir(&var, "XDG_DATA_HOME", ".local/share")?,
            cache_dir: xdg_dir(&var, "XDG_CACHE_HOME", ".cache")?,
        })
    }

    /// Resolve directories without needing `HOME`. For each directory the
    /// first of these wins: an `XDG_*` override, then `%APPDATA%` or
    /// `%LOCALAPPDATA%`, then the shell's known folder.
    #[cfg(windows)]
    pub fn from_env() -> Result<Self> {
        Self::from_windows_env(|var| env::var_os(var), windows::known_folder)
    }

    #[cfg(windows)]
    fn from_windows_env(
        var: impl Fn(&str) -> Option<std::ffi::OsString>,
        known_folder: impl Fn(windows::Folder) -> Result<PathBuf>,
    ) -> Result<Self> {
        use windows::Folder;
        let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let dir = |xdg: &str, native: &str, folder, below: &[&str]| -> Result<PathBuf> {
            if let Some(path) = absolute(xdg) {
                return Ok(path.join("macman"));
            }
            let mut base = match absolute(native) {
                Some(path) => path,
                None => known_folder(folder)?,
            };
            base.extend(below);
            Ok(base)
        };
        let local = "LOCALAPPDATA";
        Ok(Self {
            config_dir: dir(
                "XDG_CONFIG_HOME",
                "APPDATA",
                Folder::RoamingAppData,
                &["macman"],
            )?,
            data_dir: dir("XDG_DATA_HOME", local, Folder::LocalAppData, &["macman"])?,
            cache_dir: dir(
                "XDG_CACHE_HOME",
                local,
                Folder::LocalAppData,
                &["macman", "cache"],
            )?,
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
    /// Held by whatever is putting a new machine directory in place.
    pub fn machines_lock(&self) -> PathBuf {
        self.locks().join("machines.lock")
    }
    /// One directory per live `run`; holds throwaway disk copies.
    pub fn sessions(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    /// Working space for `macman import`, on the same filesystem as the data.
    pub fn staging(&self) -> PathBuf {
        self.data_dir.join("staging")
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

#[cfg(not(windows))]
fn xdg_dir(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    var: &str,
    home_relative: &str,
) -> Result<PathBuf> {
    // The XDG spec says relative values must be ignored.
    if let Some(value) = lookup(var) {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path.join("macman"));
        }
    }
    let Some(home) = lookup("HOME").filter(|h| !h.is_empty()) else {
        bail!("neither {var} nor HOME is set; set one so macman can find its files");
    };
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(anyhow::anyhow!("HOME is not an absolute path"))
            .context("cannot locate macman directories");
    }
    Ok(home.join(home_relative).join("macman"))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Folder {
        RoamingAppData,
        LocalAppData,
    }

    pub fn known_folder(folder: Folder) -> Result<PathBuf> {
        let id = match folder {
            Folder::RoamingAppData => &FOLDERID_RoamingAppData,
            Folder::LocalAppData => &FOLDERID_LocalAppData,
        };
        let mut raw = std::ptr::null_mut();
        // SAFETY: `id` and `raw` are valid for the call. On return `raw` is
        // either null or a NUL-terminated string this function must free.
        let result = unsafe {
            SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut raw)
        };
        let path = (result >= 0 && !raw.is_null()).then(|| {
            // SAFETY: as above; the terminator bounds the read.
            let wide = unsafe {
                let length = (0..).take_while(|&i| *raw.add(i) != 0).count();
                std::slice::from_raw_parts(raw, length)
            };
            PathBuf::from(OsString::from_wide(wide))
        });
        // SAFETY: `raw` came from SHGetKnownFolderPath; null is allowed.
        unsafe { CoTaskMemFree(raw.cast()) };
        match path {
            Some(path) if path.is_absolute() => Ok(path),
            _ => bail!("cannot locate the {folder:?} folder (error {result:#x})"),
        }
    }
}

/// These hold on macOS as on Linux: macman deliberately stays out of
/// `~/Library`, so the same files are found in the same places on both.
#[cfg(all(test, not(windows)))]
mod unix_tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn resolve(vars: &[(&str, &str)]) -> Result<Paths> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        Paths::from_unix_env(|name| vars.get(name).map(OsString::from))
    }

    fn dirs(paths: &Paths) -> [&str; 3] {
        [
            paths.config_dir.to_str().unwrap(),
            paths.data_dir.to_str().unwrap(),
            paths.cache_dir.to_str().unwrap(),
        ]
    }

    #[test]
    fn home_gives_the_linux_style_defaults() {
        let paths = resolve(&[("HOME", "/Users/zoe")]).unwrap();
        assert_eq!(
            dirs(&paths),
            [
                "/Users/zoe/.config/macman",
                "/Users/zoe/.local/share/macman",
                "/Users/zoe/.cache/macman",
            ]
        );
    }

    #[test]
    fn absolute_overrides_win_one_directory_at_a_time() {
        let paths = resolve(&[
            ("HOME", "/Users/zoe"),
            ("XDG_CONFIG_HOME", "/etc/xdg"),
            ("XDG_CACHE_HOME", "/Volumes/Scratch Disk"),
        ])
        .unwrap();
        assert_eq!(
            dirs(&paths),
            [
                "/etc/xdg/macman",
                "/Users/zoe/.local/share/macman",
                "/Volumes/Scratch Disk/macman",
            ]
        );
    }

    #[test]
    fn relative_and_empty_overrides_are_ignored() {
        let paths = resolve(&[
            ("HOME", "/Users/zoe"),
            ("XDG_CONFIG_HOME", "relative"),
            ("XDG_DATA_HOME", ""),
            ("XDG_CACHE_HOME", "./also/relative"),
        ])
        .unwrap();
        assert_eq!(dirs(&paths)[0], "/Users/zoe/.config/macman");
        assert_eq!(dirs(&paths)[1], "/Users/zoe/.local/share/macman");
        assert_eq!(dirs(&paths)[2], "/Users/zoe/.cache/macman");
    }

    #[test]
    fn nothing_lands_in_the_macos_library() {
        let paths = resolve(&[("HOME", "/Users/zoe")]).unwrap();
        for dir in dirs(&paths) {
            assert!(!dir.contains("Library"), "{dir}");
        }
    }

    #[test]
    fn home_is_needed_only_when_an_override_is_missing() {
        assert!(resolve(&[]).is_err());
        assert!(resolve(&[("HOME", "")]).is_err());
        assert!(resolve(&[("HOME", "relative")]).is_err());
        let paths = resolve(&[
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_CACHE_HOME", "/e"),
        ])
        .unwrap();
        assert_eq!(dirs(&paths), ["/c/macman", "/d/macman", "/e/macman"]);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::windows::{Folder, known_folder};
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn resolve(vars: &[(&str, &str)]) -> Paths {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        Paths::from_windows_env(
            |name| vars.get(name).map(OsString::from),
            |folder| {
                Ok(PathBuf::from(match folder {
                    Folder::RoamingAppData => r"C:\Known\Roaming",
                    Folder::LocalAppData => r"C:\Known\Local",
                }))
            },
        )
        .unwrap()
    }

    fn dirs(paths: &Paths) -> [&str; 3] {
        [
            paths.config_dir.to_str().unwrap(),
            paths.data_dir.to_str().unwrap(),
            paths.cache_dir.to_str().unwrap(),
        ]
    }

    #[test]
    fn the_environment_names_the_native_folders() {
        let paths = resolve(&[
            ("APPDATA", r"C:\Users\Zoë Q\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\Zoë Q\AppData\Local"),
        ]);
        assert_eq!(
            dirs(&paths),
            [
                r"C:\Users\Zoë Q\AppData\Roaming\macman",
                r"C:\Users\Zoë Q\AppData\Local\macman",
                r"C:\Users\Zoë Q\AppData\Local\macman\cache",
            ]
        );
    }

    #[test]
    fn known_folders_stand_in_for_an_empty_environment() {
        for vars in [
            &[][..],
            &[("APPDATA", ""), ("LOCALAPPDATA", "")][..],
            &[("APPDATA", "relative"), ("LOCALAPPDATA", r"also\relative")][..],
        ] {
            assert_eq!(
                dirs(&resolve(vars)),
                [
                    r"C:\Known\Roaming\macman",
                    r"C:\Known\Local\macman",
                    r"C:\Known\Local\macman\cache",
                ]
            );
        }
    }

    #[test]
    fn xdg_overrides_win_one_directory_at_a_time() {
        let paths = resolve(&[
            ("APPDATA", r"C:\Roaming"),
            ("LOCALAPPDATA", r"C:\Local"),
            ("XDG_CACHE_HOME", r"D:\scratch space"),
            ("XDG_CONFIG_HOME", "relative is ignored"),
        ]);
        assert_eq!(
            dirs(&paths),
            [
                r"C:\Roaming\macman",
                r"C:\Local\macman",
                r"D:\scratch space\macman",
            ]
        );
    }

    #[test]
    fn the_shell_knows_both_folders() {
        for folder in [Folder::RoamingAppData, Folder::LocalAppData] {
            assert!(known_folder(folder).unwrap().is_dir());
        }
    }
}
