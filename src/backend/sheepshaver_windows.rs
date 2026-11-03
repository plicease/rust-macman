//! Builds SheepShaver on Windows the way [`macemu_windows`] builds Basilisk II,
//! with MinGW-w64 and upstream's SDL package, so no MSYS2, autotools, or Perl.
//! `SheepShaver/src/Windows/configure.ac` has an x86-64 path, which this stands
//! in for. It differs from Basilisk II's in that:
//!
//! - There's no JIT. `configure.ac` turns it off on x86-64, as `dyngen` reads
//!   only 32-bit objects there, so the PowerPC code is interpreted.
//! - `MEM_BULK` reserves one block for the Mac's memory and offsets addresses
//!   into it, so nothing needs mapping at address 0 or below 4G, as on Linux.
//! - The tree has no `config.h` to correct, so [`CONFIG_HEADER`] is the whole
//!   of what `configure` would write.
//! - Most of the tree is symbolic links into Basilisk II's, which Windows won't
//!   make without privileges. [`unpack_source`] copies what each one names.
//! - `genexec.pl` lists the instruction handlers to instantiate, from the
//!   preprocessed decoder. [`execute_handlers`] does the same, without Perl.
//!
//! `ErrorAlert` and paths behave as Basilisk II's do, and so does the
//! manifest that has the narrow API read UTF-8.

use super::macemu_windows::{
    self, CROSS_PLATFORM_SOURCES, LIBRARIES, MANIFEST, SLIRP_SOURCES, edit, report_errors, run,
};
use super::minivmac::short_path;
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use tar::EntryType;

pub const EXECUTABLE: &str = "SheepShaver.exe";
pub const BUILD_DIRECTORY: &str = "SheepShaver/src/Windows";
/// Part of the build key. Change it when the steps here change what's built.
pub const RECIPE: &str = "recipe 1";
/// The preprocessor flags, with `MEM_BULK`, which `configure.ac` adds on x86-64.
pub const DEFINES: &[&str] = &["-DHAVE_CONFIG_H", "-DMEM_BULK"];
/// The parts of the tree a build reads: SheepShaver's, and Basilisk II's, which
/// its links name.
const KEPT: &[&str] = &["SheepShaver/src", "BasiliskII/src"];
const INCLUDES: &[&str] = &[
    "-I../include",
    "-I.",
    "-I../slirp",
    "-I../kpx_cpu/include",
    "-I../kpx_cpu/src",
];
/// The decoder, which `genexec.pl` reads preprocessed, and what it writes.
const DECODER: &str = "../kpx_cpu/src/cpu/ppc/ppc-decode.cpp";
const HANDLERS: &str = "ppc-execute-impl.cpp";
const MANIFEST_RESOURCE: &str = "\n1 24 \"SheepShaver.manifest\"\n";
/// What `configure` would find with 64-bit MinGW. Video on SEGV is off, as it
/// is by default in `configure.ac`.
const CONFIG_HEADER: &str = "/* Written by macman, as configure would for 64-bit MinGW. */
#define STDC_HEADERS 1
#define HAVE_INTTYPES_H 1
#define HAVE_MEMORY_H 1
#define HAVE_STDINT_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRINGS_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_UNISTD_H 1
#define HAVE_STRDUP 1
#define HAVE_STRERROR 1
#define HAVE_EXP2F 1
#define HAVE_LOG2F 1
#define HAVE_EXP2 1
#define HAVE_LOG2 1
#define HAVE_FLOORF 1
#define HAVE_ROUNDF 1
#define HAVE_CEILF 1
#define HAVE_TRUNCF 1
#define HAVE_FLOOR 1
#define HAVE_ROUND 1
#define HAVE_CEIL 1
#define HAVE_TRUNC 1
#define HAVE_WIN32_VM 1
#define HAVE_WIN32_EXCEPTIONS 1
#define HAVE_SIGSEGV_SKIP_INSTRUCTION 1
#define SIZEOF_SHORT 2
#define SIZEOF_INT 4
#define SIZEOF_LONG 4
#define SIZEOF_LONG_LONG 8
#define SIZEOF_FLOAT 4
#define SIZEOF_DOUBLE 8
#define SIZEOF_VOID_P 8
#define USE_SDL 1
#define USE_SDL2 1
#define USE_SDL_VIDEO 1
#define USE_SDL_AUDIO 1
#define PACKAGE \"SheepShaver\"
#define PACKAGE_BUGREPORT \"Christian.Bauer@uni-mainz.de\"
#define PACKAGE_NAME \"SheepShaver\"
#define PACKAGE_STRING \"SheepShaver 2.3\"
#define PACKAGE_TARNAME \"SheepShaver\"
#define PACKAGE_VERSION \"2.3\"
#define VERSION \"2.3\"
";
/// `Makefile.in`'s `SRCS`, with `CPUSRCS` and `ROUTERSRCS` as `configure.ac`
/// sets them for x86-64, without the JIT.
pub const SOURCES: &[&str] = &[
    "../main.cpp",
    "main_windows.cpp",
    "../prefs.cpp",
    "../prefs_items.cpp",
    "prefs_windows.cpp",
    "sys_windows.cpp",
    "cdenable/cache.cpp",
    "cdenable/eject_nt.cpp",
    "cdenable/ntcd.cpp",
    "../rom_patches.cpp",
    "../rsrc_patches.cpp",
    "../emul_op.cpp",
    "../name_registry.cpp",
    "../macos_util.cpp",
    "../timer.cpp",
    "timer_windows.cpp",
    "../xpram.cpp",
    "xpram_windows.cpp",
    "../adb.cpp",
    "../sony.cpp",
    "../disk.cpp",
    "../cdrom.cpp",
    "../scsi.cpp",
    "../dummy/scsi_dummy.cpp",
    "../gfxaccel.cpp",
    "../video.cpp",
    "../SDL/video_sdl.cpp",
    "../SDL/video_sdl2.cpp",
    "../SDL/video_sdl3.cpp",
    "video_blit.cpp",
    "../audio.cpp",
    "../SDL/audio_sdl.cpp",
    "../SDL/audio_sdl3.cpp",
    "../ether.cpp",
    "ether_windows.cpp",
    "../thunks.cpp",
    "../serial.cpp",
    "serial_windows.cpp",
    "../extfs.cpp",
    "extfs_windows.cpp",
    "about_window_windows.cpp",
    "../user_strings.cpp",
    "user_strings_windows.cpp",
    "../dummy/prefs_editor_dummy.cpp",
    "clip_windows.cpp",
    "util_windows.cpp",
    "vm_alloc.cpp",
    "sigsegv.cpp",
    "posix_emu.cpp",
    "../kpx_cpu/src/mathlib/ieeefp.cpp",
    "../kpx_cpu/src/mathlib/mathlib.cpp",
    "../kpx_cpu/src/cpu/ppc/ppc-cpu.cpp",
    "../kpx_cpu/src/cpu/ppc/ppc-decode.cpp",
    "../kpx_cpu/src/cpu/ppc/ppc-execute.cpp",
    "../kpx_cpu/src/cpu/ppc/ppc-translate.cpp",
    "../kpx_cpu/src/utils/utils-cpuinfo.cpp",
    "../kpx_cpu/sheepshaver_glue.cpp",
    "../kpx_cpu/ppc-dis.c",
    "../dummy/prefs_dummy.cpp",
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
];

/// What `genexec.pl` prints for the preprocessed decoder: an instantiation of
/// each handler it names, once each, in order. Upstream's script also expands
/// handlers from `DEFINE_TEMPLATE` decoders, which the pinned source doesn't
/// have, so a decoder with them stops the build rather than lose handlers.
fn execute_handlers(preprocessed: &str) -> Result<String> {
    const PREFIX: &str = "powerpc_cpu::execute_";
    if preprocessed.contains("DEFINE_TEMPLATE") {
        bail!("ppc-decode.cpp has templates, which macman doesn't expand");
    }
    let mut seen = HashSet::new();
    let mut output = String::new();
    for segment in preprocessed.lines().flat_map(|line| line.split(';')) {
        let Some((name, arguments)) = segment.match_indices(PREFIX).find_map(|(start, _)| {
            let rest = &segment[start + PREFIX.len()..];
            let word = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            // `\w+` needs one character at least.
            if word == 0 {
                return None;
            }
            let arguments = rest[word..].strip_prefix('<')?;
            // `<(.+)>` takes everything up to the last `>`.
            let end = arguments.rfind('>').filter(|&end| end > 0)?;
            Some((
                &segment[start..start + PREFIX.len() + word],
                &arguments[..end],
            ))
        }) else {
            continue;
        };
        let line = format!("template void {name}<{arguments}>(uint32);");
        if seen.insert(line.clone()) {
            output.push_str(&line);
            output.push('\n');
        }
    }
    Ok(output)
}

/// `path` with `.` and `..` worked out, or None if it climbs out of the tree.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.iter().collect())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        // A folder holding only links has had nothing unpacked into it.
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).with_context(|| format!("cannot copy {}", from.display()))?;
    }
    Ok(())
}

/// Unpack what [`KEPT`] names from the source archive into `root`, with each
/// symbolic link a copy of what it names.
fn unpack_source(archive: &Path, root: &Path) -> Result<()> {
    let file = File::open(archive).context("cannot read source archive")?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut links = Vec::new();
    for entry in tar.entries().context("cannot read source archive")? {
        let mut entry = entry.context("cannot read source archive")?;
        let path = entry.path()?.into_owned();
        // GitHub's archives hold everything in one directory, named for the commit.
        let relative = normalize(&path.components().skip(1).collect::<PathBuf>())
            .with_context(|| format!("{} is outside the source tree", path.display()))?;
        if !KEPT.iter().any(|kept| relative.starts_with(kept)) {
            continue;
        }
        let destination = root.join(&relative);
        match entry.header().entry_type() {
            EntryType::Directory => fs::create_dir_all(&destination)?,
            EntryType::Regular => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                entry
                    .unpack(&destination)
                    .with_context(|| format!("cannot unpack {}", relative.display()))?;
            }
            EntryType::Symlink => {
                let target = entry.link_name()?.context("a link names nothing")?;
                let source = relative.parent().unwrap_or(Path::new("")).join(target);
                let source = normalize(&source)
                    .with_context(|| format!("{} links outside the tree", relative.display()))?;
                links.push((relative, source));
            }
            _ => {}
        }
    }
    // A link may name another, so go round until each has been copied.
    while !links.is_empty() {
        let before = links.len();
        let mut pending = Vec::new();
        for (link, source) in links {
            if root.join(&source).exists() {
                copy_tree(&root.join(&source), &root.join(&link))?;
            } else {
                pending.push((link, source));
            }
        }
        if pending.len() == before {
            let (link, source) = &pending[0];
            bail!(
                "{} links to {}, which the source doesn't have",
                link.display(),
                source.display()
            );
        }
        links = pending;
    }
    Ok(())
}

fn stem(source: &str) -> &str {
    let name = source.rsplit('/').next().unwrap_or(source);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// Every recipe is one plain command, as make may be running without a shell.
fn makefile(sdl: &str, jobs: usize) -> String {
    let objects: Vec<String> = SOURCES
        .iter()
        .map(|source| stem(source).to_string())
        .chain(["SheepShaver".to_string()])
        .chain(SLIRP_SOURCES.iter().map(|name| format!("slirp-{name}")))
        .map(|name| format!("obj/{name}.o"))
        .collect();
    let mut directories: Vec<&str> = SOURCES
        .iter()
        .filter_map(|source| source.rsplit_once('/').map(|(dir, _)| dir))
        .collect();
    directories.sort_unstable();
    directories.dedup();
    format!(
        "MAKEFLAGS += -j{jobs}\n\
         SDL := {sdl}\n\
         DEFS := {defines}\n\
         CPPFLAGS := {includes} -I$(SDL)/include -I$(SDL)/include/SDL2\n\
         CFLAGS := -O2 -w\n\
         CXXFLAGS := -include unistd.h -O2 -fpermissive -w\n\
         OBJS := {objects}\n\
         VPATH := {vpath}\n\
         {EXECUTABLE}: $(OBJS)\n\
         \tg++ -o $@ -s -static $(OBJS) {LIBRARIES}\n\
         obj/slirp-%.o: ../slirp/%.c\n\
         \tgcc $(CPPFLAGS) $(DEFS) $(CFLAGS) -fno-strict-aliasing -c $< -o $@\n\
         obj/%.o: %.c\n\
         \tgcc $(CPPFLAGS) $(DEFS) $(CFLAGS) -c $< -o $@\n\
         obj/%.o: %.cpp\n\
         \tg++ $(CPPFLAGS) $(DEFS) $(CXXFLAGS) -c $< -o $@\n\
         obj/SheepShaver.o: SheepShaver.rc\n\
         \twindres --include-dir . -i $< -o $@\n",
        defines = DEFINES.join(" "),
        includes = INCLUDES.join(" "),
        objects = objects.join(" "),
        vpath = directories.join(":"),
    )
}

/// Compile `src` into [`BUILD_DIRECTORY`]. Every path stays relative, for make.
pub fn build(archive: &Path, sdl_archive: &Path, src: &Path, log: &File) -> Result<()> {
    let ascii_src = short_path(src);
    let src = ascii_src.as_path();
    unpack_source(archive, src)?;
    let sdl = src.join("sdl");
    fs::create_dir(&sdl).context("cannot create SDL directory")?;
    macemu_windows::unpack(
        sdl_archive,
        &sdl,
        &["*/i686-w64-mingw32", "*/docs", "*/test"],
        log,
    )?;

    let dir = src.join(BUILD_DIRECTORY);
    fs::write(dir.join("config.h"), CONFIG_HEADER)?;
    for file in CROSS_PLATFORM_SOURCES {
        let source = dir.join("../CrossPlatform").join(file);
        fs::copy(&source, dir.join(file))
            .with_context(|| format!("cannot copy {}", source.display()))?;
    }
    // Upstream has this file with CRLF line ends, unlike Basilisk II's.
    edit(&dir.join("main_windows.cpp"), |text| {
        report_errors(&text.replace("\r\n", "\n"), &["%s"])
    })?;
    fs::write(dir.join("SheepShaver.manifest"), MANIFEST)?;
    edit(&dir.join("SheepShaver.rc"), |text| {
        Ok(format!("{text}{MANIFEST_RESOURCE}"))
    })?;
    fs::create_dir(dir.join("obj")).context("cannot create object directory")?;

    let preprocessed = dir.join("ppc-decode.i");
    run(
        "g++",
        Command::new("g++")
            .args(["-E", "-DGENEXEC"])
            .args(INCLUDES)
            .arg(DECODER),
        &dir,
        Stdio::null(),
        Some(File::create(&preprocessed).context("cannot create ppc-decode.i")?),
        log,
    )?;
    let handlers = execute_handlers(&fs::read_to_string(&preprocessed)?)?;
    if handlers.is_empty() {
        bail!("ppc-decode.cpp names no instruction handlers");
    }
    fs::write(dir.join(HANDLERS), handlers)?;

    let jobs = thread::available_parallelism().map_or(1, |jobs| jobs.get());
    let sdl_from_build = "../../../sdl/x86_64-w64-mingw32";
    fs::write(dir.join("GNUmakefile"), makefile(sdl_from_build, jobs))?;
    macemu_windows::make(&dir, src, log, "SheepShaver")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    #[test]
    fn handlers_are_listed_once_each_in_order() {
        let text = "x = &powerpc_cpu::execute_addition<operand_RA, CA_BIT_0>;\n\
                    y = &powerpc_cpu::execute_branch<operand_LR>; \
                    z = &powerpc_cpu::execute_addition<operand_RA, CA_BIT_0>;\n\
                    nothing here\n";
        assert_eq!(
            execute_handlers(text).unwrap(),
            "template void powerpc_cpu::execute_addition<operand_RA, CA_BIT_0>(uint32);\n\
             template void powerpc_cpu::execute_branch<operand_LR>(uint32);\n"
        );
    }

    /// As in Perl, `(.+)>` reaches the last `>` in the statement.
    #[test]
    fn a_handler_s_arguments_run_to_the_last_angle_bracket() {
        let text = "f(&powerpc_cpu::execute_cmp<fp_load<A>, B>, other<C>)\n";
        assert_eq!(
            execute_handlers(text).unwrap(),
            "template void powerpc_cpu::execute_cmp<fp_load<A>, B>, other<C>(uint32);\n"
        );
    }

    #[test]
    fn what_is_not_a_handler_is_left_out() {
        for text in [
            "powerpc_cpu::execute_<a>\n",
            "powerpc_cpu::execute_x<>\n",
            "powerpc_cpu::execute_x(a)\n",
        ] {
            assert_eq!(execute_handlers(text).unwrap(), "", "{text}");
        }
    }

    #[test]
    fn templates_the_script_would_expand_stop_the_build() {
        let error = execute_handlers("DEFINE_TEMPLATE(x, a, b(c))\n").unwrap_err();
        assert!(error.to_string().contains("templates"), "{error}");
    }

    #[test]
    fn paths_are_worked_out_within_the_tree() {
        assert_eq!(
            normalize(Path::new(
                "SheepShaver/src/Windows/../../../BasiliskII/src/x"
            ))
            .unwrap(),
            Path::new("BasiliskII/src/x")
        );
        assert!(normalize(Path::new("a/../../b")).is_none());
    }

    fn archive(dir: &Path, entries: &[(&str, Option<&str>)]) -> PathBuf {
        let path = dir.join("source.tgz");
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::default(),
        ));
        for (name, link) in entries {
            let mut header = tar::Header::new_gnu();
            match link {
                Some(target) => {
                    header.set_entry_type(EntryType::Symlink);
                    header.set_size(0);
                    builder
                        .append_link(&mut header, format!("macemu-1/{name}"), target)
                        .unwrap();
                }
                None => {
                    header.set_entry_type(EntryType::Regular);
                    header.set_size(name.len() as u64);
                    header.set_mode(0o644);
                    builder
                        .append_data(&mut header, format!("macemu-1/{name}"), name.as_bytes())
                        .unwrap();
                }
            }
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn links_become_copies_of_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive(
            dir.path(),
            &[
                (
                    "SheepShaver/src/disk.cpp",
                    Some("../../BasiliskII/src/disk.cpp"),
                ),
                ("SheepShaver/src/SDL", Some("../../BasiliskII/src/SDL")),
                // In a folder of links only, naming one that comes later.
                (
                    "SheepShaver/src/include/sys.h",
                    Some("../../../BasiliskII/src/include/sys.h"),
                ),
                ("BasiliskII/src/include/sys.h", Some("../disk.cpp")),
                ("SheepShaver/src/main.cpp", None),
                ("BasiliskII/src/disk.cpp", None),
                ("BasiliskII/src/SDL/video.cpp", None),
                ("cxmon/src/mon.cpp", None),
            ],
        );
        let root = dir.path().join("src");
        unpack_source(&archive, &root).unwrap();
        let read = |path: &str| fs::read_to_string(root.join(path)).unwrap();
        assert_eq!(read("SheepShaver/src/disk.cpp"), "BasiliskII/src/disk.cpp");
        assert_eq!(
            read("SheepShaver/src/SDL/video.cpp"),
            "BasiliskII/src/SDL/video.cpp"
        );
        assert_eq!(
            read("SheepShaver/src/include/sys.h"),
            "BasiliskII/src/disk.cpp"
        );
        assert_eq!(read("SheepShaver/src/main.cpp"), "SheepShaver/src/main.cpp");
        assert!(!root.join("cxmon").exists());
    }

    #[test]
    fn a_link_to_nothing_stops_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive(
            dir.path(),
            &[(
                "SheepShaver/src/gone.cpp",
                Some("../../BasiliskII/src/gone.cpp"),
            )],
        );
        let error = unpack_source(&archive, &dir.path().join("src")).unwrap_err();
        assert!(error.to_string().contains("doesn't have"), "{error}");
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
        assert!(text.contains("obj/ppc-dis.o"), "{text}");
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
    fn the_config_describes_64_bit_windows() {
        for line in [
            "#define SIZEOF_VOID_P 8",
            "#define SIZEOF_LONG 4",
            "#define USE_SDL2 1",
        ] {
            assert!(CONFIG_HEADER.lines().any(|l| l == line), "{line}");
        }
        assert!(!CONFIG_HEADER.contains("ENABLE_VOSF"));
    }
}
