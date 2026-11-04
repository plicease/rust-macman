//! What Basilisk II and SheepShaver have in common. Both come from one macemu
//! source tree and share its Unix startup, disk code, and SDL front end, so
//! they're built, launched, and watched the same way. What they emulate isn't
//! shared: CPUs, ROMs, and guests stay in their own backends.

use super::minivmac::{MANIFEST, Manifest};
use super::source::{SourcePin, tool_exists};
use super::{CdImage, Check, ResolvedDisk};
use crate::disk::CdVolume;
use crate::fsutil::{sha256_file, write_toml};
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

/// How `ErrorAlert` reports without a GUI (`STR_SHELL_ERROR_PREFIX`).
const ERROR_PREFIX: &str = "ERROR: ";
/// SDL for video and sound, and no GTK, whose dialogs would wait for a click
/// in place of the `ERROR:` lines macman reads. Cue sheets are for CD images
/// with no HFS volume: see [`cd_paths`].
pub const CONFIGURE_OPTIONS: &[&str] = &[
    "--enable-sdl-video",
    "--enable-sdl-audio",
    "--with-gtk=no",
    "--with-esd=no",
    "--with-bincue",
];

/// A commit, as macemu's releases are rare. The archive is one GitHub generates
/// from it, and the fork is the one that's maintained. It holds both emulators,
/// so a host that has built one needn't download it again for the other.
pub fn pin() -> SourcePin {
    const COMMIT: &str = "f532be3d3d70b70f272b52050ce0889a4025d624";
    SourcePin {
        revision: COMMIT[..12].into(),
        url: format!("https://github.com/kanjitalk755/macemu/archive/{COMMIT}.tar.gz"),
        sha256: "dc987af49d9664c3c0453ae6480b8c027d064b8881d39dd7b701e35d1399687e".into(),
    }
}

/// One emulator from the tree, and what to record about building it.
pub struct Build<'a> {
    pub paths: &'a Paths,
    pub backend: &'a str,
    pub label: &'a str,
    pub revision: &'a str,
    pub host: &'a str,
    pub toolchain: &'a str,
    pub options: &'a [&'a str],
    /// Where the tree's build for this host happens.
    pub directory: &'a str,
    pub executable: &'a str,
}

impl Build<'_> {
    /// Run `compile`, given the directory to unpack into and the log, and
    /// publish what it made under `key`.
    pub fn run(
        &self,
        key: &str,
        verbose: bool,
        compile: impl FnOnce(&Path, &Path) -> Result<()>,
    ) -> Result<()> {
        let (label, executable) = (self.label, self.executable);
        let work = self
            .paths
            .build_work()
            .join(format!("{key}-{}", std::process::id()));
        // Whatever a prior crash left behind is unusable.
        let _ = fs::remove_dir_all(&work);
        let src = work.join("src");
        fs::create_dir_all(&src)
            .with_context(|| format!("cannot create build directory {}", src.display()))?;
        fs::create_dir_all(self.paths.build_logs())?;
        let log_path = self.paths.build_logs().join(format!("{key}.log"));

        eprintln!("Building {label} {}...", self.revision);
        let result = compile(&src, &log_path);
        if verbose && let Ok(log) = fs::read_to_string(&log_path) {
            eprint!("{log}");
        }
        let built = src.join(self.directory).join(executable);
        let result = result.and_then(|()| {
            if built.is_file() {
                Ok(())
            } else {
                bail!("the build produced no executable")
            }
        });
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&work);
            return Err(error.context(format!(
                "{label} build failed; see {} (rerun with --verbose to print it), and run \
                 `macman doctor`",
                log_path.display()
            )));
        }

        // Publish by renaming a complete directory into place.
        let publish = work.join("publish");
        fs::create_dir_all(&publish)?;
        fs::rename(&built, publish.join(executable))?;
        fs::copy(&log_path, publish.join("build.log"))?;
        write_toml(
            &publish.join(MANIFEST),
            &Manifest {
                backend: self.backend.into(),
                revision: self.revision.into(),
                host: self.host.into(),
                toolchain: self.toolchain.into(),
                options: self.options.iter().map(|s| s.to_string()).collect(),
                executable: executable.into(),
                executable_sha256: sha256_file(&publish.join(executable))?,
                bundle: None,
                bundle_sha256: None,
            },
        )?;
        let dest = self.paths.builds().join(key);
        fs::create_dir_all(self.paths.builds())?;
        // A damaged earlier build would block the rename.
        let _ = fs::remove_dir_all(&dest);
        fs::rename(&publish, &dest).context("cannot publish build")?;
        let _ = fs::remove_dir_all(&work);
        Ok(())
    }
}

/// Unpack and compile, per upstream's README, with `configure` separate from
/// `autogen.sh` so that it gets macman's options. `command` is the `bash` to
/// use, with whatever environment this host wants.
pub fn run_configure_build(
    mut command: Command,
    archive: &Path,
    src: &Path,
    log_path: &Path,
    directory: &str,
    options: &[&str],
) -> Result<()> {
    let log = File::create(log_path).context("cannot create build log")?;
    let jobs = thread::available_parallelism().map_or(1, |jobs| jobs.get());
    let script = format!(
        "set -e\n\
         tar xzf \"$1\" --strip-components=1\n\
         cd {directory}\n\
         NO_CONFIGURE=1 sh ./autogen.sh\n\
         sh ./configure {}\n\
         make -j{jobs}\n",
        options.join(" ")
    );
    let status = command
        .arg("-c")
        .arg(script)
        .arg("macman-build")
        .arg(archive)
        .current_dir(src)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .context("cannot run bash")?;
    if !status.success() {
        bail!("build script exited with {status}");
    }
    Ok(())
}

/// The first line of a command's output, if it runs and succeeds.
pub fn first_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

/// `g++` compiles most of it, and `gcc` the rest. SDL is linked dynamically,
/// but a new version may change what `configure` finds.
pub fn linux_toolchain_identity() -> Result<String> {
    let compiler = first_line("g++", &["--version"])
        .context("install a C++ compiler (run `macman doctor`)")?;
    let sdl = first_line("pkg-config", &["--modversion", "sdl2"]);
    Ok(format!("{compiler} SDL {}", sdl.as_deref().unwrap_or("?")))
}

/// Both emulators build on ARM64 macOS as on Linux, with `configure`, rather
/// than with upstream's Xcode projects, which want their libraries installed
/// system-wide. Neither has a JIT there.
pub const MACOS: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

/// What `configure` builds with on macOS that Apple's tools don't provide.
/// Mini vMac's `doctor` checks already cover those.
pub const MACOS_TOOLS: &[&str] = &["autoconf", "aclocal", "pkg-config"];

/// What the compiler needs to be told to find `libraries` on macOS, by their
/// `pkg-config` names. MacPorts and Homebrew put them where Apple's clang
/// doesn't look, and `configure` doesn't ask.
pub fn macos_library_flags(libraries: &[(&str, &str)], option: &str) -> String {
    let packages = libraries.iter().map(|(package, _)| *package);
    let args: Vec<&str> = std::iter::once(option).chain(packages).collect();
    first_line("pkg-config", &args).unwrap_or_default()
}

/// The `bash` for [`run_configure_build`] on macOS. `gcc` there may be a real
/// GCC from a package manager, not Apple's clang.
pub fn macos_build_command(libraries: &[(&str, &str)]) -> Command {
    let mut command = Command::new("bash");
    command
        .env("CC", "clang")
        .env("CXX", "clang++")
        .env(
            "CPPFLAGS",
            macos_library_flags(libraries, "--cflags-only-I"),
        )
        .env("LDFLAGS", macos_library_flags(libraries, "--libs-only-L"));
    command
}

/// A build links `libraries` dynamically, so it's only good for the versions
/// it saw, where it found them.
pub fn macos_toolchain_identity(libraries: &[(&str, &str)]) -> Result<String> {
    let versions = libraries
        .iter()
        .map(|(package, name)| {
            let version = first_line("pkg-config", &["--modversion", package]);
            format!("{name} {}", version.as_deref().unwrap_or("?"))
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "{} {versions} {}",
        super::minivmac::macos_toolchain_identity()?,
        macos_library_flags(libraries, "--libs-only-L"),
    ))
}

/// Refuse a disk that an emulator holds, which macman's own locks can't show
/// once the macman that started it has gone. On macOS, `Sys_open` takes
/// `O_EXLOCK` on every disk file, and a second emulator would only print a
/// warning that stays in its buffer, and show no disk. A file system without
/// locks can't be held, and the emulator goes on without them there too.
pub fn refuse_held_disk(disk: &Path, label: &str) -> Result<()> {
    let file = File::open(disk).with_context(|| format!("cannot open {}", disk.display()))?;
    if let Err(std::fs::TryLockError::WouldBlock) = file.try_lock() {
        bail!(
            "{} is held by a {label} that outlived macman; quit that emulator first",
            disk.display()
        );
    }
    Ok(())
}

pub fn megabytes(memory: &str) -> Option<u32> {
    memory.strip_suffix('M')?.parse().ok()
}

/// Whether the guest is to start from a CD. A disc with no HFS volume can't
/// start it, and once Mac OS has tried, it leaves that drive unmounted.
pub fn starts_from_cd(cdroms: &[CdImage]) -> bool {
    cdroms.iter().any(|cdrom| cdrom.volume == CdVolume::Hfs)
}

/// What each CD is given to the emulator as. Discs with HFS go first, as the
/// first drive is the one the guest starts from, otherwise their order is kept.
///
/// The emulator asks a CD image for its table of contents as it would a drive,
/// with an `ioctl` that fails on a file, so the guest sees a disc of one audio
/// track. An HFS volume is found by reading blocks, so an HFS disc mounts all
/// the same, and is passed as it is. ISO 9660 File Access goes by the table,
/// though, so an ISO 9660 disc gets a cue sheet that makes it one data track.
/// The emulator joins the sheet's file name to the sheet's own directory, so
/// the image is linked in beside it, under a name that needs no quoting.
pub fn cd_paths(workdir: &Path, cdroms: &[CdImage]) -> Result<Vec<PathBuf>> {
    let mut cdroms = cdroms.to_vec();
    cdroms.sort_by_key(|cdrom| cdrom.volume != CdVolume::Hfs);
    let mut paths = Vec::new();
    for (index, cdrom) in cdroms.iter().enumerate() {
        // HFS+ is found by reading blocks too, by a guest that reads it at all.
        if cdrom.volume != CdVolume::Iso9660 {
            paths.push(cdrom.path.clone());
            continue;
        }
        let image = format!("cd-{}.iso", index + 1);
        link(&cdrom.path, &workdir.join(&image))?;
        let sheet = workdir.join(format!("cd-{}.cue", index + 1));
        fs::write(&sheet, cue_sheet(&image))
            .with_context(|| format!("cannot write {}", sheet.display()))?;
        paths.push(sheet);
    }
    Ok(paths)
}

/// One data track of 2048-byte sectors. The first line has to be `FILE`.
fn cue_sheet(image: &str) -> String {
    format!("FILE \"{image}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n")
}

#[cfg(unix)]
fn link(original: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(original, link)
        .with_context(|| format!("cannot link {} into the session", original.display()))
}

/// Only Linux hosts take CDs.
#[cfg(not(unix))]
fn link(original: &Path, _link: &Path) -> Result<()> {
    bail!(
        "{} can't be attached as a CD on this host",
        original.display()
    )
}

/// A disk's `--disk` value. A `*` asks for read-only, on top of the throwaway
/// copy already being a read-only file.
pub fn disk_argument(disk: &ResolvedDisk) -> OsString {
    let mut value = OsString::from(if disk.writable { "" } else { "*" });
    value.push(&disk.path);
    value
}

/// A `doctor` line for each of `tools`.
pub fn tool_checks(tools: &[&str], label: &str, install: &str) -> Vec<Check> {
    tools
        .iter()
        .map(|tool| {
            if tool_exists(tool) {
                Check::ok(format!("{tool} found, for {label}"))
            } else {
                Check::fail(format!("{tool} not found, which {label} needs"), install)
            }
        })
        .collect()
}

/// A `doctor` line for each of `libraries`, by `pkg-config` name, and for people.
pub fn library_checks(libraries: &[(&str, &str)], label: &str, install: &str) -> Vec<Check> {
    libraries
        .iter()
        .map(
            |(package, name)| match first_line("pkg-config", &["--modversion", package]) {
                Some(version) => Check::ok(format!("{name} {version} found")),
                None => Check::fail(
                    format!("{name} headers not found, which {label} needs"),
                    install,
                ),
            },
        )
        .collect()
}

/// The `doctor` warning for a session with nowhere to open a window, if any.
pub fn display_check(label: &str) -> Option<Check> {
    let missing =
        std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none();
    missing.then(|| {
        Check::warn(
            "neither DISPLAY nor WAYLAND_DISPLAY is set",
            format!("{label} opens a window; run macman from a graphical session"),
        )
    })
}

/// Have the kernel stop the emulator if macman dies, because the locks on its
/// disks are about to go too. It has to be `SIGKILL`: SDL turns `SIGTERM` into
/// the window closing, which only asks the guest whether to shut down.
#[cfg(target_os = "linux")]
pub fn die_with_macman(command: &mut Command) {
    die_with_parent(command, std::process::id());
}

/// The signal is only armed once `prctl` runs, and a parent that dies sooner
/// never sends it. So the child compares its parent with `parent`, as noted
/// before spawning, and goes no further if macman has already gone.
#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut Command, parent: u32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `prctl` and `getppid` are async-signal-safe, and nothing else
    // runs in the child.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn die_with_macman(_command: &mut Command) {}

/// Read the emulator's output until it closes, returning its error lines.
fn watch_output(child: &mut Child, verbose: bool) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let mut output = BufReader::new(child.stdout.take().context("no emulator output")?);
    let mut line = Vec::new();
    while output.read_until(b'\n', &mut line)? != 0 {
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if verbose {
            println!("{text}");
        }
        if let Some(error) = text.strip_prefix(ERROR_PREFIX) {
            errors.push(error.to_string());
        }
        line.clear();
    }
    Ok(errors)
}

/// Start the emulator and wait for it. Its quit path exits 0 even after refusing
/// to start, so an `ERROR:` line with a clean exit is a failure.
pub fn supervise(mut command: Command, label: &str, verbose: bool) -> Result<ExitStatus> {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    die_with_macman(&mut command);
    if verbose {
        eprintln!("Launching {command:?}");
    }
    let program = command.get_program().to_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {}", program.to_string_lossy()))?;
    let errors = watch_output(&mut child, verbose);
    let status = child.wait().context("cannot wait for the emulator")?;
    match errors?.as_slice() {
        [] => Ok(status),
        errors if status.success() => {
            bail!("{label} refused to run: {}", errors.join("; "))
        }
        errors => {
            eprintln!("{label} reported: {}", errors.join("; "));
            Ok(status)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn an_iso_9660_disc_is_given_a_data_track_and_hfs_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session");
        fs::create_dir(&session).unwrap();
        let iso = dir.path().join("my \"disc\".iso");
        fs::write(&iso, vec![0u8; 4096]).unwrap();
        let cdroms = [
            CdImage {
                path: iso.clone(),
                volume: CdVolume::Iso9660,
            },
            CdImage {
                path: "/installer.toast".into(),
                volume: CdVolume::Hfs,
            },
            CdImage {
                path: "/tiger.img".into(),
                volume: CdVolume::HfsPlus,
            },
        ];
        let paths = cd_paths(&session, &cdroms).unwrap();
        // HFS+ is read by blocks too, so it's left alone, but doesn't start the Mac.
        assert_eq!(
            paths,
            [
                PathBuf::from("/installer.toast"),
                session.join("cd-2.cue"),
                PathBuf::from("/tiger.img")
            ]
        );
        assert!(!starts_from_cd(&cdroms[2..]));
        assert_eq!(
            fs::read_to_string(&paths[1]).unwrap(),
            "FILE \"cd-2.iso\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n"
        );
        assert_eq!(fs::read_link(session.join("cd-2.iso")).unwrap(), iso);
    }

    /// The signal goes out when the spawning thread ends, which is what macman
    /// dying looks like from here, without killing the tests.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_emulator_does_not_outlive_macman() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = thread::spawn(|| {
            let mut command = Command::new("sleep");
            command.arg("60");
            die_with_macman(&mut command);
            command.spawn().unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// The race itself can't be staged, but its outcome can: the child sees a
    /// parent other than the one noted, as it would if macman had just died.
    #[test]
    #[cfg(target_os = "linux")]
    fn an_emulator_whose_parent_has_already_gone_never_starts() {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let mut command = Command::new("touch");
        command.arg(&ran);
        die_with_parent(&mut command, std::process::id() + 1);

        let error = command.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(!ran.exists(), "it stopped before exec");
    }

    #[test]
    fn the_pin_names_a_whole_commit() {
        let pin = pin();
        assert_eq!(pin.revision.len(), 12);
        assert!(pin.url.contains(&pin.revision));
        assert_eq!(pin.sha256.len(), 64);
    }

    #[test]
    fn read_only_disks_are_marked() {
        let disk = |writable| ResolvedDisk {
            path: "/a disk.dsk".into(),
            writable,
        };
        assert_eq!(disk_argument(&disk(true)), "/a disk.dsk");
        assert_eq!(disk_argument(&disk(false)), "*/a disk.dsk");
    }
}
