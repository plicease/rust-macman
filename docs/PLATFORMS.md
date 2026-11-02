# Platform support

The Mini vMac backend currently builds and runs only on Linux x86-64 and Windows
x86-64. A successful Rust compile doesn't make a platform supported: macman also has to build Mini vMac
there, launch it, and track it until it exits.

| Platform | Backend works | Release binary |
|---|---|---|
| Linux x86-64 | Yes | Yes |
| Linux i686 | No | Yes |
| Linux ARM64 | No | **No, deferred** |
| macOS ARM64 | No | Yes |
| macOS x86-64 | No | **No, deferred** |
| Windows x86-64 | Yes | Yes |
| Windows i686 | No | Yes |

Where the backend doesn't work, macman still manages ROMs, system images, and machines,
but `macman run` stops with an error saying the host isn't supported.

## Release targets

`.github/workflows/release.yml` lists its targets explicitly instead of using the
reusable workflow's default matrix, so a change to that default can't publish a new
platform unnoticed.

- **Linux ARM64** (`aarch64-unknown-linux-gnu`) was in the default matrix, and is now
  left out.
- **macOS x86-64** (`x86_64-apple-darwin`) was never in the default matrix, so no such
  artifact has been published. The explicit list keeps it that way.

## Before enabling a deferred target

### Linux ARM64

- Map the host to a Mini vMac build target. The 36.04 setup tool has `larm` (Debian
  armel), but no ARM64 target, so this may need a newer upstream or a port.
- Confirm that macman's build and option translation produce a working emulator.
- Test build, cache reuse, launch, and exit tracking on real ARM64 hardware.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.

### macOS x86-64

- Build Mini vMac with its native macOS target.
- Handle the `.app` bundle that target produces, rather than a single executable.
- Track the emulator process until it exits, which `open` on an app bundle wouldn't do.
- Validate and reuse `.app` builds in the cache.
- Keep the XDG-style paths, not `~/Library`, as tracked for macOS ARM64 in #7.
- Test the whole path end to end on real hardware.
- Add the target to `release.yml` and update the tests in `tests/release_targets.rs`.
