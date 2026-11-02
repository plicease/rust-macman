# Platform support

The Mini vMac backend builds and runs on Linux x86-64, Windows x86-64, and macOS ARM64.
A successful Rust compile doesn't make a platform supported: macman also has to build
Mini vMac there, launch it, and track it until it exits.

| Platform | Backend works | Release binary |
|---|---|---|
| Linux x86-64 | Yes | Yes |
| Linux i686 | No | Yes |
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

Machines record their revision, so a machine made on another host, or by an earlier
macman on a Mac, says so when it's run. Its disks are unaffected: set `revision` under
`[emulator]` in its `machine.toml` to the one the error names.

`macman import` fails such a machine, as it does any revision this host can't build, so
for now an archive can't move a machine between macOS and the other hosts.

The 37.03 archive is one GitHub generates from the tag, and the pin is its SHA-256. If
GitHub ever changed how it generates archives, downloads would be refused rather than
trusted, and the pin would need checking against the tag.

## Release targets

`.github/workflows/release.yml` lists its targets explicitly instead of using the
reusable workflow's default matrix, so a change to that default can't publish a new
platform unnoticed.

- **Linux ARM64** (`aarch64-unknown-linux-gnu`) was in the default matrix, and is now
  left out.
- **macOS x86-64** (`x86_64-apple-darwin`) was never in the default matrix, so no such
  artifact has been published. The explicit list keeps it that way.
- **Windows i686** (`i686-pc-windows-msvc`) was in the default matrix, and is now left
  out. It compiles, but the backend refuses the host, so it would only mislead.

## Before enabling a deferred target

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
