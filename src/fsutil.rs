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

/// Copy `src` to a new, writable, independent file at `dst`, which must not
/// exist. Where the file system can, the copy shares blocks with `src` until
/// either is written (a reflink, or a clone on APFS), so it's quick and takes
/// no room at first; elsewhere it's a full copy. Never a hard link, so writes
/// to the copy can't reach `src`.
pub fn independent_copy(src: &Path, dst: &Path) -> Result<()> {
    if dst.symlink_metadata().is_ok() {
        bail!("{} already exists", dst.display());
    }
    let tmp = temp_sibling(dst)?;
    let result = (|| -> Result<()> {
        if !reflink(src, &tmp)? {
            // The standard library clones on macOS, and lets the kernel copy
            // on Linux, which keeps holes on some file systems.
            fs::copy(src, &tmp)?;
        }
        let mut perms = fs::metadata(&tmp)?.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&tmp, perms)?;
        }
        // Windows flushes a file only through a handle that can write to it.
        fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        publish_new(&tmp, dst)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot copy {} to {}", src.display(), dst.display()))
}

/// Make `dst` a reflink of `src`, returning false, with nothing at `dst`,
/// where the file system can't.
#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let source = File::open(src)?;
    let target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    // SAFETY: both descriptors are open for as long as the call lasts.
    let cloned = unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0;
    drop(target);
    if !cloned {
        fs::remove_file(dst)?;
    }
    Ok(cloned)
}

#[cfg(not(target_os = "linux"))]
fn reflink(_src: &Path, _dst: &Path) -> Result<bool> {
    Ok(false)
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

/// A digest of a directory: every entry's relative path and kind, and each
/// file's contents and executable bit, or each link's target.
pub fn sha256_tree(root: &Path) -> Result<String> {
    let mut listing = String::new();
    hash_tree_into(root, Path::new(""), &mut listing)?;
    Ok(sha256_bytes(listing.as_bytes()))
}

fn hash_tree_into(dir: &Path, relative: &Path, listing: &mut String) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("cannot read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = relative.join(entry.file_name());
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let shown = name.to_string_lossy();
        if meta.is_symlink() {
            let target = fs::read_link(&path)?;
            listing.push_str(&format!("link {shown:?} {:?}\n", target.to_string_lossy()));
        } else if meta.is_dir() {
            listing.push_str(&format!("dir {shown:?}\n"));
            hash_tree_into(&path, &name, listing)?;
        } else {
            let mode = if is_executable(&meta) { "exec" } else { "file" };
            let sha = sha256_file(&path)?;
            listing.push_str(&format!("{mode} {shown:?} {sha}\n"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Copy a directory, keeping permissions and links. Refuses to merge into an
/// existing `dst`.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&from)
            .with_context(|| format!("cannot read {}", from.display()))?;
        if meta.is_symlink() {
            symlink_file(&fs::read_link(&from)?, &to)
                .with_context(|| format!("cannot create {}", to.display()))?;
        } else if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("cannot copy {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
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

/// [`write_toml`], except that it won't replace an existing file.
pub fn write_toml_new<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value)
        .with_context(|| format!("cannot serialize {}", path.display()))?;
    let tmp = temp_sibling(path)?;
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        publish_new(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot write {}", path.display()))
}

/// The smallest and largest disk `macman create --disk-size` makes. The top is
/// the limit of an HFS or HFS+ volume under Mac OS 9.
const MIN_DISK_SIZE: u64 = 1 << 20;
const MAX_DISK_SIZE: u64 = 2 << 40;

/// A disk size in bytes, from a whole number of K, M, or G, in units of 1024.
pub fn parse_disk_size(text: &str) -> Result<u64> {
    let invalid = || anyhow::anyhow!("invalid disk size {text:?}: use a size such as 800M or 1G");
    let unit = text.chars().last().ok_or_else(invalid)?;
    let number = &text[..text.len() - unit.len_utf8()];
    let unit: u64 = match unit.to_ascii_uppercase() {
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        _ => return Err(invalid()),
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let size = number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(unit))
        .filter(|size| (MIN_DISK_SIZE..=MAX_DISK_SIZE).contains(size));
    size.with_context(|| format!("disk size {text:?} is out of range: use 1M to 2048G"))
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
pub fn windows_can_name(name: &str) -> bool {
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

/// A file lock that has been taken, and is released when this is dropped.
///
/// Closing the file isn't enough to rely on. A process being spawned, by any
/// thread, has a copy of the descriptor until it reaches `exec`, and a `flock`
/// lasts until the last copy closes. That can take long enough on macOS for
/// the next attempt to find the lock taken. Unlocking applies to every copy.
#[derive(Debug)]
pub struct HeldLock(File);

impl HeldLock {
    /// `file` must already hold its lock.
    pub fn new(file: File) -> Self {
        Self(file)
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_independent_copy_is_writable_and_never_reaches_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("boot.dsk");
        fs::write(&source, vec![7u8; 64 * 1024]).unwrap();
        let mut perms = fs::metadata(&source).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&source, perms).unwrap();

        let copy = dir.path().join("copy.dsk");
        independent_copy(&source, &copy).unwrap();
        assert!(!fs::metadata(&copy).unwrap().permissions().readonly());
        fs::write(&copy, b"guest").unwrap();
        assert_eq!(fs::read(&source).unwrap(), vec![7u8; 64 * 1024]);

        let error = independent_copy(&source, &copy).unwrap_err();
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        assert_eq!(fs::read(&copy).unwrap(), b"guest");
        let leftovers = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 2, "no temporary file is left");
    }

    #[cfg(unix)]
    #[test]
    fn a_released_lock_is_free_while_processes_start() {
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    while !done.load(Ordering::Relaxed) {
                        let _ = Command::new("true").stdin(Stdio::null()).status();
                    }
                });
            }
            let result = (0..5000).try_for_each(|_| {
                let file = File::create(&path).unwrap();
                file.try_lock()?;
                drop(HeldLock::new(file));
                Ok::<_, fs::TryLockError>(())
            });
            done.store(true, Ordering::Relaxed);
            result.unwrap();
        });
    }

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

    #[cfg(unix)]
    fn fake_bundle(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bundle = root.join("fake.app");
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::write(bundle.join("Contents/Info.plist"), b"plist").unwrap();
        let executable = bundle.join("Contents/MacOS/fake");
        fs::write(&executable, b"binary").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        symlink_file(Path::new("MacOS/fake"), &bundle.join("Contents/link")).unwrap();
        bundle
    }

    #[cfg(unix)]
    #[test]
    fn a_copied_tree_is_identical_and_stays_runnable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(dir.path());
        let copy = dir.path().join("copy.app");

        copy_tree(&bundle, &copy).unwrap();
        assert_eq!(sha256_tree(&bundle).unwrap(), sha256_tree(&copy).unwrap());
        let mode = fs::metadata(copy.join("Contents/MacOS/fake"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        assert!(copy.join("Contents/link").is_symlink());
        assert!(copy_tree(&bundle, &copy).is_err(), "never merges");
    }

    #[cfg(unix)]
    #[test]
    fn the_tree_digest_notices_any_change_in_the_tree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(dir.path());
        let original = sha256_tree(&bundle).unwrap();
        let digest = || sha256_tree(&bundle).unwrap();

        let plist = bundle.join("Contents/Info.plist");
        fs::write(&plist, b"other").unwrap();
        assert_ne!(digest(), original, "contents");
        fs::write(&plist, b"plist").unwrap();
        assert_eq!(digest(), original);

        let executable = bundle.join("Contents/MacOS/fake");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        assert_ne!(digest(), original, "executable bit");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        fs::rename(&plist, bundle.join("Contents/Moved.plist")).unwrap();
        assert_ne!(digest(), original, "name");
        fs::rename(bundle.join("Contents/Moved.plist"), &plist).unwrap();

        let link = bundle.join("Contents/link");
        fs::remove_file(&link).unwrap();
        assert_ne!(digest(), original, "missing entry");
        symlink_file(Path::new("elsewhere"), &link).unwrap();
        assert_ne!(digest(), original, "link target");
        fs::remove_file(&link).unwrap();
        symlink_file(Path::new("MacOS/fake"), &link).unwrap();
        assert_eq!(digest(), original);
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

    #[test]
    fn disk_sizes_are_whole_units_within_range() {
        assert_eq!(parse_disk_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_disk_size("800m").unwrap(), 800 << 20);
        assert_eq!(parse_disk_size("1024K").unwrap(), 1 << 20);
        assert_eq!(parse_disk_size("2048G").unwrap(), 2 << 40);
        for bad in ["", "G", "1", "1.5G", "-1G", "1 G", "1T", "1GB", "é"] {
            let error = parse_disk_size(bad).unwrap_err().to_string();
            assert!(error.contains("invalid disk size"), "{bad:?}: {error}");
        }
        for bad in [
            "0M",
            "1023K",
            "2049G",
            "99999999999999999999G",
            "18014398509481984G",
        ] {
            let error = parse_disk_size(bad).unwrap_err().to_string();
            assert!(error.contains("out of range"), "{bad:?}: {error}");
        }
    }
}
