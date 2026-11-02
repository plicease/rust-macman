//! Disk-image safety (design §8): format checks, advisory locks, and the
//! session records used to spot abnormal exits.

use crate::fsutil::{atomic_copy, list_dir, read_toml, sha256_bytes, write_toml};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    RawMfs,
    RawHfs,
    DiskCopy42,
}

const MDB_OFFSET: u64 = 1024;
const DC42_HEADER: u64 = 84;

/// Identify an image by content, because `.dsk`/`.img` names promise nothing.
pub fn detect_format(path: &Path) -> Result<DiskFormat> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();

    if size >= DC42_HEADER {
        let mut header = [0u8; DC42_HEADER as usize];
        file.read_exact(&mut header)?;
        let data_size = u32::from_be_bytes(header[64..68].try_into()?);
        let tag_size = u32::from_be_bytes(header[68..72].try_into()?);
        let total = DC42_HEADER + u64::from(data_size) + u64::from(tag_size);
        if header[82..84] == [0x01, 0x00] && header[0] <= 63 && data_size > 0 && total == size {
            return Ok(DiskFormat::DiskCopy42);
        }
    }

    if size >= MDB_OFFSET + 2 && size.is_multiple_of(512) {
        let mut signature = [0u8; 2];
        file.seek(SeekFrom::Start(MDB_OFFSET))?;
        file.read_exact(&mut signature)?;
        match &signature {
            [0xD2, 0xD7] => return Ok(DiskFormat::RawMfs),
            b"BD" => return Ok(DiskFormat::RawHfs),
            _ => {}
        }
    }

    bail!(
        "{} is not a supported disk image; macman accepts raw MFS/HFS volumes and \
         Disk Copy 4.2 images (no partition maps or archives). Convert it with an external \
         tool first; macman does not convert images",
        path.display()
    )
}

/// An exclusive or shared advisory lock on a disk image, held until dropped.
///
/// Locks live in a sidecar directory, keyed by file identity rather than path,
/// so symlinks and hard links to one image contend for the same lock.
#[derive(Debug)]
pub struct ImageLock {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// The image may be written; no one else may touch it.
    Exclusive,
    /// The image is only being read (say, copied); writers are kept out.
    Shared,
}

pub fn lock_image(paths: &Paths, image: &Path, mode: LockMode) -> Result<ImageLock> {
    let key = file_identity(image)?;
    fs::create_dir_all(paths.locks()).context("cannot create lock directory")?;
    let lock_path = paths.locks().join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open lock {}", lock_path.display()))?;
    let result = match mode {
        LockMode::Exclusive => file.try_lock(),
        LockMode::Shared => file.try_lock_shared(),
    };
    match result {
        Ok(()) => Ok(ImageLock { _file: file }),
        Err(TryLockError::WouldBlock) => bail!(
            "{} is in use by another macman process or machine; close that emulator first, \
             or work on an explicit copy",
            image.display()
        ),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", image.display()))
        }
    }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(format!("{:x}-{:x}", meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(sha256_bytes(path.to_string_lossy().as_bytes()))
}

/// Metadata for one live `run`; a session whose lock is free died uncleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub machine: String,
    pub pid: u32,
    /// Writable disks the session had attached.
    pub writable_disks: Vec<PathBuf>,
}

/// A running session. Dropping it without [`Session::finish`] leaves the
/// record behind, which is how the next run learns of an abnormal exit.
#[derive(Debug)]
pub struct Session {
    pub dir: PathBuf,
    _lock: File,
}

const SESSION_LOCK: &str = "session.lock";
const SESSION_RECORD: &str = "session.toml";

impl Session {
    pub fn start(paths: &Paths, machine: &str) -> Result<Self> {
        let unique =
            sha256_bytes(format!("{machine}-{:?}", std::time::SystemTime::now()).as_bytes());
        let dir =
            paths
                .sessions()
                .join(format!("{machine}-{}-{}", std::process::id(), &unique[..8]));
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create session directory {}", dir.display()))?;
        // Lock before any record exists: a sweeper skips recordless sessions,
        // and treats a recorded session as dead only if this lock is free.
        let lock = File::create(dir.join(SESSION_LOCK)).context("cannot create session lock")?;
        lock.try_lock().context("cannot lock new session")?;
        Ok(Self { dir, _lock: lock })
    }

    pub fn record(&self, record: &SessionRecord) -> Result<()> {
        write_toml(&self.dir.join(SESSION_RECORD), record)
    }

    /// Copy `source` into this session and make the copy read-only, so the
    /// emulator can only open it read-only and never holds the source at all.
    pub fn throwaway_copy(&self, paths: &Paths, source: &Path, index: usize) -> Result<PathBuf> {
        let _guard = lock_image(paths, source, LockMode::Shared)?;
        let name = source
            .file_name()
            .context("disk path has no file name")?
            .to_string_lossy();
        let copy = self.dir.join(format!("{index}-{name}"));
        atomic_copy(source, &copy)?;
        let mut perms = fs::metadata(&copy)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&copy, perms)?;
        Ok(copy)
    }

    /// Record a clean shutdown and discard any throwaway copies.
    pub fn finish(self) -> Result<()> {
        fs::remove_dir_all(&self.dir)
            .with_context(|| format!("cannot remove session {}", self.dir.display()))
    }
}

/// A recorded session whose owner is gone.
#[derive(Debug)]
pub struct StaleSession {
    pub dir: PathBuf,
    pub record: SessionRecord,
}

impl StaleSession {
    /// Drop the record and its throwaway copies. User disks live elsewhere
    /// and are untouched.
    pub fn clear(self) -> Result<()> {
        fs::remove_dir_all(&self.dir)
            .with_context(|| format!("cannot remove session {}", self.dir.display()))
    }
}

pub fn stale_sessions(paths: &Paths) -> Result<Vec<StaleSession>> {
    let mut stale = Vec::new();
    for name in list_dir(&paths.sessions())? {
        let dir = paths.sessions().join(name);
        let record_path = dir.join(SESSION_RECORD);
        if !record_path.exists() {
            continue;
        }
        let Ok(lock) = File::open(dir.join(SESSION_LOCK)) else {
            continue;
        };
        if lock.try_lock().is_ok() {
            stale.push(StaleSession {
                record: read_toml(&record_path)?,
                dir,
            });
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_image(signature: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 800 * 1024];
        bytes[1024..1026].copy_from_slice(signature);
        bytes
    }

    #[test]
    fn detects_supported_formats() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image");

        fs::write(&file, raw_image(b"BD")).unwrap();
        assert_eq!(detect_format(&file).unwrap(), DiskFormat::RawHfs);

        fs::write(&file, raw_image(&[0xD2, 0xD7])).unwrap();
        assert_eq!(detect_format(&file).unwrap(), DiskFormat::RawMfs);

        let mut dc42 = vec![0u8; 84 + 512];
        dc42[64..68].copy_from_slice(&512u32.to_be_bytes());
        dc42[82] = 1;
        fs::write(&file, dc42).unwrap();
        assert_eq!(detect_format(&file).unwrap(), DiskFormat::DiskCopy42);
    }

    #[test]
    fn rejects_unknown_formats() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image");
        // Blank media, HFS+, and a truncated volume.
        for bytes in [
            vec![0u8; 4096],
            raw_image(b"H+"),
            raw_image(b"BD")[..2000].to_vec(),
        ] {
            fs::write(&file, bytes).unwrap();
            let err = detect_format(&file).unwrap_err();
            assert!(err.to_string().contains("not a supported"), "{err}");
        }
    }

    #[test]
    fn locks_follow_the_file_through_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let image = dir.path().join("a.dsk");
        fs::write(&image, b"x").unwrap();
        let symlink = dir.path().join("sym.dsk");
        let hardlink = dir.path().join("hard.dsk");
        std::os::unix::fs::symlink(&image, &symlink).unwrap();
        fs::hard_link(&image, &hardlink).unwrap();

        let held = lock_image(&paths, &image, LockMode::Exclusive).unwrap();
        for alias in [&image, &symlink, &hardlink] {
            assert!(lock_image(&paths, alias, LockMode::Exclusive).is_err());
            assert!(lock_image(&paths, alias, LockMode::Shared).is_err());
        }
        drop(held);

        let reader = lock_image(&paths, &image, LockMode::Shared).unwrap();
        let _second_reader = lock_image(&paths, &symlink, LockMode::Shared).unwrap();
        assert!(lock_image(&paths, &image, LockMode::Exclusive).is_err());
        drop(reader);
    }

    #[test]
    fn sessions_only_go_stale_when_their_owner_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let record = SessionRecord {
            machine: "m".into(),
            pid: 1,
            writable_disks: vec![],
        };

        let live = Session::start(&paths, "m").unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty()); // no record yet
        live.record(&record).unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty()); // record, but locked

        drop(live); // an abnormal exit: no finish()
        let stale = stale_sessions(&paths).unwrap();
        assert_eq!(stale.len(), 1);
        stale.into_iter().next().unwrap().clear().unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty());

        let clean = Session::start(&paths, "m").unwrap();
        clean.record(&record).unwrap();
        clean.finish().unwrap();
        assert!(stale_sessions(&paths).unwrap().is_empty());
    }

    #[test]
    fn throwaway_copy_is_read_only_and_separate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let source = dir.path().join("app.dsk");
        fs::write(&source, b"data").unwrap();

        let session = Session::start(&paths, "m").unwrap();
        let copy = session.throwaway_copy(&paths, &source, 0).unwrap();
        assert_ne!(copy, source);
        assert!(fs::metadata(&copy).unwrap().permissions().readonly());
        assert!(!fs::metadata(&source).unwrap().permissions().readonly());
        session.finish().unwrap();
        assert!(!copy.exists() && source.exists());
    }
}
