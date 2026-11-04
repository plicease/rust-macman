//! `macman du --physical` and the unique physical usage behind it, on every
//! file system the host offers for it. They're in the temporary directory,
//! and any listed in `MACMAN_TEST_FS_DIRS`, separated as in `PATH`, so CI can
//! add Btrfs and XFS mounts, and a ReFS volume on Windows. Where a file system
//! can't be measured, its tests check that the figure is unavailable, with a
//! reason, rather than guessed; where reflinks, deduplication, or compression
//! aren't supported, those checks are skipped.

mod common;

use common::{PLUS_ROM_CHECKSUM, write_disk, write_rom};
use macman::physical::unique_physical;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

#[cfg(target_os = "linux")]
use linux::reflink;
#[cfg(windows)]
use windows::reflink;

/// A directory for each file system to test on.
fn roots() -> Vec<TempDir> {
    let mut bases = vec![std::env::temp_dir()];
    if let Some(dirs) = std::env::var_os("MACMAN_TEST_FS_DIRS") {
        bases.extend(std::env::split_paths(&dirs).filter(|d| !d.as_os_str().is_empty()));
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

/// CI names in `MACMAN_TEST_MEASURABLE_DIRS` the file systems it set up to be
/// measured, so a change that leaves one unavailable fails rather than having
/// every test skip it.
#[test]
fn file_systems_meant_to_be_measurable_are() {
    let Some(dirs) = std::env::var_os("MACMAN_TEST_MEASURABLE_DIRS") else {
        return;
    };
    for base in std::env::split_paths(&dirs).filter(|d| !d.as_os_str().is_empty()) {
        let dir = TempDir::new_in(&base).unwrap();
        if let Err(why) = measurable(dir.path()) {
            panic!("{} isn't measurable: {why}", base.display());
        }
    }
}

fn physical(files: &[&Path]) -> u64 {
    unique_physical(files.iter().copied()).unwrap()
}

/// Overwrite part of `path` with `data` at `offset`.
fn write_at(path: &Path, data: &[u8], offset: u64) {
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(data).unwrap();
}

/// A new file at `path` whose unwritten parts, on Windows too, are holes.
fn sparse_file(path: &Path) -> fs::File {
    let file = fs::File::create(path).unwrap();
    #[cfg(windows)]
    assert!(windows::set_sparse(&file), "{}", path.display());
    file
}

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_COMPRESSED, GetDiskFreeSpaceW};
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_EXTERNAL_BACKING,
        FSCTL_SET_SPARSE,
    };

    /// Send `code` to `file`, with `input` if any, and whether it worked.
    fn control<T>(file: &File, code: u32, input: Option<&T>) -> bool {
        let (pointer, size) = match input {
            Some(input) => (std::ptr::from_ref(input).cast(), size_of::<T>() as u32),
            None => (std::ptr::null(), 0),
        };
        let mut returned = 0u32;
        // SAFETY: the handle is open and the input, if any, is the size
        // given, both live for the call, which has no output.
        unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                pointer,
                size,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        }
    }

    pub fn set_sparse(file: &File) -> bool {
        control::<()>(file, FSCTL_SET_SPARSE, None)
    }

    /// The cluster size of the volume `path` is on.
    fn cluster_size(path: &Path) -> u64 {
        let root = path.ancestors().last().unwrap();
        let root: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
        let (mut sectors, mut bytes) = (0u32, 0u32);
        // SAFETY: `root` is terminated and the counts are writable.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                root.as_ptr(),
                &mut sectors,
                &mut bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "{}", path.display());
        u64::from(sectors) * u64::from(bytes)
    }

    /// Make `to` a block clone of `from`, if the file system can, as ReFS can.
    pub fn reflink(from: &Path, to: &Path) -> bool {
        let source = File::open(from).unwrap();
        let length = source.metadata().unwrap().len();
        let _ = std::fs::remove_file(to);
        let dest = File::create(to).unwrap();
        // Cloning is of whole clusters, so the last one is taken whole and
        // the copy cut back to length after.
        let cluster = cluster_size(from);
        let whole = length.div_ceil(cluster) * cluster;
        dest.set_len(whole).unwrap();
        let request = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle(),
            SourceFileOffset: 0,
            TargetFileOffset: 0,
            ByteCount: whole as i64,
        };
        let cloned = control(&dest, FSCTL_DUPLICATE_EXTENTS_TO_FILE, Some(&request));
        dest.set_len(length).unwrap();
        cloned
    }

    /// Run `compact /c` on `path` with `options`.
    fn compact(path: &Path, options: &[&str]) {
        let _ = Command::new("compact")
            .arg("/c")
            .args(options)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// Compress `path` as NTFS does, if the file system can.
    pub fn compress(path: &Path) -> bool {
        compact(path, &[]);
        std::fs::metadata(path).unwrap().file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0
    }

    /// Compress `path` as `compact /exe` does, into a stream the Windows
    /// Overlay Filter keeps, if it can.
    pub fn compact_exe(path: &Path) -> bool {
        compact(path, &["/exe:xpress4k"]);
        overlaid(path)
    }

    /// Whether the overlay filter keeps `path`'s data, which hides the reparse
    /// point that says so from its attributes.
    pub fn overlaid(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        let mut info = [0u8; 64];
        let mut returned = 0u32;
        // SAFETY: the handle is open, there's no input, and the output is
        // the size given.
        unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_EXTERNAL_BACKING,
                std::ptr::null(),
                0,
                info.as_mut_ptr().cast(),
                info.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        }
    }
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

#[test]
fn distinct_files_add_up_and_holes_take_nothing() {
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
        sparse_file(&sparse).set_len(1 << 30).unwrap();
        write_at(&sparse, &noise(3, 64 * 1024), 512 << 20);
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

        #[cfg(any(target_os = "linux", windows))]
        {
            let clone = dir.join("clone.dsk");
            if reflink(&a, &clone) {
                assert_eq!(physical(&[&a, &clone]), alone, "{}", dir.display());
                // Rewriting part of the clone gives it blocks of its own.
                write_at(&clone, &noise(5, 4096), 128 * 1024);
                let both = physical(&[&a, &clone]);
                assert!(
                    both > alone && both < alone + (1 << 20),
                    "{both} after rewriting 4K of a {alone}-byte clone in {}",
                    dir.display()
                );
            } else {
                eprintln!("no reflinks in {}", dir.display());
            }
        }

        #[cfg(target_os = "linux")]
        {
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

/// `path`, filled with data that compresses well, compressed by the file
/// system, if it can.
#[cfg(any(target_os = "linux", windows))]
fn compressed(path: &Path) -> bool {
    let data = vec![0x5Au8; 1 << 20];
    #[cfg(target_os = "linux")]
    {
        // Btrfs compresses what's written after the flag is set.
        fs::write(path, b"").unwrap();
        let set = linux::compress(path);
        fs::write(path, data).unwrap();
        set
    }
    #[cfg(windows)]
    {
        fs::write(path, data).unwrap();
        windows::compress(path)
    }
}

/// Compressed extents' sizes on disk aren't reported, so a compressed file
/// leaves it unavailable rather than counted at its uncompressed length.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_compressed_file_leaves_it_unavailable() {
    for root in roots() {
        let dir = root.path();
        if measurable(dir).is_err() {
            continue;
        }
        let file = dir.join("packed.dsk");
        if !compressed(&file) {
            eprintln!("no compression in {}", dir.display());
            continue;
        }
        match unique_physical([file.as_path()]) {
            Err(why) => assert!(why.contains("compressed"), "{why}"),
            // The file system may store it uncompressed after all.
            Ok(bytes) => assert!(bytes >= 1 << 20, "{bytes} in {}", dir.display()),
        }
    }
}

/// An empty file takes nothing, and a small one either its blocks or, where
/// it's kept in its metadata, as on NTFS or Btrfs, leaves it unavailable;
/// it's never counted as taking nothing.
#[test]
fn small_files_are_counted_or_unavailable_and_empty_ones_take_nothing() {
    for root in roots() {
        let dir = root.path();
        if measurable(dir).is_err() {
            continue;
        }
        let empty = dir.join("empty.dsk");
        fs::write(&empty, b"").unwrap();
        assert_eq!(physical(&[&empty]), 0, "{}", dir.display());

        let small = dir.join("small.dsk");
        fs::write(&small, noise(7, 100)).unwrap();
        match unique_physical([small.as_path()]) {
            Ok(bytes) => assert!(bytes >= 100, "{bytes} in {}", dir.display()),
            Err(why) => assert!(why.contains("alongside metadata"), "{why}"),
        }
    }
}

/// `compact /exe` moves a file's data into a stream the overlay filter keeps,
/// with no clusters of the file's own, which leaves it unavailable rather
/// than counted as taking nothing, or as its uncompressed size. Measuring
/// doesn't open it to write, which would decompress it.
#[cfg(windows)]
#[test]
fn a_compacted_file_leaves_it_unavailable_and_compacted() {
    for root in roots() {
        let dir = root.path();
        if measurable(dir).is_err() {
            continue;
        }
        let file = dir.join("compact.dsk");
        fs::write(&file, vec![0x5Au8; 1 << 20]).unwrap();
        if !windows::compact_exe(&file) {
            eprintln!("no compact /exe in {}", dir.display());
            continue;
        }
        let why = unique_physical([file.as_path()]).unwrap_err();
        assert!(why.contains("compressed"), "{why}");
        assert!(windows::overlaid(&file), "{}", file.display());
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
            if reflink(&a, &clone) {
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

/// Whether macman can keep its data in `root`: it needs hard links, which
/// FAT and exFAT lack.
fn holds_data(root: &Path) -> bool {
    let probe = root.join("link-probe");
    fs::write(&probe, b"probe").unwrap();
    let linked = fs::hard_link(&probe, root.join("link-probe-2")).is_ok();
    let _ = fs::remove_file(root.join("link-probe-2"));
    fs::remove_file(&probe).unwrap();
    if !linked {
        eprintln!("macman can't keep its data in {}", root.display());
    }
    linked
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
        if !holds_data(root) {
            continue;
        }
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

        #[cfg(any(target_os = "linux", windows))]
        {
            // Reflinked to what they're copies of: a checkpoint of its
            // machine's disk, and a medium of a system, so across entries.
            if !reflink(&disk, &checkpoint) || !reflink(&system, &medium) {
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
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = (before, own);
    }
}

#[test]
fn a_missing_file_leaves_it_unavailable() {
    for root in roots() {
        let root = root.path();
        if !holds_data(root) {
            continue;
        }
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

#[cfg(not(any(target_os = "linux", windows)))]
#[test]
fn other_platforms_say_it_isnt_implemented() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("a.dsk");
    fs::write(&file, b"data").unwrap();
    let why = unique_physical([file.as_path()]).unwrap_err();
    assert!(why.contains("isn't implemented"), "{why}");
}
