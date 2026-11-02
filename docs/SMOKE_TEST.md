# Real end-to-end smoke test

The automated tests use a mock backend and fixture sources, so run this by hand
before changing how Mini vMac is built or launched. It needs legally obtained ROMs and
bootable disk images, which is why it isn't in CI. It reads your originals and doesn't
change them.

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
