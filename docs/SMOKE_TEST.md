# Real end-to-end smoke test

The automated tests use a mock backend and fixture sources, so run this by hand
before changing how Mini vMac is built or launched. It needs legally obtained ROMs and
bootable disk images, which is why it isn't in CI. It reads your originals and doesn't
change them.

The steps below are written for Windows. See [macOS ARM64](#macos-arm64) for what
differs there.

On Windows, use PowerShell, and put the data under a path with a space and characters
outside your code page, to prove those paths work:

```powershell
$root = "$env:TEMP\smoke Zoë 日本"
$env:APPDATA = "$root\Roaming"; $env:LOCALAPPDATA = "$root\Local"
Remove-Item Env:HOME, Env:XDG_CONFIG_HOME, Env:XDG_DATA_HOME, Env:XDG_CACHE_HOME -ErrorAction Ignore

macman doctor
macman rom add MacPlus.ROM --name plus-v3
macman rom add MacIIx.ROM --name maciix
macman system add system608.img --name system-6.0.8 --version 6.0.8
macman system add system710.img --name system-7.1 --version 7.1
macman create plus-6 --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman create plus-b --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman create iix-71 --model iix --rom maciix --system system-7.1 --memory 8M --display 640x480x8
```

Then check each of these:

| Step | Expected |
|---|---|
| `macman run plus-6` | Downloads, builds, and boots to the System 6 Finder. |
| While it runs, in a second terminal: `macman run plus-6` | Refused, because the disk is in use. |
| While it runs: `macman doctor` | The machine is still reported as runnable. |
| End `minivmac.exe` in Task Manager | `macman run` warns and fails; `macman doctor` reports the abnormal run. |
| `macman run plus-b --verbose` | Says it's using the cached emulator, warns once about the earlier run, and boots. |
| Hold Ctrl and press Q, then Y | Exit code 0, and the `sessions` directory is empty. |
| `macman list --builds` | One Plus build. |
| `macman run iix-71 --disk "extra disk.dsk" --readonly` | Boots System 7.1 in color with the extra disk mounted. |
| End `macman.exe` while the emulator runs, then `macman run iix-71` | Warns about the earlier run, then refuses the disk as in use. |
| Close the emulator, and compare the extra disk's hash | The hash is unchanged. |
| The next `macman run` | Clears the notice left by the abandoned session. |

## macOS ARM64

Run this on a native ARM64 Mac. Put the data under a path with a space and a non-ASCII character, and use the Linux-style
environment, to prove both:

```sh
root="$TMPDIR/smoke Zoë"
export XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" XDG_CACHE_HOME="$root/cache"

macman doctor
macman rom add MacPlus.ROM --name plus-v3
macman rom add MacIIx.ROM --name maciix
macman system add system608.img --name system-6.0.8 --version 6.0.8
macman system add system710.img --name system-7.1 --version 7.1
macman create plus-6 --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman create plus-b --model plus --rom plus-v3 --system system-6.0.8 --memory 4M
macman create iix-71 --model iix --rom maciix --system system-7.1 --memory 8M --display 640x480x8
```

| Step | Expected |
|---|---|
| `macman doctor` before installing anything | Names `xcode-select --install`, or passes every tool check. |
| `macman run plus-6` | Downloads 37.03, builds, opens a window, and boots to the System 6 Finder at the ROM and disk macman chose. |
| While it runs: `ps -o ppid= -p "$(pgrep -n minivmac)"` | The parent is `macman`, not `launchd`. |
| While it runs: `file "$XDG_DATA_HOME"/macman/sessions/*/minivmac.app/Contents/MacOS/minivmac` | `Mach-O 64-bit executable arm64`. |
| While it runs, in a second terminal: `macman run plus-6` | Refused, because the disk is in use. |
| While it runs: `macman doctor` | The machine is still reported as runnable. |
| `pkill -SEGV minivmac` | `macman run` warns of a host crash and fails; `macman doctor` reports the abnormal run. |
| `macman run plus-b --verbose` | Says it's using the cached emulator, warns once about the earlier run, and boots. |
| Hold Ctrl and press Q, then Y | Exit code 0, and the `sessions` directory is empty. |
| `macman list --builds` | One Plus build. |
| Change a byte of `Contents/Info.plist` in the cached bundle, then `macman doctor` | Reports the build as damaged. The next run rebuilds it. |
| `macman run iix-71 --disk "extra disk.dsk" --readonly` | Boots System 7.1 in color with the extra disk mounted, and locked. |
| In the guest, save a file to the system disk, then shut down | Exit code 0, and the file is there on the next run: writes reach the real disk through its link. |
| Close the emulator, and compare the extra disk's hash | The hash is unchanged. |
| `macman run plus-6 --disk blank.dsk`, with `blank.dsk` an 800K file of zeros | The guest offers to initialize the disk. There's no "Unsupported Disk Image" alert. |
| `macman import` an archive exported on Linux or Windows, then `macman run NAME --verbose` | Says the machine records 36.04 and runs on 37.03, then boots. |
| End `macman` with `kill -9` while the emulator runs, then `macman run iix-71` | Warns about the earlier run. Its lock died with `macman`, so the new emulator is the one to refuse the disk, with an "image in use" alert. |
| Close the emulators. The next `macman run` | Clears the notice left by the abandoned session. |

## Basilisk II

Run this on Linux x86-64, Windows x86-64, or macOS ARM64. It needs a real ROM and system image, and it
uses separate directories, so it doesn't touch your machines. On Windows, set `$root`
and the rest as at the top of this file, instead of the `export` line.

```sh
root="$(mktemp -d)/smoke Zoë"
export XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" XDG_CACHE_HOME="$root/cache"

macman doctor
macman rom add Quadra-650.ROM --name quadra650
macman system add system755.img --name system-7.5.5 --version 7.5.5
macman create q7 --backend basilisk2 --rom quadra650 --system system-7.5.5 --memory 64M
macman create q7b --backend basilisk2 --rom quadra650 --system system-7.5.5 --memory 128M
```

| Step | Expected |
|---|---|
| `macman doctor` before installing anything | Names the missing tools, as warnings until a machine needs them. |
| `macman run q7` | Downloads, builds, opens a window, and boots System 7.5.5 to the Finder. |
| The same run, with speakers or headphones on | The startup chime plays on the host's real sound device. |
| While it runs, in a second terminal: `macman run q7` | Refused, because the disk is in use. |
| While it runs: `macman doctor` | The machine is still reported as runnable. |
| In the guest, save a file, then choose Shut Down | Exit code 0, the `sessions` directory is empty, and the file is there on the next run. |
| Close the window instead | The guest asks whether to shut down. |
| The desktop | Shows the boot disk and Trash, and no `Unix` volume. |
| `pkill -KILL BasiliskII` | `macman run` warns that it was killed, and fails; `macman doctor` reports the abnormal run. The emulator handles `SIGSEGV` itself and carries on, so that won't do. |
| `macman run q7b --verbose` | Says it's using the cached emulator, warns once about the earlier run, and boots with 128M. |
| `macman list --builds` | One Basilisk II build. |
| Change a byte of the cached `BasiliskII`, then `macman doctor` | Reports the build as damaged. The next run rebuilds it. |
| `macman run q7 --disk "extra disk.dsk" --readonly` | Mounts the extra disk locked, and its hash stays the same. |
| Kill `macman` with `kill -9` while the emulator runs | The emulator stops at once, with no alert in the guest. The next `macman run` warns about the earlier run. |
| `pkill -KILL BasiliskII` while a machine runs, then `macman doctor` | Warns that it wasn't shut down cleanly. |
| `macman run` that machine, from a terminal, and press Enter | Leaves the flag unset, and starts the machine anyway. |
| `macman run` that machine again, and answer `y` | Sets the flag, then boots, or shows the "not shut down properly" alert. |
| `macman create qs --backend basilisk2 ... --shared-folder "a folder"`, then run it | A `Unix` volume holds that folder's files and no others. |
| `macman create q2 --backend basilisk2 --rom <a IIx ROM> ...` | Refused, naming the ROMs it needs. |
| Run a ROM macman accepts but the emulator refuses | `macman run` fails, though the emulator exits 0. |
| `macman run q8 --disk MacOS8_1.iso --readonly`, with `q8` a 7.5.5 machine | The CD mounts in the guest. Its installer upgrades the machine, which then boots Mac OS 8.1 and shuts down with exit code 0. |
| `macman import` an archive of `q7` into empty directories, then `macman run q7` | No compiling until the run, and then it boots. |

On Windows, these replace the matching steps:

| Step | Expected |
|---|---|
| `Stop-Process -Name BasiliskII -Force` | `macman run` reports the exit as abnormal; `macman doctor` does too. |
| `macman run q7 --disk "extra 日本.dsk" --readonly` | Mounts the extra disk locked, and its hash stays the same. |
| `Stop-Process -Name macman -Force` while the emulator runs | The emulator carries on, and opening its disk from elsewhere fails, as does `macman run q7`. `macman doctor` warns that macman stopped first. |
| `macman create qs --backend basilisk2 ... --shared-folder "a folder"` | Refused. |
| Close the window instead | On System 7.5.5 and Mac OS 8.1, the guest asks whether to shut down. System 7.1 ignores it. |

On macOS, these replace the matching steps:

| Step | Expected |
|---|---|
| `macman doctor` before installing anything | Names the missing tools and libraries, including MPFR, as warnings until a machine needs them. |
| Kill `macman` with `kill -9` while the emulator runs | The emulator carries on. `macman run q7` warns about the earlier run, then refuses, saying the disk is held by a Basilisk II that outlived macman. |
| Quit that emulator, then `macman run q7` | Boots. |
| On Linux: `macman create qn --backend basilisk2 --rom quadra --disk-size 1G --memory 128M`, then `macman run qn --cdrom MacOS8_1.iso` | Starts from the CD, and offers to initialize the disk. Install, with "Update Apple Hard Disk Drivers" off under Options, then Shut Down. |
| On Linux: `macman run qn` | Starts Mac OS 8.1 from its own disk. The ISO's hash is unchanged. |
| On Linux: make two data discs with `genisoimage -o a.iso DIR` and `genisoimage -J -o b.iso DIR`, then `macman run qn --cdrom a.iso --cdrom b.iso` | Starts from its own disk, and both discs mount with their files, not as audio CDs. |
| On Windows or macOS: `macman create ... --disk-size 1G`, or `macman run q7 --cdrom MacOS8_1.iso` | Refused, saying it isn't available on this host. |

## SheepShaver

Run this on Linux x86-64 or Windows x86-64, the candidate hosts. It has been run in part,
with Mac OS 8.1 and 9.0.4 on Linux and 9.0.4 on Windows, and
[PLATFORMS.md](PLATFORMS.md#sheepshaver) lists what passed and what's left. On Windows, set
up the directories as at the top of this file, skip the `vm.mmap_min_addr` steps, use the
Windows steps under [Basilisk II](#basilisk-ii) for killing processes, and expect a shared
folder, `--disk-size`, and `--cdrom` to be refused. Update it with what you find, and
expect to adjust the backend. It needs a PowerPC ROM and a system image of your own, and it
uses separate directories, so it doesn't touch your machines.

```sh
root="$(mktemp -d)/smoke Zoë"
export XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" XDG_CACHE_HOME="$root/cache"

macman doctor
macman rom add "Mac OS ROM" --name ppc
macman system add macos9.img --name mac-os-9 --version 9.0.4
macman create p9 --backend sheepshaver --rom ppc --system mac-os-9 --memory 128M
macman create p9b --backend sheepshaver --rom ppc --system mac-os-9 --memory 256M
```

| Step | Expected |
|---|---|
| `macman doctor` | Names any missing tools, and reports `vm.mmap_min_addr`. |
| `macman run p9` with `vm.mmap_min_addr` above 0 | Downloads and builds, then fails, naming the `sysctl` that fixes it. If it boots instead, the requirement is wrong: remove it. |
| `sudo sysctl vm.mmap_min_addr=0`, then `macman run p9` | Boots to the Finder. Note what the ROM and guest were. A New World ROM has crashed with Mac OS 8.1, so pair it with 8.5 or later. |
| The same run, with speakers or headphones on | The startup chime plays on the host's real sound device. |
| While it runs, in a second terminal: `macman run p9` | Refused, because the disk is in use. |
| While it runs: `macman doctor` | The machine is still reported as runnable. |
| In the guest, save a file, then choose Shut Down | Exit code 0, the `sessions` directory is empty, and the file is there on the next run. |
| The desktop | Shows the boot disk and Trash, and no `Unix` volume. |
| Monitors control panel | Offers depths up to the desktop's. |
| `pkill -KILL SheepShaver` | `macman run` warns that it was killed, and fails; `macman doctor` reports the abnormal run. |
| `macman run p9b --verbose` | Says it's using the cached emulator, warns once about the earlier run, and boots with 256M. |
| `macman list --builds` | One SheepShaver build. |
| Change a byte of the cached `SheepShaver`, then `macman doctor` | Reports the build as damaged. The next run rebuilds it. |
| `macman run p9 --disk "extra disk.dsk" --readonly` | Mounts the extra disk locked, and its hash stays the same. |
| `macman run p9 --disk MacOS9.iso --readonly` | The CD mounts in the guest. |
| Kill `macman` with `kill -9` while the emulator runs | The emulator stops at once. The next `macman run` warns about the earlier run. |
| `macman create ps --backend sheepshaver ... --shared-folder "a folder" --network slirp`, then run it | A `Unix` volume holds that folder's files and no others, and the guest gets an address by DHCP. |
| `macman create px --backend sheepshaver --rom <a Quadra ROM> ...` | Refused, naming the ROMs it needs. |
| Run a ROM macman accepts but the emulator refuses | `macman run` fails, though the emulator exits 0. |
| A later guest, if you have one: Mac OS 8.x and 9.0.4 | Boots to the Finder. List every combination tried in PLATFORMS.md, and say which weren't. |
| `macman import` an archive of `p9` into empty directories, then `macman run p9` | No compiling until the run, and then it boots. |
| `macman create pn --backend sheepshaver --rom ppc --disk-size 1G --memory 256M`, then `macman run pn` | A sparse 1G disk. The Mac stops at a question mark, and macman says to use `--cdrom`. |
| `macman run pn --cdrom MacOS9.iso` | Starts from the CD, which shows as a CD, and offers to initialize the disk. Install onto it, turning off "Update Apple Hard Disk Drivers" under Options, then Shut Down. |
| `macman run pn` | Starts from its own disk. The ISO's hash is unchanged. |
| `macman run pn --cdrom MacOS9.iso` again | Starts from the CD, even though the disk has a system. |
| `macman run pn --cdrom a.iso --cdrom b.iso`, with the data discs above | Starts from its own disk, and both discs mount with their files, not as audio CDs. |
| `macman run pn --cdrom a.iso --cdrom MacOS9.iso` | Starts from the 9.0.4 CD, though it was named second. |
| `sudo sysctl vm.mmap_min_addr=65536`, or your distribution's value | Puts the host back as it was. |
