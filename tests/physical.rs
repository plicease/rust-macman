//! `macman du --physical` and the unique physical usage behind it, on every
//! file system the host offers for it. They're in the temporary directory,
//! and any listed, colon-separated, in `MACMAN_TEST_FS_DIRS`, so CI can add
//! Btrfs and XFS mounts. Where a file system can't be measured, its tests
//! check that the figure is unavailable, with a reason, rather than guessed;
//! where reflinks or deduplication aren't supported, those checks are skipped.

mod common;

use common::{PLUS_ROM_CHECKSUM, write_disk, write_rom};
use macman::physical::unique_physical;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// A directory for each file system to test on.
fn roots() -> Vec<TempDir> {
    let mut bases = vec![std::env::temp_dir()];
    if let Ok(dirs) = std::env::var("MACMAN_TEST_FS_DIRS") {
        bases.extend(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    }
    bases
        .iter()
        .map(|base| TempDir::new_in(base).unwrap())
        .collect()
}

/// `size` bytes that no file system will compress or find duplicated.
fn noise(seed: u64, size: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Whether `dir` is on a file system whose physical usage can be measured.
fn measurable(dir: &Path) -> Result<(), String> {
    let probe = dir.join("probe");
    fs::write(&probe, noise(0, 4096)).unwrap();
    let result = unique_physical([probe.as_path()]).map(|_| ());
    fs::remove_file(&probe).unwrap();
    result
}

fn physical(files: &[&Path]) -> u64 {
    unique_physical(files.iter().copied()).unwrap()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    // _IOW(0x94, 9, int)
    const FICLONE: libc::c_ulong = 0x4004_9409;
    // _IOWR(0x94, 54, struct file_dedupe_range)
    const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;

    /// Make `to` a reflink of `from`, if the file system can.
    pub fn reflink(from: &Path, to: &Path) -> bool {
        let source = File::open(from).unwrap();
        let _ = std::fs::remove_file(to);
        let dest = File::create(to).unwrap();
        // SAFETY: both descriptors are open for the call.
        unsafe { libc::ioctl(dest.as_raw_fd(), FICLONE as _, source.as_raw_fd()) == 0 }
    }

    #[repr(C)]
    struct DedupeRange {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }

    // _IOW('f', 2, long), which takes an int all the same.
    const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
    const FS_COMPR_FL: libc::c_int = 0x0000_0004;

    /// Ask for `path`, still empty, to be compressed, if the file system can.
    pub fn compress(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        let mut flags = FS_COMPR_FL;
        // SAFETY: `flags` is the int the request reads, live for the call.
        unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS as _, &raw mut flags) == 0 }
    }

    /// Have the file system share `from`'s blocks with `to`, whose first
    /// `length` bytes are the same, if it can.
    pub fn dedupe(from: &Path, to: &Path, length: u64) -> bool {
        let source = File::open(from).unwrap();
        let dest = OpenOptions::new().read(true).write(true).open(to).unwrap();
        let mut range = DedupeRange {
            src_offset: 0,
            src_length: length,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            dest_fd: i64::from(dest.as_raw_fd()),
            dest_offset: 0,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        };
        // SAFETY: `range` is a one-destination request, live for the call.
        let result = unsafe {
            libc::ioctl(
                source.as_raw_fd(),
                FIDEDUPERANGE as _,
                std::ptr::from_mut(&mut range),
            )
        };
        result == 0 && range.status == 0 && range.bytes_deduped == length
    }
}

#[cfg(unix)]
#[test]
fn distinct_files_add_up_and_holes_take_nothing() {
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    for root in roots() {
        let dir = root.path();
        if let Err(why) = measurable(dir) {
            eprintln!("skipping {}: {why}", dir.display());
            continue;
        }
        let a = dir.join("a.dsk");
        let b = dir.join("b.dsk");
        fs::write(&a, noise(1, 256 * 1024)).unwrap();
        fs::write(&b, noise(2, 128 * 1024)).unwrap();
        let (one, two) = (physical(&[&a]), physical(&[&b]));
        assert!(one >= 256 * 1024 && two >= 128 * 1024, "{one} {two}");
        assert_eq!(physical(&[&a, &b]), one + two, "{}", dir.display());
        assert_eq!(physical(&[&a, &a]), one, "a file listed twice");

        // A 1 GiB file with 64 KiB written in the middle.
        let sparse = dir.join("sparse.dsk");
        let file = File::create(&sparse).unwrap();
        file.set_len(1 << 30).unwrap();
        file.write_all_at(&noise(3, 64 * 1024), 512 << 20).unwrap();
        drop(file);
        let used = physical(&[&sparse]);
        assert!(
            (64 * 1024..1 << 20).contains(&used),
            "{used} for 64K of data in {}",
            dir.display()
        );
    }
}

#[test]
fn hard_links_reflinks_and_deduplication_count_once() {
    for root in roots() {
        let dir = root.path();
        if let Err(why) = measurable(dir) {
            eprintln!("skipping {}: {why}", dir.display());
            continue;
        }
        let a = dir.join("a.dsk");
        fs::write(&a, noise(4, 512 * 1024)).unwrap();
        let alone = physical(&[&a]);

        let link = dir.join("link.dsk");
        fs::hard_link(&a, &link).unwrap();
        assert_eq!(physical(&[&a, &link]), alone, "{}", dir.display());

        #[cfg(target_os = "linux")]
        {
            let clone = dir.join("clone.dsk");
            if linux::reflink(&a, &clone) {
                assert_eq!(physical(&[&a, &clone]), alone, "{}", dir.display());
                // Rewriting part of the clone gives it blocks of its own.
                use std::os::unix::fs::FileExt;
                let file = fs::OpenOptions::new().write(true).open(&clone).unwrap();
                file.write_all_at(&noise(5, 4096), 128 * 1024).unwrap();
                drop(file);
                let both = physical(&[&a, &clone]);
                assert!(
                    both > alone && both < alone + (1 << 20),
                    "{both} after rewriting 4K of a {alone}-byte clone in {}",
                    dir.display()
                );
            } else {
                eprintln!("no reflinks in {}", dir.display());
            }

            let twin = dir.join("twin.dsk");
            fs::write(&twin, noise(4, 512 * 1024)).unwrap();
            assert_eq!(physical(&[&a, &twin]), 2 * alone, "{}", dir.display());
            if linux::dedupe(&a, &twin, 512 * 1024) {
                assert_eq!(physical(&[&a, &twin]), alone, "{}", dir.display());
            } else {
                eprintln!("no deduplication in {}", dir.display());
            }
        }
    }
}

/// Compressed extents' sizes on disk aren't reported, so a compressed file
/// leaves it unavailable rather than counted at its uncompressed length.
#[cfg(target_os = "linux")]
#[test]
fn a_compressed_file_leaves_it_unavailable() {
    for root in roots() {
        let dir = root.path();
        if measurable(dir).is_err() {
            continue;
        }
        let file = dir.join("packed.dsk");
        fs::write(&file, b"").unwrap();
        if !linux::compress(&file) {
            eprintln!("no compression in {}", dir.display());
            continue;
        }
        fs::write(&file, vec![0x5Au8; 1 << 20]).unwrap();
        match unique_physical([file.as_path()]) {
            Err(why) => assert!(why.contains("compressed"), "{why}"),
            // The file system may store it uncompressed after all.
            Ok(bytes) => assert!(bytes >= 1 << 20, "{bytes} in {}", dir.display()),
        }
    }
}

/// A reflink between file systems isn't possible, but one between Btrfs
/// subvolumes is, and their device numbers differ: it still counts once.
#[cfg(target_os = "linux")]
#[test]
fn a_reflink_between_directories_counts_once() {
    let roots = roots();
    for from in &roots {
        for to in &roots {
            if from.path() == to.path() || measurable(from.path()).is_err() {
                continue;
            }
            let a = from.path().join("shared.dsk");
            fs::write(&a, noise(6, 256 * 1024)).unwrap();
            let clone = to.path().join("shared-clone.dsk");
            if linux::reflink(&a, &clone) {
                eprintln!("reflinked {} to {}", a.display(), clone.display());
                assert_eq!(physical(&[&a, &clone]), physical(&[&a]));
            }
            let _ = fs::remove_file(&clone);
        }
    }
}

/// Run macman with its directories in `root`.
fn macman(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_macman"))
        .args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(root: &Path, args: &[&str]) -> String {
    let output = macman(root, args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn du(root: &Path) -> serde_json::Value {
    serde_json::from_str(&ok(root, &["--json", "du", "--physical"])).unwrap()
}

/// A ROM, a system, a medium, and a machine with a checkpoint, registered in
/// `root`, and the medium's and checkpoint's files.
fn registered(root: &Path) -> (PathBuf, PathBuf) {
    let rom = write_rom(root, "plus.bin", PLUS_ROM_CHECKSUM, 128 * 1024);
    let disk = write_disk(root, "sys.dsk");
    ok(
        root,
        &["rom", "add", rom.to_str().unwrap(), "--name", "plus-v3"],
    );
    ok(
        root,
        &["system", "add", disk.to_str().unwrap(), "--name", "sys"],
    );
    ok(
        root,
        &["media", "add", disk.to_str().unwrap(), "--name", "tools"],
    );
    ok(
        root,
        &[
            "create", "m", "--model", "plus", "--rom", "plus-v3", "--system", "sys",
        ],
    );
    ok(root, &["checkpoint", "create", "m", "--name", "kept"]);
    let data = root.join("data/macman");
    (
        data.join("media/tools.img"),
        data.join("machines/m/checkpoints/kept.dsk"),
    )
}

#[test]
fn du_counts_storage_shared_across_entries_once() {
    for root in roots() {
        let root = root.path();
        let (medium, checkpoint) = registered(root);
        let data = root.join("data/macman");
        let system = data.join("systems/sys.dsk");
        let disk = data.join("machines/m/system.dsk");

        // Without --physical, nothing changes.
        let plain: serde_json::Value = serde_json::from_str(&ok(root, &["--json", "du"])).unwrap();
        assert!(plain.get("unique_physical_bytes").is_none(), "{plain}");
        assert!(plain.get("unique_physical_unavailable").is_none());
        let text = ok(root, &["du"]);
        assert!(!text.contains("physical"), "{text}");

        let json = du(root);
        assert!(json["total_host_disk_used_bytes"].is_u64(), "{json}");
        if let Err(why) = measurable(root) {
            assert!(json["unique_physical_bytes"].is_null(), "{json}");
            let reason = json["unique_physical_unavailable"].as_str().unwrap();
            assert!(!reason.is_empty(), "{json}");
            let output = macman(root, &["du", "--physical"]);
            let text = String::from_utf8(output.stdout).unwrap();
            let last: Vec<_> = text.lines().last().unwrap().split_whitespace().collect();
            assert_eq!(last, ["physical", "-", "-"], "{text}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                stderr.contains("unique physical usage is unavailable"),
                "{stderr}"
            );
            eprintln!("unavailable in {}: {why}", root.display());
            continue;
        }
        assert!(json["unique_physical_unavailable"].is_null(), "{json}");
        let rom = data.join("roms/plus-v3.rom");
        let all = [&rom, &system, &medium, &disk, &checkpoint].map(PathBuf::as_path);
        assert_eq!(json["unique_physical_bytes"], physical(&all), "{json}");

        // Give the medium and checkpoint blocks of their own, as copying
        // may have shared them already.
        for file in [&medium, &checkpoint] {
            let bytes = fs::read(file).unwrap();
            fs::remove_file(file).unwrap();
            fs::write(file, bytes).unwrap();
        }
        let before = du(root)["unique_physical_bytes"].as_u64().unwrap();
        let own = physical(&[&medium]) + physical(&[&checkpoint]);

        #[cfg(target_os = "linux")]
        {
            // Reflinked to what they're copies of: a checkpoint of its
            // machine's disk, and a medium of a system, so across entries.
            if !linux::reflink(&disk, &checkpoint) || !linux::reflink(&system, &medium) {
                eprintln!("no reflinks in {}", root.display());
                continue;
            }
            let json = du(root);
            assert_eq!(
                json["unique_physical_bytes"],
                before - own,
                "{} {json}",
                root.display()
            );
            // The allocated-file total still counts each file in full.
            assert!(
                json["total_host_disk_used_bytes"].as_u64().unwrap()
                    > json["unique_physical_bytes"].as_u64().unwrap(),
                "{json}"
            );
            let text = ok(root, &["du", "--physical"]);
            let last = text.lines().last().unwrap();
            assert!(
                last.starts_with("physical") && !last.ends_with(" -"),
                "{text}"
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (before, own);
    }
}

#[test]
fn a_missing_file_leaves_it_unavailable() {
    for root in roots() {
        let root = root.path();
        let (medium, _) = registered(root);
        fs::remove_file(&medium).unwrap();
        let json = du(root);
        assert!(json["unique_physical_bytes"].is_null(), "{json}");
        assert!(json["total_host_disk_used_bytes"].is_null(), "{json}");
        let reason = json["unique_physical_unavailable"].as_str().unwrap();
        // Where it's measurable, the file is the reason.
        if measurable(root).is_ok() {
            assert!(reason.contains("tools.img"), "{reason}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn other_platforms_say_it_isnt_implemented() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("a.dsk");
    fs::write(&file, b"data").unwrap();
    let why = unique_physical([file.as_path()]).unwrap_err();
    assert!(why.contains("isn't implemented"), "{why}");
}
