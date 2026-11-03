//! SheepShaver adapter, for classic PowerPC Mac OS. Everything here comes from
//! the source of the pinned macemu commit (`SheepShaver/src`): `prefs_items.cpp`,
//! `rom_patches.cpp`, `Unix/main_unix.cpp`, `Unix/prefs_unix.cpp`, and
//! `SDL/video_sdl2.cpp`. See [`macemu`] for what it shares with Basilisk II.
//!
//! It presents a generic PowerPC Mac, not a particular Power Mac, and it isn't
//! Basilisk II with another CPU:
//!
//! - It starts from a PowerPC ROM: a 4M dump of a PCI Power Mac, whose nanokernel
//!   names its type, or a `Mac OS ROM` file, which is compressed and smaller.
//!   Only the first 4M is read, and a whole 4M counts as a dump, so a compressed
//!   file has to be smaller than that. No 68k ROM will do.
//! - RAM is 16M at least, and anything less is raised, with only a warning. It
//!   has to fit below the ROM's fixed address, and macman stops at 1024M, which
//!   fits whichever of upstream's two RAM addresses is in use.
//! - There's no CPU or FPU to choose, and no `modelid`.
//! - Color depth can't be chosen. There's no `displaycolordepth` here: it starts
//!   at the desktop's depth, and the guest can pick less in Monitors. So machines
//!   record 32 bits, and anything else is refused rather than quietly ignored.
//! - Classic resolutions are left out of its list, so 640x480 is the smallest.
//!   A window bigger than the desktop is cut down to it.
//! - On x86-64 it's built with real addressing, upstream's default, whose
//!   precompiled JIT macman doesn't second-guess. Mac addresses are host ones, so
//!   low memory has to be mapped at address 0, and that needs
//!   `vm.mmap_min_addr` to be 0. `doctor` says so, as does a failed start. For
//!   the same reason it forces SDL onto X11, or XWayland.
//!
//! What it does share is the same as for Basilisk II: every setting is run-time
//! and comes from a preferences file in the session, so one build serves every
//! machine, and its NVRAM lasts one run. `QuitEmulator` exits 0 after a refusal.
//! Linux builds take no disk locks, so it dies with macman. A Disk Copy image is
//! written without updating its checksum. `extfs` defaults to `/`.
//!
//! Windows x86-64 is offered too, built by [`sheepshaver_windows`]. There:
//!
//! - There's no JIT, so the PowerPC code is interpreted, and `jit` is off.
//! - Mac addresses are offsets into one block of host memory, so nothing is
//!   mapped at address 0, and `vm.mmap_min_addr` doesn't come into it.
//! - Disks, paths, and host files are as for Basilisk II on Windows, from the
//!   same code: see [`macemu_windows`]. The emulator denies sharing on its
//!   disks, so it needn't die with macman, no shared folder can be offered, and
//!   disks must be under 4G.
//!
//! macOS ARM64 builds as Linux does, as Basilisk II does there, and not with
//! upstream's Xcode project. There:
//!
//! - There's no JIT, as `configure` only knows how to make one for ELF, so it's
//!   configured out and `jit` is off.
//! - A process can't map address 0, as a 64-bit Mach-O program's `__PAGEZERO`
//!   covers it. `configure` knows the host, as `arm-apple-darwin`, and gives it
//!   upstream's fixed offset for Mac addresses, `0x400000000000`, so Mac memory
//!   starts there and `vm.mmap_min_addr` doesn't come into it. RAM starts at
//!   Mac address `0x10000000` and the ROM at `0x50000000`, so 1024M fits.
//! - SDL comes from MacPorts or Homebrew, and only SDL: the PowerPC emulator's
//!   FPU needs no MPFR.
//! - The emulator locks its disks, and its CD images too, so it needn't die
//!   with macman, and macman tries those locks first, as for Basilisk II.

use super::macemu::{
    self, Build, MACOS, disk_argument, display_check, library_checks, megabytes,
    run_configure_build, supervise, tool_checks,
};
use super::source::{self, Source, SourcePin, tool_exists};
use super::{
    CdImage, Check, EmulatorBackend, Executable, LaunchRequest, macemu_windows, sheepshaver_windows,
};
use crate::disk::{self, Classification, DiskFormat};
use crate::fsutil::sha256_bytes;
use crate::machine::{Display, Machine, Network, SheepShaverSettings};
use crate::paths::Paths;
use crate::registry::{self, Rom};
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

pub const BACKEND_NAME: &str = "sheepshaver";
pub const DEFAULT_MEMORY: &str = "64M";
pub const DEFAULT_DISPLAY: (u32, u32, u32) = (640, 480, 32);
const LABEL: &str = "SheepShaver";
const WINDOWS: bool = cfg!(all(windows, target_arch = "x86_64"));
const EXECUTABLE: &str = if WINDOWS {
    sheepshaver_windows::EXECUTABLE
} else {
    "SheepShaver"
};
const BUILD_DIRECTORY: &str = if WINDOWS {
    sheepshaver_windows::BUILD_DIRECTORY
} else {
    "SheepShaver/src/Unix"
};
const PREFERENCES: &str = "prefs";
const HOST: Option<&str> = if WINDOWS {
    Some("x86_64-pc-windows-gnu")
} else if MACOS {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else {
    None
};
const UNSUPPORTED_HOST: &str = "the SheepShaver backend supports only Linux x86-64, Windows \
                                x86-64, and macOS ARM64 hosts; see docs/PLATFORMS.md";
/// Whether low memory is mapped at address 0, which only Linux builds do.
const LOW_MEMORY_AT_ZERO: bool = cfg!(target_os = "linux");
/// How a failed start shows that low memory couldn't be mapped
/// (`STR_LOW_MEM_MMAP_ERR`).
const LOW_MEMORY_ERROR: &str = "Low Memory";
const LOW_MEMORY_FIX: &str = "run `sudo sysctl vm.mmap_min_addr=0`, which lets programs map \
                              address 0 until the next reboot; see docs/PLATFORMS.md";
/// Where `run --cdrom` and `create --disk-size` have been tried. On Windows the
/// emulator finds CDs its own way, which hasn't been.
const INSTALLS_FROM_CD: bool = cfg!(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
));
/// Only Linux x86-64 builds have a JIT.
const JIT: bool = !(WINDOWS || MACOS);
/// The JIT is configured out where `configure` can't make one, so the build
/// says so, and asks for nothing it won't do.
const MACOS_CONFIGURE_OPTIONS: &[&str] = &[
    "--enable-sdl-video",
    "--enable-sdl-audio",
    "--with-gtk=no",
    "--with-esd=no",
    "--with-bincue",
    "--disable-jit",
];
/// What a macOS build links with, by `pkg-config` name, and for people.
const MACOS_LIBRARIES: &[(&str, &str)] = &[("sdl2", "SDL")];
/// The reference number of the CD driver (`CDROMRefNum`).
const CDROM_DRIVER: i32 = -62;
const MMAP_MIN_ADDR: &str = "/proc/sys/vm/mmap_min_addr";
const INSTALL: &str = "install what SheepShaver builds with (Debian/Ubuntu: apt install \
                       build-essential autoconf automake pkg-config libsdl2-dev)";
const MACOS_INSTALL: &str = "install what SheepShaver builds with (MacPorts: port install \
                             autoconf automake pkgconfig libsdl2; Homebrew: brew install \
                             autoconf automake pkg-config sdl2)";
const TOOLS: &[&str] = &[
    "gcc",
    "g++",
    "make",
    "bash",
    "tar",
    "autoconf",
    "aclocal",
    "pkg-config",
];

pub struct SheepShaver {
    paths: Paths,
    pin: SourcePin,
    /// Optional local copy of the pinned archive.
    source_archive: Option<PathBuf>,
    /// The SDL package, on Windows.
    sdl: SourcePin,
    /// Optional local copy of that.
    sdl_archive: Option<PathBuf>,
}

impl SheepShaver {
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
                sheepshaver_windows::build(&archive, &sdl, src, &log)
            } else {
                let command = if MACOS {
                    macemu::macos_build_command(MACOS_LIBRARIES)
                } else {
                    Command::new("bash")
                };
                run_configure_build(
                    command,
                    &archive,
                    src,
                    log_path,
                    BUILD_DIRECTORY,
                    build_options(),
                )
            }
        })
    }

    fn toolchain_identity(&self) -> Result<String> {
        if WINDOWS {
            macemu_windows::toolchain_identity(&self.sdl, sheepshaver_windows::RECIPE)
        } else if MACOS {
            macemu::macos_toolchain_identity(MACOS_LIBRARIES)
        } else {
            macemu::linux_toolchain_identity()
        }
    }
}

fn build_options() -> &'static [&'static str] {
    if WINDOWS {
        sheepshaver_windows::DEFINES
    } else if MACOS {
        MACOS_CONFIGURE_OPTIONS
    } else {
        macemu::CONFIGURE_OPTIONS
    }
}

/// Below 16M the emulator raises it. Above 1024M isn't known to be mapped.
fn validate_memory(memory: &str) -> Result<()> {
    match megabytes(memory) {
        Some(16..=1024) => Ok(()),
        _ => {
            bail!("memory {memory} is not supported; use whole megabytes, 16M to 1024M, as in 128M")
        }
    }
}

fn validate_display(display: &Display) -> Result<()> {
    let Display {
        width,
        height,
        depth,
        scale,
        ..
    } = *display;
    if !(640..=7680).contains(&width) || !(480..=4320).contains(&height) {
        bail!("display size {width}x{height} is out of range; 640x480 to 7680x4320 work");
    }
    if depth != 32 {
        bail!(
            "display depth {depth} can't be chosen with SheepShaver, which starts at the \
             desktop's depth; use 32, and pick less in the guest's Monitors control panel"
        );
    }
    if !(1..=4).contains(&scale) {
        bail!("display scale {scale} is not supported; use 1 to 4");
    }
    Ok(())
}

fn settings(machine: &Machine) -> SheepShaverSettings {
    machine.sheepshaver.clone().unwrap_or_default()
}

/// The contents of the preferences file. The JIT is left on, unlike Basilisk
/// II's: it's upstream's default here, and the interpreter is slow for the
/// guests this is for. Windows and macOS builds have only the interpreter.
///
/// `extfs` is there to be empty, as for Basilisk II: upstream's default is `/`,
/// a read-write volume of the whole host. `nocdrom` keeps the guest off the
/// host's drive, but also leaves out the CD driver, so a run with a CD image
/// turns it off. The emulator then adds the host's drive itself, `/dev/cdrom`
/// on Linux and whatever CD media the host has on macOS, and that can't be
/// prevented. `bootdriver` is what makes the guest start from the CD, on an Old
/// World ROM through PRAM, and on a New World one by reordering the drives. It's
/// set only for a disc that can start a Mac: see [`macemu::starts_from_cd`].
fn preferences(machine: &Machine, cdroms: &[CdImage]) -> String {
    let settings = settings(machine);
    let cdrom = !cdroms.is_empty();
    let bootdriver = if macemu::starts_from_cd(cdroms) {
        CDROM_DRIVER
    } else {
        0
    };
    let display = &machine.display;
    let megabytes = megabytes(&machine.hardware.memory).unwrap_or(0);
    let screen = if display.fullscreen { "dga" } else { "win" };
    let mut text = format!(
        "# Written by macman for this run of {name}; changes here are not kept.\n\
         nogui true\n\
         title {name}\n\
         ramsize {ramsize}\n\
         screen {screen}/{width}/{height}\n\
         mag_rate {scale}\n\
         nosound {nosound}\n\
         nocdrom {nocdrom}\n\
         jit {jit}\n\
         idlewait true\n\
         bootdrive 0\n\
         bootdriver {bootdriver}\n\
         extfs \n\
         {host}",
        name = machine.name,
        ramsize = u64::from(megabytes) * 1024 * 1024,
        width = display.width,
        height = display.height,
        scale = display.scale,
        nosound = !settings.sound,
        nocdrom = !cdrom,
        jit = JIT,
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

/// The emulator's arguments. Paths go here, as the file's lines are cut at 256
/// bytes.
fn arguments(request: &LaunchRequest<'_>, preferences: &Path, cdroms: &[PathBuf]) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        "--config".into(),
        preferences.into(),
        "--rom".into(),
        request.rom.into(),
    ];
    for disk in request.disks {
        arguments.extend(["--disk".into(), disk_argument(disk)]);
    }
    for cdrom in cdroms {
        arguments.extend(["--cdrom".into(), cdrom.into()]);
    }
    if let Some(folder) = settings(request.machine).shared_folder {
        arguments.extend(["--extfs".into(), folder.into()]);
    }
    arguments
}

/// The `doctor` line for the contents of `/proc/sys/vm/mmap_min_addr`.
fn low_memory_check(mmap_min_addr: &str) -> Check {
    match mmap_min_addr.trim() {
        "0" => Check::ok("programs may map low memory, as SheepShaver needs"),
        value => Check::fail(
            format!("vm.mmap_min_addr is {value}, and SheepShaver needs low memory at address 0"),
            LOW_MEMORY_FIX,
        ),
    }
}

impl EmulatorBackend for SheepShaver {
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
            "pinned to SheepShaver {recorded}, which isn't known to behave the same as {}, the \
             revision this host builds; upgrade macman",
            self.pin.revision
        )
    }

    fn validate(&self, machine: &Machine) -> Result<()> {
        self.resolve_revision(&machine.emulator.revision)
            .with_context(|| format!("machine {:?} can't run here", machine.name))?;
        if machine.hardware.model.is_some() {
            bail!(
                "machine {:?} names a Mini vMac model, which SheepShaver doesn't have",
                machine.name
            );
        }
        validate_memory(&machine.hardware.memory)?;
        validate_display(&machine.display)?;
        if WINDOWS && settings(machine).shared_folder.is_some() {
            macemu_windows::refuse_shared_folder(LABEL)?;
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
        if registry::rom_file_is_powerpc(file, rom.size)? {
            return Ok(());
        }
        bail!(
            "ROM {:?} can't start SheepShaver, which needs a 4M ROM from a PCI Power Mac, or a \
             `Mac OS ROM` file; it won't run from a 68k ROM",
            rom.name
        )
    }

    fn check_disk(&self, path: &Path, writable: bool) -> Result<()> {
        if WINDOWS {
            macemu_windows::check_disk_size(path, LABEL)?;
        }
        if writable && disk::classify(path)? == Classification::Supported(DiskFormat::DiskCopy42) {
            bail!(
                "{} is a Disk Copy 4.2 image, and SheepShaver would change it without updating \
                 its checksum; pass --readonly, or take the volume out with `macman disk extract`",
                path.display()
            );
        }
        Ok(())
    }

    fn needs_unmounted_boot_volume(&self) -> bool {
        true
    }

    fn installs_from_cd(&self) -> bool {
        INSTALLS_FROM_CD
    }

    fn mounts_partitioned_disks(&self) -> bool {
        true
    }

    fn prepare(&self, machine: &Machine, verbose: bool) -> Result<Executable> {
        self.check_host()?;
        self.validate(machine)?;
        let toolchain = self.toolchain_identity()?;
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
            let disks = request.disks.iter().map(|disk| &disk.path);
            for path in disks.chain(request.cdroms.iter().map(|cdrom| &cdrom.path)) {
                macemu::refuse_held_disk(path, LABEL)?;
            }
        }
        let file = request.workdir.join(PREFERENCES);
        fs::write(&file, preferences(request.machine, request.cdroms))
            .with_context(|| format!("cannot write {}", file.display()))?;

        let arguments = if WINDOWS {
            macemu_windows::arguments(request, PREFERENCES, LABEL)?
        } else {
            let cdroms = macemu::cd_paths(request.workdir, request.cdroms)?;
            arguments(request, &file, &cdroms)
        };
        let mut command = Command::new(&request.executable.path);
        command.args(arguments).current_dir(request.workdir);
        supervise(command, LABEL, request.verbose).map_err(|error| {
            let message = error.to_string();
            if LOW_MEMORY_AT_ZERO && message.contains(LOW_MEMORY_ERROR) {
                anyhow!("{}; {LOW_MEMORY_FIX}", message.trim_end_matches('.'))
            } else {
                error
            }
        })
    }

    fn doctor(&self) -> Vec<Check> {
        if HOST.is_none() {
            return vec![Check::warn(
                "SheepShaver isn't available on this host",
                UNSUPPORTED_HOST,
            )];
        }
        if WINDOWS {
            // Mini vMac's checks already cover the rest of MinGW.
            let mut checks = vec![if tool_exists("g++") {
                Check::ok("g++ found, for SheepShaver")
            } else {
                Check::fail(
                    "g++ not found, which SheepShaver needs",
                    "install MinGW-w64, then open a new terminal: \
                     winget install BrechtSanders.WinLibs.POSIX.UCRT",
                )
            }];
            checks.push(self.source().check());
            checks.push(self.sdl_source().check());
            return checks;
        }
        if MACOS {
            let mut checks = tool_checks(macemu::MACOS_TOOLS, LABEL, MACOS_INSTALL);
            checks.extend(library_checks(MACOS_LIBRARIES, LABEL, MACOS_INSTALL));
            checks.push(self.source().check());
            return checks;
        }
        let mut checks = tool_checks(TOOLS, LABEL, INSTALL);
        checks.extend(library_checks(MACOS_LIBRARIES, LABEL, INSTALL));
        checks.push(self.source().check());
        if let Ok(value) = fs::read_to_string(MMAP_MIN_ADDR) {
            checks.push(low_memory_check(&value));
        }
        checks.extend(display_check(LABEL));
        checks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CheckLevel, ResolvedDisk};
    use crate::disk::CdVolume;
    use crate::machine::{EmulatorConfig, Hardware};

    fn backend() -> SheepShaver {
        SheepShaver::new(Paths::under(Path::new("/nonexistent")), macemu::pin(), None)
    }

    fn machine() -> Machine {
        Machine {
            schema_version: 1,
            name: "beige".into(),
            id: None,
            emulator: EmulatorConfig {
                backend: BACKEND_NAME.into(),
                revision: macemu::pin().revision,
            },
            hardware: Hardware {
                model: None,
                rom: "r".into(),
                memory: "128M".into(),
            },
            display: Display {
                width: 800,
                height: 600,
                depth: 32,
                scale: 2,
                fullscreen: false,
            },
            system: Default::default(),
            basilisk2: None,
            sheepshaver: Some(SheepShaverSettings::default()),
            disks: vec![],
        }
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().filter(|line| !line.starts_with('#')).collect()
    }

    #[test]
    fn translates_a_machine_to_preferences() {
        let mut expected = vec![
            "nogui true",
            "title beige",
            "ramsize 134217728",
            "screen win/800/600",
            "mag_rate 2",
            "nosound false",
            "nocdrom true",
            if JIT { "jit true" } else { "jit false" },
            "idlewait true",
            "bootdrive 0",
            "bootdriver 0",
            "extfs ",
        ];
        if WINDOWS {
            expected.extend(["enableextfs false", "pollmedia false"]);
        }
        assert_eq!(lines(&preferences(&machine(), &[])), expected);
    }

    #[test]
    #[cfg(windows)]
    fn a_shared_folder_is_refused_where_only_whole_drives_can_be_shared() {
        let mut m = machine();
        m.sheepshaver.as_mut().unwrap().shared_folder = Some("C:\\shared".into());
        let error = backend().validate(&m).unwrap_err().to_string();
        assert!(error.contains("whole drives"), "{error}");
    }

    /// An unknown keyword would only be a warning in the emulator's output.
    #[test]
    fn only_preferences_sheepshaver_has_are_written() {
        let text = preferences(&machine(), &[]);
        for keyword in ["cpu ", "fpu ", "modelid ", "displaycolordepth "] {
            assert!(!text.contains(keyword), "{keyword}: {text}");
        }
    }

    #[test]
    fn a_cd_turns_on_its_driver_and_is_what_starts() {
        let hfs = CdImage {
            path: "/installer.iso".into(),
            volume: CdVolume::Hfs,
        };
        let iso = CdImage {
            path: "/data.iso".into(),
            volume: CdVolume::Iso9660,
        };
        let text = preferences(&machine(), &[iso.clone(), hfs]);
        for line in ["nocdrom false", "bootdriver -62"] {
            assert!(text.lines().any(|l| l == line), "{line}: {text}");
        }
        // Starting from a disc with no HFS would fail, and leave it unmounted.
        let text = preferences(&machine(), &[iso]);
        for line in ["nocdrom false", "bootdriver 0"] {
            assert!(text.lines().any(|l| l == line), "{line}: {text}");
        }
    }

    #[test]
    fn options_reach_the_preferences() {
        let mut m = machine();
        m.display.fullscreen = true;
        m.sheepshaver = Some(SheepShaverSettings {
            sound: false,
            network: Network::Slirp,
            shared_folder: None,
        });
        let text = preferences(&m, &[]);
        for line in ["screen dga/800/600", "nosound true", "ether slirp"] {
            assert!(lines(&text).contains(&line), "{line}: {text}");
        }
    }

    #[test]
    fn the_guest_gets_no_host_files_unless_it_is_given_a_folder() {
        let text = preferences(&machine(), &[]);
        assert!(
            text.lines().any(|line| line.trim_end() == "extfs"),
            "{text}"
        );
    }

    #[test]
    fn no_preference_line_can_hold_a_path_or_outgrow_the_parser() {
        for line in preferences(&machine(), &[]).lines() {
            assert!(line.len() < 256, "{line}");
            assert!(!line.contains('/') || line.starts_with("screen"), "{line}");
        }
    }

    #[test]
    fn disks_keep_their_order_and_read_only_ones_are_marked() {
        let mut m = machine();
        m.sheepshaver.as_mut().unwrap().shared_folder = Some("/shared folder".into());
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
            rom: Path::new("/roms/ppc.rom"),
            disks: &disks,
            cdroms: &[],
            workdir: Path::new("/session"),
            verbose: false,
        };
        let cdroms = [PathBuf::from("/installer disc.iso")];
        assert_eq!(
            arguments(&request, Path::new("/session/prefs"), &cdroms),
            [
                "--config",
                "/session/prefs",
                "--rom",
                "/roms/ppc.rom",
                "--disk",
                "/boot disk.dsk",
                "--disk",
                "*/session/1-app.dsk",
                "--cdrom",
                "/installer disc.iso",
                "--extfs",
                "/shared folder",
            ]
        );
    }

    #[test]
    fn rejects_what_the_emulator_would_quietly_change() {
        type Change = fn(&mut Machine);
        let cases: [(Change, &str); 8] = [
            (|m| m.hardware.memory = "15M".into(), "16M to 1024M"),
            (|m| m.hardware.memory = "1025M".into(), "16M to 1024M"),
            (|m| m.hardware.memory = "32768K".into(), "whole megabytes"),
            (|m| m.display.depth = 8, "can't be chosen"),
            (|m| m.display.width = 512, "out of range"),
            (|m| m.display.height = 342, "out of range"),
            (|m| m.display.scale = 5, "scale 5"),
            (
                |m| m.hardware.model = Some(crate::model::Model::II),
                "Mini vMac model",
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
    fn the_limits_themselves_are_accepted() {
        for memory in ["16M", "1024M"] {
            let mut m = machine();
            m.hardware.memory = memory.into();
            backend().validate(&m).unwrap();
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
        let error = backend().check_disk(&path, true).unwrap_err().to_string();
        assert!(error.contains("--readonly"), "{error}");
        backend().check_disk(&path, false).unwrap();
    }

    #[test]
    fn doctor_names_the_fix_for_low_memory() {
        assert_eq!(low_memory_check("0\n").level, CheckLevel::Ok);
        let check = low_memory_check("65536\n");
        assert_eq!(check.level, CheckLevel::Fail);
        assert!(check.message.contains("65536"), "{}", check.message);
        assert!(check.fix.unwrap().contains("vm.mmap_min_addr=0"));
    }
}
