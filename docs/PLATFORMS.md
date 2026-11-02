# Platform support

The Mini vMac backend builds and runs on Linux x86-64, Windows x86-64, and macOS ARM64.
A successful Rust compile doesn't make a platform supported: macman also has to build
Mini vMac there, launch it, and track it until it exits.

| Platform | Backend works | Release binary |
|---|---|---|
| Linux x86-64 | Yes | Yes |
| Linux i686 | No | **No, deferred** |
| Linux ARM64 | No | **No, deferred** |
| macOS ARM64 | Yes | Yes |
| macOS x86-64 | No | **No, deferred** |
| Windows x86-64 | Yes | Yes |
| Windows i686 | No | **No, deferred** |

Where the backend doesn't work, macman still manages ROMs, system images, and machines,
but `macman run` stops with an error saying the host isn't supported.

## macOS paths

macOS uses the same layout as Linux: `~/.config/macman`, `~/.local/share/macman`, and
`~/.cache/macman`, with absolute `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME`
honored exactly as on Linux. macman doesn't use `~/Library`, and doesn't migrate anything.

## Mini vMac revisions

The revision is per host, because 36.04 has no ARM64 target of any kind. 37.03 is the
beta that adds one, as tagged by the repository that continues Mini vMac.

| Host | Revision | Build target | Source |
|---|---|---|---|
| Linux x86-64 | 36.04 | `lx64` | gryphel.com |
| Windows x86-64 | 36.04 | `wx64` | gryphel.com |
| macOS ARM64 | 37.03 | `mcar` | [minivmac/minivmac](https://github.com/minivmac/minivmac) tag |

A machine isn't tied to a revision, so it runs on whichever one its host builds. Its
`revision` under `[emulator]` records where it came from, and it's never rewritten, so
there's nothing to migrate or edit: existing machines and archives work as they are,
in either direction. The build key and build manifest hold the revision really used,
and `--verbose` says when that differs from the machine's.

36.04 and 37.03 are interchangeable because their sources were compared: the emulated
models, memory sizes, ROMs, and disk formats match, and 37.03's changes to the emulator
core are bug fixes. Two of its defaults would change what a guest sees, so macman turns
them back. `-ndp 0` keeps blank and unrecognized images mountable, and `-as 0` keeps the
II and IIx from slowing down automatically.

A revision that hasn't been compared this way is an error when importing or running,
and it asks you to upgrade macman, because there's no way to know the guest would
behave the same. Add a new revision to `INTERCHANGEABLE_REVISIONS` only after the same
comparison, and resolve any such difference there, in the build options.

The 37.03 archive is one GitHub generates from the tag, and the pin is its SHA-256. If
GitHub ever changed how it generates archives, downloads would be refused rather than
trusted, and the pin would need checking against the tag.

## Basilisk II

| Host | Status |
|---|---|
| Linux x86-64 | Yes: builds, and boots System 7.5.5 from a Quadra 650 ROM |
| Windows x86-64 | Yes: builds, and boots System 7.1 from a Quadra 650 ROM with a 68030 setting |
| macOS ARM64 | Yes: builds, and boots System 7.1 from a Quadra 650 ROM with a 68030 setting |

On any other host macman refuses to create or run such a machine, while still importing
one for passing on. No host counts as supported until
[the smoke test](SMOKE_TEST.md#basilisk-ii) has passed there.

Mac OS 8.1 hasn't been tried, for want of an image, and nor have sound on a real device,
`slirp` networking, or full screen. System 7.5.5 was tested on Linux with 68030 and 68040
settings.

On Windows, upstream builds only 32-bit MinGW with MSYS2, and macman doesn't follow it. It
compiles a 64-bit static emulator itself, with MinGW-w64 and the official
`SDL2-devel-2.32.10-mingw.tar.gz`, whose SHA-256 it pins, so neither Autotools nor MSYS2
is needed; the changes this takes to the source are explained in
`src/backend/macemu_windows.rs`.

The Windows smoke test hasn't covered a Shut Down from inside the guest, as Windows kept
scripted input from reaching the emulator's menus; only the fixture test sees a clean exit
there. A 68040 boots with a IIci's model, but the System 7.1 image at hand stops with an
error as a Quadra 900, so the default `--cpu` is yet to be tried, on System 7.5.5 or later.

On macOS ARM64, macman doesn't use upstream's Xcode project, which wants SDL and MPFR
installed system-wide. It runs the same `autogen.sh`, `configure`, and `make` as on Linux,
with the JIT left out, as it is in upstream's ARM64 builds. SDL 2, GMP, and MPFR come from
MacPorts or Homebrew, are found with `pkg-config`, and are linked dynamically, so a change
in their version or location means a new build. MPFR is the ARM64 FPU core: ARM64 has no
80-bit `long double`.

The macOS smoke test covered Shut Down from inside the guest, killing the emulator, and
killing macman, but not the default `--cpu 68040`, full screen, sound, `slirp` networking,
closing the window, or a shared folder. Homebrew hasn't been tried, only MacPorts.

The pin is commit `f532be3d3d70` of [kanjitalk755/macemu](https://github.com/kanjitalk755/macemu),
the maintained fork, and its SHA-256 is that of the archive GitHub generates for it. If
GitHub changed how it generates archives, downloads would be refused, not trusted.

Upstream documents System 7 and Mac OS 8.0 and 8.1 on 32-bit clean ROMs. macman
accepts those only: a Classic ROM needs a build with virtual addressing, and it's not
what this backend is for.

## Release targets

`.github/workflows/release.yml` lists its targets explicitly instead of using the
reusable workflow's default matrix, so a change to that default can't publish a new
platform unnoticed.

- **Linux i686** (`i686-unknown-linux-gnu`) was published through 0.2.0, and is now left
  out. It compiles, but the backend refuses the host, so it would only mislead.
- **Linux ARM64** (`aarch64-unknown-linux-gnu`) was in the default matrix, and is now
  left out.
- **macOS x86-64** (`x86_64-apple-darwin`) was never in the default matrix, so no such
  artifact has been published. The explicit list keeps it that way.
- **Windows i686** (`i686-pc-windows-msvc`) was in the default matrix, and is now left
  out. It compiles, but the backend refuses the host, so it would only mislead.

## Before enabling a deferred target

### Linux i686

- Map the host to upstream's `lx86` target, and accept a 32-bit `gcc`.
- Test build, cache reuse, launch, and exit tracking on a 32-bit toolchain.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.

### Linux ARM64

- Map the host to a Mini vMac build target. The 36.04 setup tool has `larm` (Debian
  armel), but no ARM64 target, so this may need a newer upstream or a port.
- Confirm that macman's build and option translation produce a working emulator.
- Test build, cache reuse, launch, and exit tracking on real ARM64 hardware.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.

### macOS x86-64

- Map the host to upstream's `mc64` target, and accept a `clang` that targets it.
- Check that the generated Makefile's flags suit the target. `-no_pie` is one difference.
- Test the whole path end to end on real hardware, as for macOS ARM64.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.

### Windows i686

- Map the host to upstream's `wx86` target, and accept a 32-bit MinGW `gcc`.
- Test build, cache reuse, launch, and exit tracking on a 32-bit toolchain.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.
