# macman

A [perlbrew](https://perlbrew.pl/)-inspired manager for named classic Macintosh
emulation environments. It creates machines from your own ROMs and bootable disk
images, builds the right [Mini vMac](https://www.gryphel.com/c/minivmac/) for each
one, or [Basilisk II](#basilisk-ii) for later 68k systems, and launches them with
disk-image safety checks.

macman does not emulate anything itself, and it does not include or download Apple
ROMs or system software. See [docs/DESIGN.md](docs/DESIGN.md) for the full design.

## Requirements

macman can build and run Mini vMac on Linux x86-64, Windows x86-64, and macOS ARM64;
see [docs/PLATFORMS.md](docs/PLATFORMS.md). Linux needs an X11 session, plus what Mini
vMac needs to compile: `gcc`, `make`, `bash`, `tar`, and the X11 headers. On Debian or
Ubuntu:

```sh
apt install build-essential libx11-dev libxext-dev curl
```

Windows needs a native MinGW-w64 toolchain. WSL, Cygwin, and MSVC aren't used. `tar` and `curl` come with Windows, and this provides the rest:

```powershell
winget install BrechtSanders.WinLibs.POSIX.UCRT
```

Open a new terminal afterwards so that `gcc` is on your `PATH`.

macOS needs Apple's command line tools, or Xcode with its license accepted:

```sh
xcode-select --install
```

Run `macman doctor` to check your setup.

## Usage

```sh
macman rom add ~/roms/MacPlus.rom --name plus-v3
macman system add ~/images/System-6.0.8.dsk --name system-6.0.8 --version 6.0.8

macman create mac-plus --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman use mac-plus
macman run
```

`macman rom add` checks a ROM the way Mini vMac does, so a damaged or truncated file is
refused at import. If a valid ROM is followed by extra data, as some `vMac.ROM` files
are, macman stores just the ROM and tells you. Your file isn't changed.

`macman create` copies the system image, so each machine boots its own private
disk and the registered image stays untouched. The first `run` of a hardware
configuration compiles Mini vMac; later runs, and other machines with the same
configuration, reuse that build.

To try a freshly built disk image without saving anything to the machine:

```sh
macman run mac-plus --disk ./build/tico68.dsk --readonly
```

With `--readonly`, the emulator gets a read-only throwaway copy, so your file can't
change. Without it, the emulator writes to the file itself.

Other commands: `list [--builds]`, `current`, `rom list`, `system list`,
`config show`, `clone`, `export`, `import`, `remove`, and `doctor`. Add `--verbose` for build and launch
logs.

On Linux and macOS, files live under the XDG directories: `~/.config/macman`,
`~/.local/share/macman`, and `~/.cache/macman`. macman doesn't use `~/Library`. On Windows, configuration is in
`%APPDATA%\macman`, and machines, ROMs, and images are in `%LOCALAPPDATA%\macman`, with
builds in `%LOCALAPPDATA%\macman\cache`.

On every system, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME` override
their directory when set to an absolute path, and macman adds `macman` to that path.
On Windows, an unset override falls back to `%APPDATA%` or `%LOCALAPPDATA%`, and then
to the folder Windows reports, so `HOME` isn't needed.

On Windows, your cache path must either be plain ASCII or have an 8.3 short name,
because MinGW's `make` can't work in a folder with characters outside your code page.
Short names are normally on for the system drive. If a build says otherwise, set
`XDG_CACHE_HOME` to a plain ASCII path. Disk paths need to be under 260 characters.

Mini vMac refuses to quit from its window while disks are mounted, so shut down the
Mac first, or use Control Mode: hold Ctrl and press Q, then Y.

## Supported hardware

| Model | `--model` | Memory | Display |
|---|---|---|---|
| Twiggy prototypes | `twiggy`, `twig43` | 128K, 512K | monochrome |
| 128K | `128k` | 128K, 512K | monochrome |
| 512Ke | `512ke` | 128K, 512K | monochrome |
| Plus | `plus` | 128K to 4M | monochrome |
| SE, SE FDHD, Classic | `se`, `sefdhd`, `classic` | 512K to 4M | monochrome |
| PowerBook 100 | `pb100` | 4M | 640x400 monochrome |
| II, IIx | `ii`, `iix` | 1M to 8M | up to 32-bit color |

## Disk images

macman identifies images by content, never by file name. Mini vMac can attach raw
MFS and HFS volumes and Disk Copy 4.2 images, but it can't mount a partitioned disk,
such as a Zip or hard-disk image. `macman system add` handles that for you by
extracting the volume, leaving your original untouched:

```sh
macman system add ~/images/system710-zip.image --name system-7.1 --version 7.1
```

If a disk has several volumes, choose one with `--partition N`. Disk Copy images are
unwrapped the same way, because Mini vMac attaches them read-only.

To get a volume without registering anything, use `macman disk`. `disk info` shows an
image's format and partitions, and `disk extract` copies a volume out, following the
same rules and never overwriting an existing file:

```sh
macman disk info system710-zip.image
macman disk extract system710-zip.image system710-hfs.img
```

`--raw` with `--partition N` extracts a partition whatever it holds, such as a driver.

`macman run --disk` takes an image as it stands, so a partitioned disk needs
extracting or registering first. Blank images are fine, and `--any-format` attaches an image macman
can't identify, ideally with `--readonly`. See
[docs/DISK_FORMATS.md](docs/DISK_FORMATS.md) for the details.

## Basilisk II

macman can also run [Basilisk II](https://github.com/kanjitalk755/macemu), for later 68k
software, generally System 7 through Mac OS 8.1. Keep Mini vMac for early Macs and for
exact models: Basilisk II imitates no particular Macintosh, and has no PowerPC, no Mac
OS 8.5 or later, and no exact Quadra hardware. It has been tested with System 7.5.5, and
not yet with Mac OS 8; see [docs/PLATFORMS.md](docs/PLATFORMS.md#basilisk-ii).

It's for Linux x86-64, Windows x86-64, and macOS ARM64. On Linux, besides what Mini vMac
needs, it compiles with:

```sh
apt install build-essential autoconf automake pkg-config libsdl2-dev
```

On Windows it needs only the same MinGW-w64 toolchain, with `g++`, and Windows 10 version
1903 or later. macman builds it without Autotools or MSYS2, and downloads a pinned copy
of SDL 2 for the purpose.

On macOS, besides Apple's command line tools, it compiles with either of these:

```sh
port install autoconf automake pkgconfig libsdl2 mpfr
brew install autoconf automake pkg-config sdl2 mpfr
```

It needs a 512K or 1M 32-bit clean ROM of your own, such as from a IIci, a Centris, or a
Quadra. The Plus, SE, II, and IIx ROMs that suit Mini vMac won't start it, and neither
will a Classic's.

```sh
macman rom add Quadra-650.ROM --name quadra650
macman system add System-7.5.5.img --name system-7.5.5 --version 7.5.5
macman create quadra --backend basilisk2 --rom quadra650 --system system-7.5.5 \
  --memory 64M --display 800x600x16 --cpu 68040
```

| Option | Values | Default |
|---|---|---|
| `--cpu` | `68020`, `68030`, `68040` | `68040` |
| `--no-fpu` | not with a 68040 | FPU on |
| `--no-sound` | | sound on |
| `--network` | `none`, `slirp` | `none` |
| `--shared-folder DIR` | a host folder the guest can read and write | none |
| `--memory` | `1M` to `1023M`, in whole megabytes | `32M` |
| `--display` | 512x342 to 7680x4320, at depth 8, 16, or 32 | `640x480x8` |
| `--scale` | 1 to 4; a window bigger than the screen is cut off | 2 |

A 68040 presents a Quadra 900, which Mac OS 8 needs, and anything else a IIci. These
are all run-time settings, so every machine on a host shares one build.

`--disk`, `--readonly`, locks, `doctor`, `list --builds`, and `export` work as they do
for Mini vMac. A Disk Copy image needs `--readonly`, since Basilisk II would write to it
without updating its checksum. The shared folder belongs to the host, so `export` leaves
it out. PRAM settings last for one run.

Closing the window presses the Mac's power key, so the guest asks to shut down. Choose
Shut Down, or press Ctrl-Esc to quit at once, which is as abrupt as a power cut. If
macman is killed, it takes the emulator with it, because Basilisk II doesn't lock its
disks on Linux and macman's locks would go. The guest's disks are then as after a crash.

A boot disk left that way can stop the Mac starting at all, at a flashing question mark.
`macman doctor` warns about one, and `macman run` offers to mark it as clean. That only
lets it start, without checking it, so run Disk First Aid in the guest afterwards.

On Windows, there are a few differences:

- There's no shared folder, as Basilisk II can only offer whole drives there, so a machine
  with one is refused.
- Disks must be under 4G, as bigger ones would be truncated in the guest, and their paths
  under 260 bytes.
- If macman is killed, the emulator keeps running, and Windows itself keeps others off its
  disks until it stops.
- The window's close button does nothing on System 7.1, and Ctrl-Esc, which Windows uses
  for the Start menu, hasn't been tested. Choose Shut Down in the guest.

On macOS, if macman is killed, the emulator keeps running and holds a lock on its disks.
`macman run` refuses them, saying why, until you quit that emulator.

## Moving machines between hosts

`macman export` writes machines to an archive, with each one's current disks and the
ROM it uses. `macman import` brings them in on another host, with no compiling until
the first `run`:

```sh
macman export wombat-6 maciix-7 -o classic-macs.tar.gz
macman import classic-macs.tar.gz
```

Export refuses a machine whose disks are in use, so close the emulator first. Import
never replaces anything: importing the same archive again reports `unchanged`, and a
machine or ROM that differs from what's already there is reported as a conflict and
left alone, while the rest of the archive still imports. `macman import` exits with
status 1 if anything was left unresolved. To keep both, import a single-machine
archive as an independent copy:

```sh
macman import wombat-6.tar.gz --rename wombat-6-imported
```

Apple ROMs and System Software are copyrighted, and archives contain them. Use
archives to move your own machines between your own computers, and don't publish
them. See [docs/EXPORT_IMPORT.md](docs/EXPORT_IMPORT.md) for the details.

## Working offline

macman downloads its emulators on the first build and verifies its SHA-256. That's 36.04 on
Linux and Windows, and 37.03 on macOS, the first version with an ARM64 target. For a
machine without network access, download the source archive for
[36.04](https://www.gryphel.com/d/minivmac/minivmac-36.04/minivmac-36.04.src.tgz) or
[37.03](https://github.com/minivmac/minivmac/archive/refs/tags/37.03.tar.gz)
yourself and point `config.toml` (in the configuration directory above) at it:

```toml
[minivmac]
source_archive = "/path/to/minivmac-36.04.src.tgz"
```

Basilisk II is a [macemu commit](https://github.com/kanjitalk755/macemu/archive/f532be3d3d70b70f272b52050ce0889a4025d624.tar.gz),
set the same way under `[basilisk2]`. On Windows, its SDL 2 package is set under `[sdl]`.

## License

macman is MIT licensed. Mini vMac and Basilisk II, which it fetches and compiles but
does not include, are GPL-2.0.
