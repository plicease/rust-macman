# macman

A [perlbrew](https://perlbrew.pl/)-inspired manager for named classic Macintosh
emulation environments. It creates machines from your own ROMs and bootable disk
images, builds the right [Mini vMac](https://www.gryphel.com/c/minivmac/) for each
one, and launches them with disk-image safety checks.

macman does not emulate anything itself, and it does not include or download Apple
ROMs or system software. See [docs/DESIGN.md](docs/DESIGN.md) for the full design.

## Requirements

Linux x86-64 with an X11 session, plus what Mini vMac needs to compile: `gcc`,
`make`, `bash`, `tar`, and the X11 headers. On Debian or Ubuntu:

```sh
apt install build-essential libx11-dev libxext-dev curl
```

Run `macman doctor` to check your setup.

## Usage

```sh
macman rom add ~/roms/MacPlus.rom --name plus-v3
macman system add ~/images/System-6.0.8.dsk --name system-6.0.8 --version 6.0.8

macman create wombat-6 --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman use wombat-6
macman run
```

`macman create` copies the system image, so each machine boots its own private
disk and the registered image stays untouched. The first `run` of a hardware
configuration compiles Mini vMac; later runs, and other machines with the same
configuration, reuse that build.

To try a freshly built disk image without saving anything to the machine:

```sh
macman run wombat-6 --disk ./build/tico68.dsk --readonly
```

With `--readonly`, the emulator gets a read-only throwaway copy, so your file can't
change. Without it, the emulator writes to the file itself.

Other commands: `list [--builds]`, `current`, `rom list`, `system list`,
`config show`, `clone`, `remove`, and `doctor`. Add `--verbose` for build and launch
logs.

Files live under the XDG directories: `~/.config/macman`, `~/.local/share/macman`,
and `~/.cache/macman`.

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

`macman run --disk` takes an image as it stands, so a partitioned disk needs
registering first. Blank images are fine, and `--any-format` attaches an image macman
can't identify, ideally with `--readonly`. See
[docs/DISK_FORMATS.md](docs/DISK_FORMATS.md) for the details.

## Working offline

macman downloads Mini vMac 36.04 on the first build and verifies its SHA-256. For a
machine without network access, download
[the source archive](https://www.gryphel.com/d/minivmac/minivmac-36.04/minivmac-36.04.src.tgz)
yourself and point `~/.config/macman/config.toml` at it:

```toml
[minivmac]
source_archive = "/path/to/minivmac-36.04.src.tgz"
```

## License

macman is MIT licensed. Mini vMac, which it fetches and compiles but does not
include, is GPL-2.0.
