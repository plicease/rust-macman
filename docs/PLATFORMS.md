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
| Linux x86-64 | Yes: builds, and boots System 7.5.5 and Mac OS 8.1 from a Quadra 650 ROM |
| Windows x86-64 | Yes: builds, and boots System 7.5.5 and Mac OS 8.1 from a Quadra 650 ROM |
| macOS ARM64 | Yes: builds, and boots System 7.5.5 and Mac OS 8.1 from a Quadra 650 ROM |

On any other host macman refuses to create or run such a machine, while still importing
one for passing on. No host counts as supported until
[the smoke test](SMOKE_TEST.md#basilisk-ii) has passed there.

Mac OS 8.1 was tried on Linux, with a 68040 setting: a System 7.5.5 machine was
upgraded from the 8.1 CD image, attached with `--disk`, and it boots and shuts down
cleanly. There, with `slirp` networking and DHCP, it reached a plain HTTP site by name,
and still shut down cleanly. Full screen works on Linux too: a machine can start in it,
and Ctrl+Enter switches it on and off. Writing to a shared folder works there as well, and
the changes persist. System 7.5.5 was tested on Linux with 68030 and 68040 settings, and
played its startup chime on a real device.

On Windows, upstream builds only 32-bit MinGW with MSYS2, and macman doesn't follow it. It
compiles a 64-bit static emulator itself, with MinGW-w64 and the official
`SDL2-devel-2.32.10-mingw.tar.gz`, whose SHA-256 it pins, so neither Autotools nor MSYS2
is needed; the changes this takes to the source are explained in
`src/backend/macemu_windows.rs`.

On Windows, System 7.5.5 and Mac OS 8.1 were each tested with the default 68040 setting,
and Mac OS 8.1 with `slirp` networking too, and with sound on a real device, played from
its Sound control panel. On both of those, closing the window opens the shutdown dialog; System
7.1 ignores it. Ctrl+Enter switches full screen on and off. After ending the
emulator from Task Manager, the next run offers the repair: declining it leaves the
machine at the flashing question mark, and accepting it lets the machine boot. System 7.1
was tested with a 68030 setting; the image at hand stops with an error as a Quadra 900.
Windows keeps Ctrl-Esc for the Start menu, so that quit key can't be used there.

On macOS ARM64, macman doesn't use upstream's Xcode project, which wants SDL and MPFR
installed system-wide. It runs the same `autogen.sh`, `configure`, and `make` as on Linux,
with the JIT left out, as it is in upstream's ARM64 builds. SDL 2, GMP, and MPFR come from
MacPorts or Homebrew, are found with `pkg-config`, and are linked dynamically, so a change
in their version or location means a new build. MPFR is the ARM64 FPU core: ARM64 has no
80-bit `long double`.

On macOS, System 7.5.5 and Mac OS 8.1 were each tested with the default 68040 setting, on
machines brought in with `macman import`, and sound on a real device and full screen worked. Mac OS 8.1 was tested with `slirp`
networking too. Closing the window opens the shutdown dialog in the guest. A shared folder
works in both directions, written from the host and from the guest. System 7.1 was tested
with a 68030 setting.

The macOS smoke test covered Shut Down from inside the guest, killing the emulator, killing
macman, and rebuilding a damaged build. Homebrew hasn't been tried, only MacPorts.

Installing from a CD has been tried on Linux x86-64 and macOS ARM64, and Windows refuses
it. On Linux, a machine made with `--disk-size 1G`, the Quadra 650 ROM, and 128M started from the Mac OS 8.1
CD image through `macman run --cdrom`, offered to initialize the disk, and took a basic
install, with "Update Apple Hard Disk Drivers" turned off. After Shut Down, which ended the
run normally, it started Mac OS 8.1 from its own disk without the CD. The ISO was unchanged.
With cue sheets, a Mac OS 8.1 machine started from its own disk with a plain and a Joliet ISO
9660 image attached, and both mounted as data discs whose files the Finder showed.

On macOS, a machine made the same way installed Mac OS 8.1 from its CD image, again with
"Update Apple Hard Disk Drivers" off, and then started 8.1 from its own disk. The build
there now has `--with-bincue`, for the cue sheets, so a macOS Basilisk II build from before
is rebuilt once. The CD image's hash was unchanged.

The pin is commit `f532be3d3d70` of [kanjitalk755/macemu](https://github.com/kanjitalk755/macemu),
the maintained fork, and its SHA-256 is that of the archive GitHub generates for it. If
GitHub changed how it generates archives, downloads would be refused, not trusted.

Upstream documents System 7 and Mac OS 8.0 and 8.1 on 32-bit clean ROMs. macman
accepts those only: a Classic ROM needs a build with virtual addressing, and it's not
what this backend is for.

## SheepShaver

| Host | Status |
|---|---|
| Linux x86-64 | Candidate: **boots Mac OS 8.1, and installs and boots 9.0.4**, smoke test unfinished |
| Windows x86-64 | Candidate: **builds and boots Mac OS 9.0.4**, interpreted, smoke test unfinished |
| macOS ARM64 | Candidate: **builds and boots Mac OS 9.0.4**, interpreted, smoke test unfinished |

On Linux x86-64 macman offers the backend, so that it can be tested. It's been built from
the pinned source and run on one host, Ubuntu 26.04 with GCC 15.2 and SDL 2.32. The build
took about 16 seconds, with real addressing and the JIT on. No host counts as supported
until [the smoke test](SMOKE_TEST.md#sheepshaver) has passed there, and release targets
don't depend on this backend.

These parts of the smoke test have been seen to work on that host:

- `macman doctor` reports `vm.mmap_min_addr` and an intact build, `macman list --builds`
  shows the build, and a later run reuses it.
- With `vm.mmap_min_addr` at 65536 the emulator stops with "Cannot map Low Memory Globals",
  and macman names the fix.
- With a guest running: the window, the 2x scale, and the RAM setting are right, and the
  guest writes to its own disk.
- A 68k ROM is refused. A ROM macman accepts and the emulator refuses, with
  `Unsupported ROM type.` and exit code 0, is reported as a failed start.
- Killing the emulator is reported as signal 9, and a crash as signal 11.
- Shut Down from inside the guest ends the run normally, and leaves the disk marked
  cleanly unmounted.
- Installing from a CD, below, and then cloning, exporting, and importing the installed
  machine, whose copies are byte for byte the same and boot.
- `slirp` networking, a shared folder, a read-only disk mounted in a running guest, and
  sound on a real device.

These haven't been tested: killing macman, and rebuilding a damaged build.

### Installing from a CD

On the same host, with a retail Mac OS 9.0.4 CD image (a partitioned ISO) and the New World
ROM:

1. `macman create ss904 --backend sheepshaver --rom nw --disk-size 1G --memory 256M` made
   a 1G sparse disk, using 2M on the host.
2. `macman run ss904 --cdrom <the ISO>` started from the CD, which showed as a CD, and the
   Finder offered to initialize the blank disk, as Mac OS Standard.
3. The installer hung at "Updating Apple hard disk drivers". After a forced quit, the next
   run with the CD, and the installer with that option turned off, installed Mac OS 9
   in about two minutes. Shut Down in the guest then ended the run normally.
4. `macman run ss904`, without `--cdrom`, started Mac OS 9 from its own disk, at the Setup
   Assistant.
5. With a working system on its disk, a run with the 9.0.4 CD still started from the CD. A
   plain ISO 9660 image, with no HFS volume, didn't stop it starting from its disk.
6. Before cue sheets, a plain ISO 9660 image showed in Mac OS 9 as "Audio CD 1", holding one
   audio track. With them, plain, Joliet, and Rock Ridge images made by `genisoimage` and
   `xorriso` mounted as data discs and opened in the Finder, four at once. With the
   emulator set to start from a CD, the first drive's ISO 9660 disc never mounted, so that's
   now only set for a disc with HFS. With a plain ISO given before the 9.0.4 CD, the Mac
   started from the 9.0.4 CD, whose system offered to initialize the plain disc.

The ISOs were unchanged after every run. The same 9.0.4 CD also started on the Old World
ROM, as did the Mac OS 8.1 CD, and the Old World ROM offered to initialize a blank disk
too. Nothing was installed with it.

### Windows x86-64

On Windows x86-64 macman offers it too, with a recipe of its own in
`src/backend/sheepshaver_windows.rs`, built like Basilisk II's with MinGW-w64 and the
pinned SDL 2 package, and no MSYS2, Autotools, or Perl. Upstream's `configure.ac` turns the
JIT off for 64-bit Windows, as its code generator reads only 32-bit objects, so the
PowerPC code is interpreted. Mac memory is one reserved block (`MEM_BULK`), so nothing
needs mapping at address 0. It's been built and run on one host, Windows 11 with
Strawberry Perl's MinGW-w64 GCC 13.2, in about 15 seconds. These have been seen to work
there:

- A build from an empty cache under a Unicode path, and a second machine reusing it.
- Mac OS 9.0.4 from a New World `Mac OS ROM`, which reached the Finder within 45 seconds
  at 800x600, interpreted. Run by macman at 640x480 with 2x scale, from a disk a killed run
  had left, it reached Disk First Aid's alert, which waits for a click. Waiting there, the
  emulator used about one core.
- A second run of a running machine, and a clone of it, are refused.
- Killing the emulator is reported as an abnormal exit. Killing macman leaves the emulator
  running with its disk held, so another run is refused until it stops.
- A ROM the emulator refuses prints `ERROR: Invalid ROM file size.`, with exit code 0.

On Windows a shared folder is refused, and disks must be under 4G, as for Basilisk II,
whose disk and file code it shares. With Mac OS 9.0.4, Shut Down from inside the guest,
`slirp` networking, full screen, and sound work there. Sound needs "Built-in" chosen as the
output in the guest's Sound control panel; until it is, the alert sound only flashes the
menu bar. `run --cdrom` and `create --disk-size` are refused there, as for Basilisk II,
since the emulator finds CDs its own way on Windows, which hasn't been tried. Mac OS 8.x
hasn't been tried there either.

On macOS ARM64, macman builds it as it does Basilisk II there, with `configure`, not
upstream's Xcode project, and with SDL 2 from MacPorts or Homebrew. `configure` names the
host `arm-apple-darwin`, and gives it upstream's fixed offset for Mac addresses,
`0x400000000000`, as a 64-bit Mach-O program can't map address 0. So nothing is mapped
there, and `vm.mmap_min_addr` doesn't come into it. There's no JIT, as `configure` makes one
only for ELF, so the build says `--disable-jit` and the PowerPC code is interpreted. The
emulator locks its disks there, so if macman dies it keeps running, and macman refuses those
disks until it's quit. It locks CD images there too, so a run holds its CDs exclusively,
not shared as on Linux, and a second machine naming one is refused rather than left without
it. With a CD attached, the emulator also adds a drive that follows the host's own CD
media, as it adds `/dev/cdrom` on Linux.

On an M1 Max with Xcode 26.3 and MacPorts SDL 2.32.10, it built from the pinned source, and
booted Mac OS 9.0.4 to the Finder on a New World `Mac OS ROM`, with 128M at 800x600. Shut
Down from inside the guest ended the run with exit code 0 and left no session. A second
machine, with 256M, reused the build. A second run of a running machine was refused.
Killing macman left the emulator running with its disk held, and the next run was refused,
saying why. Killing the emulator was reported as signal 9.

A machine made with `--disk-size 1G`, the same ROM, and 256M started from the Mac OS 9.0.4
CD image through `macman run --cdrom`, and installed onto the disk once "Update Apple Hard
Disk Drivers" was turned off; with it on, the installer stalled there, as on Linux. After
Shut Down it started 9.0.4 from its own disk, with a plain ISO 9660 image attached, which
mounted as a data disc with its folder and file. While the install ran, a second machine
naming the same CD image was refused. The CD images' hashes were unchanged.

`slirp` networking, sound on a real device, full screen, a shared folder, and a read-only
disk work there too. A damaged build, Mac OS 8.x, Homebrew, several HFS CDs at once, and
host CD media haven't been tried there.

### Other hosts, and guests

| Guest | ROM | Status |
|---|---|---|
| Mac OS 8.1 | Old World 4M dump (TNT nanokernel) | Boots to the Finder, 128M at 800x600. The disk was installed under Basilisk II |
| Mac OS 8.1 | New World `Mac OS ROM` (labelled 8.6) | **Crashes the emulator** (SIGSEGV), from the disk or the CD image |
| Mac OS 8.1 CD | Old World 4M dump (TNT nanokernel) | Starts from the CD |
| Mac OS 9.0.4 | New World `Mac OS ROM` (labelled 8.6) | Installed from the CD, then boots from its own disk |
| Mac OS 9.0.4 CD | Old World 4M dump (TNT nanokernel) | Starts from the CD. Not installed |
| No system, a blank disk | Either of those ROMs | Stays up. New World reaches the question-mark disk |
| Mac OS 9.0.4, on Windows | New World `Mac OS ROM` | Boots to the Finder, 64M at 800x600 |
| Mac OS 9.0.4, on macOS | New World `Mac OS ROM` | Boots to the Finder, 128M at 800x600 |
| Mac OS 7.5.2 to 8.0, 8.5 to 9.0 | One that suits the guest | Untested |
| Mac OS 9.1, 9.2.x, Mac OS X | Any | Not claimed. The emulator decodes such ROMs, but macman makes no promise |

macman accepts a 4M dump with a nanokernel the emulator knows (TNT, Alchemy, Zanzibar,
Gazelle, Gossamer, or NewWorld), or a compressed `Mac OS ROM` file under 4M, and refuses
everything else. That says it can be decoded, not that a given guest will boot from it.

The New World crash happened with the JIT on or off, and at 64M, 128M, and 256M, and only
once a Mac OS 8.1 system was involved. The same ROM starts and installs Mac OS 9.0.4, and
New World ROMs were made for Mac OS 8.5 and later, so the cause is most likely the pairing.
Use an Old World ROM for Mac OS 8.1.

It's a generic PowerPC Mac: the emulator patches the ROM to look like a Power Mac 9500,
and models no particular machine. No Power Mac's own hardware or timing is represented.

The pin is the same macemu commit as Basilisk II's, so the two share one download.

### Low memory

x86-64 builds default to real addressing, where Mac addresses are host addresses, so
`main_unix.cpp` has to map memory at address 0. Linux refuses that unless
`vm.mmap_min_addr` is 0, and it's 65536 on most distributions, so this does stop the
emulator. `macman doctor` reports this, and a failed start repeats the fix. Setting it to 0 allows every program on the
host to map address 0, which makes some kernel bugs easier to exploit, so it's for you to
decide, and macman never changes it.

`configure` also has a direct addressing mode that avoids this. macman doesn't use it,
because the precompiled JIT operations may assume real addressing. It hasn't been tried.

For the same reason, the emulator forces SDL onto X11, so a Wayland desktop needs XWayland.

## QEMU

| Host | Status |
|---|---|
| Linux x86-64 | Candidate: **starts the Mac OS 9.2.1 CD to the Finder** on `mac99`, smoke test unfinished |
| Others | Untried |

macman runs the host's own `qemu-system-ppc`, so the backend is offered wherever one is
found, but only Linux x86-64 has been tried: Ubuntu 26.04, with its QEMU 10.2.1 and
OpenBIOS v1.1. No host counts as supported until [the smoke test](SMOKE_TEST.md#qemu)
has passed there, and release targets don't depend on this backend.

These parts of the smoke test have been seen to work on that host:

- `macman doctor` reports QEMU's version and path, and that it starts a `mac99` and a
  `g3beige`.
- `doctor`'s other QEMU checks were seen with stand-ins, as the packages weren't removed.
  With no `qemu-system-ppc` on `PATH`, it warned QEMU isn't available, and failed each
  qemu-ppc machine, with the install hint. A wrapper on `PATH` passing a missing `-bios`
  made it fail both machines, quoting QEMU, and name `openbios-ppc`. One leaving `sdl` out
  of `-display help` made it warn there's no SDL window, and name `qemu-system-gui`.
- `macman create --backend qemu-ppc --disk-size 2G`, then `macman run --media` with the Mac
  OS 9.2.1 CD, started to the Finder from the CD, and quitting QEMU ended the run with
  exit code 0 and no session left.
- With no CD, a blank disk stops at OpenBIOS's `0 >` prompt.
- A second run of a running machine is refused, and `doctor` still reports it runnable.
- Killing macman with `kill -9` stops QEMU, and the next run warns about the earlier one.
- On KDE Plasma under Wayland, QEMU's GTK window let the grabbed pointer slip out, even
  in full screen, and SDL's held it, which is why macman asks for SDL on Linux. `macman
  run` opens SDL's window by itself, and Ctrl-Alt-G lets the mouse go there. With the
  Mac's relative mouse, the pointer is slower in full screen than in a window, as full
  screen magnifies the Mac's display. Full screen at 1440x960 on a 2880x1920 panel, with
  the speed raised in the Mouse control panel, was usable, if less responsive than the
  host. The new speed took effect only once the control panel's window closed. Mac OS
  9.2.1 ignored QEMU's `usb-tablet`.
- QEMU emulates no Power Mac sound chip, so Mac OS 9.2.1 had no sound. With
  `-device usb-audio`, chosen as the output in the Sound control panel, it played. Until
  then, the device only had silence from it. It was tried first through a wrapper, then
  as macman adds it for a `mac99` with sound on, where the Sound control panel's choice,
  saved on the Mac's disk, carried over to the next run.
- `slirp` networking worked in Mac OS 9.2.1.
- A machine made by the wizard from the 9.2.1 CD installed onto its blank disk, and then
  started from that disk with no CD.

- The wizard, started from the registered Mac OS X 10.4 DVD, recommended `mac99` and
  suggested a 10G blank disk. A size under 4G, typed in, was taken with a warning.
- Mac OS X 10.4's retail DVD, as a bare HFS+ volume, registered with `media add`, which
  put a partition map in front, and `macman run --media` started its installer to the
  language screen. OpenBIOS found no startup file on the volume without the map.

What's been found about guests:

- The Mac OS 9.0.4 CD didn't start on `mac99`, with or without `via=pmu`.
- A `g3beige` can't start classic Mac OS: OpenBIOS has no Old World ROM, which it needs.
  It's offered for PowerPC Mac OS X.
- Mac OS X 10.4's kernel panics, with `pmap_map_block: collision`, with some memory sizes:
  770M, 1001M, 1014M, 1018M, 1022M, 1023M, 1540M, and 2044M did, and 260M, 510M, 516M,
  772M, 896M, 960M, 992M, 1000M, 1008M, 1012M, 1016M, 1020M, 1024M, 1056M, 1152M, 1280M, 1536M, and
  2048M didn't. No rule fits both but multiples of 32M, which macman requires.
- Mac OS X 10.4's installer starts on `mac99`. Installing it, other releases, and Mac OS X
  on a `g3beige` haven't been tried.

A blank disk for Mac OS X, from the wizard, is bigger than the 1G it suggests for classic
Mac OS: Apple's requirement for the release, with room for applications and updates. A
smaller size typed in is taken, with a warning that it's below the requirement.

| Release | Apple's requirement | Wizard suggests |
|---|---|---|
| 10.0, 10.1 | 1.5GB ([Macintosh Repository](https://www.macintoshrepository.org/45-mac-os-x-for-ppc-osx-10-0-10-1-10-2-10-3-10-4-10-5), [TechRadar](https://www.techradar.com/news/computing/apple/macos-through-the-ages-a-visual-guide-1254878/2)) | 4G |
| 10.2 | 3GB ([TechRadar](https://www.techradar.com/news/computing/apple/macos-through-the-ages-a-visual-guide-1254878/2)) | 6G |
| 10.3 | 1.5GB ([Wikipedia](https://en.wikipedia.org/wiki/Mac_OS_X_Panther)) | 6G |
| 10.4 | 3GB, 4GB with the Xcode developer tools ([Wikipedia](https://en.wikipedia.org/wiki/Mac_OS_X_Tiger), [Apple Discussions](https://discussions.apple.com/thread/2411058?page=1)) | 10G |
| 10.5 | 9GB ([Apple](https://support.apple.com/kb/sp517)) | 20G |

The warning uses 4GB for 10.4, as the developer tools are on its DVD. Disks are sparse
where the host allows, so a bigger one costs little until it fills.

These haven't been tested: a read-only extra disk, and moving a machine to another host. The wizard
recommends `mac99` for Mac OS 9.1 to 9.2.2, from 9.2.1's start, and Mac OS X 10.4, from its
installer's start, and never `g3beige` for classic Mac OS.

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
