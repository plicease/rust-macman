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
| End `macman` with `kill -9` while the emulator runs, then `macman run iix-71` | Warns about the earlier run. Its lock died with `macman`, so the new emulator is the one to refuse the disk, with an "image in use" alert. |
| Close the emulators. The next `macman run` | Clears the notice left by the abandoned session. |
