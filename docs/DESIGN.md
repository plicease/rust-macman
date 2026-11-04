# MacMan — Classic Macintosh Environment Manager

**Status:** Design proposal  
**Initial implementation:** Rust CLI, Linux x86-64 host  
**Initial backend:** Mini vMac  
**Later backends:** Basilisk II, and SheepShaver as a preview  
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
- Basilisk II or SheepShaver integration (later backends).
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
macman config edit wombat-6
macman config set wombat-6 hardware.memory 4M display.scale 2
macman config set --machine wombat-6 display.width 640 display.height 480
macman config keys wombat-6
macman clone wombat-6 wombat-test
macman remove wombat-test
macman system remove system-6.0.8
macman export wombat-6 -o wombat-6.tar.gz
macman export --all -o machines.tar.gz
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
  media/
    macos-9.0.4.img     # read-only, the whole image as registered
    macos-9.0.4.toml
  aliases.toml          # other names for machines, each with its machine's name and id
  machines/
    wombat-6/
      machine.toml
      system.dsk
      data.dsk
      checkpoints/
        clean-install.dsk   # read-only copy of the boot disk
        clean-install.toml  # created, disk, size, sha256; written last
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

A checkpoint exists once its `.toml` does. The image is copied, verified, and made read-only first, so a `.dsk` without one is an unfinished checkpoint that nothing lists, counts, or rolls back to, and the next `checkpoint create` of that name replaces it. Rollback copies the checkpoint to a temporary file beside the boot disk, verifies it, and renames it over the disk.

Machine files and templates live in the data directory; generated executables and source checkouts can be evicted and rebuilt from the cache. Avoid implicit deletion of user-owned disks. Use atomic writes for configuration and registration metadata.

Example `machine.toml`:

```toml
schema_version = 1
name = "wombat-6"
id = "6f1c1f6e-7a3d-4c55-9d0e-3a0f2f6f0a11"  # stable across export and import
ephemeral = false  # true: a plain `run` boots a temporary copy of the boot disk

[emulator]
backend = "minivmac"
revision = "pinned-source-identifier"

[hardware]
model = "plus"
rom = "plus-v3"  # absent for QEMU, which takes no Mac ROM
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

- **Revision:** 36.04 has no ARM64 target. The 37.03 beta adds `mcar`, so macOS pins it, from the GitHub repository that continues Mini vMac. Other hosts stay on the stable 36.04. A machine's `revision` records where it came from, and the backend resolves it to the host's revision, so machines move between hosts. `-ndp 0` and, for the II and IIx, `-as 0` restore the 36.04 defaults a guest would notice. See [PLATFORMS.md](PLATFORMS.md#mini-vmac-revisions).
- **Build:** `-e xcd -cl -t mcar` asks for a Makefile and `clang`, so the command line tools are enough, where a plain `-e xcd` would generate a project needing all of Xcode. The setup tool is compiled with `clang` too.
- **SDK:** the generated Makefile fixes `-isysroot` to `/Library/Developer/CommandLineTools/SDKs/MacOSX11.1.sdk`, which current tools don't have, so macman substitutes `xcrun --show-sdk-path`.
- **Cache:** a build is `minivmac.app`, whole. The manifest records a digest of the bundle's names, contents, executable bits, and links, so damage anywhere causes a rebuild. Its key includes the host, `clang`'s target and version, and the SDK's version and path, which keeps it apart from any Linux, Intel Mac, or other-SDK build.
- **Launch:** `OSGLUCCO.m` never reads its arguments, so there are no `-r` or `-d` flags and no paths to pass. It looks beside its bundle for the ROM by a per-model name, and for `disk1.dsk`, `disk2.dsk`, and so on, stopping at the first gap. Each run copies the bundle and ROM into the session directory, with the disks as consecutive links there, boot disk first. A link reaches the real file through `fopen`; only Finder aliases are resolved separately.
- **Process:** macman runs `Contents/MacOS/minivmac` directly, not through `open`, so it's the emulator's parent and gets its real exit status.
- **Exit status:** signal numbers differ from Linux. SIGBUS is 10 rather than 7, which is reported as a host crash.
- **Locks and sharing:** as on Linux, macman's locks last as long as it does. The emulator also takes `flock` on writable disks, so if macman dies and leaves it running, a second emulator refuses the disk itself.

### Basilisk II

Checked against the pinned macemu source and real builds; see [PLATFORMS.md](PLATFORMS.md#basilisk-ii). The module comment in `src/backend/basilisk2.rs` has the details. It differs from Mini vMac in these ways:

- **Configuration:** everything is a run-time preference, so one build serves every machine on a host. A `[basilisk2]` table holds `cpu`, `fpu`, `sound`, `network`, and `shared_folder`, and `[hardware] model` is left out.
- **Build:** `autogen.sh`, `configure`, and `make` under `BasiliskII/src/Unix`, with SDL 2 and without GTK. The key covers the source, the configure options, `g++`, and SDL.
- **Launch:** each run writes a preferences file to its session directory and passes it with `--config`, so the user's own settings and XPRAM aren't touched. That file cuts lines at 256 bytes, so the ROM, disks, and shared folder go on the command line.
- **Host files:** upstream's default `extfs` is `/`, a read-write volume of the whole host. macman always sets it, empty unless the machine has a shared folder.
- **Exit status:** `QuitEmulator` always exits 0, even for a refused ROM, so macman reads the emulator's output, and counts an `ERROR:` line as a failure to start. GTK would have shown a dialog instead.
- **Locks:** Linux builds take no locks of their own, so `PR_SET_PDEATHSIG` stops the emulator if macman dies, and the disks are never written with macman's locks gone. It sends `SIGKILL`, because SDL turns `SIGTERM` into the window closing, which only asks the guest to shut down. The child then checks `getppid`, and doesn't start if macman died before the signal was armed.
- **ROMs:** it starts only from 32-bit clean ROMs, which have no registered models, so backends check ROMs themselves.
- **Disks:** it writes straight through a Disk Copy image, so those attach read-only or not at all.

On Windows it also differs from its Linux self. `src/backend/macemu_windows.rs` has the details of the build:

- **Build:** no `configure`, as the tree's Windows port targets 32-bit MSVC. macman edits `config.h`, generates a Makefile, and links a static 64-bit `BasiliskII.exe` against a pinned SDL 2 package. The key covers the source, the compiler, SDL, and `RECIPE`, which changes with these steps.
- **Patches:** `vm_acquire` allocates bottom-up, or memory comes back in descending order and Mac addresses wrap. `ErrorAlert` prints under `nogui`, or a refusal would be silent.
- **Unicode:** a manifest sets the active code page to UTF-8, since SDL hands `main` UTF-8 and the emulator opens files with narrow calls. Short names won't do, as NTFS keeps characters like `ë` in them.
- **Launch:** preferences and a copy of the ROM go in the session, by relative name, as XPRAM lands in the current directory, and every path is limited to 259 bytes.
- **Host files:** `extfs` is ignored, and `enableextfs` shows whole drives, so it's always off, and a shared folder is refused.
- **Locks:** `CreateFile` opens disks with no sharing, so Windows protects them itself, and the emulator can outlive macman.
- **Disks:** `GetFileSize` is read as 32 bits, so disks of 4G or more are refused.
- **Installing:** `create --disk-size` and `run --cdrom` are refused here, until they're tried. On Linux and macOS they work as for SheepShaver, below.

On macOS ARM64 it builds as on Linux, and differs in these ways:

- **Build:** upstream's Xcode project wants SDL and MPFR installed system-wide, so macman runs `configure` under `BasiliskII/src/Unix` with clang, without the JIT. `pkg-config` finds SDL, GMP, and MPFR, and the key covers their versions and library directories, as they're linked dynamically.
- **FPU:** the core is MPFR's, as ARM64 has no 80-bit `long double`.
- **Locks:** `Sys_open` takes `O_EXLOCK` on every disk file, and there's no `PR_SET_PDEATHSIG`, so the emulator can outlive macman. A second emulator's warning sits unflushed in its output and it shows no disk, so macman tries that lock before launching, and refuses with a reason.

### SheepShaver

Read from the pinned macemu source, tested with a stand-in for the emulator, and partly checked on the real one on Linux x86-64; see [PLATFORMS.md](PLATFORMS.md#sheepshaver). The module comment in `src/backend/sheepshaver.rs` has the details. It works as Basilisk II does on Linux, and differs in these ways:

- **Configuration:** a `[sheepshaver]` table holds `sound`, `network`, and `shared_folder`. There's no CPU, FPU, or model to choose.
- **ROMs:** a 4M dump with a nanokernel it knows, or a compressed `Mac OS ROM` file under 4M.
- **Limits:** 16M to 1024M of RAM, and at least 640x480. Depth must be 32, as it starts at the desktop's depth, and has no setting.
- **Addressing:** on Linux, upstream's default, real addressing, which needs `vm.mmap_min_addr` to be 0. `doctor` and failed starts say so, and the requirement has been confirmed on a real run.
- **JIT:** on, unlike Basilisk II, as the interpreter is too slow for its guests.
- **Hosts:** Linux x86-64, Windows x86-64, and macOS ARM64.
- **macOS:** built with `configure`, as Basilisk II is there, with SDL from `pkg-config` and no MPFR. `configure` gives the `arm-apple-darwin` host a fixed offset for Mac addresses, `0x400000000000`, so nothing is mapped at address 0. There's no JIT, as `configure` makes one only for ELF, so it's configured out and `jit` is off. The emulator locks its disks, so macman tries those locks before launching, as for Basilisk II.
- **Installing:** on Linux and macOS, as on Windows the emulator finds CDs its own way, which hasn't been tried. On macOS the emulator takes `O_EXLOCK` on CD images as on disks, so macman locks them exclusively there, not shared, and tries the emulator's lock before launching. `create --disk-size` makes a sparse, unformatted boot disk in place of a system image's copy. `run --cdrom` passes images with `--cdrom`, not as disks, and turns `nocdrom` off and sets `bootdriver` to the CD driver's -62 for that run, so the Mac starts from the CD. The emulator opens them read-only, so they're locked shared and not copied. Turning `nocdrom` off also makes the emulator add `/dev/cdrom`. A blank or partitioned boot disk is fine for this backend, as the guest formats its own.
- **ISO 9660 discs:** the emulator reads a CD image's table of contents with a drive `ioctl`, which fails on a file and leaves the guest seeing one audio track. HFS is found by reading blocks, so HFS discs are passed as they are. An image with only ISO 9660 is linked into the session beside a cue sheet of one `MODE1/2048` track, which the emulator reads with `--with-bincue`, so ISO 9660 File Access sees a data disc. Both emulators share this.

On Windows, `src/backend/sheepshaver_windows.rs` builds it as Basilisk II is built there, with these differences:

- **Build:** the tree has no `config.h`, so macman writes all of it. `MEM_BULK` keeps Mac memory in one reserved block, so there's no need for address 0. There's no JIT, as `configure.ac` turns it off on x86-64.
- **Links:** much of the tree is symbolic links into Basilisk II's. macman unpacks the archive itself and copies what each link names, as Windows won't make links without privileges.
- **Generated code:** `genexec.pl` lists the instruction handlers to instantiate. macman does the same in Rust, so Perl isn't needed, and refuses a decoder with templates, which the script would expand.
- **Launch:** preferences, ROM, disks, and limits are as for Basilisk II on Windows, from shared code in `src/backend/macemu_windows.rs`.

### QEMU

Run with the host's own QEMU and real guests; see [PLATFORMS.md](PLATFORMS.md#qemu). The
module comment in `src/backend/qemu.rs` has the details. It differs from the others in
these ways:

- **Emulator:** nothing is built. macman runs `qemu-system-ppc` from `PATH`, or the one
  `[qemu-ppc] executable` names in `config.toml`. Its `--version` and `-machine help` say
  what it is and which Power Macs it has.
- **Revision:** the QEMU version a machine was made with. Any QEMU runs it, so it moves
  between hosts as it is.
- **Configuration:** a `[qemu]` table holds `machine` (`mac99` or `g3beige`), `sound`,
  and `network`. There's no `[hardware] model`, and no `rom`.
- **ROMs:** none. QEMU starts from OpenBIOS, its own Open Firmware, so `rom` is left out of
  `[hardware]`, archives carry no ROM for the machine, and `--rom` is refused.
- **Limits:** 640x480 to 3840x2160 at depth 8, 15, or 32, scale 1, and four IDE drives for
  disks and CDs together. Memory is 32M to 2048M on a `mac99` and 2016M on a `g3beige`, in
  steps of 32M: Mac OS X 10.4's kernel panicked, with `pmap_map_block: collision`, on
  sizes such as 770M, 1023M, 1540M, and 2044M, and started on every multiple of 32M tried.
- **Launch:** disks and CDs are given whole and raw, CDs read-only, with HFS discs first,
  and `-boot d` when one can start the Mac. `mac99` gets `via=pmu`, for USB input.
  `auto-boot?` is set, so OpenBIOS starts the guest without stopping.
- **Sound:** QEMU has neither Power Mac's sound chip, so with sound on a `mac99` gets
  `-device usb-audio`, which Mac OS 9 drives once it's chosen in the Sound control panel.
  A `g3beige` has no USB bus, so it gets `-audio none` either way.
- **HFS+ discs:** a CD with HFS+ in a partition map starts the Mac, as an HFS one does.
  OpenBIOS won't start a bare HFS+ volume, so `media add` stores one behind a two-entry
  partition map, the map and an `Apple_HFS` partition, recording the source's digest.
  SheepShaver and Basilisk II get HFS+ discs as they are, as data discs.
- **Window:** on Linux, SDL's when `-display help` lists it. GTK's, QEMU's default, grabs
  the mouse by warping the pointer, which Wayland refuses, so the pointer escapes; SDL
  confines it. Elsewhere QEMU chooses. Mac OS 9 doesn't drive `usb-tablet`, so the mouse
  stays relative.
- **Exit status:** QEMU reports problems on stderr and exits with a failure, so macman
  shows its words. On Linux it stops with macman, as Basilisk II does.

### Shared code

Fetching a pinned source and publishing a build are in `src/backend/source.rs`, for every backend. What the two macemu emulators have in common is in `src/backend/macemu.rs`: the pin, the `configure` build, `doctor` lines, and launching and watching the process. Nothing about CPUs, ROMs, or guests is shared.

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
- Prefer copy-on-create and explicit copies for experimentation. `run --ephemeral` boots a writable copy of the boot disk in the session directory, never a hard link, and holds a shared lock on the saved disk for the run; the copy goes with the session, and a sweep of stale sessions reclaims it after a crash.
- Do not advertise an attachment as read-only unless the backend or an isolated copy *actually prevents writes*. If Mini vMac cannot enforce read-only attachment, use a throwaway copy and discard changes, with explicit CLI messaging.
- Never delete registered source templates during machine removal; prompt or require an explicit destructive flag to delete private machine disks.

Disk formats and capacity restrictions vary by emulated model and System version. Validate supported formats and reject unknown ones with actionable diagnostics. Avoid data conversion, except that registering a system image may store just the volume from a container Mini vMac cannot use writably, and registering a bare HFS+ volume as media stores it behind a partition map, which Open Firmware needs to start from it, leaving the source untouched both times. See [DISK_FORMATS.md](DISK_FORMATS.md).

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

- SheepShaver adapter.
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
