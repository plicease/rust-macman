# macman

A [perlbrew](https://perlbrew.pl/)-inspired manager for named classic Macintosh
emulation environments. It creates machines from your own ROMs and bootable disk
images, builds the right [Mini vMac](https://www.gryphel.com/c/minivmac/) for each
one, and launches them with disk-image safety checks.

macman does not emulate anything itself, and it does not include or download Apple
ROMs or system software. See [docs/DESIGN.md](docs/DESIGN.md) for the full design.

## Requirements

macman can currently build and run Mini vMac only on Linux x86-64 and Windows x86-64;
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

Open a new terminal afterwards so that `gcc` is on your `PATH`. Use a 64-bit macman;
the 32-bit Windows build can't run the Mini vMac backend.

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
`config show`, `clone`, `remove`, and `doctor`. Add `--verbose` for build and launch
logs.

On Linux, files live under the XDG directories: `~/.config/macman`,
`~/.local/share/macman`, and `~/.cache/macman`. On Windows, configuration is in
`%APPDATA%\macman`, and machines, ROMs, and images are in `%LOCALAPPDATA%\macman`, with
builds in `%LOCALAPPDATA%\macman\cache`.

On either system, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME` override
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

## Working offline

macman downloads Mini vMac 36.04 on the first build and verifies its SHA-256. For a
machine without network access, download
[the source archive](https://www.gryphel.com/d/minivmac/minivmac-36.04/minivmac-36.04.src.tgz)
yourself and point `config.toml` (in the configuration directory above) at it:

```toml
[minivmac]
source_archive = "/path/to/minivmac-36.04.src.tgz"
```

## License

macman is MIT licensed. Mini vMac, which it fetches and compiles but does not
include, is GPL-2.0.
