//! Mini vMac adapter. Option spellings and limits below come from the source
//! of the pinned release (`setup/SPBLDOPT.i`, `src/OSGLUXWN.c`).
//!
//! Mini vMac fixes the model, RAM, display, and window scale at compile time,
//! so nearly all of a machine's settings end up in the build key. At run time
//! it takes only `-r ROM`, `-d DIR`, and disk paths. A disk is attached
//! read-only exactly when the emulator cannot open it for writing.
//!
//! The Windows glue (`src/OSGLUWIN.c`) differs: it rejects flags, and finds
//! the ROM only by a fixed name beside its own executable. So each run gets a
//! private copy of both in the session directory.

use super::{Check, EmulatorBackend, Executable, LaunchRequest};
use crate::fsutil::{atomic_copy, sha256_bytes, sha256_file, without_verbatim_prefix, write_toml};
use crate::machine::{Display, Machine};
use crate::model::Model;
use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

pub const BACKEND_NAME: &str = "minivmac";
pub const MANIFEST: &str = "manifest.toml";
const EXECUTABLE: &str = if cfg!(windows) {
    "minivmac.exe"
} else {
    BACKEND_NAME
};
/// `_MAX_PATH`, which Mini vMac's Windows glue uses for every path buffer.
const WINDOWS_MAX_PATH: usize = 260;

/// An exact upstream source archive.
#[derive(Debug, Clone)]
pub struct SourcePin {
    pub revision: String,
    pub url: String,
    pub sha256: String,
}

impl Default for SourcePin {
    fn default() -> Self {
        Self {
            revision: "36.04".into(),
            url: "https://www.gryphel.com/d/minivmac/minivmac-36.04/minivmac-36.04.src.tgz".into(),
            sha256: "9b7343cec87723177a203e69ad3baf20f49b4e8f03619e366c4bf2705167dfa4".into(),
        }
    }
}

/// Written beside every published build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub backend: String,
    pub revision: String,
    pub host: String,
    pub toolchain: String,
    pub options: Vec<String>,
    /// File name within the build directory.
    #[serde(default = "default_executable")]
    pub executable: String,
    pub executable_sha256: String,
}

fn default_executable() -> String {
    BACKEND_NAME.into()
}

pub struct MiniVmac {
    paths: Paths,
    pin: SourcePin,
    /// Optional local copy of the pinned archive.
    source_archive: Option<PathBuf>,
}

impl MiniVmac {
    pub fn new(paths: Paths, pin: SourcePin, source_archive: Option<PathBuf>) -> Self {
        Self {
            paths,
            pin,
            source_archive,
        }
    }

    fn cached_archive(&self) -> PathBuf {
        self.paths
            .sources()
            .join(&self.pin.revision)
            .join("source.tgz")
    }

    /// Put the verified source archive in the cache, downloading if needed.
    fn fetch_source(&self) -> Result<PathBuf> {
        let cached = self.cached_archive();
        if cached.exists() {
            if sha256_file(&cached)? == self.pin.sha256 {
                return Ok(cached);
            }
            eprintln!("warning: cached Mini vMac source is damaged; fetching it again");
            fs::remove_file(&cached)?;
        }

        if let Some(local) = &self.source_archive {
            atomic_copy(local, &cached).context("cannot use configured source_archive")?;
        } else {
            eprintln!("Downloading Mini vMac {}...", self.pin.revision);
            let partial = cached.with_extension("partial");
            let directory = cached.parent().context("no source directory")?;
            fs::create_dir_all(directory)?;
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                // Relative, so curl needn't cope with the cache's real path.
                .arg(partial.file_name().context("no source file name")?)
                .arg(&self.pin.url)
                .current_dir(directory)
                .status()
                .context(
                    "cannot run curl; install it, or set minivmac.source_archive in config.toml",
                )?;
            if !status.success() {
                let _ = fs::remove_file(&partial);
                bail!(
                    "download of {} failed; check the network, or download it yourself and set \
                     minivmac.source_archive in config.toml",
                    self.pin.url
                );
            }
            fs::rename(&partial, &cached)?;
        }

        if sha256_file(&cached)? != self.pin.sha256 {
            fs::remove_file(&cached)?;
            bail!(
                "Mini vMac source archive does not match the pinned SHA-256 {}; refusing to \
                 build from it",
                self.pin.sha256
            );
        }
        Ok(cached)
    }

    fn build_key(&self, options: &[String], toolchain: &str) -> String {
        sha256_bytes(
            format!(
                "{BACKEND_NAME}\n{}\n{}\n{}\n{toolchain}\n{}\n",
                self.pin.revision,
                self.pin.sha256,
                host_triple(),
                options.join(" ")
            )
            .as_bytes(),
        )
    }

    /// The executable in a published build, provided it matches its manifest.
    fn cached_build(&self, key: &str) -> Option<PathBuf> {
        let dir = self.paths.builds().join(key);
        let manifest: Manifest = crate::fsutil::read_toml(&dir.join(MANIFEST)).ok()?;
        let executable = dir.join(&manifest.executable);
        (sha256_file(&executable).ok()? == manifest.executable_sha256).then_some(executable)
    }

    fn build(&self, key: &str, options: &[String], toolchain: &str, verbose: bool) -> Result<()> {
        let archive = self.fetch_source()?;
        let work = self
            .paths
            .build_work()
            .join(format!("{key}-{}", std::process::id()));
        // Whatever a prior crash left behind is unusable.
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work)
            .with_context(|| format!("cannot create build directory {}", work.display()))?;
        fs::create_dir_all(self.paths.build_logs())?;
        let log_path = self.paths.build_logs().join(format!("{key}.log"));

        eprintln!(
            "Building Mini vMac {} ({})...",
            self.pin.revision,
            options.join(" ")
        );
        let result = run_build(&archive, &work, options, &log_path);
        if verbose && let Ok(log) = fs::read_to_string(&log_path) {
            eprint!("{log}");
        }
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&work);
            return Err(error.context(build_failure_hint(&log_path)));
        }

        let executable = work.join("src").join(EXECUTABLE);
        if !executable.is_file() {
            let _ = fs::remove_dir_all(&work);
            bail!(
                "build produced no executable; {}",
                build_failure_hint(&log_path)
            );
        }

        // Publish by renaming a complete directory into place.
        let publish = work.join("publish");
        fs::create_dir_all(&publish)?;
        fs::rename(&executable, publish.join(EXECUTABLE))?;
        fs::copy(&log_path, publish.join("build.log"))?;
        write_toml(
            &publish.join(MANIFEST),
            &Manifest {
                backend: BACKEND_NAME.into(),
                revision: self.pin.revision.clone(),
                host: host_triple().into(),
                toolchain: toolchain.into(),
                options: options.to_vec(),
                executable: EXECUTABLE.into(),
                executable_sha256: sha256_file(&publish.join(EXECUTABLE))?,
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

fn build_failure_hint(log: &Path) -> String {
    format!(
        "Mini vMac build failed; see {} (rerun with --verbose to print it), and run `macman doctor`",
        log.display()
    )
}

fn run_build(archive: &Path, work: &Path, options: &[String], log_path: &Path) -> Result<()> {
    let src = work.join("src");
    fs::create_dir_all(&src)?;
    let log = File::create(log_path).context("cannot create build log")?;
    if cfg!(windows) {
        run_windows_build(archive, &src, options, &log)
    } else {
        run_unix_build(archive, &src, options, log)
    }
}

/// Unpack and compile, per upstream's documented procedure: compile the
/// configuration tool, have it generate the sources' setup script, then make.
fn run_unix_build(archive: &Path, src: &Path, options: &[String], log: File) -> Result<()> {
    let tool_args = options.join(" ");
    let script = format!(
        "set -e\n\
         tar xzf \"$1\" --strip-components=1\n\
         gcc setup/tool.c -o setup_t\n\
         ./setup_t {tool_args} > setup.sh\n\
         bash ./setup.sh\n\
         make\n"
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

/// The same procedure with no POSIX shell: `bash` on Windows may well be the
/// WSL launcher. Upstream's tool picks its compiler and script language when
/// compiled, so ask it for MinGW and a batch file. Every path stays relative
/// to `src`, because neither the batch file nor make quote well.
fn run_windows_build(archive: &Path, src: &Path, options: &[String], log: &File) -> Result<()> {
    let ascii_src = short_path(src);
    let src = ascii_src.as_path();
    let step = |what: &str, command: &mut Command, stdin: Stdio| -> Result<()> {
        let status = command
            .current_dir(src)
            .stdin(stdin)
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .status()
            .with_context(|| format!("cannot run {what}"))?;
        if !status.success() {
            bail!("{what} exited with {status}");
        }
        Ok(())
    };

    // Piped, as GNU tar would read `C:\...` as a remote host.
    let source = File::open(archive).context("cannot read source archive")?;
    step(
        "tar",
        Command::new("tar").args(["xzf", "-", "--strip-components=1"]),
        source.into(),
    )?;
    step(
        "gcc",
        Command::new("gcc").args([
            "-Dcur_ide=gbk_ide_mgw",
            "-Dgbo_script=gbk_script_xp",
            "-DUseCmndLine=1",
            "setup/tool.c",
            "-o",
            "setup_t.exe",
        ]),
        Stdio::null(),
    )?;

    let script = src.join("setup.bat");
    let status = Command::new(src.join("setup_t.exe"))
        .args(options)
        .current_dir(src)
        .stdin(Stdio::null())
        .stdout(File::create(&script).context("cannot create setup script")?)
        .stderr(log.try_clone()?)
        .status()
        .context("cannot run Mini vMac's configuration tool")?;
    if !status.success() {
        bail!("Mini vMac's configuration tool exited with {status}");
    }
    fs::write(&script, fix_batch_redirects(&fs::read_to_string(&script)?))?;
    step(
        "the setup script",
        Command::new("cmd")
            .args(["/d", "/c", r".\setup.bat"])
            .env("my_project_d", "."),
        Stdio::null(),
    )?;

    let makefile = src.join("Makefile");
    let text = fs::read_to_string(&makefile).context("the setup script wrote no Makefile")?;
    fs::write(&makefile, use_wide_windows_api(&text)?)?;
    step("make", &mut Command::new(make_tool()), Stdio::null()).with_context(|| {
        if src.to_string_lossy().is_ascii() {
            String::from("cannot compile Mini vMac")
        } else {
            format!(
                "MinGW make can't work in {}, which has no ASCII short name; set XDG_CACHE_HOME \
                 to a plain ASCII path",
                src.display()
            )
        }
    })
}

/// The 8.3 form of `path` if it has one. MinGW's make uses the narrow Windows
/// API, and stops at a working directory outside the system code page.
#[cfg(windows)]
fn short_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut short = vec![0u16; wide.len()];
    // SAFETY: `wide` is NUL-terminated, and the length is that of `short`. A
    // short form is never longer than the long one, which `short` can hold.
    let length =
        unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), short.len() as u32) };
    match short.get(..length as usize) {
        Some(short) if length != 0 => PathBuf::from(OsString::from_wide(short)),
        _ => path.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn short_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Upstream's batch file ends lines with `echo TEXT>>"%DestFile%"`, so a line
/// whose text ends in a digit, like `#define NumDrives 6`, redirects that
/// file handle instead of writing the digit. Redirecting first is immune.
fn fix_batch_redirects(script: &str) -> String {
    const REDIRECT: &str = ">>\"%DestFile%\"";
    let mut fixed = String::with_capacity(script.len());
    for line in script.lines() {
        match line.strip_suffix(REDIRECT) {
            Some(echo) if echo.starts_with("echo") => {
                fixed.extend([REDIRECT, " ", echo, "\r\n"]);
            }
            _ => fixed.extend([line, "\r\n"]),
        }
    }
    fixed
}

/// Build for the wide Windows API, which upstream's source supports but only
/// turns on for Windows CE. The narrow build garbles any path outside the
/// system code page, even the directory of its own executable.
fn use_wide_windows_api(makefile: &str) -> Result<String> {
    const COMPILE: &str = "mk_COptionsCommon = -c ";
    const LINK: &str = " -mwindows ";
    if !makefile.contains(COMPILE) || !makefile.contains(LINK) {
        bail!("the generated Makefile lacks the expected compile and link options");
    }
    Ok(makefile
        .replacen(COMPILE, "mk_COptionsCommon = -c -DUNICODE -D_UNICODE ", 1)
        .replacen(LINK, " -mwindows -municode ", 1))
}

/// MinGW's own make goes by this name; a plain `make` may be something else.
fn make_tool() -> &'static str {
    if tool_exists("mingw32-make") {
        "mingw32-make"
    } else {
        "make"
    }
}

fn host_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-gnu"
    } else {
        "unsupported"
    }
}

/// The `-t` option of upstream's configuration tool.
fn target_option() -> &'static str {
    if cfg!(windows) { "wx64" } else { "lx64" }
}

fn toolchain_identity() -> Result<String> {
    let output = Command::new("gcc")
        .arg("--version")
        .output()
        .context("cannot run gcc; install a C compiler (run `macman doctor`)")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.lines().next().unwrap_or("unknown").trim().to_string();
    if !cfg!(windows) {
        return Ok(version);
    }
    let target = gcc_target()?;
    if !is_mingw_x64(&target) {
        bail!("gcc targets {target}, not 64-bit Windows; run `macman doctor`");
    }
    Ok(format!("{target} {version}"))
}

fn gcc_target() -> Result<String> {
    let output = Command::new("gcc")
        .arg("-dumpmachine")
        .output()
        .context("cannot run gcc; install a C compiler (run `macman doctor`)")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Cygwin and MSYS compilers also run on Windows, but need their own runtimes.
fn is_mingw_x64(target: &str) -> bool {
    target.starts_with("x86_64-") && target.contains("mingw")
}

/// The name Mini vMac looks for beside its executable (`setup/SPCNFGAP.i`).
fn rom_file_name(model: Model) -> &'static str {
    match model {
        Model::Twig43 => "Twig43.ROM",
        Model::Twiggy => "Twiggy.ROM",
        Model::Mac128K => "Mac128K.ROM",
        Model::Se => "MacSE.ROM",
        Model::SeFdhd => "SEFDHD.ROM",
        Model::Classic => "Classic.ROM",
        Model::Pb100 => "PB100.ROM",
        Model::II => "MacII.ROM",
        Model::IIx => "MacIIx.ROM",
        Model::Mac512Ke | Model::Plus => "vMac.ROM",
    }
}

/// How the emulator, started in `workdir`, should be told about `disk`. Paths
/// over the limit are silently skipped by Mini vMac, so refuse them here.
fn windows_disk_argument(disk: &Path, workdir: &Path) -> Result<PathBuf> {
    let argument = match disk.strip_prefix(workdir) {
        Ok(name) => name.to_path_buf(),
        Err(_) => without_verbatim_prefix(disk),
    };
    check_windows_path_length(&argument)?;
    Ok(argument)
}

fn check_windows_path_length(path: &Path) -> Result<()> {
    if path.to_string_lossy().encode_utf16().count() >= WINDOWS_MAX_PATH {
        bail!(
            "{} is too long for Mini vMac, which handles under {WINDOWS_MAX_PATH} characters; \
             move it to a shorter path",
            path.display()
        );
    }
    Ok(())
}

/// Give the run its own emulator and ROM, then start from that directory.
fn windows_command(request: &LaunchRequest<'_>) -> Result<Command> {
    let workdir = request.workdir;
    let rom = workdir.join(rom_file_name(request.machine.hardware.model));
    let executable = workdir.join(EXECUTABLE);
    check_windows_path_length(&rom)?;
    check_windows_path_length(&executable)?;
    let mut command = Command::new(&executable);
    command.current_dir(workdir);
    for disk in request.disks {
        command.arg(windows_disk_argument(&disk.path, workdir)?);
    }
    atomic_copy(&request.executable.path, &executable)?;
    atomic_copy(request.rom, &rom)?;
    Ok(command)
}

fn memory_sizes(model: Model) -> &'static [&'static str] {
    match model {
        Model::Twig43 | Model::Twiggy | Model::Mac128K | Model::Mac512Ke => &["128K", "512K"],
        Model::Plus => &["128K", "512K", "1M", "2M", "2.5M", "4M"],
        Model::Se | Model::SeFdhd | Model::Classic => &["512K", "1M", "2M", "2.5M", "4M"],
        Model::II | Model::IIx => &["1M", "2M", "4M", "5M", "8M"],
        Model::Pb100 => &["4M"],
    }
}

fn model_option(model: Model) -> &'static str {
    match model {
        Model::Twig43 => "Twig43",
        Model::Twiggy => "Twiggy",
        Model::Mac128K => "128K",
        Model::Mac512Ke => "512Ke",
        Model::Plus => "Plus",
        Model::Se => "SE",
        Model::SeFdhd => "SEFDHD",
        Model::Classic => "Classic",
        Model::Pb100 => "PB100",
        Model::II => "II",
        Model::IIx => "IIx",
    }
}

/// How many disks fit: 2 on the earliest models, 6 otherwise (upstream defaults).
fn drive_count(model: Model) -> usize {
    match model {
        Model::Twig43 | Model::Twiggy | Model::Mac128K => 2,
        _ => 6,
    }
}

/// `-depth` takes log2 of the bits per pixel.
fn depth_option(depth: u32) -> Option<u32> {
    [1, 2, 4, 8, 16, 32].contains(&depth).then(|| depth.ilog2())
}

fn validate_display(model: Model, display: &Display) -> Result<()> {
    let Display {
        width,
        height,
        depth,
        scale,
        ..
    } = *display;
    if !width.is_multiple_of(32) {
        bail!(
            "display width {width} must be a multiple of 32 (try {})",
            width & !0x1F
        );
    }
    if !(128..32 * 1024).contains(&width) || !(128..32 * 1024).contains(&height) {
        bail!("display size {width}x{height} is out of range; each side must be 128 to 32767");
    }
    if scale == 0 {
        bail!("display scale must be at least 1");
    }
    if depth_option(depth).is_none() {
        bail!("display depth {depth} is not supported; use 1, 2, 4, 8, 16, or 32 bits");
    }

    let video_bytes = (u64::from(width) * u64::from(height) * u64::from(depth)).div_ceil(8);
    let video_bytes = video_bytes.next_power_of_two();
    if model.has_video_card() {
        if video_bytes > 4 * 1024 * 1024 {
            bail!("display {width}x{height}x{depth} needs over 4M of video memory; reduce it");
        }
        return Ok(());
    }
    if depth != 1 {
        bail!("model {model} is monochrome; use depth 1, or a color-capable model (ii, iix)");
    }
    if model == Model::Pb100 {
        if (width, height, depth) != model.default_display() {
            bail!("model pb100 has a fixed 640x400 display");
        }
        return Ok(());
    }
    if u64::from(width) * u64::from(height) > 2 * 1024 * 1024 || video_bytes > 256 * 1024 {
        bail!("display {width}x{height} is too large for model {model} (256K video memory limit)");
    }
    Ok(())
}

/// Compile-time options for the configuration tool.
fn build_options(machine: &Machine) -> Vec<String> {
    let hardware = &machine.hardware;
    let display = &machine.display;
    let depth = depth_option(display.depth).unwrap_or(0);
    let mut options: Vec<String> = [
        ("-t", target_option().to_string()),
        ("-m", model_option(hardware.model).to_string()),
        ("-mem", hardware.memory.clone()),
        ("-hres", display.width.to_string()),
        ("-vres", display.height.to_string()),
        ("-depth", depth.to_string()),
        ("-fullscreen", u8::from(display.fullscreen).to_string()),
    ]
    .into_iter()
    .flat_map(|(flag, value)| [flag.to_string(), value])
    .collect();
    if display.scale > 1 {
        options.extend([
            "-mf".to_string(),
            display.scale.to_string(),
            "-magnify".to_string(),
            "1".to_string(),
        ]);
    } else {
        options.extend(["-magnify".to_string(), "0".to_string()]);
    }
    options
}

impl EmulatorBackend for MiniVmac {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    fn revision(&self) -> &str {
        &self.pin.revision
    }

    fn validate(&self, machine: &Machine) -> Result<()> {
        if machine.emulator.revision != self.pin.revision {
            bail!(
                "machine {:?} is pinned to Mini vMac {}, but this macman supports {}; \
                 set [emulator] revision in its machine.toml",
                machine.name,
                machine.emulator.revision,
                self.pin.revision
            );
        }
        let model = machine.hardware.model;
        let sizes = memory_sizes(model);
        if !sizes.contains(&machine.hardware.memory.as_str()) {
            bail!(
                "model {model} cannot have {} of memory; supported sizes: {}",
                machine.hardware.memory,
                sizes.join(", ")
            );
        }
        validate_display(model, &machine.display)
    }

    fn validate_disks(&self, machine: &Machine, count: usize) -> Result<()> {
        let limit = drive_count(machine.hardware.model);
        if count > limit {
            bail!(
                "model {} supports {limit} disks at once, but this run attaches {count}",
                machine.hardware.model
            );
        }
        Ok(())
    }

    fn prepare(&self, machine: &Machine, verbose: bool) -> Result<Executable> {
        if host_triple() == "unsupported" {
            bail!("the Mini vMac backend supports only Linux and Windows x86-64 hosts");
        }
        self.validate(machine)?;
        let options = build_options(machine);
        let toolchain = toolchain_identity()?;
        let key = self.build_key(&options, &toolchain);
        if let Some(path) = self.cached_build(&key) {
            return Ok(Executable {
                path,
                build_key: key,
                built: false,
            });
        }

        // Two runs of one configuration must not build it twice.
        fs::create_dir_all(self.paths.build_work())?;
        let lock_path = self.paths.build_work().join(format!(".{key}.lock"));
        let lock = File::create(&lock_path).context("cannot create build lock")?;
        lock.lock().context("cannot lock build")?;
        if let Some(path) = self.cached_build(&key) {
            return Ok(Executable {
                path,
                build_key: key,
                built: false,
            });
        }
        self.build(&key, &options, &toolchain, verbose)?;
        let path = self
            .cached_build(&key)
            .context("published Mini vMac build failed validation")?;
        Ok(Executable {
            path,
            build_key: key,
            built: true,
        })
    }

    fn launch(&self, request: &LaunchRequest<'_>) -> Result<ExitStatus> {
        let mut command = if cfg!(windows) {
            windows_command(request)?
        } else {
            let mut command = Command::new(&request.executable.path);
            command
                .arg("-d")
                .arg(request.workdir)
                .arg("-r")
                .arg(request.rom)
                .current_dir(request.workdir);
            for disk in request.disks {
                command.arg(&disk.path);
            }
            command
        };
        if request.verbose {
            eprintln!("Launching {command:?}");
        }
        command
            .status()
            .with_context(|| format!("cannot start {}", request.executable.path.display()))
    }

    fn doctor(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        if host_triple() == "unsupported" {
            checks.push(Check::fail(
                "host is not x86-64 Linux or Windows",
                "the Mini vMac backend supports only Linux and Windows x86-64 hosts",
            ));
        }
        if cfg!(windows) {
            windows_tool_checks(&mut checks);
        } else {
            unix_tool_checks(&mut checks);
        }

        let cached = self.cached_archive();
        match sha256_file(&cached) {
            Ok(sha) if sha == self.pin.sha256 => checks.push(Check::ok(format!(
                "Mini vMac {} source cached",
                self.pin.revision
            ))),
            Ok(_) => checks.push(Check::warn(
                "cached Mini vMac source is damaged",
                "it will be fetched again on the next build",
            )),
            Err(_) => match &self.source_archive {
                Some(local) if local.is_file() => checks.push(Check::ok(format!(
                    "Mini vMac {} source available from source_archive",
                    self.pin.revision
                ))),
                Some(_) => checks.push(Check::fail(
                    "configured minivmac.source_archive is missing",
                    "fix or remove it in config.toml",
                )),
                None if tool_exists("curl") => checks.push(Check::warn(
                    format!("Mini vMac {} source not downloaded yet", self.pin.revision),
                    "it will be downloaded on the first build; offline, set \
                     minivmac.source_archive in config.toml",
                )),
                None => checks.push(Check::fail(
                    "Mini vMac source not cached, and curl not found",
                    "install curl, or set minivmac.source_archive in config.toml",
                )),
            },
        }

        if !cfg!(windows) && std::env::var_os("DISPLAY").is_none() {
            checks.push(Check::warn(
                "DISPLAY is not set",
                "Mini vMac is an X11 program; run macman from a graphical session",
            ));
        }
        checks
    }
}

fn unix_tool_checks(checks: &mut Vec<Check>) {
    for tool in ["gcc", "make", "bash", "tar"] {
        if tool_exists(tool) {
            checks.push(Check::ok(format!("{tool} found")));
        } else {
            checks.push(Check::fail(
                format!("{tool} not found"),
                format!("install {tool} (Debian/Ubuntu: apt install build-essential)"),
            ));
        }
    }
    if header_exists("X11/Xlib.h") {
        checks.push(Check::ok("X11 headers found"));
    } else {
        checks.push(Check::fail(
            "X11 headers not found",
            "install them (Debian/Ubuntu: apt install libx11-dev libxext-dev)",
        ));
    }
}

fn windows_tool_checks(checks: &mut Vec<Check>) {
    const INSTALL: &str = "install MinGW-w64, then open a new terminal: \
                           winget install BrechtSanders.WinLibs.POSIX.UCRT";
    match gcc_target() {
        Ok(target) if is_mingw_x64(&target) => {
            checks.push(Check::ok(format!("gcc for {target} found")));
        }
        Ok(target) => checks.push(Check::fail(
            format!("gcc targets {target}, not 64-bit Windows"),
            format!("put a 64-bit MinGW gcc first on PATH, or {INSTALL}"),
        )),
        Err(_) => checks.push(Check::fail("gcc not found", INSTALL)),
    }
    for tool in ["windres", "strip"] {
        if tool_exists(tool) {
            checks.push(Check::ok(format!("{tool} found")));
        } else {
            checks.push(Check::fail(format!("{tool} not found"), INSTALL));
        }
    }
    if tool_exists(make_tool()) {
        checks.push(Check::ok(format!("{} found", make_tool())));
    } else {
        checks.push(Check::fail("mingw32-make not found", INSTALL));
    }
    if tool_exists("tar") {
        checks.push(Check::ok("tar found"));
    } else {
        checks.push(Check::fail(
            "tar not found",
            "use Windows 10 version 1803 or later, which includes it",
        ));
    }
}

fn tool_exists(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Ask the compiler, since include paths differ between distributions.
fn header_exists(header: &str) -> bool {
    Command::new("bash")
        .arg("-c")
        .arg(format!(
            "echo '#include <{header}>' | gcc -E -x c - >/dev/null 2>&1"
        ))
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{EmulatorConfig, Hardware};

    fn machine(model: Model, memory: &str, display: (u32, u32, u32)) -> Machine {
        Machine {
            schema_version: 1,
            name: "m".into(),
            id: None,
            emulator: EmulatorConfig {
                backend: BACKEND_NAME.into(),
                revision: "36.04".into(),
            },
            hardware: Hardware {
                model,
                rom: "r".into(),
                memory: memory.into(),
            },
            display: Display {
                width: display.0,
                height: display.1,
                depth: display.2,
                scale: 2,
                fullscreen: false,
            },
            system: Default::default(),
            disks: vec![],
        }
    }

    fn backend() -> MiniVmac {
        MiniVmac::new(
            Paths::under(Path::new("/nonexistent")),
            SourcePin::default(),
            None,
        )
    }

    #[test]
    fn every_model_default_is_valid() {
        for model in crate::model::ALL_MODELS {
            let m = machine(model, model.default_memory(), model.default_display());
            backend()
                .validate(&m)
                .unwrap_or_else(|e| panic!("{model}: {e}"));
        }
    }

    #[test]
    fn rejects_unsupported_hardware() {
        let cases = [
            (machine(Model::Plus, "8M", (512, 342, 1)), "memory"),
            (machine(Model::II, "512K", (640, 480, 8)), "memory"),
            (machine(Model::Plus, "4M", (512, 342, 8)), "monochrome"),
            (machine(Model::II, "8M", (641, 480, 8)), "multiple of 32"),
            (machine(Model::II, "8M", (640, 480, 3)), "depth 3"),
            (machine(Model::II, "8M", (4096, 4096, 32)), "video memory"),
            (machine(Model::Pb100, "4M", (512, 342, 1)), "fixed"),
            (machine(Model::Plus, "4M", (2048, 2048, 1)), "too large"),
        ];
        for (machine, expected) in cases {
            let err = backend().validate(&machine).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn rejects_other_revisions() {
        let mut m = machine(Model::Plus, "4M", (512, 342, 1));
        m.emulator.revision = "floating".into();
        assert!(backend().validate(&m).is_err());
    }

    #[test]
    fn translates_machine_to_upstream_options() {
        let m = machine(Model::II, "8M", (640, 480, 8));
        let target = target_option();
        assert_eq!(
            build_options(&m).join(" "),
            format!(
                "-t {target} -m II -mem 8M -hres 640 -vres 480 -depth 3 -fullscreen 0 -mf 2 \
                 -magnify 1"
            )
        );
        let mut m = machine(Model::Plus, "4M", (512, 342, 1));
        m.display.scale = 1;
        m.display.fullscreen = true;
        assert_eq!(
            build_options(&m).join(" "),
            format!(
                "-t {target} -m Plus -mem 4M -hres 512 -vres 342 -depth 0 -fullscreen 1 -magnify 0"
            )
        );
    }

    #[test]
    fn build_key_tracks_compile_time_inputs_only() {
        let b = backend();
        let plus = machine(Model::Plus, "4M", (512, 342, 1));
        let key = |m: &Machine, gcc| b.build_key(&build_options(m), gcc);

        let mut other_disks = plus.clone();
        other_disks.name = "other".into();
        other_disks.hardware.rom = "other-rom".into();
        other_disks.disks.push(crate::machine::Disk {
            path: "x.dsk".into(),
            readonly: false,
            boot: true,
        });
        assert_eq!(key(&plus, "gcc 1"), key(&other_disks, "gcc 1"));

        let mut more_memory = plus.clone();
        more_memory.hardware.memory = "2M".into();
        assert_ne!(key(&plus, "gcc 1"), key(&more_memory, "gcc 1"));
        assert_ne!(key(&plus, "gcc 1"), key(&plus, "gcc 2"));
    }

    #[test]
    fn batch_redirects_survive_trailing_digits() {
        let script = "@echo off\n\
                      set DestFile=%my_config_d%\\CNFGGLOB.h\n\
                      echo.>\"%DestFile%\"\n\
                      echo.>>\"%DestFile%\"\n\
                      echo #define NumDrives 6>>\"%DestFile%\"\n";
        assert_eq!(
            fix_batch_redirects(script),
            "@echo off\r\n\
             set DestFile=%my_config_d%\\CNFGGLOB.h\r\n\
             echo.>\"%DestFile%\"\r\n\
             >>\"%DestFile%\" echo.\r\n\
             >>\"%DestFile%\" echo #define NumDrives 6\r\n"
        );
    }

    #[test]
    fn wide_api_reaches_compile_and_link_or_nothing_builds() {
        let makefile = "mk_COptionsCommon = -c -Wall\n\tbld/main.res -mwindows -lwinmm\n";
        assert_eq!(
            use_wide_windows_api(makefile).unwrap(),
            "mk_COptionsCommon = -c -DUNICODE -D_UNICODE -Wall\n\
             \tbld/main.res -mwindows -municode -lwinmm\n"
        );
        assert!(use_wide_windows_api("mk_COptionsCommon = -c -Wall\n").is_err());
        assert!(use_wide_windows_api("\t-mwindows -lwinmm\n").is_err());
    }

    #[test]
    fn only_mingw_for_x64_counts() {
        for good in ["x86_64-w64-mingw32", "x86_64-pc-mingw64"] {
            assert!(is_mingw_x64(good), "{good}");
        }
        for bad in ["i686-w64-mingw32", "x86_64-pc-cygwin", "x86_64-pc-msys"] {
            assert!(!is_mingw_x64(bad), "{bad}");
        }
    }

    #[test]
    fn every_model_has_a_rom_name() {
        for model in crate::model::ALL_MODELS {
            assert!(rom_file_name(model).ends_with(".ROM"), "{model}");
        }
        assert_eq!(rom_file_name(Model::Plus), "vMac.ROM");
        assert_eq!(rom_file_name(Model::IIx), "MacIIx.ROM");
    }

    #[test]
    fn disks_are_relative_when_possible_and_refused_when_too_long() {
        let workdir = Path::new("session");
        assert_eq!(
            windows_disk_argument(&workdir.join("0-app.dsk"), workdir).unwrap(),
            Path::new("0-app.dsk")
        );
        assert_eq!(
            windows_disk_argument(Path::new(r"\\?\D:\data.dsk"), workdir).unwrap(),
            Path::new(r"D:\data.dsk")
        );

        // The limit counts UTF-16 units, so it's the path as Windows sees it.
        let fits = format!("{}.dsk", "日".repeat(WINDOWS_MAX_PATH - 5));
        windows_disk_argument(Path::new(&fits), workdir).unwrap();
        let long = format!("{}.dsk", "日".repeat(WINDOWS_MAX_PATH - 4));
        let error = windows_disk_argument(Path::new(&long), workdir).unwrap_err();
        assert!(error.to_string().contains("too long"), "{error}");
    }

    #[test]
    fn drive_limit_is_enforced() {
        let m = machine(Model::Mac128K, "128K", (512, 342, 1));
        backend().validate_disks(&m, 2).unwrap();
        assert!(backend().validate_disks(&m, 3).is_err());
    }
}
