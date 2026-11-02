//! Basilisk II adapter, for later 68k software. Everything here comes from the
//! source of the pinned macemu commit (`BasiliskII/src`): `prefs_items.cpp`,
//! `Unix/main_unix.cpp`, `Unix/sys_unix.cpp`, and `SDL/video_sdl2.cpp`, and on
//! Windows, their counterparts in `Windows/`, as noted below and in
//! [`macemu_windows`], which builds it there.
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
//!
//! Windows differs again:
//!
//! - `Sys_open` denies sharing, so Windows protects an attached disk for as long
//!   as the emulator has it, and the emulator needn't die with macman.
//! - Paths reach narrow `CreateFile` calls in buffers of 260 bytes, and longer
//!   ones are cut short, not refused. The build's manifest makes those calls
//!   read UTF-8, which is what SDL hands `main`, so any characters work.
//! - `extfs` is ignored. `enableextfs` instead shows the guest whole drives, so
//!   this backend can't honor a shared folder here, and refuses one.
//! - `GetFileSize` reads only the low 32 bits of a disk's size.
//!
//! macOS builds as Linux does, not with upstream's Xcode project, which wants
//! SDL and its math libraries installed system-wide. It differs in that:
//!
//! - ARM64 has no 80-bit `long double`, so the FPU core is MPFR's, which needs
//!   MPFR and GMP. `configure` doesn't look for them or say where they are, so
//!   `pkg-config` does, as they aren't where Apple's compiler looks.
//! - Upstream's ARM64 macOS builds have no JIT, so it's configured out, where
//!   elsewhere it's only switched off in the preferences.
//! - `Sys_open` takes `O_EXLOCK` on every disk file, so the emulator guards its
//!   own disks, and needn't die with macman. A second one would only print a
//!   warning that stays in its buffer, and show no disk, so macman tries the
//!   lock first, to refuse with a reason.

use super::macemu::{
    self, Build, disk_argument, display_check, first_line, library_checks, megabytes,
    run_configure_build, supervise, tool_checks,
};
use super::minivmac::{gcc_target, is_mingw_x64, macos_toolchain_identity};
use super::source::{self, Source, SourcePin, tool_exists};
use super::{Check, EmulatorBackend, Executable, LaunchRequest, macemu_windows};
use crate::disk::{self, Classification, DiskFormat};
use crate::fsutil::{atomic_copy, sha256_bytes, without_verbatim_prefix};
use crate::machine::{Basilisk2Settings, Cpu, Display, Machine, Network};
use crate::paths::Paths;
use crate::registry::{self, Rom};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

pub const BACKEND_NAME: &str = "basilisk2";
pub const DEFAULT_MEMORY: &str = "32M";
pub const DEFAULT_DISPLAY: (u32, u32, u32) = (640, 480, 8);
const LABEL: &str = "Basilisk II";
const WINDOWS: bool = cfg!(all(windows, target_arch = "x86_64"));
const MACOS: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));
const EXECUTABLE: &str = if WINDOWS {
    macemu_windows::EXECUTABLE
} else {
    "BasiliskII"
};
const BUILD_DIRECTORY: &str = if WINDOWS {
    macemu_windows::BUILD_DIRECTORY
} else {
    "BasiliskII/src/Unix"
};
const PREFERENCES: &str = "prefs";
/// The session's copy of the ROM, on Windows.
const ROM_COPY: &str = "rom";
/// Windows reads a disk's size as 32 bits (`Sys_open`).
const WINDOWS_MAX_DISK: u64 = 1 << 32;
const WINDOWS_MAX_PATH: usize = 260;
/// `modelid` is the Gestalt ID less 6. Mac OS 8 needs a 68040 model, and the
/// Quadra 900 is upstream's; older systems expect the IIci that's its default.
const MODEL_ID_IICI: u32 = 5;
const MODEL_ID_QUADRA_900: u32 = 14;
const HOST: Option<&str> = if WINDOWS {
    Some("x86_64-pc-windows-gnu")
} else if MACOS {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else {
    None
};
const UNSUPPORTED_HOST: &str = "the Basilisk II backend supports only Linux x86-64, Windows \
                                x86-64, and macOS ARM64 hosts; see docs/PLATFORMS.md";
const MACOS_CONFIGURE_OPTIONS: &[&str] = &[
    "--enable-sdl-video",
    "--enable-sdl-audio",
    "--with-gtk=no",
    "--with-esd=no",
    "--disable-jit-compiler",
];
/// What an ARM64 macOS build links with, by `pkg-config` name, and for people.
const MACOS_LIBRARIES: &[(&str, &str)] = &[("sdl2", "SDL"), ("gmp", "GMP"), ("mpfr", "MPFR")];

pub struct Basilisk2 {
    paths: Paths,
    pin: SourcePin,
    /// Optional local copy of the pinned archive.
    source_archive: Option<PathBuf>,
    /// The SDL package, on Windows.
    sdl: SourcePin,
    /// Optional local copy of that.
    sdl_archive: Option<PathBuf>,
}

impl Basilisk2 {
    pub fn new(paths: Paths, pin: SourcePin, source_archive: Option<PathBuf>) -> Self {
        Self {
            paths,
            pin,
            source_archive,
            sdl: macemu_windows::sdl_pin(),
            sdl_archive: None,
        }
    }

    pub fn with_sdl(self, sdl: SourcePin, sdl_archive: Option<PathBuf>) -> Self {
        Self {
            sdl,
            sdl_archive,
            ..self
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

    fn sdl_source(&self) -> Source<'_> {
        Source {
            paths: &self.paths,
            pin: &self.sdl,
            local: self.sdl_archive.as_deref(),
            label: "SDL",
            config_table: "sdl",
        }
    }

    fn build_key(&self, toolchain: &str) -> String {
        sha256_bytes(
            format!(
                "{BACKEND_NAME}\n{}\n{}\n{toolchain}\n{}\n",
                self.pin.revision,
                self.pin.sha256,
                build_options().join(" ")
            )
            .as_bytes(),
        )
    }

    fn build(&self, key: &str, toolchain: &str, verbose: bool) -> Result<()> {
        let archive = self.source().fetch()?;
        let build = Build {
            paths: &self.paths,
            backend: BACKEND_NAME,
            label: LABEL,
            revision: &self.pin.revision,
            host: HOST.context(UNSUPPORTED_HOST)?,
            toolchain,
            options: build_options(),
            directory: BUILD_DIRECTORY,
            executable: EXECUTABLE,
        };
        build.run(key, verbose, |src, log_path| {
            if WINDOWS {
                let sdl = self.sdl_source().fetch()?;
                let log = File::create(log_path).context("cannot create build log")?;
                macemu_windows::build(&archive, &sdl, src, &log)
            } else {
                run_build(&archive, src, log_path)
            }
        })
    }
}

fn build_options() -> &'static [&'static str] {
    if WINDOWS {
        macemu_windows::DEFINES
    } else if MACOS {
        MACOS_CONFIGURE_OPTIONS
    } else {
        macemu::CONFIGURE_OPTIONS
    }
}

/// What the compiler needs to be told to find the libraries, on macOS.
fn macos_library_flags(option: &str) -> String {
    let packages = MACOS_LIBRARIES.iter().map(|(package, _)| *package);
    let args: Vec<&str> = std::iter::once(option).chain(packages).collect();
    first_line("pkg-config", &args).unwrap_or_default()
}

/// On macOS, `gcc` may be a real GCC from a package manager, not Apple's clang.
fn run_build(archive: &Path, src: &Path, log_path: &Path) -> Result<()> {
    let mut command = Command::new("bash");
    if MACOS {
        command
            .env("CC", "clang")
            .env("CXX", "clang++")
            .env("CPPFLAGS", macos_library_flags("--cflags-only-I"))
            .env("LDFLAGS", macos_library_flags("--libs-only-L"));
    }
    run_configure_build(
        command,
        archive,
        src,
        log_path,
        BUILD_DIRECTORY,
        build_options(),
    )
}

fn toolchain_identity(sdl_pin: &SourcePin) -> Result<String> {
    if MACOS {
        return Ok(format!(
            "{} {} {}",
            macos_toolchain_identity()?,
            macos_library_versions(),
            macos_library_flags("--libs-only-L"),
        ));
    }
    if WINDOWS {
        let hint = "install a C++ compiler (run `macman doctor`)";
        let compiler = first_line("g++", &["--version"]).context(hint)?;
        let target = gcc_target()?;
        if !is_mingw_x64(&target) {
            bail!("gcc targets {target}, not 64-bit Windows; run `macman doctor`");
        }
        let recipe = macemu_windows::RECIPE;
        return Ok(format!(
            "{target} {compiler} SDL {} {recipe}",
            sdl_pin.sha256
        ));
    }
    macemu::linux_toolchain_identity()
}

/// A build links these dynamically, so it's only good for the ones it saw.
fn macos_library_versions() -> String {
    MACOS_LIBRARIES
        .iter()
        .map(|(package, name)| {
            let version = first_line("pkg-config", &["--modversion", package]);
            format!("{name} {}", version.as_deref().unwrap_or("?"))
        })
        .collect::<Vec<_>>()
        .join(" ")
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
         extfs \n\
         {host}",
        name = machine.name,
        ramsize = u64::from(megabytes) * 1024 * 1024,
        cpu = cpu_level(settings.cpu),
        fpu = settings.fpu,
        width = display.width,
        height = display.height,
        depth = display.depth,
        scale = display.scale,
        nosound = !settings.sound,
        // Without these, Windows would offer the host's drives and media.
        host = if WINDOWS {
            "enableextfs false\npollmedia false\n"
        } else {
            ""
        },
    );
    if settings.network == Network::Slirp {
        text.push_str("ether slirp\n");
    }
    text
}

/// The emulator's arguments.
fn arguments(request: &LaunchRequest<'_>, preferences: &Path) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        "--config".into(),
        preferences.into(),
        "--rom".into(),
        request.rom.into(),
    ];
    for disk in request.disks {
        arguments.extend(["--disk".into(), disk_argument(disk)]);
    }
    if let Some(folder) = settings(request.machine).shared_folder {
        arguments.extend(["--extfs".into(), folder.into()]);
    }
    arguments
}

/// What Basilisk II can open on Windows, or why it can't. A session's own disk
/// goes by its name there, which keeps it short.
fn windows_disk_path(disk: &Path, workdir: &Path) -> Result<PathBuf> {
    let path = match disk.strip_prefix(workdir) {
        Ok(name) => name.to_path_buf(),
        Err(_) => without_verbatim_prefix(disk),
    };
    // Bytes, in UTF-8, not characters: that's what its buffers hold.
    if path.to_string_lossy().len() >= WINDOWS_MAX_PATH {
        bail!(
            "{} is too long for Basilisk II, which handles paths under {WINDOWS_MAX_PATH} bytes; \
             move it to a shorter path",
            disk.display()
        );
    }
    Ok(path)
}

/// The arguments on Windows: see the module comment for why they're different.
fn windows_arguments(request: &LaunchRequest<'_>) -> Result<Vec<OsString>> {
    atomic_copy(request.rom, &request.workdir.join(ROM_COPY))?;
    let mut arguments: Vec<OsString> = vec![
        "--config".into(),
        PREFERENCES.into(),
        "--rom".into(),
        ROM_COPY.into(),
    ];
    for disk in request.disks {
        let mut value = OsString::from(if disk.writable { "" } else { "*" });
        value.push(windows_disk_path(&disk.path, request.workdir)?);
        arguments.extend(["--disk".into(), value]);
    }
    Ok(arguments)
}

/// Refuse a disk that an emulator holds, which macman's own locks can't show
/// once the macman that started it has gone. A file system without locks can't
/// be held, and the emulator goes on without them there too.
fn refuse_held_disk(disk: &Path) -> Result<()> {
    let file = File::open(disk).with_context(|| format!("cannot open {}", disk.display()))?;
    if let Err(TryLockError::WouldBlock) = file.try_lock() {
        bail!(
            "{} is held by a Basilisk II that outlived macman; quit that emulator first",
            disk.display()
        );
    }
    Ok(())
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
        if WINDOWS && settings.shared_folder.is_some() {
            bail!(
                "shared folders aren't available with Basilisk II on Windows, where it can only \
                 show the guest whole drives"
            );
        }
        Ok(())
    }

    /// The emulator keeps its drives in a list, with no limit to refuse.
    fn validate_disks(&self, _machine: &Machine, _count: usize) -> Result<()> {
        Ok(())
    }

    fn check_host(&self) -> Result<()> {
        if HOST.is_none() {
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
        if WINDOWS && fs::metadata(path)?.len() >= WINDOWS_MAX_DISK {
            bail!(
                "{} is too big for Basilisk II on Windows, which handles disks under 4G",
                path.display()
            );
        }
        if writable && disk::classify(path)? == Classification::Supported(DiskFormat::DiskCopy42) {
            bail!(
                "{} is a Disk Copy 4.2 image, and Basilisk II would change it without updating \
                 its checksum; pass --readonly, or take the volume out with `macman disk extract`",
                path.display()
            );
        }
        Ok(())
    }

    fn needs_unmounted_boot_volume(&self) -> bool {
        true
    }

    fn mounts_partitioned_disks(&self) -> bool {
        true
    }

    fn prepare(&self, machine: &Machine, verbose: bool) -> Result<Executable> {
        self.check_host()?;
        self.validate(machine)?;
        let toolchain = toolchain_identity(&self.sdl)?;
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
        if MACOS {
            for disk in request.disks {
                refuse_held_disk(&disk.path)?;
            }
        }
        let file = request.workdir.join(PREFERENCES);
        fs::write(&file, preferences(request.machine))
            .with_context(|| format!("cannot write {}", file.display()))?;

        let arguments = if WINDOWS {
            windows_arguments(request)?
        } else {
            arguments(request, &file)
        };
        let mut command = Command::new(&request.executable.path);
        command.args(arguments).current_dir(request.workdir);
        supervise(command, LABEL, request.verbose)
    }

    fn doctor(&self) -> Vec<Check> {
        if HOST.is_none() {
            return vec![Check::warn(
                "Basilisk II isn't available on this host",
                UNSUPPORTED_HOST,
            )];
        }
        let mut checks = Vec::new();
        if WINDOWS {
            // Mini vMac's checks already cover the rest of MinGW.
            if tool_exists("g++") {
                checks.push(Check::ok("g++ found, for Basilisk II"));
            } else {
                checks.push(Check::fail(
                    "g++ not found, which Basilisk II needs",
                    "install MinGW-w64, then open a new terminal: \
                     winget install BrechtSanders.WinLibs.POSIX.UCRT",
                ));
            }
            checks.push(self.source().check());
            checks.push(self.sdl_source().check());
            return checks;
        }
        const LINUX_INSTALL: &str = "install what Basilisk II builds with (Debian/Ubuntu: apt \
                                     install build-essential autoconf automake pkg-config \
                                     libsdl2-dev)";
        const MACOS_INSTALL: &str = "install what Basilisk II builds with (MacPorts: port install \
                                     autoconf automake pkgconfig libsdl2 mpfr; Homebrew: brew \
                                     install autoconf automake pkg-config sdl2 mpfr)";
        const LINUX_TOOLS: &[&str] = &[
            "gcc",
            "g++",
            "make",
            "bash",
            "tar",
            "autoconf",
            "aclocal",
            "pkg-config",
        ];
        // Mini vMac's checks already cover Apple's tools.
        const MACOS_TOOLS: &[&str] = &["autoconf", "aclocal", "pkg-config"];
        let (tools, libraries, install) = if MACOS {
            (MACOS_TOOLS, MACOS_LIBRARIES, MACOS_INSTALL)
        } else {
            (LINUX_TOOLS, &MACOS_LIBRARIES[..1], LINUX_INSTALL)
        };
        checks.extend(tool_checks(tools, LABEL, install));
        checks.extend(library_checks(libraries, LABEL, install));
        checks.push(self.source().check());
        if !MACOS {
            checks.extend(display_check(LABEL));
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
        Basilisk2::new(Paths::under(Path::new("/nonexistent")), macemu::pin(), None)
    }

    fn machine() -> Machine {
        Machine {
            schema_version: 1,
            name: "quadra".into(),
            id: None,
            emulator: EmulatorConfig {
                backend: BACKEND_NAME.into(),
                revision: macemu::pin().revision,
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
            sheepshaver: None,
            disks: vec![],
        }
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().filter(|line| !line.starts_with('#')).collect()
    }

    #[test]
    fn translates_a_machine_to_preferences() {
        let text = preferences(&machine());
        let mut expected = vec![
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
        ];
        if WINDOWS {
            expected.extend(["enableextfs false", "pollmedia false"]);
        }
        assert_eq!(lines(&text), expected);
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
    fn a_windows_session_disk_goes_by_its_ascii_name() {
        let session = Path::new("session");
        assert_eq!(
            windows_disk_path(&session.join("1-app.dsk"), session).unwrap(),
            Path::new("1-app.dsk")
        );
    }

    /// Longer ones would be cut short and open the wrong file, or none. This
    /// counts bytes, so 90 characters of Japanese are just over the limit.
    #[test]
    fn a_windows_path_must_fit_the_emulator_s_buffers_in_bytes() {
        let session = Path::new("session");
        let fits = PathBuf::from(format!("{}.dsk", "x".repeat(WINDOWS_MAX_PATH - 5)));
        assert_eq!(windows_disk_path(&fits, session).unwrap(), fits);
        for name in [
            format!("{}.dsk", "x".repeat(WINDOWS_MAX_PATH - 4)),
            format!("{}.dsk", "日".repeat(WINDOWS_MAX_PATH / 3)),
        ] {
            let error = windows_disk_path(Path::new(&name), session)
                .unwrap_err()
                .to_string();
            assert!(error.contains("too long"), "{error}");
        }
    }

    #[test]
    #[cfg(windows)]
    fn a_shared_folder_is_refused_where_only_whole_drives_can_be_shared() {
        let mut m = machine();
        m.basilisk2.as_mut().unwrap().shared_folder = Some("C:\\shared".into());
        let error = backend().validate(&m).unwrap_err().to_string();
        assert!(error.contains("whole drives"), "{error}");
    }

    #[test]
    #[cfg(windows)]
    fn windows_refuses_a_disk_whose_size_it_would_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.dsk");
        File::create(&path)
            .unwrap()
            .set_len(WINDOWS_MAX_DISK)
            .unwrap();
        let error = backend().check_disk(&path, true).unwrap_err().to_string();
        assert!(error.contains("under 4G"), "{error}");
    }

    #[test]
    fn only_macos_configures_the_jit_out() {
        assert_eq!(build_options().contains(&"--disable-jit-compiler"), MACOS);
    }
}
