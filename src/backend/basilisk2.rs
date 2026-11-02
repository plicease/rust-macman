//! Basilisk II adapter, for later 68k software. Everything here comes from the
//! source of the pinned macemu commit (`BasiliskII/src`): `prefs_items.cpp`,
//! `Unix/main_unix.cpp`, `Unix/sys_unix.cpp`, and `SDL/video_sdl2.cpp`.
//!
//! Unlike Mini vMac, Basilisk II settles everything at run time, from a
//! preferences file. So one build serves every machine on a host, and the build
//! key holds nothing of the machine's. Each run writes its own file into its
//! session directory, and `--config` keeps the emulator away from the user's
//! `~/.basilisk_ii_prefs`, and its XPRAM in that directory too. The file's lines
//! are cut at 256 bytes, so paths go on the command line, which has no such
//! limit and adds to or replaces what the file says.
//!
//! Three things differ from what the rest of macman assumes:
//!
//! - `QuitEmulator` always exits 0, even after refusing a ROM. Without GTK those
//!   refusals are printed as `ERROR: ...`, so the output has to be watched.
//! - It takes no locks on Linux. macman's lock on a disk lasts only as long as
//!   macman, so the emulator is tied to macman's life: if macman dies, it does
//!   too, rather than go on writing to a disk that looks free.
//! - A Disk Copy image is written straight through, leaving its checksum wrong.

use super::minivmac::{MANIFEST, Manifest};
use super::source::{self, Source, SourcePin, tool_exists};
use super::{Check, EmulatorBackend, Executable, LaunchRequest};
use crate::disk::{self, Classification, DiskFormat};
use crate::fsutil::{sha256_bytes, sha256_file, write_toml};
use crate::machine::{Basilisk2Settings, Cpu, Display, Machine, Network};
use crate::paths::Paths;
use crate::registry::{self, Rom};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

pub const BACKEND_NAME: &str = "basilisk2";
pub const DEFAULT_MEMORY: &str = "32M";
pub const DEFAULT_DISPLAY: (u32, u32, u32) = (640, 480, 8);
const LABEL: &str = "Basilisk II";
const EXECUTABLE: &str = "BasiliskII";
const BUILD_DIRECTORY: &str = "BasiliskII/src/Unix";
const PREFERENCES: &str = "prefs";
/// How `ErrorAlert` reports without a GUI (`STR_SHELL_ERROR_PREFIX`).
const ERROR_PREFIX: &str = "ERROR: ";
/// `modelid` is the Gestalt ID less 6. Mac OS 8 needs a 68040 model, and the
/// Quadra 900 is upstream's; older systems expect the IIci that's its default.
const MODEL_ID_IICI: u32 = 5;
const MODEL_ID_QUADRA_900: u32 = 14;
const HOST_SUPPORTED: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));
const UNSUPPORTED_HOST: &str = "the Basilisk II backend supports only Linux x86-64 hosts; see \
                                docs/PLATFORMS.md";
/// SDL for video and sound, and no GTK, whose dialogs would wait for a click
/// in place of the `ERROR:` lines macman reads.
const CONFIGURE_OPTIONS: &[&str] = &[
    "--enable-sdl-video",
    "--enable-sdl-audio",
    "--with-gtk=no",
    "--with-esd=no",
];

/// A commit, as macemu's releases are rare. The archive is one GitHub generates
/// from it, and the fork is the one that's maintained.
pub fn pin() -> SourcePin {
    const COMMIT: &str = "f532be3d3d70b70f272b52050ce0889a4025d624";
    SourcePin {
        revision: COMMIT[..12].into(),
        url: format!("https://github.com/kanjitalk755/macemu/archive/{COMMIT}.tar.gz"),
        sha256: "dc987af49d9664c3c0453ae6480b8c027d064b8881d39dd7b701e35d1399687e".into(),
    }
}

pub struct Basilisk2 {
    paths: Paths,
    pin: SourcePin,
    /// Optional local copy of the pinned archive.
    source_archive: Option<PathBuf>,
}

impl Basilisk2 {
    pub fn new(paths: Paths, pin: SourcePin, source_archive: Option<PathBuf>) -> Self {
        Self {
            paths,
            pin,
            source_archive,
        }
    }

    fn source(&self) -> Source<'_> {
        Source {
            paths: &self.paths,
            pin: &self.pin,
            local: self.source_archive.as_deref(),
            label: LABEL,
            config_table: BACKEND_NAME,
        }
    }

    fn build_key(&self, toolchain: &str) -> String {
        sha256_bytes(
            format!(
                "{BACKEND_NAME}\n{}\n{}\n{toolchain}\n{}\n",
                self.pin.revision,
                self.pin.sha256,
                CONFIGURE_OPTIONS.join(" ")
            )
            .as_bytes(),
        )
    }

    fn build(&self, key: &str, toolchain: &str, verbose: bool) -> Result<()> {
        let archive = self.source().fetch()?;
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

        eprintln!("Building {LABEL} {}...", self.pin.revision);
        let result = run_build(&archive, &src, &log_path);
        if verbose && let Ok(log) = fs::read_to_string(&log_path) {
            eprint!("{log}");
        }
        let built = src.join(BUILD_DIRECTORY).join(EXECUTABLE);
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
                "{LABEL} build failed; see {} (rerun with --verbose to print it), and run \
                 `macman doctor`",
                log_path.display()
            )));
        }

        // Publish by renaming a complete directory into place.
        let publish = work.join("publish");
        fs::create_dir_all(&publish)?;
        fs::rename(&built, publish.join(EXECUTABLE))?;
        fs::copy(&log_path, publish.join("build.log"))?;
        write_toml(
            &publish.join(MANIFEST),
            &Manifest {
                backend: BACKEND_NAME.into(),
                revision: self.pin.revision.clone(),
                host: "x86_64-unknown-linux-gnu".into(),
                toolchain: toolchain.into(),
                options: CONFIGURE_OPTIONS.iter().map(|s| s.to_string()).collect(),
                executable: EXECUTABLE.into(),
                executable_sha256: sha256_file(&publish.join(EXECUTABLE))?,
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
/// `autogen.sh` so that it gets macman's options.
fn run_build(archive: &Path, src: &Path, log_path: &Path) -> Result<()> {
    let log = File::create(log_path).context("cannot create build log")?;
    let jobs = thread::available_parallelism().map_or(1, |jobs| jobs.get());
    let script = format!(
        "set -e\n\
         tar xzf \"$1\" --strip-components=1\n\
         cd {BUILD_DIRECTORY}\n\
         NO_CONFIGURE=1 sh ./autogen.sh\n\
         sh ./configure {}\n\
         make -j{jobs}\n",
        CONFIGURE_OPTIONS.join(" ")
    );
    let status = Command::new("bash")
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
fn first_line(program: &str, args: &[&str]) -> Option<String> {
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
fn toolchain_identity() -> Result<String> {
    let hint = "install a C++ compiler (run `macman doctor`)";
    let compiler = first_line("g++", &["--version"]).context(hint)?;
    let sdl = first_line("pkg-config", &["--modversion", "sdl2"]);
    Ok(format!("{compiler} SDL {}", sdl.as_deref().unwrap_or("?")))
}

fn megabytes(memory: &str) -> Option<u32> {
    memory.strip_suffix('M')?.parse().ok()
}

/// `main_unix.cpp` caps RAM at 1023M, and rounds down to whole megabytes.
fn validate_memory(memory: &str) -> Result<()> {
    match megabytes(memory) {
        Some(1..=1023) => Ok(()),
        _ => bail!("memory {memory} is not supported; use whole megabytes, 1M to 1023M, as in 64M"),
    }
}

/// `displaycolordepth` picks among these. Anything else would silently mean
/// the desktop's depth. A guest can still pick a lower one in Monitors.
fn validate_display(display: &Display) -> Result<()> {
    let Display {
        width,
        height,
        depth,
        scale,
        ..
    } = *display;
    if !(512..=7680).contains(&width) || !(342..=4320).contains(&height) {
        bail!("display size {width}x{height} is out of range; 512x342 to 7680x4320 work");
    }
    if ![8, 16, 32].contains(&depth) {
        bail!("display depth {depth} is not supported; use 8, 16, or 32 bits");
    }
    if !(1..=4).contains(&scale) {
        bail!("display scale {scale} is not supported; use 1 to 4");
    }
    Ok(())
}

fn settings(machine: &Machine) -> Basilisk2Settings {
    machine.basilisk2.clone().unwrap_or_default()
}

fn cpu_level(cpu: Cpu) -> u32 {
    match cpu {
        Cpu::M68020 => 2,
        Cpu::M68030 => 3,
        Cpu::M68040 => 4,
    }
}

/// The contents of the preferences file. The JIT stays off: it's upstream's
/// own default, and interpreted code behaves the same on every host.
///
/// `extfs` is there to be empty, which keeps the guest off the host's files.
/// Upstream's default is `/`, a read-write volume of the whole host, and
/// `ExtFSInit` gives up on a path it can't `stat`. A shared folder replaces it
/// from the command line.
fn preferences(machine: &Machine) -> String {
    let settings = settings(machine);
    let display = &machine.display;
    let megabytes = megabytes(&machine.hardware.memory).unwrap_or(0);
    let model_id = match settings.cpu {
        Cpu::M68040 => MODEL_ID_QUADRA_900,
        Cpu::M68020 | Cpu::M68030 => MODEL_ID_IICI,
    };
    let screen = if display.fullscreen { "dga" } else { "win" };
    let mut text = format!(
        "# Written by macman for this run of {name}; changes here are not kept.\n\
         nogui true\n\
         title {name}\n\
         ramsize {ramsize}\n\
         cpu {cpu}\n\
         fpu {fpu}\n\
         modelid {model_id}\n\
         screen {screen}/{width}/{height}\n\
         displaycolordepth {depth}\n\
         mag_rate {scale}\n\
         nosound {nosound}\n\
         nocdrom true\n\
         jit false\n\
         idlewait true\n\
         bootdrive 0\n\
         bootdriver 0\n\
         extfs \n",
        name = machine.name,
        ramsize = u64::from(megabytes) * 1024 * 1024,
        cpu = cpu_level(settings.cpu),
        fpu = settings.fpu,
        width = display.width,
        height = display.height,
        depth = display.depth,
        scale = display.scale,
        nosound = !settings.sound,
    );
    if settings.network == Network::Slirp {
        text.push_str("ether slirp\n");
    }
    text
}

/// The emulator's arguments. A `*` before a disk asks for read-only, on top of
/// the throwaway copy already being a read-only file.
fn arguments(request: &LaunchRequest<'_>, preferences: &Path) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        "--config".into(),
        preferences.into(),
        "--rom".into(),
        request.rom.into(),
    ];
    for disk in request.disks {
        let mut value = OsString::from(if disk.writable { "" } else { "*" });
        value.push(&disk.path);
        arguments.extend(["--disk".into(), value]);
    }
    if let Some(folder) = settings(request.machine).shared_folder {
        arguments.extend(["--extfs".into(), folder.into()]);
    }
    arguments
}

/// Have the kernel stop the emulator if macman dies, because the locks on its
/// disks are about to go too. It has to be `SIGKILL`: SDL turns `SIGTERM` into
/// the window closing, which only asks the guest whether to shut down.
#[cfg(target_os = "linux")]
fn die_with_macman(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `prctl` is async-signal-safe, and nothing else runs in the child.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_macman(_command: &mut Command) {}

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

impl EmulatorBackend for Basilisk2 {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    fn revision(&self) -> &str {
        &self.pin.revision
    }

    fn resolve_revision(&self, recorded: &str) -> Result<String> {
        if recorded == self.pin.revision {
            return Ok(recorded.to_string());
        }
        bail!(
            "pinned to Basilisk II {recorded}, which isn't known to behave the same as {}, the \
             revision this host builds; upgrade macman",
            self.pin.revision
        )
    }

    fn validate(&self, machine: &Machine) -> Result<()> {
        self.resolve_revision(&machine.emulator.revision)
            .with_context(|| format!("machine {:?} can't run here", machine.name))?;
        if machine.hardware.model.is_some() {
            bail!(
                "machine {:?} names a Mini vMac model, which Basilisk II doesn't have",
                machine.name
            );
        }
        validate_memory(&machine.hardware.memory)?;
        validate_display(&machine.display)?;
        let settings = settings(machine);
        if settings.cpu == Cpu::M68040 && !settings.fpu {
            bail!("a 68040 always has an FPU; pick --cpu 68030 to leave it out");
        }
        Ok(())
    }

    /// The emulator keeps its drives in a list, with no limit to refuse.
    fn validate_disks(&self, _machine: &Machine, _count: usize) -> Result<()> {
        Ok(())
    }

    fn check_host(&self) -> Result<()> {
        if !HOST_SUPPORTED {
            bail!(UNSUPPORTED_HOST);
        }
        Ok(())
    }

    fn check_rom(&self, _machine: &Machine, rom: &Rom, file: &Path) -> Result<()> {
        if registry::rom_file_is_32_bit_clean(file, rom.size)? {
            return Ok(());
        }
        bail!(
            "ROM {:?} can't start Basilisk II, which needs a 512K or 1M 32-bit clean ROM, such \
             as from a Mac IIci or a Quadra; it won't run from the Plus, SE, II, or IIx ROMs \
             that suit Mini vMac",
            rom.name
        )
    }

    fn check_disk(&self, path: &Path, writable: bool) -> Result<()> {
        if writable && disk::classify(path)? == Classification::Supported(DiskFormat::DiskCopy42) {
            bail!(
                "{} is a Disk Copy 4.2 image, and Basilisk II would change it without updating \
                 its checksum; pass --readonly, or take the volume out with `macman disk extract`",
                path.display()
            );
        }
        Ok(())
    }

    fn prepare(&self, machine: &Machine, verbose: bool) -> Result<Executable> {
        self.check_host()?;
        self.validate(machine)?;
        let toolchain = toolchain_identity()?;
        let key = self.build_key(&toolchain);
        source::cached_or_built(&self.paths, key, LABEL, |key| {
            self.build(key, &toolchain, verbose)
        })
    }

    fn launch(&self, request: &LaunchRequest<'_>) -> Result<ExitStatus> {
        if let Some(folder) = settings(request.machine).shared_folder
            && !folder.is_dir()
        {
            bail!("the shared folder {} is missing", folder.display());
        }
        let file = request.workdir.join(PREFERENCES);
        fs::write(&file, preferences(request.machine))
            .with_context(|| format!("cannot write {}", file.display()))?;

        let mut command = Command::new(&request.executable.path);
        command
            .args(arguments(request, &file))
            .current_dir(request.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        die_with_macman(&mut command);
        if request.verbose {
            eprintln!("Launching {command:?}");
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", request.executable.path.display()))?;
        let errors = watch_output(&mut child, request.verbose);
        let status = child.wait().context("cannot wait for the emulator")?;
        match errors?.as_slice() {
            [] => Ok(status),
            errors if status.success() => {
                bail!("Basilisk II refused to run: {}", errors.join("; "))
            }
            errors => {
                eprintln!("Basilisk II reported: {}", errors.join("; "));
                Ok(status)
            }
        }
    }

    fn doctor(&self) -> Vec<Check> {
        if !HOST_SUPPORTED {
            return vec![Check::warn(
                "Basilisk II isn't available on this host",
                UNSUPPORTED_HOST,
            )];
        }
        let mut checks = Vec::new();
        const INSTALL: &str = "install what Basilisk II builds with (Debian/Ubuntu: apt install \
                               build-essential autoconf automake pkg-config libsdl2-dev)";
        for tool in [
            "gcc",
            "g++",
            "make",
            "bash",
            "tar",
            "autoconf",
            "aclocal",
            "pkg-config",
        ] {
            if tool_exists(tool) {
                checks.push(Check::ok(format!("{tool} found, for Basilisk II")));
            } else {
                checks.push(Check::fail(
                    format!("{tool} not found, which Basilisk II needs"),
                    INSTALL,
                ));
            }
        }
        match first_line("pkg-config", &["--modversion", "sdl2"]) {
            Some(version) => checks.push(Check::ok(format!("SDL {version} found"))),
            None => checks.push(Check::fail(
                "SDL 2 headers not found, which Basilisk II needs",
                INSTALL,
            )),
        }
        checks.push(self.source().check());
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            checks.push(Check::warn(
                "neither DISPLAY nor WAYLAND_DISPLAY is set",
                "Basilisk II opens a window; run macman from a graphical session",
            ));
        }
        checks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ResolvedDisk;
    use crate::machine::{EmulatorConfig, Hardware};

    fn backend() -> Basilisk2 {
        Basilisk2::new(Paths::under(Path::new("/nonexistent")), pin(), None)
    }

    fn machine() -> Machine {
        Machine {
            schema_version: 1,
            name: "quadra".into(),
            id: None,
            emulator: EmulatorConfig {
                backend: BACKEND_NAME.into(),
                revision: pin().revision,
            },
            hardware: Hardware {
                model: None,
                rom: "r".into(),
                memory: "64M".into(),
            },
            display: Display {
                width: 800,
                height: 600,
                depth: 16,
                scale: 2,
                fullscreen: false,
            },
            system: Default::default(),
            basilisk2: Some(Basilisk2Settings::default()),
            disks: vec![],
        }
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().filter(|line| !line.starts_with('#')).collect()
    }

    #[test]
    fn translates_a_machine_to_preferences() {
        assert_eq!(
            lines(&preferences(&machine())),
            [
                "nogui true",
                "title quadra",
                "ramsize 67108864",
                "cpu 4",
                "fpu true",
                "modelid 14",
                "screen win/800/600",
                "displaycolordepth 16",
                "mag_rate 2",
                "nosound false",
                "nocdrom true",
                "jit false",
                "idlewait true",
                "bootdrive 0",
                "bootdriver 0",
                "extfs ",
            ]
        );
    }

    #[test]
    fn options_reach_the_preferences() {
        let mut m = machine();
        m.display.fullscreen = true;
        m.basilisk2 = Some(Basilisk2Settings {
            cpu: Cpu::M68030,
            fpu: false,
            sound: false,
            network: Network::Slirp,
            shared_folder: None,
        });
        let text = preferences(&m);
        for line in [
            "cpu 3",
            "fpu false",
            "modelid 5",
            "screen dga/800/600",
            "nosound true",
            "ether slirp",
        ] {
            assert!(lines(&text).contains(&line), "{line}: {text}");
        }
    }

    #[test]
    fn the_guest_gets_no_host_files_unless_it_is_given_a_folder() {
        let text = preferences(&machine());
        assert!(
            text.lines().any(|line| line.trim_end() == "extfs"),
            "{text}"
        );
    }

    #[test]
    fn no_preference_line_can_hold_a_path_or_outgrow_the_parser() {
        // Names are checked elsewhere; the longest here is the title.
        for line in preferences(&machine()).lines() {
            assert!(line.len() < 256, "{line}");
            assert!(!line.contains('/') || line.starts_with("screen"), "{line}");
        }
    }

    #[test]
    fn disks_keep_their_order_and_read_only_ones_are_marked() {
        let mut m = machine();
        m.basilisk2.as_mut().unwrap().shared_folder = Some("/shared folder".into());
        let executable = Executable {
            path: "/bin".into(),
            build_key: "k".into(),
            built: false,
        };
        let disks = [
            ResolvedDisk {
                path: "/boot disk.dsk".into(),
                writable: true,
            },
            ResolvedDisk {
                path: "/session/1-app.dsk".into(),
                writable: false,
            },
        ];
        let request = LaunchRequest {
            machine: &m,
            executable: &executable,
            rom: Path::new("/roms/q.rom"),
            disks: &disks,
            workdir: Path::new("/session"),
            verbose: false,
        };
        assert_eq!(
            arguments(&request, Path::new("/session/prefs")),
            [
                "--config",
                "/session/prefs",
                "--rom",
                "/roms/q.rom",
                "--disk",
                "/boot disk.dsk",
                "--disk",
                "*/session/1-app.dsk",
                "--extfs",
                "/shared folder",
            ]
        );
    }

    #[test]
    fn rejects_what_the_emulator_would_quietly_change() {
        type Change = fn(&mut Machine);
        let cases: [(Change, &str); 8] = [
            (|m| m.hardware.memory = "512K".into(), "whole megabytes"),
            (|m| m.hardware.memory = "1024M".into(), "1023M"),
            (|m| m.hardware.memory = "0M".into(), "whole megabytes"),
            (|m| m.display.depth = 1, "depth 1"),
            (|m| m.display.width = 256, "out of range"),
            (|m| m.display.scale = 5, "scale 5"),
            (
                |m| m.hardware.model = Some(crate::model::Model::II),
                "Mini vMac model",
            ),
            (
                |m| m.basilisk2.as_mut().unwrap().fpu = false,
                "always has an FPU",
            ),
        ];
        backend().validate(&machine()).unwrap();
        for (change, expected) in cases {
            let mut m = machine();
            change(&mut m);
            let error = backend().validate(&m).unwrap_err().to_string();
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn an_unknown_revision_is_a_compatibility_error() {
        let mut m = machine();
        m.emulator.revision = "floating".into();
        let error = format!("{:#}", backend().validate(&m).unwrap_err());
        assert!(error.contains("upgrade macman"), "{error}");
    }

    #[test]
    fn the_build_key_ignores_the_machine_and_follows_the_toolchain() {
        assert_eq!(backend().build_key("gcc 1"), backend().build_key("gcc 1"));
        assert_ne!(backend().build_key("gcc 1"), backend().build_key("gcc 2"));
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

    #[test]
    fn a_disk_copy_image_is_only_attached_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("floppy.image");
        let mut image = vec![0u8; 84 + 800 * 1024];
        image[64..68].copy_from_slice(&(800u32 * 1024).to_be_bytes());
        image[82..84].copy_from_slice(&[0x01, 0x00]);
        fs::write(&path, image).unwrap();
        assert_eq!(
            disk::classify(&path).unwrap(),
            Classification::Supported(DiskFormat::DiskCopy42)
        );
        let error = backend().check_disk(&path, true).unwrap_err().to_string();
        assert!(error.contains("--readonly"), "{error}");
        backend().check_disk(&path, false).unwrap();
    }

    #[test]
    fn the_pin_names_a_whole_commit() {
        let pin = pin();
        assert_eq!(pin.revision.len(), 12);
        assert!(pin.url.contains(&pin.revision));
        assert_eq!(pin.sha256.len(), 64);
    }
}
