//! Mini vMac adapter. Option spellings and limits below come from the source
//! of the pinned release (`setup/SPBLDOPT.i`, `src/OSGLUXWN.c`).
//!
//! Mini vMac fixes the model, RAM, display, and window scale at compile time,
//! so nearly all of a machine's settings end up in the build key. At run time
//! it takes only `-r ROM`, `-d DIR`, and disk paths. A disk is attached
//! read-only exactly when the emulator cannot open it for writing.

use super::{Check, EmulatorBackend, Executable, LaunchRequest};
use crate::fsutil::{atomic_copy, sha256_bytes, sha256_file, write_toml};
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
    pub executable_sha256: String,
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
            fs::create_dir_all(cached.parent().context("no source directory")?)?;
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                .arg(&partial)
                .arg(&self.pin.url)
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
        let executable = dir.join(BACKEND_NAME);
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

        let executable = work.join("src").join(BACKEND_NAME);
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
        fs::rename(&executable, publish.join(BACKEND_NAME))?;
        fs::copy(&log_path, publish.join("build.log"))?;
        write_toml(
            &publish.join(MANIFEST),
            &Manifest {
                backend: BACKEND_NAME.into(),
                revision: self.pin.revision.clone(),
                host: host_triple().into(),
                toolchain: toolchain.into(),
                options: options.to_vec(),
                executable_sha256: sha256_file(&publish.join(BACKEND_NAME))?,
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

/// Unpack and compile, per upstream's documented procedure: compile the
/// configuration tool, have it generate the sources' setup script, then make.
fn run_build(archive: &Path, work: &Path, options: &[String], log_path: &Path) -> Result<()> {
    let src = work.join("src");
    fs::create_dir_all(&src)?;
    let tool_args = options.join(" ");
    let script = format!(
        "set -e\n\
         tar xzf \"$1\" --strip-components=1\n\
         gcc setup/tool.c -o setup_t\n\
         ./setup_t {tool_args} > setup.sh\n\
         bash ./setup.sh\n\
         make\n"
    );
    let log = File::create(log_path).context("cannot create build log")?;
    let status = Command::new("bash")
        .arg("-c")
        .arg(script)
        .arg("macman-build")
        .arg(archive)
        .current_dir(&src)
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

fn host_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

fn toolchain_identity() -> Result<String> {
    let output = Command::new("gcc")
        .arg("--version")
        .output()
        .context("cannot run gcc; install a C compiler (run `macman doctor`)")?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().next().unwrap_or("unknown").trim().to_string())
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
        ("-t", "lx64".to_string()),
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
            bail!("the Mini vMac backend currently supports only Linux x86-64 hosts");
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
                "host is not Linux x86-64",
                "the Mini vMac backend currently supports only Linux x86-64 hosts",
            ));
        }
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

        if std::env::var_os("DISPLAY").is_none() {
            checks.push(Check::warn(
                "DISPLAY is not set",
                "Mini vMac is an X11 program; run macman from a graphical session",
            ));
        }
        checks
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
        assert_eq!(
            build_options(&m).join(" "),
            "-t lx64 -m II -mem 8M -hres 640 -vres 480 -depth 3 -fullscreen 0 -mf 2 -magnify 1"
        );
        let mut m = machine(Model::Plus, "4M", (512, 342, 1));
        m.display.scale = 1;
        m.display.fullscreen = true;
        assert_eq!(
            build_options(&m).join(" "),
            "-t lx64 -m Plus -mem 4M -hres 512 -vres 342 -depth 0 -fullscreen 1 -magnify 0"
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
    fn drive_limit_is_enforced() {
        let m = machine(Model::Mac128K, "128K", (512, 342, 1));
        backend().validate_disks(&m, 2).unwrap();
        assert!(backend().validate_disks(&m, 3).is_err());
    }
}
