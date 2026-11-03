//! Builds Basilisk II on Windows with what Mini vMac already needs, so no MSYS2
//! or autotools. Upstream's README only covers 32-bit MSYS2, but `configure.ac`
//! has an x86-64 path, and the same source boots once the things below are
//! seen to. This stands in for `configure`: it writes the `config.h` that
//! would have produced, and a Makefile with `BasiliskII/src/Windows/Makefile.in`'s
//! file lists and none of its need for a POSIX shell. SDL comes from upstream's
//! own MinGW package, and is linked statically, so a build is a single file.
//!
//! - RAM must sit below the frame buffer. The guest's addresses are 32-bit
//!   offsets from RAM, so one below it wraps around and lands nowhere, and the
//!   screen stays black. `vm_alloc.cpp` ignores `VM_MAP_32BIT` on Windows, so
//!   [`allocate_upwards`] keeps every area low, and each above the last.
//! - `nogui` silences `ErrorAlert` here, where Unix prints it. [`report_errors`]
//!   makes it print, as the only other sign of a refused ROM is exit code 0.
//! - The tree's `config.h` describes 32-bit MSVC. [`config_header`] corrects it.
//!   `FPU_IEEE` needs the x87 code in `X86_64_ASSEMBLY`, as without that
//!   `mathlib.h` gives a Windows `long double` build no math functions.
//! - SDL hands `main` UTF-8, while the narrow calls that open disks read the
//!   system code page, so a path outside ASCII would fail, and silently.
//!   [`MANIFEST`] has Windows make those calls read UTF-8 as well. Short names
//!   won't do: NTFS keeps characters like `ë` in them.
//!
//! SheepShaver's Windows port shares its startup's habits and its disk code,
//! so what's here about launching serves both; [`super::sheepshaver_windows`]
//! builds it.

use super::minivmac::{gcc_target, is_mingw_x64, make_tool, short_path};
use super::source::SourcePin;
use super::{LaunchRequest, macemu::first_line};
use crate::fsutil::{atomic_copy, without_verbatim_prefix};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

pub const EXECUTABLE: &str = "BasiliskII.exe";
pub const BUILD_DIRECTORY: &str = "BasiliskII/src/Windows";
/// Part of the build key. Change it when the steps here change what's built.
pub const RECIPE: &str = "recipe 1";
/// The session's copy of the ROM.
const ROM_COPY: &str = "rom";
/// `Sys_open` reads a disk's size as 32 bits.
pub(super) const MAX_DISK: u64 = 1 << 32;
const MAX_PATH: usize = 260;
/// Windows 10 version 1903 and later honor this for the narrow API.
pub(super) const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1">
  <application>
    <windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#;
/// `RT_MANIFEST`, as the resource for an executable.
const MANIFEST_RESOURCE: &str = "\n1 24 \"BasiliskII.manifest\"\n";
/// The preprocessor flags `configure` would choose, less the JIT, which is
/// off on this host in `configure.ac` too.
pub const DEFINES: &[&str] = &[
    "-DHAVE_CONFIG_H",
    "-DDIRECT_ADDRESSING",
    "-DUNALIGNED_PROFITABLE",
    "-DFPU_IEEE",
    "-DOPTIMIZED_FLAGS",
    "-DX86_64_ASSEMBLY",
];
/// `Makefile.in`'s `SRCS`, with `CDENABLESRCS`, `ROUTERSRCS`, and `CPUSRCS`
/// spelled out. The generated `cpuemu.cpp` is compiled in parts as well.
pub const SOURCES: &[&str] = &[
    "../main.cpp",
    "main_windows.cpp",
    "../prefs.cpp",
    "../prefs_items.cpp",
    "prefs_windows.cpp",
    "sys_windows.cpp",
    "../rom_patches.cpp",
    "../slot_rom.cpp",
    "../rsrc_patches.cpp",
    "../emul_op.cpp",
    "../macos_util.cpp",
    "../xpram.cpp",
    "xpram_windows.cpp",
    "../timer.cpp",
    "timer_windows.cpp",
    "../adb.cpp",
    "../serial.cpp",
    "serial_windows.cpp",
    "../ether.cpp",
    "ether_windows.cpp",
    "../sony.cpp",
    "../disk.cpp",
    "../cdrom.cpp",
    "../scsi.cpp",
    "../dummy/scsi_dummy.cpp",
    "../video.cpp",
    "../SDL/video_sdl.cpp",
    "../SDL/video_sdl2.cpp",
    "../SDL/video_sdl3.cpp",
    "video_blit.cpp",
    "../audio.cpp",
    "../SDL/audio_sdl.cpp",
    "../SDL/audio_sdl3.cpp",
    "clip_windows.cpp",
    "../extfs.cpp",
    "extfs_windows.cpp",
    "../user_strings.cpp",
    "user_strings_windows.cpp",
    "vm_alloc.cpp",
    "sigsegv.cpp",
    "posix_emu.cpp",
    "util_windows.cpp",
    "../dummy/prefs_editor_dummy.cpp",
    "cdenable/cache.cpp",
    "cdenable/eject_nt.cpp",
    "cdenable/ntcd.cpp",
    "router/arp.cpp",
    "router/dump.cpp",
    "router/dynsockets.cpp",
    "router/ftp.cpp",
    "router/icmp.cpp",
    "router/mib/interfaces.cpp",
    "router/iphelp.cpp",
    "router/ipsocket.cpp",
    "router/mib/mibaccess.cpp",
    "router/router.cpp",
    "router/tcp.cpp",
    "router/udp.cpp",
    "b2ether/packet32.cpp",
    "../uae_cpu/basilisk_glue.cpp",
    "../uae_cpu/memory.cpp",
    "../uae_cpu/newcpu.cpp",
    "../uae_cpu/readcpu.cpp",
    "../uae_cpu/fpu/fpu_ieee.cpp",
    "cpustbl.cpp",
    "cpudefs.cpp",
];
/// `Makefile.in`'s `SLIRP_SRCS`, which SheepShaver's has too.
pub const SLIRP_SOURCES: &[&str] = &[
    "bootp",
    "cksum",
    "debug",
    "if",
    "ip_icmp",
    "ip_input",
    "ip_output",
    "mbuf",
    "misc",
    "sbuf",
    "slirp",
    "socket",
    "tcp_input",
    "tcp_output",
    "tcp_subr",
    "tcp_timer",
    "tftp",
    "udp",
];
/// `Makefile.in`'s `XPLATSRCS`, which it links in from `CrossPlatform`.
pub const CROSS_PLATFORM_SOURCES: &[&str] = &[
    "vm_alloc.cpp",
    "vm_alloc.h",
    "sigsegv.cpp",
    "sigsegv.h",
    "video_vosf.h",
    "video_blit.cpp",
    "video_blit.h",
];
/// What `sdl2-config --static-libs` lists, as linked from `Makefile.in`'s `LIBS`.
pub(super) const LIBRARIES: &str = "-lws2_32 -lwsock32 -liphlpapi -lmingw32 $(SDL)/lib/libSDL2main.a \
                         $(SDL)/lib/libSDL2.a -mwindows -lm -ldinput8 -ldxguid -ldxerr8 -luser32 \
                         -lgdi32 -lwinmm -limm32 -lole32 -loleaut32 -lshell32 -lsetupapi \
                         -lversion -luuid";
/// The math functions the tree's `config.h` leaves out for MSVC, and MinGW has.
const MATH_FUNCTIONS: &[&str] = &[
    "ACOSHL", "ACOSL", "ASINHL", "ASINL", "ATANH", "ATANHL", "ATANL", "CEILL", "COSHL", "COSL",
    "EXPL", "FABSL", "FINITE", "FLOORL", "ISINF", "ISINFL", "ISNAN", "ISNANL", "ISNORMAL",
    "LOG10L", "LOGL", "POWL", "SIGNBIT", "SINHL", "SINL", "SQRTL", "TANHL", "TANL",
];

/// The SDL 2 development package for MinGW, as released by upstream, whose
/// static library needs nothing else at run time.
pub fn sdl_pin() -> SourcePin {
    const VERSION: &str = "2.32.10";
    SourcePin {
        revision: VERSION.into(),
        url: format!(
            "https://github.com/libsdl-org/SDL/releases/download/release-{VERSION}/\
             SDL2-devel-{VERSION}-mingw.tar.gz"
        ),
        sha256: "83a5d74012311edc3c0d40ea6faecbe57ad692aa033fa5dc273cc937e3938ff2".into(),
    }
}

/// What a build depends on besides the source: the compiler, SDL, and `recipe`.
pub fn toolchain_identity(sdl_pin: &SourcePin, recipe: &str) -> Result<String> {
    let hint = "install a C++ compiler (run `macman doctor`)";
    let compiler = first_line("g++", &["--version"]).context(hint)?;
    let target = gcc_target()?;
    if !is_mingw_x64(&target) {
        bail!("gcc targets {target}, not 64-bit Windows; run `macman doctor`");
    }
    Ok(format!(
        "{target} {compiler} SDL {} {recipe}",
        sdl_pin.sha256
    ))
}

/// What the emulator can open, or why it can't. A session's own disk goes by
/// its name, which keeps it short.
fn disk_path(disk: &Path, workdir: &Path, label: &str) -> Result<PathBuf> {
    let path = match disk.strip_prefix(workdir) {
        Ok(name) => name.to_path_buf(),
        Err(_) => without_verbatim_prefix(disk),
    };
    // Bytes, in UTF-8, not characters: that's what its buffers hold.
    if path.to_string_lossy().len() >= MAX_PATH {
        bail!(
            "{} is too long for {label}, which handles paths under {MAX_PATH} bytes; move it to \
             a shorter path",
            disk.display()
        );
    }
    Ok(path)
}

/// The emulator's arguments, with `preferences` already in the session. The
/// ROM is copied there, so that everything of the session's goes by its name.
pub(super) fn arguments(
    request: &LaunchRequest<'_>,
    preferences: &str,
    label: &str,
) -> Result<Vec<OsString>> {
    atomic_copy(request.rom, &request.workdir.join(ROM_COPY))?;
    let mut arguments: Vec<OsString> = vec![
        "--config".into(),
        preferences.into(),
        "--rom".into(),
        ROM_COPY.into(),
    ];
    for disk in request.disks {
        let mut value = OsString::from(if disk.writable { "" } else { "*" });
        value.push(disk_path(&disk.path, request.workdir, label)?);
        arguments.extend(["--disk".into(), value]);
    }
    Ok(arguments)
}

/// Refuse a disk whose size `Sys_open` would misread.
pub(super) fn check_disk_size(path: &Path, label: &str) -> Result<()> {
    if fs::metadata(path)?.len() >= MAX_DISK {
        bail!(
            "{} is too big for {label} on Windows, which handles disks under 4G",
            path.display()
        );
    }
    Ok(())
}

/// `extfs` is ignored, and `enableextfs` shows the guest whole drives.
pub(super) fn refuse_shared_folder(label: &str) -> Result<()> {
    bail!(
        "shared folders aren't available with {label} on Windows, where it can only show the \
         guest whole drives"
    )
}

/// `text` with one `from` turned into `to`, or an error if upstream's source
/// isn't what this was written against.
pub(super) fn replace_once(text: &str, from: &str, to: &str, file: &str) -> Result<String> {
    if !text.contains(from) {
        bail!("{file} lacks the expected code");
    }
    Ok(text.replacen(from, to, 1))
}

/// `config.h` as `configure` would leave it for 64-bit MinGW. Video on SEGV is
/// off, as it is by default in `configure.ac`, because SDL doesn't need it.
fn config_header(text: &str) -> Result<String> {
    const FILE: &str = "config.h";
    let text = replace_once(
        text,
        "#ifndef _DEBUG\n#define ENABLE_VOSF 1\n#endif\n",
        "",
        FILE,
    )?;
    let text = replace_once(
        &text,
        "#define SIZEOF_LONG_DOUBLE 8\n",
        "#define SIZEOF_LONG_DOUBLE 16\n",
        FILE,
    )?;
    let mut text = replace_once(
        &text,
        "#define SIZEOF_VOID_P 4\n",
        "#define SIZEOF_VOID_P 8\n",
        FILE,
    )?;
    text.push_str("\n#define USE_SDL2 1\n");
    for function in MATH_FUNCTIONS {
        text.push_str(&format!(
            "#undef HAVE_{function}\n#define HAVE_{function} 1\n"
        ));
    }
    Ok(text)
}

/// Keep what `vm_acquire` hands out in the low 4G, in ascending order.
fn allocate_upwards(text: &str) -> Result<String> {
    replace_once(
        text,
        "\tif ((addr = VirtualAlloc(NULL, size, alloc_type, PAGE_EXECUTE_READWRITE)) == NULL)\n\
         \t\treturn VM_MAP_FAILED;\n",
        "\tstatic vm_uintptr_t next_area = 0x10000000;\n\
         \taddr = NULL;\n\
         \tfor (; addr == NULL && next_area < 0xF0000000; next_area += 0x100000)\n\
         \t\taddr = VirtualAlloc((LPVOID)next_area, size, alloc_type, PAGE_EXECUTE_READWRITE);\n\
         \tif (addr == NULL)\n\
         \t\treturn VM_MAP_FAILED;\n\
         \tnext_area = ((vm_uintptr_t)addr + size + 0xFFFF) & ~(vm_uintptr_t)0xFFFF;\n",
        "vm_alloc.cpp",
    )
}

/// Print errors the way Unix does without a GUI, for macman to read. There's
/// an `ErrorAlert` for each of `formats`, in order, each for its string type.
pub(super) fn report_errors(text: &str, formats: &[&str]) -> Result<String> {
    const FILE: &str = "main_windows.cpp";
    const SILENT: &str = "\tif (PrefsFindBool(\"nogui\"))\n\t\treturn;\n\n\tVideoQuitFullScreen();";
    let mut text = text.to_string();
    for format in formats {
        let print = format!(
            "\tif (PrefsFindBool(\"nogui\")) {{\n\t\tprintf(\"ERROR: {format}\\n\", text);\n\
             \t\treturn;\n\t}}\n\n\tVideoQuitFullScreen();"
        );
        text = replace_once(&text, SILENT, &print, FILE)?;
    }
    Ok(text)
}

fn stem(source: &str) -> &str {
    let name = source.rsplit('/').next().unwrap_or(source);
    name.strip_suffix(".cpp").unwrap_or(name)
}

/// Every recipe is one plain command, as make may be running without a shell.
/// `sdl` is relative, and `unistd.h` brings in `usleep`, which `config.h` keeps
/// from the sources that want it.
fn makefile(sdl: &str, jobs: usize) -> String {
    let objects: Vec<String> = SOURCES
        .iter()
        .map(|source| stem(source).to_string())
        .chain(["BasiliskII".to_string()])
        .chain(SLIRP_SOURCES.iter().map(|name| format!("slirp-{name}")))
        .chain((1..=8).map(|part| format!("cpuemu{part}")))
        .map(|name| format!("obj/{name}.o"))
        .collect();
    format!(
        "MAKEFLAGS += -j{jobs}\n\
         SDL := {sdl}\n\
         DEFS := {defines}\n\
         CPPFLAGS := -I../include -I. -I../CrossPlatform -I../uae_cpu -I../slirp \
         -I$(SDL)/include -I$(SDL)/include/SDL2\n\
         CFLAGS := -O2 -fwrapv -w\n\
         CXXFLAGS := -include unistd.h -O2 -fwrapv -fpermissive -w\n\
         OBJS := {objects}\n\
         VPATH := ..:../SDL:../dummy:../uae_cpu:../uae_cpu/fpu:cdenable:router:router/mib:b2ether\n\
         {EXECUTABLE}: $(OBJS)\n\
         \tg++ -o $@ -s -static $(OBJS) {LIBRARIES}\n\
         obj/cpuemu%.o: cpuemu.cpp\n\
         \tg++ $(CPPFLAGS) $(DEFS) -DPART_$* $(CXXFLAGS) -c $< -o $@\n\
         obj/slirp-%.o: ../slirp/%.c\n\
         \tgcc $(CPPFLAGS) $(DEFS) $(CFLAGS) -fno-strict-aliasing -c $< -o $@\n\
         obj/%.o: %.cpp\n\
         \tg++ $(CPPFLAGS) $(DEFS) $(CXXFLAGS) -c $< -o $@\n\
         obj/BasiliskII.o: BasiliskII.rc\n\
         \twindres --include-dir . -i $< -o $@\n",
        defines = DEFINES.join(" "),
        objects = objects.join(" "),
    )
}

/// Run `command` in `directory`, with its output in `log` unless `stdout` is set.
pub(super) fn run(
    what: &str,
    command: &mut Command,
    directory: &Path,
    stdin: Stdio,
    stdout: Option<File>,
    log: &File,
) -> Result<()> {
    let status = command
        .current_dir(directory)
        .stdin(stdin)
        .stdout(match stdout {
            Some(file) => Stdio::from(file),
            None => Stdio::from(log.try_clone()?),
        })
        .stderr(log.try_clone()?)
        .status()
        .with_context(|| format!("cannot run {what}"))?;
    if !status.success() {
        bail!("{what} exited with {status}");
    }
    Ok(())
}

/// Unpack one archive into `directory`. It's piped, as GNU tar would read
/// `C:\...` as a remote host, and `exclude` keeps out what isn't needed, which
/// includes macemu's symbolic links, which Windows would refuse.
pub(super) fn unpack(archive: &Path, directory: &Path, exclude: &[&str], log: &File) -> Result<()> {
    let mut tar = Command::new("tar");
    tar.args(["xzf", "-", "--strip-components=1"]);
    for pattern in exclude {
        tar.arg(format!("--exclude={pattern}"));
    }
    let source = File::open(archive).context("cannot read source archive")?;
    run("tar", &mut tar, directory, source.into(), None, log)
}

pub(super) fn edit(file: &Path, change: impl Fn(&str) -> Result<String>) -> Result<()> {
    let text =
        fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    fs::write(file, change(&text)?).with_context(|| format!("cannot write {}", file.display()))
}

/// Compile `src` into [`BUILD_DIRECTORY`]. Every path stays relative, for make.
pub fn build(archive: &Path, sdl_archive: &Path, src: &Path, log: &File) -> Result<()> {
    let ascii_src = short_path(src);
    let src = ascii_src.as_path();
    unpack(archive, src, &["*/SheepShaver", "*/cxmon"], log)?;
    let sdl = src.join("sdl");
    fs::create_dir(&sdl).context("cannot create SDL directory")?;
    unpack(
        sdl_archive,
        &sdl,
        &["*/i686-w64-mingw32", "*/docs", "*/test"],
        log,
    )?;

    let dir = src.join(BUILD_DIRECTORY);
    edit(&dir.join("config.h"), config_header)?;
    for file in CROSS_PLATFORM_SOURCES {
        let source = dir.join("../CrossPlatform").join(file);
        fs::copy(&source, dir.join(file))
            .with_context(|| format!("cannot copy {}", source.display()))?;
    }
    edit(&dir.join("vm_alloc.cpp"), allocate_upwards)?;
    edit(&dir.join("main_windows.cpp"), |text| {
        report_errors(text, &["%s", "%ls"])
    })?;
    fs::write(dir.join("BasiliskII.manifest"), MANIFEST)?;
    edit(&dir.join("BasiliskII.rc"), |text| {
        Ok(format!("{text}{MANIFEST_RESOURCE}"))
    })?;
    fs::create_dir(dir.join("obj")).context("cannot create object directory")?;

    // `make` can't redirect portably without a shell, so these run from here.
    let includes = [
        "-I../include",
        "-I.",
        "-I../CrossPlatform",
        "-I../uae_cpu",
        "-I../slirp",
    ];
    run(
        "gcc",
        Command::new("gcc").args(includes).args(DEFINES).args([
            "-O2",
            "-w",
            "-o",
            "build68k.exe",
            "../uae_cpu/build68k.c",
        ]),
        &dir,
        Stdio::null(),
        None,
        log,
    )?;
    run(
        "build68k",
        Command::new(dir.join("build68k.exe")).arg("../uae_cpu/table68k"),
        &dir,
        Stdio::null(),
        Some(File::create(dir.join("cpudefs.cpp")).context("cannot create cpudefs.cpp")?),
        log,
    )?;
    run(
        "g++",
        Command::new("g++")
            .args(includes)
            .args(DEFINES)
            .args(["-fpermissive", "-w", "-O2", "-o", "gencpu.exe", "-x", "c++"])
            .args(["../uae_cpu/gencpu.c", "-x", "none"])
            .args(["../uae_cpu/readcpu.cpp", "cpudefs.cpp"]),
        &dir,
        Stdio::null(),
        None,
        log,
    )?;
    run(
        "gencpu",
        &mut Command::new(dir.join("gencpu.exe")),
        &dir,
        Stdio::null(),
        None,
        log,
    )?;

    let jobs = thread::available_parallelism().map_or(1, |jobs| jobs.get());
    let sdl_from_build = "../../../sdl/x86_64-w64-mingw32";
    fs::write(dir.join("GNUmakefile"), makefile(sdl_from_build, jobs))?;
    make(&dir, src, log, "Basilisk II")
}

/// Run the `GNUmakefile` in `dir`, under `src`, which is as found by
/// [`short_path`].
pub(super) fn make(dir: &Path, src: &Path, log: &File, label: &str) -> Result<()> {
    run(
        "make",
        &mut Command::new(make_tool()),
        dir,
        Stdio::null(),
        None,
        log,
    )
    .with_context(|| {
        if src.to_string_lossy().is_ascii() {
            format!("cannot compile {label}")
        } else {
            format!(
                "MinGW make can't work in {}, which has no ASCII short name; set XDG_CACHE_HOME \
                 to a plain ASCII path",
                src.display()
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "#ifndef _DEBUG\n#define ENABLE_VOSF 1\n#endif\n\
                          #define SIZEOF_LONG_DOUBLE 8\n#define SIZEOF_VOID_P 4\n";

    #[test]
    fn the_config_describes_64_bit_mingw() {
        let text = config_header(CONFIG).unwrap();
        assert!(!text.contains("ENABLE_VOSF"), "{text}");
        assert!(text.contains("#define SIZEOF_VOID_P 8\n"), "{text}");
        assert!(text.contains("#define SIZEOF_LONG_DOUBLE 16\n"), "{text}");
        assert!(text.contains("#define USE_SDL2 1\n"), "{text}");
        assert!(text.contains("#define HAVE_LOGL 1\n"), "{text}");
    }

    #[test]
    fn the_allocator_goes_upwards_from_low_memory() {
        let text = allocate_upwards(
            "#elif defined(HAVE_WIN32_VM)\n\
             \tif ((addr = VirtualAlloc(NULL, size, alloc_type, PAGE_EXECUTE_READWRITE)) == NULL)\n\
             \t\treturn VM_MAP_FAILED;\n",
        )
        .unwrap();
        assert!(!text.contains("VirtualAlloc(NULL"), "{text}");
        assert!(text.contains("VirtualAlloc((LPVOID)next_area"), "{text}");
    }

    #[test]
    fn errors_are_printed_for_both_string_types() {
        let body = "\tif (PrefsFindBool(\"nogui\"))\n\t\treturn;\n\n\tVideoQuitFullScreen();\n";
        let text = report_errors(&format!("char {body}wide {body}"), &["%s", "%ls"]).unwrap();
        assert!(text.contains(r#"printf("ERROR: %s\n", text);"#), "{text}");
        assert!(text.contains(r#"printf("ERROR: %ls\n", text);"#), "{text}");
    }

    #[test]
    fn source_that_has_changed_stops_the_build() {
        let error = allocate_upwards("something else").unwrap_err();
        assert!(error.to_string().contains("vm_alloc.cpp"), "{error}");
        assert!(config_header("something else").is_err());
        assert!(report_errors("something else", &["%s"]).is_err());
    }

    #[test]
    fn a_session_disk_goes_by_its_name() {
        let session = Path::new("session");
        assert_eq!(
            disk_path(&session.join("1-app.dsk"), session, "E").unwrap(),
            Path::new("1-app.dsk")
        );
    }

    /// Longer ones would be cut short and open the wrong file, or none. This
    /// counts bytes, so 90 characters of Japanese are just over the limit.
    #[test]
    fn a_path_must_fit_the_emulator_s_buffers_in_bytes() {
        let session = Path::new("session");
        let fits = PathBuf::from(format!("{}.dsk", "x".repeat(MAX_PATH - 5)));
        assert_eq!(disk_path(&fits, session, "E").unwrap(), fits);
        for name in [
            format!("{}.dsk", "x".repeat(MAX_PATH - 4)),
            format!("{}.dsk", "日".repeat(MAX_PATH / 3)),
        ] {
            let error = disk_path(Path::new(&name), session, "E")
                .unwrap_err()
                .to_string();
            assert!(error.contains("too long for E"), "{error}");
        }
    }

    #[test]
    fn a_disk_whose_size_would_be_misread_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.dsk");
        File::create(&path).unwrap().set_len(MAX_DISK).unwrap();
        let error = check_disk_size(&path, "E").unwrap_err().to_string();
        assert!(error.contains("under 4G"), "{error}");
        File::create(&path).unwrap().set_len(MAX_DISK - 1).unwrap();
        check_disk_size(&path, "E").unwrap();
    }

    #[test]
    fn the_makefile_has_an_object_for_every_source_and_no_shell_syntax() {
        let text = makefile("../sdl", 4);
        for source in SOURCES {
            assert!(
                text.contains(&format!("obj/{}.o", stem(source))),
                "{source}"
            );
        }
        assert!(text.contains("obj/cpuemu8.o"), "{text}");
        assert!(text.contains("obj/slirp-udp.o"), "{text}");
        for recipe in text.lines().filter(|line| line.starts_with('\t')) {
            assert!(!recipe.contains(['>', '|', ';', '&']), "{recipe}");
        }
    }

    #[test]
    fn no_two_sources_share_an_object() {
        let mut stems: Vec<&str> = SOURCES.iter().map(|source| stem(source)).collect();
        stems.sort_unstable();
        stems.dedup();
        assert_eq!(stems.len(), SOURCES.len());
    }

    #[test]
    fn the_sdl_pin_names_its_version() {
        let pin = sdl_pin();
        assert!(pin.url.contains(&pin.revision));
        assert_eq!(pin.sha256.len(), 64);
    }
}
