# macman

A [perlbrew](https://perlbrew.pl/)-inspired manager for named classic Macintosh
emulation environments. It creates machines from your own ROMs and bootable disk
images, builds the right [Mini vMac](https://www.gryphel.com/c/minivmac/) for each
one, [Basilisk II](#basilisk-ii) for later 68k systems, or a preview of
[SheepShaver](#sheepshaver) for PowerPC Mac OS, and launches them with
disk-image safety checks. It can also run machines on the host's own
[QEMU](#qemu) for later PowerPC Mac OS.

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

Run `macman doctor` to check your setup. On a terminal it colors each status, green for
`ok`, yellow for `warn`, and red for `FAIL`. Redirected output, or any with `NO_COLOR` set,
is plain text.

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

Or let `macman wizard` walk you through it, starting from a disk or CD image: see
[Making a machine with the wizard](#making-a-machine-with-the-wizard).

To try a freshly built disk image without saving anything to the machine:

```sh
macman run mac-plus --disk ./build/tico68.dsk --readonly
```

With `--readonly`, the emulator gets a read-only throwaway copy, so your file can't
change. Without it, the emulator writes to the file itself.

To try something out and then forget it, run on an ephemeral boot disk:

```sh
macman run mac-plus --ephemeral    # or -e
```

The machine boots from a temporary, writable copy of its boot disk, so the guest can
install software, change settings, and restart as usual within the run, and every
change to that disk is discarded when the run ends, however it ends. The next run starts
from the saved disk again. This is an ephemeral system disk, not a snapshot of the whole
machine: other disks the machine has, and `--disk` files, keep their changes as always,
and macman says so. `--readonly` still applies to `--disk` files only.

To make that the default for a machine, create it with `--ephemeral`, or set
`ephemeral = true` in its `machine.toml`. A plain `run` then uses a temporary copy, and
`run --no-ephemeral` uses the saved disk for that run. `macman list` shows each
machine as `persistent` or `ephemeral`, and clone, export, and import keep the setting.

While an ephemeral run is going, its saved boot disk is locked for reading: other
ephemeral runs of the machine may start, each from its own copy, but a persistent run,
`remove`, or anything else that would change the disk waits until it ends. The copy is a
reflink where the file system supports one (Btrfs, XFS, or APFS), so it's quick and
takes no room at first; elsewhere it's a full copy. It lives in the run's session
directory, and if macman is killed, the next `run` removes it.

Other commands: `list [--builds]`, `current`, `rom list`, `system list`, `system remove`,
`media list`, `media info`, `media remove`, `config show`, `config edit`, `wizard`, `clone`, `export`,
`import`, `remove`, `du [--physical]`, and `doctor`. Add `--verbose` for build and launch logs. Add `--json` to
`list`, `doctor`, `rom list`, `system list`, `media list`, `media info`, `disk info`,
`config show`, `current`, or `du` for output that scripts can read; [docs/JSON.md](docs/JSON.md) has the fields.

`rom list` shows each ROM's name, size, kind, and the start of its hash, then the
backends that can start from it, such as `minivmac` or `basilisk2`, or `unknown` for one
macman doesn't recognize. That's judged from the ROM's contents, never its name, and it
doesn't promise that every guest will boot.

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

### Changing a machine's settings

`macman config edit [MACHINE]` opens an editor in the terminal for a machine's settings,
grouped as Machine, Hardware, Display, and, for Basilisk II, SheepShaver, and QEMU, Emulator. It
offers only what the machine's backend has: Mini vMac has no CPU, sound, network, or shared
folder settings, a Mac Plus is offered at most 4M of memory and a 1-bit display, and
Basilisk II on Windows, which can't share a folder, shows that setting read-only. Each
list of choices marks with `✗` those that won't work with the rest of the machine, and
says why when one is highlighted, such as a ROM another backend needs. The checks are the
ones `create` and `run` make, so anything the editor saves, `run` accepts.

| Key | Does |
| --- | --- |
| `↑`/`↓` or `k`/`j` | Move between settings, or between choices |
| `Enter` or `Space` | Turn a setting on or off, open its choices, or pick one |
| `←`/`→` or `h`/`l` | Step to the previous or next choice that works |
| `e` | Type a value, for memory, display size, or the shared folder |
| `u` | Put the selected setting back as it was saved |
| `s` | Review the changes, then `y` to save them |
| `q` or `Esc` | Leave; with unsaved changes, it asks first |
| `Ctrl-C` | Leave at once, saving nothing |

Changed settings are marked `*`, and the title says `[unsaved changes]`. The review lists
each change, with notes such as that Mini vMac will build a new emulator for a different
memory size or display, or that a different ROM may not start the installed system.
Nothing is written until you confirm, and then `machine.toml` is replaced at once, never
left half-written.

The name, backend, model, and disks are shown but can't be changed here: they decide
what was built and what's installed, so make a new machine (or `clone` one) instead. The
machine's id, disk paths, and everything else the editor doesn't show are kept as they
were. It refuses a machine that's running, and won't save over a `machine.toml` that
something else changed while the editor was open.

`config edit` needs an interactive terminal. With its input or output redirected, it
fails with an error, writing no escape sequences, and changes nothing; use `config show`
to read the settings instead. It refuses `--json`.

For scripts, and anywhere without a terminal, `macman config set` makes the same changes
by name:

```sh
macman config set wombat-6 hardware.memory 4M
macman config set wombat-6 display.width 640 display.height 480 display.depth 8
macman config set ss904 sheepshaver.network slirp
macman config set --machine wombat-6 display.width 640 display.height 480
macman config set ephemeral true    # the current machine
```

Keys are named as in `machine.toml`. Give several `KEY VALUE` pairs to change settings
that only work together, such as a display's width and height or a CPU and its FPU: they
are applied together, and then the machine they make is checked once, whole, with the
same checks `config edit` makes before it saves, so the two accept and refuse the same
configurations. If anything is wrong, nothing is written, and the error says why, with the
values that would work where there's a list of them.

Name the machine with `--machine NAME`, or as the first word, or leave it out for the
current one. Without `--machine`, an odd number of words means the first is the machine.
`--machine` is clearer in scripts: every other word is then a key or its value, and the
machine can't also be given as a word. Either way, a value left out is reported as missing,
such as `"display.scale" has no value`, never read as a different machine or key.

| Key | Takes | Machines |
| --- | --- | --- |
| `ephemeral` | `true` or `false` | All |
| `hardware.rom` | A registered ROM | All but QEMU |
| `hardware.memory` | A size, such as `4M` or `512K` | All |
| `display.width`, `display.height` | Pixels | All |
| `display.depth` | Bits per pixel | All |
| `display.scale` | 1 to 4 | All |
| `display.fullscreen` | `true` or `false` | All |
| `basilisk2.cpu` | `68020`, `68030`, or `68040` | Basilisk II |
| `basilisk2.fpu` | `true` or `false` | Basilisk II |
| `basilisk2.sound`, `sheepshaver.sound`, `qemu.sound` | `true` or `false` | That backend |
| `basilisk2.network`, `sheepshaver.network`, `qemu.network` | `none` or `slirp` | That backend |
| `basilisk2.shared_folder`, `sheepshaver.shared_folder` | A host folder, or `""` for none | That backend |

`true` and `false` may also be given as `on`/`off` or `yes`/`no`. `macman config keys
[MACHINE]` lists the keys a machine has, with each one's value and, as the machine is now,
the values it can take. Other keys, such as `name`, `id`, `emulator.backend`,
`hardware.model`, `qemu.machine`, and `disks`, can't be set, for the reasons `config edit` shows them
read-only, and say so; one that's another backend's, or that the machine's backend
refuses, as Basilisk II on Windows refuses a shared folder, explains that too.

`config set` prints each change and then `Saved machine NAME`, or `No change to machine
NAME` if every value was already set, which isn't an error. Notes, such as that Mini vMac
will build a new emulator, go to stderr. It exits 0 when the machine has the settings
given, and 1 for anything refused, with the machine unchanged. Like `config edit`, it
refuses a machine that's running, and its write is atomic. Two `config set`s of one
machine at once each wait their turn, so neither loses the other's change, and a `config
edit` open meanwhile refuses to save over one. With `--json` it prints the changes; see
[docs/JSON.md](docs/JSON.md). In zsh, bash, and fish, completion offers the machine's keys,
and the values each can take, as for `config keys`.

### Aliases

A machine can have other names, which work wherever its own name does: `run`, `use`,
`config`, `checkpoint`, `rollback`, `clone`'s source, `export`, `remove`, and every
other command that takes an existing machine.

```sh
macman alias wombat-6 wombat       # wombat now names wombat-6
macman alias wombat plus           # and so does plus; aliases name the machine itself
macman run wombat                  # the same as macman run wombat-6
macman unalias wombat              # just the alias; the machine is untouched
```

`macman list` shows each machine's aliases, alphabetically, as `aliases plus, wombat`, or
`aliases -` for none, and `list --json` gives them as an `aliases` array. An alias names
the machine itself, never another alias, so `alias` given an alias for its machine makes
the new one name the machine; there are no chains. The machine keeps its own name and id,
and `use` with an alias makes the machine current, so `current` and the `*` in `list` show
the machine's name.

Machine names and aliases are one set of names: an alias can't be a machine's name or
another machine's alias, and `create`, `clone`, and `import`, with or without `--rename`,
refuse a name an alias has. Giving a machine an alias it already has does nothing, and
succeeds. `unalias` refuses a machine's name, so it can't be mistaken for `remove`.
Aliases are names on this host: `clone` doesn't copy them, and `export` doesn't carry
them, so an imported machine arrives with none. `remove` deletes a machine's aliases with
it, and names them when it asks. An alias follows its machine's id, so it still finds a
machine whose directory was renamed by hand; one whose machine is gone says so when used,
and `doctor` lists it, until `unalias` removes it.

Aliases are kept in `aliases.toml` in the data directory, replaced whole on each change,
and changed under the same lock that creating a machine takes, so simultaneous changes
each take effect and a new machine and a new alias can't take the same name. Completion
offers aliases wherever it offers machine names, and only aliases after `unalias`.

## Shell completion

`macman completions SHELL` prints a completion script for `zsh`, `bash`, `fish`,
`powershell`, or `elvish`. It covers subcommands, options, backend, model, CPU, and
network names, and file and folder arguments. In zsh, bash, and fish, it also
completes the names of your machines, ROMs, and system images, such as after
`macman run`, and `config set`'s keys and their values. The script asks the `macman` on your `PATH` for these when you press
Tab, so they're always current. It changes nothing. Install the script yourself.

For zsh, put the script in a folder on `fpath`, and add that folder to `fpath` before
`compinit` runs:

```sh
mkdir -p ~/.zfunc
macman completions zsh > ~/.zfunc/_macman
```

```sh
# in ~/.zshrc, above any existing compinit
fpath=(~/.zfunc $fpath)
autoload -Uz compinit
compinit
```

New shells pick it up. To use it in the shell you're in, run those three lines
there. If completion still doesn't appear, delete zsh's cache with
`rm -f ~/.zcompdump*` and start a new shell. Rerun `macman completions zsh` after
upgrading macman.

For bash, with the bash-completion package installed, which loads it in new shells:

```sh
mkdir -p ~/.local/share/bash-completion/completions
macman completions bash > ~/.local/share/bash-completion/completions/macman
```

To try it in the current bash without installing anything, run
`eval "$(macman completions bash)"`.

For fish:

```sh
macman completions fish > ~/.config/fish/completions/macman.fish
```

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
OS 8.5 or later, and no exact Quadra hardware. It has been tested with System 7.5.5 and
Mac OS 8.1; see [docs/PLATFORMS.md](docs/PLATFORMS.md#basilisk-ii).

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
`macman doctor` warns about one, and `macman run` asks whether to set its HFS
cleanly-unmounted flag. That only lets it start, and doesn't check or repair the disk, so
run Disk First Aid in the guest afterwards.

On Windows, there are a few differences:

- There's no shared folder, as Basilisk II can only offer whole drives there, so a machine
  with one is refused.
- Disks must be under 4G, as bigger ones would be truncated in the guest, and their paths
  under 260 bytes.
- If macman is killed, the emulator keeps running, and Windows itself keeps others off its
  disks until it stops.
- The window's close button asks to shut down on System 7.5.5 and Mac OS 8.1, as above,
  but does nothing on System 7.1. Windows keeps Ctrl-Esc for
  the Start menu, so it won't quit the emulator. Choose Shut Down in the guest, or, as a
  last resort, end `BasiliskII.exe` from Task Manager, as abrupt as a power cut.

On macOS, if macman is killed, the emulator keeps running and holds a lock on its disks.
`macman run` refuses them, saying why, until you quit that emulator.

## SheepShaver

macman can also run [SheepShaver](https://github.com/kanjitalk755/macemu), for classic
PowerPC Mac OS, generally Mac OS 7.5.2 through 9.0.4, depending on the ROM and the guest.
It imitates no particular Power Mac, and macman makes no claim for Mac OS 9.1 or later,
Mac OS X, or G4 and G5 machines.

**This backend is a preview.** On Linux x86-64 it has booted Mac OS 8.1 on an Old World ROM,
and installed and booted Mac OS 9.0.4 on a New World ROM, and `slirp` networking, a shared
folder, read-only disks, and sound on a real device work. On Windows x86-64 it has booted
Mac OS 9.0.4, with `slirp`, full screen, and sound working, and on macOS ARM64 it has
installed and booted Mac OS 9.0.4, with `slirp`, full screen, sound, a shared folder, and
read-only disks working. Treat it as a preview until
[the smoke test](docs/SMOKE_TEST.md#sheepshaver) has passed; see
[docs/PLATFORMS.md](docs/PLATFORMS.md#sheepshaver).

It's offered on Linux x86-64, Windows x86-64, and macOS ARM64. On Linux it compiles with
the same packages as Basilisk II:

```sh
apt install build-essential autoconf automake pkg-config libsdl2-dev
```

On Windows it needs what Basilisk II does there, and builds without a JIT, so it's slower.
A shared folder and installing from a CD aren't available, disks must be under 4G, and if
macman is killed the emulator keeps running, with Windows keeping others off its disks. For
sound, choose "Built-in" as the output in the guest's Sound control panel; see
[docs/PLATFORMS.md](docs/PLATFORMS.md#sheepshaver).

On macOS it needs what Basilisk II does there, less MPFR:

```sh
port install autoconf automake pkgconfig libsdl2
brew install autoconf automake pkg-config sdl2
```

It builds without a JIT there too, so it's slower. If macman is killed, the emulator keeps running and holds a lock on its disks,
as Basilisk II does, and macman refuses those disks until it's quit.

On Linux it builds to upstream's defaults, where Mac addresses are host addresses, so it needs to
map memory at address 0, which most distributions forbid. `macman doctor` says when that
applies. Allowing it weakens a protection against kernel bugs, for every program, until
the next reboot:

```sh
sudo sysctl vm.mmap_min_addr=0
```

It needs a PowerPC ROM of your own: a 4M dump from a PCI Power Mac, or a `Mac OS ROM` file
from a System Folder or an installer, which must be under 4M. No 68k ROM will start it.
The ROM has to suit the guest: a New World ROM crashed the emulator with Mac OS 8.1, while
an Old World one booted it.

```sh
macman rom add "Mac OS ROM" --name newworld
macman system add Mac-OS-9.0.4.img --name mac-os-9.0.4 --version 9.0.4
macman create beige --backend sheepshaver --rom newworld --system mac-os-9.0.4 \
  --memory 256M --display 1024x768x32
```

| Option | Values | Default |
|---|---|---|
| `--no-sound` | | sound on |
| `--network` | `none`, `slirp` | `none` |
| `--shared-folder DIR` | a host folder the guest can read and write | none |
| `--memory` | `16M` to `1024M`, in whole megabytes | `64M` |
| `--display` | 640x480 to 7680x4320, at depth 32 | `640x480x32` |
| `--scale` | 1 to 4 | 2 |

There's no `--cpu` or `--model`. Depth has to be 32, because SheepShaver starts at the
desktop's depth and has no setting for it; pick less in the guest's Monitors control
panel. A window bigger than the desktop is cut down to it.

`macman run --disk` attaches CD images and partitioned disks, as for Basilisk II. An HFS+
volume needs its HFS wrapper, which disks formatted by Mac OS have. `--readonly`, locks,
`doctor`, `list --builds`, and `export` work as they do there, Disk Copy images need
`--readonly`, the shared folder stays out of exports, and NVRAM lasts one run. On Linux,
if macman is killed, it takes the emulator with it.

## QEMU

macman can run machines on [QEMU](https://www.qemu.org/)'s PowerPC emulator,
`qemu-system-ppc`, which it doesn't build: it uses the one installed on the host. It
emulates two Power Macs, chosen with `--model`:

| `--model` | Machine | Meant for |
|---|---|---|
| `mac99` (default) | A New World G4, with the PMU and USB keyboard and mouse | Mac OS 9.1 to 9.2.2, and PowerPC Mac OS X |
| `g3beige` | An Old World beige G3, with ADB | PowerPC Mac OS X only |

The wizard recommends `mac99` for Mac OS 9.1 to 9.2.2, and offers `g3beige` for
nothing classic.

**This backend is new.** On Linux x86-64, with QEMU 10.2.1, a `mac99` machine has
started the Mac OS 9.2.1 installer CD to the Finder. The Mac OS 9.0.4 CD didn't start
on `mac99`, and a `g3beige` can't start classic Mac OS at all, as QEMU's Open Firmware
has no Old World ROM. Mac OS X hasn't been tried, nor has any host but Linux. See
[the smoke test](docs/SMOKE_TEST.md#qemu).

On Debian or Ubuntu:

```sh
apt install qemu-system-ppc
```

That brings OpenBIOS, QEMU's Open Firmware, as `openbios-ppc`. macman uses the
`qemu-system-ppc` on your `PATH`, or the one `config.toml` names:

```toml
[qemu-ppc]
executable = "/opt/qemu/bin/qemu-system-ppc"
```

`macman doctor` says which QEMU it found, its version, and whether it starts each
machine. A machine records the QEMU version it was made with, but runs on whichever is
installed, so it can move to another host with `export` and `import`.

There's no Mac ROM: QEMU starts from its own firmware, so a machine names none, and
`--rom` is refused.

```sh
macman media add Mac-OS-9.2.1.toast --name macos-9.2.1
macman create g4 --backend qemu-ppc --disk-size 2G
macman run g4 --media macos-9.2.1
```

| Option | Values | Default |
|---|---|---|
| `--model` | `mac99`, `g3beige` | `mac99` |
| `--no-sound` | | sound on |
| `--network` | `none`, `slirp` | `none` |
| `--memory` | `32M` to `2048M`, or `2047M` on a `g3beige`, in whole megabytes | `512M` |
| `--display` | 640x480 to 3840x2160, at depth 8, 15, or 32 | `1024x768x32` |
| `--scale` | 1 | 1 |

`slirp` is QEMU's user-mode networking, with a Sun GEM card, which Mac OS drives as
Apple's own. The window isn't magnified, so the scale is 1, and there's no shared
folder.

Disks and CDs are IDE drives, and QEMU has four, so a run takes at most four disks and CDs
together. Images are attached whole, as raw disks, so partitioned disks and CD images
mount as they are. A Disk Copy 4.2 image is refused, since QEMU would see its header as
part of the disk; take the volume out with `macman disk extract`. A system image makes a
whole copy for the machine's boot disk.

With a blank disk and no CD, QEMU stops at OpenBIOS's `0 >` prompt; close the window
and attach an installer. Once you click in QEMU's window, it keeps the mouse; Ctrl-Alt-G
lets it go.

On Linux, macman asks for QEMU's SDL window. QEMU's usual GTK window holds the mouse by
moving the pointer back, which Wayland doesn't allow, so the pointer slips out of it.
`macman doctor` warns when QEMU has no SDL window; on Debian and Ubuntu it comes with
`qemu-system-gui`. The Mac gets a relative USB mouse, as Mac OS 9 doesn't drive QEMU's
absolute tablet, so the pointer moves by the host's mouse movement, not by where it is.
Full screen magnifies the Mac's screen, so the pointer seems slower there than in a
window; set its speed in the Mouse control panel for the way you use it, or choose a
larger `--display` for full screen.

## Installing from a CD

On Linux and macOS, a SheepShaver or Basilisk II machine can start with a blank disk
instead of a system image, and install from a CD image of your own. So can a
[QEMU](#qemu) machine:

```sh
macman create ss904 --backend sheepshaver --rom newworld --disk-size 1G --memory 256M
macman run ss904 --cdrom Mac-OS-9.0.4.iso

macman create q81 --backend basilisk2 --rom quadra --disk-size 1G --memory 128M
macman run q81 --cdrom MacOS8_1.iso
```

Both emulators on Windows refuse both options for now, as their builds there find CDs in
another way, which hasn't been tried. `macman run --disk` still attaches a CD image there,
as a disk.

`--disk-size` takes K, M, or G, from 1M to 2048G, and makes a sparse file of zeros. It
isn't formatted: the guest offers to initialize it. It's the machine's own disk from then
on, and clone, export, and locks treat it like any other.

`--cdrom` attaches an image as a CD for that run only. It's never written to, and never
copied. It takes a `.iso` or `.toast` image with an HFS volume, partitioned or not, and
the Mac starts from it. Leave it off to start from the machine's disk. A plain ISO 9660
image, with no HFS volume, attaches as a data disc, which the Mac reads with ISO 9660 File
Access but can't start from, so with only such discs it starts from its own disk. It can
be repeated: discs with HFS go first, and the first is the one the Mac starts from. An
installer CD's own system may not read a plain ISO 9660 disc, and offers to initialize it,
which it can't. While a CD image is attached, the guest also sees the host's
`/dev/cdrom` on Linux, if there is one, and on macOS whatever CD media the host has.

On macOS the emulator locks each CD image while it runs, so one image serves one machine at
a time, and macman refuses a second run that names it.

If the installer stops at "Updating Apple hard disk drivers", start it again with that
option turned off under Options. The emulator's disk isn't an Apple drive.

## Installation media

Instead of giving a CD's path on every run, register it once as media, along with
installer floppies and other images you boot or read from for a while:

```sh
macman media add Mac_OS_9.0.4.toast --name macos-9.0.4 --version "Mac OS 9.0.4"
macman media add "System 6 Install 1.image" --name sys6-install-1
macman media add "System 6 Install 2.image" --name sys6-install-2
macman media list
macman media info macos-9.0.4

macman create ss904 --backend sheepshaver --rom newworld --disk-size 1G --media macos-9.0.4
macman run ss904 --media macos-9.0.4

macman run plus-6 --media sys6-install-1 --media sys6-install-2
```

Media are registry entries like ROMs and system images, and different from both. A
system image is a bootable volume that `create` clones into a machine's own writable
disk. A medium is kept exactly as it was given, the whole image with its partition map,
because an installer may check its own disc, and nothing ever writes to it: macman
stores a read-only copy, and the file you registered from is left as it is. Removing a
medium with `macman media remove NAME` changes no machine.

`media add` works out from the contents, never the file name, how each image is
attached:

| Kind | Found for | `run --media` attaches it |
| --- | --- | --- |
| `cdrom` | A partition map with an HFS volume, ISO 9660, or HFS larger than a floppy | As a CD, read-only, as `--cdrom` does |
| `floppy` | A raw MFS or HFS volume up to 1440K, or a Disk Copy 4.2 image | As a disk, from a read-only throwaway copy |
| `disk` | Given with `--kind disk`, or an unrecognized image | As a floppy is |

`--kind` overrides what's found, for instance `--kind disk` for a partitioned hard disk
image, but a `cdrom` must still hold HFS or ISO 9660. An image macman doesn't recognize
is refused unless you add `--any-format`. `media list` shows each one's kind, size,
format, whether it has boot blocks, and the version you gave with `--version`, which is
for display only and never guessed. `media info` also shows the partitions, and reads
the whole image to check it against the SHA-256 it was registered with.

`run --media NAME` repeats, and its media come after any `--disk` and `--cdrom` files,
in the order given. That's how a multi-floppy installer gets all its disks: Mini vMac
mounts each in its own drive, up to as many as the model has. A CD needs a backend that takes `--cdrom`, so Mini vMac
refuses one. `create --media NAME` checks the media against the new machine before it's
made, then prints the `run` command to use. The media aren't saved with the machine, so
a plain `run` afterwards starts from its own disk, and installing the system is up to
you in the guest.

Media names and paths never mix: `--media` takes only registered names, and `--cdrom` and
`--disk` only paths. Shell completion offers registered media names after `--media`,
including `wizard --media`, and `media info` and `media remove`.

`macman wizard` lists registered media beside a new image file, and can register a new
installer image for you once it has made the machine; see below.

Installation CDs and floppies are copyrighted like ROMs and System Software; register
your own, and don't redistribute them.

## Making a machine with the wizard

`macman wizard` makes a machine step by step in the terminal, from an image you already
have:

```sh
macman wizard                                 # pick an image, or set up by hand
macman wizard "Mac OS 9.0.4.toast"            # start from an installer CD image
macman wizard --image ~/images/MacHD.img      # the same, as an option
macman wizard --media macos-9.0.4             # start from registered media
```

It goes through these steps, and n, b, and q move forward, back, and out at any of
them. Going back to change the image, the emulator, or the boot disk works out the rest
again, keeping the name and settings you chose where they still apply.

1. **Start from**: an image file, typed in (spaces and quotes are fine), or one of your
   registered media. Then what it is: a disk with a system installed, an installer CD, an
   installer or startup floppy, or nothing, to set a machine up by hand. The wizard
   suggests one from what's on the image, and you can pick another.
2. **What the image holds**: what macman read, kept apart from what it infers. *Observed*
   is the layout, partition map, volumes, their names, boot blocks, and blessed System
   Folder. *Inferred* is the system it probably holds, from a volume name such as "Mac OS
   9.0.4" or the version given to `media add`, and how sure that is. File names are
   never used, and when nothing says which system it is, none is assumed.
3. **Emulator**: every backend, and Mini vMac model, ranked for that system and this
   host. When a system runs on more than one, as Mac OS 8.1 does on SheepShaver and
   Basilisk II, both are recommended, and you pick. Ones that can't be had here say why:
   no CD installs on this host, no registered ROM that fits (register one with `macman
   rom add`, then press r), or a host the backend doesn't run on. One the guess says won't
   work stays available, marked unlikely, since the guess may be wrong.
4. **Boot disk and name**: a copy of the image's volume, as `system add` takes it, or a
   blank disk of a size you choose to install onto from the CD. A whole CD is attached
   read-only as a CD, partition map and all, and never turned into a hard disk. A new
   installer image can also be registered as media here, to attach by name later.
5. **Settings**: the same editor as `config edit`, with the same choices and checks, and
   a ROM can be chosen among the registered ones that fit.
6. **Review the plan**: everything that will be made and where, what's only read, the
   emulator build the first start needs, and anything to know before starting. Enter
   makes it.
7. **Made**: l starts the machine now, with the installer CD attached if it installs from
   one, and Enter finishes. Either way, what to do next is printed when the wizard
   closes, so you can read it again.

Nothing is written until you confirm the plan: leaving at any step, with q or Ctrl-C,
leaves no machine, and nothing registered. The machine is made the way `macman create`
makes it, all at once or not at all, and the images you start from are only read. When
you ask to register an image, it's registered after the machine is made.

The wizard prepares the machine; installing the system is yours to do, in the guest.
Along the way it gives advice for what you chose. For example, a Mac OS installer on
SheepShaver or Basilisk II may hang updating hard disk drivers, which the installer's
Options can turn off. Mini vMac can't yet install from a series of floppies (#28), so a
floppy is only offered as a boot disk to copy, when it can start a Mac.

It needs an interactive terminal, and works over SSH. Without one, as when output is
redirected, it stops with an error and draws nothing: use `macman create` instead, such
as `macman create NAME --backend sheepshaver --rom ROM --disk-size 1G`, then `macman run
NAME --cdrom FILE`. `--json` isn't accepted.

## Checkpoints

A checkpoint is a saved copy of a machine's boot disk, to come back to after an
experiment, a failed install, or a guest that won't start any more:

```sh
macman checkpoint create wombat-6 --name clean-install
macman checkpoint list wombat-6
macman rollback wombat-6 --checkpoint clean-install
macman checkpoint delete wombat-6 --name clean-install
```

Without `--name`, a checkpoint is named for when it was made, in UTC, as
`2026-11-03T21-04-05Z`. These are copies of a disk, not snapshots of a running Mac:
macman makes and restores them only with the machine fully shut down, and refuses while
any emulator has one of its disks open. Shut the guest down, quit the emulator, then
checkpoint. Only the boot disk is kept. The ROM, `machine.toml`, other disks, and the
system image the machine came from are left as they are, and so are left alone by a
rollback. A blank boot disk, one `create --disk-size` made and nothing has written to
yet, has nothing to keep, and neither has a read-only one.

A checkpoint never changes once it's made. It's copied to the side and checked against
its SHA-256 before it's listed, so an interrupted `checkpoint create` leaves nothing that
looks like a checkpoint. `rollback` asks first, or takes `--yes`. It copies the
checkpoint beside the boot disk, checks that copy against the digest, and only then
swaps it in, so the disk is either all as it was or all the checkpoint; a checkpoint
that no longer matches its digest is refused, and nothing changes. The checkpoint stays,
to roll back to again. What's on the boot disk now is lost, unless `--save NAME` keeps
it as a checkpoint first, which makes the rollback one to undo. If the boot disk is
missing, such as deleted by mistake, `rollback` restores it from the checkpoint and says
so; there's nothing then for `--save` to keep, so given `--save`, it refuses and changes
nothing.

Each checkpoint is a full copy of the disk, so it takes as much space on the host as the
disk did, less where the file system shares or skips storage: macman copies with a
reflink where the file system can, as Btrfs, XFS, and APFS can. They're kept in the
machine's `checkpoints/` directory. `clone` and `export` take a machine's current disks
and none of its checkpoints, and `import` leaves a machine's checkpoints alone, as it
never replaces a machine. `remove` deletes them with the machine, and says how many
there are when it asks.

## Disk usage

`macman list` shows each machine's boot disk capacity, as `disk`, and what all its disks
take on the host, as `used`. `rom list`, `system list`, and `media list` show `used` for
each stored file beside its size, and `macman du` lists everything, largest last, as in
this example:

```text
$ macman du
NAME          TYPE     DISK USED
plus-v3       rom        128 KiB
sys6-install  media      800 KiB
system-6.0.8  system     800 KiB
wombat-6      machine     41 MiB
ss904         machine    412 MiB
total         -          455 MiB
```

The two numbers measure different things. A disk's capacity is what the guest sees: the
file's length, or a Disk Copy image's data, whether or not the host has stored it all.
Its usage is what the file system has allocated, so a 1 GiB blank disk from `create
--disk-size` takes almost nothing on the host until the guest writes to it. A machine's
usage counts every disk in its `machine.toml`, the boot disk and any others, and its
[checkpoints](#checkpoints), and nothing else: not its ROM or the system image it was created from, which `du` lists on their
own, and not a run's temporary copies, such as an ephemeral boot disk, which go when the
run ends. Checkpoints don't change `disk`, which is the boot disk alone, and `checkpoint
list` shows what each one uses. A checkpoint still being made isn't counted until it's
done.

`du` sorts by usage, then by type and name, so its order is the same every time. An entry
whose usage can't be measured, such as a machine with a missing disk or checkpoint, shows `-`, never
`0 B`, and comes after the rest; listings carry on past it. The last line, of type `-`, is
the total: what all the entries' files take together, measured from the files rather than
added up from the lines above, so a file that two entries share by a hard link is counted
once and the total can be less than their sum. It shows `-` if any entry does, as a
partial total would understate the whole.

Usage is allocation as the platform reports it, `st_blocks` on Linux and macOS and the
allocation size on Windows, which may round up to whole blocks or clusters. It can't see
storage shared between files: a reflinked or deduplicated copy, as Btrfs, XFS, and APFS
can make, is counted in full for each file that shares it, so totals can overstate what
removing something would free. Transparent compression may be
reported either way, depending on the file system. Hard links to one file are counted
once within a machine, checkpoints included, and once within `du`'s total, wherever the
links are. macman itself makes copies rather than links, so they only arise when made
by hand. The total is still of allocated files, not of unique physical storage: blocks
that different files share, by reflinks or deduplication, are counted for each of them,
in the total as in every entry, because what the platform reports per file can't say
which blocks are shared. `du --physical` measures that where it can, as below.

`--json` gives the raw byte counts: `system_disk_capacity_bytes` and
`host_disk_used_bytes` in `list`, `host_disk_used_bytes` in the registry lists, and
`du`'s entries in the same order as its text, with the total as
`total_host_disk_used_bytes`. See [docs/JSON.md](docs/JSON.md).

### Unique physical usage

`macman du --physical` adds a last line, `physical`, for the unique physical storage
everything takes: each block on the host counted once, however many files use it, whether
they share it by a hard link, a reflink (a copy made with `cp --reflink`, or by a file
system that clones rather than copies), or the file system's deduplication. It covers
the same files as `total`, so persistent disks and checkpoints, and not a run's temporary
copies.

```text
$ macman du --physical
NAME          TYPE     DISK USED
...
total         -          455 MiB
physical      -          214 MiB
```

That makes three different measures, and `du` shows only the first two:

- **Allocated bytes**, each entry's `DISK USED` and `total`: what the file system has
  allocated to each file, with a file reached by several hard links counted once.
- **Unique physical bytes**, `physical`: the blocks those files occupy, with shared
  blocks counted once, so it's never more than `total` and can be much less.
- **What deleting would free**, which isn't shown: blocks shared with something macman
  doesn't know about, such as a snapshot or a reflinked copy outside its directories,
  stay in use after macman's files go.

Where the platform can't say reliably which blocks files share, `physical` is `-`, never
an estimate, and a note on stderr says why. It's measured:

| Host | File system | `physical` |
|---|---|---|
| Linux | ext4 | Measured |
| Linux | XFS | Measured, unless a file is on a realtime device |
| Linux | Btrfs | Measured, unless a file is compressed; reflinks between subvolumes count once |
| Linux | ZFS, tmpfs, overlayfs, NFS, SMB, FUSE, and others | Unavailable |
| macOS | APFS, HFS+ | Unavailable, not implemented yet |
| Windows | NTFS, ReFS | Unavailable, not implemented yet |

On Linux it reads where each file's data is with `FIEMAP`, and counts the physical block
ranges, per file system. That's only trusted where the ranges identify blocks, so it's
unavailable, too, for a file that's missing or unreadable, that has data whose place isn't
known, or is stored in its metadata, or that kept changing while it was read. ZFS has
block cloning and deduplication but doesn't say which of a file's blocks are shared.

What's counted is the blocks the files point at. Btrfs keeps a whole extent while any of
it is used, so after a file's blocks are partly rewritten, space no file uses any more
can remain allocated and isn't counted, and redundancy below the file system, such as
Btrfs RAID1 or DUP, or a RAID device, isn't multiplied in.

Reading every extent of every file is slower than `du` alone, and each file's pending
writes are flushed first so its blocks are final. For a consistent figure, run it while no
machine is running: a file that changes as it's read is tried again, then left
unavailable, but changes between one file and the next aren't noticed.

## Moving machines between hosts

`macman export` writes machines to an archive, with each one's current disks and the
ROM it uses. `macman import` brings them in on another host, with no compiling until
the first `run`:

```sh
macman export wombat-6 maciix-7 -o classic-macs.tar.gz
macman import classic-macs.tar.gz
```

Archives of machines leave out registered system images, which a machine doesn't
need once it's created. Name the ones to take with `--system NAME`, which repeats, or
take them all with `--all-systems`, with or without machines:

```sh
macman export wombat-6 --system system-6.0.8 -o wombat.tar.gz
macman export --all-systems -o systems.tar.gz
```

Media are taken the same way, with `--media NAME` or `--all-media`, and an archive of
media alone needs no machines:

```sh
macman export --media macos-9.0.4 --media sys6-install-1 -o media.tar.gz
```

ROMs come with the machines that use them. To take others, or ROMs alone, name them with
`--rom NAME`, which repeats, or take every registered ROM with `--all-roms`; each is
registered under its own name on import, used or not. `--all-machines` takes every machine
with the ROMs they use, and nothing else:

```sh
macman export --rom plus-v3 --rom maciix -o roms.tar.gz
macman export --all-machines --system system-6.0.8 -o machines.tar.gz
macman export --all-roms --all-systems -o registries.tar.gz
```

The selectors add up, so an archive holds everything any of them asks for, and each file
in it once. `macman export --all -o everything.tar.gz` takes every machine, ROM, system
image, and medium, whether or not a machine uses it, and works with no machines at all; it
can't be combined with the others, which it already covers. On the other host,
`system list` shows the imported images, and `create --system` uses them.

Export refuses a machine whose disks are in use, so close the emulator first. Import
never replaces anything: importing the same archive again reports `unchanged`, and a
machine, ROM, system image, or medium that differs from what's already there is reported
as a conflict and left alone, while the rest of the archive still imports. `macman import` exits with
status 1 if anything was left unresolved. To keep both, import a single-machine
archive as an independent copy:

```sh
macman import wombat-6.tar.gz --rename wombat-6-imported
```

To see what an import would do first, add `--dry-run`. It checks the whole archive
and reports each ROM, system image, and machine, but changes nothing:

```sh
macman import classic-macs.tar.gz --dry-run
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
SheepShaver comes from the same archive, and is set under `[sheepshaver]`. macman
downloads nothing for QEMU, which is the host's own.

## License

macman is MIT licensed. Mini vMac, Basilisk II, and SheepShaver, which it fetches and
compiles but does not include, are GPL-2.0. QEMU, which it runs but neither fetches
nor includes, is GPL-2.0 too.
