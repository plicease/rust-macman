# MacMan — Classic Macintosh Environment Manager

**Status:** Design proposal  
**Initial implementation:** Rust CLI, Linux x86-64 host  
**Initial backend:** Mini vMac  
**Future backends:** Basilisk II, SheepShaver  
**Distribution:** GitHub source/releases; no crates.io publication required

## 1. Summary

MacMan (`macman`) is a perlbrew-inspired command-line tool for creating, managing, switching between, and launching named classic Macintosh emulation environments. It makes different Macintosh hardware models and System Software installations easy to use without manually selecting emulator executables, ROMs, and disk images.

Mini vMac is the first emulator backend. Its build-time configuration variants make automatic build caching particularly valuable. MacMan is *not* itself a Macintosh emulator and does not bundle copyrighted Apple ROMs or operating system media.

## 2. Goals

- Create and run named Macintosh environments using concise, repeatable commands.
- Manage machine model, ROM, RAM, display, emulator build, and disk attachments in one place.
- Register user-supplied ROM and bootable disk images; validate known combinations where feasible.
- Build or reuse the appropriate Mini vMac executable automatically.
- Protect disk images from accidental concurrent writes or destructive operations.
- Support local cross-development, particularly Retro68 and `tico68` test workflows.
- Keep machine definitions portable, human-readable, and independent of one emulator where practical.
- Work without a service, database, GUI, or internet connection after initial setup.

## 3. Non-goals for v0.01

- Emulating Macintosh hardware directly.
- Redistributing Apple ROM images or System Software installers.
- Fully automated installation of classic Mac OS from arbitrary installer media.
- Basilisk II or SheepShaver integration (future backends only).
- GUI configuration, networking, SCSI device passthrough, advanced snapshots, or automated visual tests.
- Guaranteeing OS/model compatibility beyond documented and tested combinations.
- Modifying HFS volumes directly for the initial release.

## 4. Core concepts

| Concept | Meaning |
|---|---|
| **Backend** | Emulator adapter, initially `minivmac`. |
| **Build** | A specific executable compiled for host, emulator revision, and build-time hardware/display options. |
| **ROM** | User-imported ROM identified by a friendly name and cryptographic checksum. |
| **System image** | A registered user-supplied, bootable disk image, optionally annotated with OS version. |
| **Machine** | Named environment with hardware configuration, ROM, backend, and ordered disk attachments. |
| **Current machine** | Default target of commands such as `macman run` (similar to perlbrew selection). |

A *machine* need not share a name with physical hardware. For example, `wombat-6` may be a Macintosh Plus / System 6 image while `macii-7` is a Macintosh II / System 7 image.

## 5. Command-line interface

These examples define the proposed **MacMan** API, not guaranteed existing Mini vMac CLI switches.

```sh
macman doctor
macman list
macman list --builds
macman rom add ~/roms/MacPlus.rom --name plus-v3
macman rom list
macman system add ~/images/System-6.0.8.dsk --name system-6.0.8 --version 6.0.8
macman system list

macman create wombat-6 --model plus --rom plus-v3 \
  --system system-6.0.8 --memory 4M
macman create macii-7 --model ii --rom macii \
  --system system-7.1 --memory 8M --display 640x480x8
macman use wombat-6
macman current
macman run
macman run macii-7
macman run wombat-6 --disk ./build/tico68.dsk --readonly
macman config show wombat-6
macman clone wombat-6 wombat-test
macman remove wombat-test
macman export wombat-6 -o wombat-6.tar.gz
macman import wombat-6.tar.gz
```

`macman create` should **clone** a registered system image into a machine's writable private disk by default, not boot and modify the shared template. A `--disk` argument on `run` attaches an additional temporary disk without updating the saved configuration. The `--readonly` option applies to that temporary attachment; implementation must ensure the emulator genuinely enforces read-only access (see §8).

A later version may add `macman build`, `macman disk attach/detach`, `macman snapshot`, and `macman test`.

### Selection semantics

`macman use NAME` sets a persistent per-user default, in the manner of a perlbrew global selection. `macman run NAME` overrides it for one invocation. Future shell integration could add per-shell selection, but v0.01 must not require shell hooks.

## 6. Filesystem layout

On Windows, configuration lives in `%APPDATA%macman`, data in `%LOCALAPPDATA%macman`, and the cache in `%LOCALAPPDATA%macmanche`. An absolute `XDG_*` variable overrides its directory on both systems; otherwise Windows uses the environment variable, then the shell's known folder, so `HOME` is never needed.

macOS deliberately shares the Linux layout, not `~/Library`, so that a data directory means the same thing on both.

Use XDG paths on Linux and macOS, respecting `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME`:

```text
~/.config/macman/
  config.toml
~/.local/share/macman/
  roms/
    plus-v3.rom
  systems/
    system-6.0.8.dsk
  machines/
    wombat-6/
      machine.toml
      system.dsk
      data.dsk
    macii-7/
      machine.toml
      system.dsk
~/.cache/macman/
  builds/
    <content-addressed-build-key>/
      minivmac
      manifest.toml
  sources/
    <version-or-commit>/
```

`macman import` also uses `staging/` under the data directory, and `import.lock` beside it.

Machine files and templates live in the data directory; generated executables and source checkouts can be evicted and rebuilt from the cache. Avoid implicit deletion of user-owned disks. Use atomic writes for configuration and registration metadata.

Example `machine.toml`:

```toml
schema_version = 1
name = "wombat-6"
id = "6f1c1f6e-7a3d-4c55-9d0e-3a0f2f6f0a11"  # stable across export and import

[emulator]
backend = "minivmac"
revision = "pinned-source-identifier"

[hardware]
model = "plus"
rom = "plus-v3"
memory = "4M"

[display]
width = 512
height = 342
depth = 1
scale = 2
fullscreen = false

[system]
version = "6.0.8"  # descriptive metadata, not an emulator switch

[[disks]]
path = "system.dsk"
readonly = false
boot = true

[[disks]]
path = "data.dsk"
readonly = false
```

The build adapter translates configuration into supported Mini vMac build options. Distinguish compile-time options (e.g. emulated machine or display capabilities) from runtime launcher settings. Validate and reject unsupported combinations rather than silently ignoring them.

## 7. Mini vMac backend

### Build lifecycle

1. Resolve a pinned, supported Mini vMac source revision. Avoid floating branch tips for reproducibility.
2. Resolve host target, compiler, libraries, and build dependencies.
3. Normalize backend-specific build options and compute a cache key.
4. If the artifact and manifest already exist and validate, reuse them.
5. Otherwise invoke the supported upstream configuration/build procedure, capture logs, and atomically publish the executable and manifest.
6. Launch the executable with the required ROM and disk images in a controlled working directory or through verified supported arguments.

A cache key must include source revision, host triple, build toolchain identity (or explicit invalidation policy), hardware model, memory, resolution, color depth, and other compile-time flags. Do not rebuild merely because two machines use different boot disks.

Mini vMac model/ROM compatibility, build option spellings, host build prerequisites, supported color depths, and launch behavior must be checked against a pinned upstream version during implementation. Sample command lines in discussion are illustrative, not a substitute for validation.

### Windows x86-64

Checked against the Mini vMac 36.04 source and real builds. The Windows backend differs from the Linux one in these ways:

- **Build:** `-t wx64` with a native MinGW-w64 `gcc`. `setup/tool.c` selects its compiler and script language at compile time, so it is built with `-Dcur_ide=gbk_ide_mgw -Dgbo_script=gbk_script_xp -DUseCmndLine=1`, producing a batch file that needs `my_project_d` set. No POSIX shell is involved, and `bash` is avoided because it may be the WSL launcher.
- **Generated script:** upstream writes `echo TEXT>>"%DestFile%"`, which loses a trailing digit to `cmd`'s handle redirection (`6>>`), so macman moves each redirect to the front of its line.
- **Unicode:** the default build uses the narrow API, which can't name paths outside the system code page, so macman adds `-DUNICODE -D_UNICODE` and `-municode` to the generated Makefile. Upstream enables this only for Windows CE. The one side effect is that `GetProcAddress` gets a wide name and fails, disabling the `%APPDATA%\Gryphel` ROM fallback, which macman doesn't use.
- **Short paths:** MinGW `make` is a narrow program too, and stops with `readdir .: Invalid argument` in a directory outside the code page, so builds run from the 8.3 form of the build directory.
- **Launch:** `OSGLUWIN.c` rejects every flag and treats every argument as a disk. It finds the ROM only by a per-model name beside its own executable, so each run copies the verified executable and ROM into the session directory and starts them there. Disks are positional, and throwaway copies are named relative to the session.
- **Path limit:** arguments of 260 characters or more are silently skipped by the emulator, so macman refuses them.
- **Exit status:** a process ended by an exception or Ctrl-C exits with an NTSTATUS code rather than a signal, and these are reported as abnormal. A Windows exit code is 32 bits, and a failure must not round to success.
- **Locks and sharing:** Mini vMac opens writable disks with sharing denied. That shuts out macman's own reads, so disks are probed through the lock first. It also means a disk stays protected if macman dies and leaves its emulator running, and macman reports such a disk as in use.

### macOS ARM64

Checked against the Mini vMac 37.03 source and real builds; see [SMOKE_TEST.md](SMOKE_TEST.md#macos-arm64). The macOS backend differs from the Linux one in these ways:

- **Revision:** 36.04 has no ARM64 target. The 37.03 beta adds `mcar`, so macOS pins it, from the GitHub repository that continues Mini vMac. Other hosts stay on the stable 36.04.
- **Build:** `-e xcd -cl -t mcar` asks for a Makefile and `clang`, so the command line tools are enough, where a plain `-e xcd` would generate a project needing all of Xcode. The setup tool is compiled with `clang` too.
- **SDK:** the generated Makefile fixes `-isysroot` to `/Library/Developer/CommandLineTools/SDKs/MacOSX11.1.sdk`, which current tools don't have, so macman substitutes `xcrun --show-sdk-path`.
- **Cache:** a build is `minivmac.app`, whole. The manifest records a digest of the bundle's names, contents, executable bits, and links, so damage anywhere causes a rebuild. Its key includes the host, `clang`'s target and version, and the SDK's version and path, which keeps it apart from any Linux, Intel Mac, or other-SDK build.
- **Launch:** `OSGLUCCO.m` never reads its arguments, so there are no `-r` or `-d` flags and no paths to pass. It looks beside its bundle for the ROM by a per-model name, and for `disk1.dsk`, `disk2.dsk`, and so on, stopping at the first gap. Each run copies the bundle and ROM into the session directory, with the disks as consecutive links there, boot disk first. A link reaches the real file through `fopen`; only Finder aliases are resolved separately.
- **Process:** macman runs `Contents/MacOS/minivmac` directly, not through `open`, so it's the emulator's parent and gets its real exit status.
- **Exit status:** signal numbers differ from Linux. SIGBUS is 10 rather than 7, which is reported as a host crash.
- **Locks and sharing:** as on Linux, macman's locks last as long as it does. The emulator also takes `flock` on writable disks, so if macman dies and leaves it running, a second emulator refuses the disk itself.

### Backend boundary

Expose a narrow internal trait/interface, conceptually:

```rust
trait EmulatorBackend {
    fn validate(&self, machine: &Machine) -> Result<()>;
    fn prepare(&self, machine: &Machine) -> Result<Executable>;
    fn launch(&self, machine: &Machine, disks: &[ResolvedDisk]) -> Result<ExitStatus>;
}
```

Keep generic model and disk metadata separate from backend extensions. Avoid prematurely implementing a full hypervisor-style abstraction; the initial adapter should remain small.

## 8. ROM, System, and disk-image safety

**ROMs:** Require user-supplied files; identify imports by SHA-256 and user-friendly names. Verify readability and, where known, expected ROM format/size. Warn when a ROM is unlikely to match the target model. Never download proprietary ROMs automatically.

**System images:** Register existing bootable images and their descriptive version metadata. For v0.01, do not assume images can be synthesized from an OS version string. Creating a machine clones the template, preserving its original. An actual OS installer workflow is deferred.

**Writable disk protection:**
- Acquire per-image advisory locks before launching and hold them for the process lifetime.
- Refuse simultaneous writable attachment of the same underlying file (including aliases/symlinks, where detectable).
- Record clean startup/shutdown state; report stale locks and abnormal exits without deleting disks.
- Prefer copy-on-create and explicit copies for experimentation.
- Do not advertise an attachment as read-only unless the backend or an isolated copy *actually prevents writes*. If Mini vMac cannot enforce read-only attachment, use a throwaway copy and discard changes, with explicit CLI messaging.
- Never delete registered source templates during machine removal; prompt or require an explicit destructive flag to delete private machine disks.

Disk formats and capacity restrictions vary by emulated model and System version. Validate supported formats and reject unknown ones with actionable diagnostics. Avoid data conversion, except that registering a system image may store just the volume from a container Mini vMac cannot use writably, leaving the source untouched. See [DISK_FORMATS.md](DISK_FORMATS.md).

## 9. Developer workflow

The primary developer use case is quickly testing a freshly built 68k Macintosh application on more than one OS/hardware configuration:

```sh
# Build disk image externally (Retro68, tico68, etc.)
macman run wombat-6 --disk ./build/tico68.dsk --readonly
macman run macii-7 --disk ./build/tico68.dsk --readonly
```

A later `macman test` subcommand may iterate over a matrix of configurations and collect screenshots or automation results. Do not claim unattended testing until an automation protocol and reliable guest-observation mechanism exist.

## 10. Error handling and diagnostics

`macman doctor` checks compiler/build dependencies, emulator source availability, configured ROMs, registered system images, cache integrity, and machine-to-backend compatibility. Errors should suggest concrete actions, such as importing a compatible ROM or installing a missing host dependency.

Use clear exit codes, human-friendly messages by default, and a `--verbose` option that exposes build and launch logs. Keep sensitive host paths out of routine diagnostic sharing unless requested.

## 11. Milestones

### v0.01 — Minimum useful manager

- Rust CLI and TOML-based machine registry.
- Linux x86-64 support with XDG directory layout.
- ROM and bootable disk-image registration.
- Mini vMac build manager with version pinning, caching, and logs.
- Named machines: create, list, use, run, inspect, clone, remove.
- Temporary additional disk attachment with safe disk access.
- Disk locking, template preservation, and `doctor` checks.
- Integration tests using mock backend; optional emulator smoke tests when legally obtained ROM/OS images are available.

### v0.02 — Developer ergonomics

- Persistent disk attach/detach commands.
- Portable machine import/export without proprietary assets.
- Better build matrix diagnostics and disk-image helper commands.
- Optional shell completion and per-shell machine selection.

### Future

- Basilisk II and SheepShaver adapters.
- Snapshot/copy-on-write support where feasible.
- HFS file injection/extraction through safe external tooling.
- Scripted guest test automation and CI smoke-testing.
- Linux ARM64 and Intel Mac hosts as supported by individual backends. Windows x86-64 and macOS ARM64 are supported; see §7 and [SMOKE_TEST.md](SMOKE_TEST.md).

## 12. Open questions

1. Which upstream Mini vMac fork/release should be the supported baseline, and how should it be pinned?
2. What is the desired minimum host/compiler toolchain (system packages vs vendored builds)?
3. Should machine creation require an existing bootable image, or also support a blank disk plus installer media?
4. What storage-image formats should v0.01 accept, and what safeguards are possible for each?
5. Should `macman use` remain a persistent default, or eventually offer perlbrew-style shell-local selection?
6. Should the first real test matrix use Macintosh Plus/System 6 plus Macintosh II/System 7, reflecting the Retro68 workflow?

## 13. Acceptance criteria for v0.01

1. A user imports a legally obtained ROM and an existing bootable System image.
2. A user creates two named environments with distinct, supported Mini vMac hardware configurations.
3. `macman run NAME` builds an appropriate executable the first time, launches the correct ROM and private boot image, and reuses the cached binary thereafter.
4. Two matching hardware environments share one build, while independent writable disk images remain separate.
5. Running with a temporary application disk does not modify the source artifact when read-only behavior is requested.
6. Conflicting writable disk attachment is blocked; wrong/missing ROMs and unsupported hardware options produce useful errors.
7. `macman doctor` reports actionable failures, and machine deletion never silently destroys a registered source image.

---

**Working name:** MacMan (`macman`)  
**Tagline:** Classic Macintosh Environment Manager  
**Licensing:** To be selected before repository creation; dependencies and upstream emulator licenses must be reviewed separately.
