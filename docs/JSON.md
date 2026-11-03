# JSON output

`--json` makes a command that reports data print one JSON document on stdout, in place of
its tables or text, for scripts and other tools. Like `--verbose`, it goes anywhere on the
command line:

```sh
macman --json list
macman list --json
macman rom --json list
```

Without `--json` nothing changes. The output is the same whether stdout is a terminal, a
pipe, or a file. It's never colored, and has nothing else mixed in: progress, warnings, and
diagnostics go to stderr, as they always do.

## Stability

The field names and types below are the schema. New fields may be added, so ignore any you
don't know, but none is renamed or removed, and none changes type. Strings marked as one of
a set of values won't gain new values without a note here.

Sizes, offsets, and lengths are numbers of bytes. A field shown as `string or null` is
always present, and is `null` when there's nothing to say.

## Errors

When a command fails under `--json`, stdout holds one object, and the exit code is the same
as without `--json`: 1, or 2 for arguments that can't be parsed.

```json
{"error": "no current machine; pick one with `macman use NAME`"}
```

The message is also printed to stderr, as `error: ...`, as it is without `--json`.
`--help` and `--version` print their usual text.

## Commands that refuse `--json`

These change things and report as they go, so they have no single result to give:
`create`, `run`, `use`, `clone`, `remove`, `export`, `import` (except with `--dry-run`),
`rom add`, `system add`, `system remove`, and `disk extract`. Given `--json`, they do
nothing, and fail with an error as above.

## `macman list`

```json
{
  "machines": [
    {
      "name": "wombat-6",
      "current": true,
      "backend": "minivmac",
      "model": "plus",
      "memory": "4M",
      "display": {"width": 512, "height": 342, "depth": 1, "scale": 2, "fullscreen": false},
      "system_version": "6.0.8",
      "ephemeral": false
    }
  ]
}
```

| Field | Type | |
|---|---|---|
| `name` | string | |
| `current` | boolean | Whether it's the machine `macman use` chose |
| `backend` | string | `minivmac`, `basilisk2`, or `sheepshaver` |
| `model` | string or null | The Mini vMac model, such as `plus` or `iix`; `null` for other backends |
| `memory` | string | As configured, such as `4M` |
| `display` | object | `width`, `height`, `depth`, and `scale` are numbers; `fullscreen` is a boolean |
| `system_version` | string or null | The system version, for display only |
| `ephemeral` | boolean | Whether a plain `run` boots a temporary copy of the boot disk; the machine's setting, not that of a run under way |

A machine whose definition can't be read has only `name`, `current`, and `error`, a string.

## `macman list --builds`

```json
{"builds": [{"key": "54e8f91c82a1...", "backend": "sheepshaver", "revision": "f532be3d3d70", "options": ["--enable-sdl-video"]}]}
```

| Field | Type | |
|---|---|---|
| `key` | string | The whole build key. The text output shows its first 12 characters |
| `backend` | string | |
| `revision` | string | The emulator's revision |
| `options` | array of strings | What it was built with |

A build without a valid manifest has only `key` and `error`, a string.

## `macman doctor`

```json
{
  "ok": false,
  "checks": [
    {"status": "ok", "message": "make found", "fix": null},
    {"status": "error", "message": "machine m cannot run", "fix": "disk system.dsk is missing"}
  ]
}
```

| Field | Type | |
|---|---|---|
| `ok` | boolean | Whether no check is an `error`. The exit code is 1 when it's `false`, 0 otherwise |
| `checks[].status` | string | `ok`, `warning`, or `error` |
| `checks[].message` | string | |
| `checks[].fix` | string or null | What to do about it |

Checks come in the order the text output has them.

## `macman rom list`

```json
{"roms": [{"name": "plus-v3", "size": 131072, "sha256": "dd908e...", "models": ["512ke", "plus"], "backend": "minivmac", "emulators": ["minivmac"]}]}
```

| Field | Type | |
|---|---|---|
| `name` | string | |
| `size` | number | |
| `sha256` | string | All 64 hex digits |
| `models` | array of strings | Mini vMac models it's a genuine ROM for; empty for any other ROM |
| `backend` | string or null | The first of `emulators`, or `null` if it's empty |
| `emulators` | array of strings | Each backend whose ROM check it passes: `minivmac`, `basilisk2`, or `sheepshaver`, in that order; empty if unrecognized |

A backend listed in `emulators` can start from the ROM; that doesn't mean every guest
will boot on it. Mac OS 8.1, for one, crashes SheepShaver with a New World ROM.

## `macman system list`

```json
{"systems": [{"name": "system-6.0.8", "version": "6.0.8", "size": 819200, "sha256": "...", "format": "hfs"}]}
```

| Field | Type | |
|---|---|---|
| `name` | string | |
| `version` | string or null | |
| `size` | number | |
| `sha256` | string | Of the stored image |
| `format` | string or null | `mfs` or `hfs` for a raw volume, `disk_copy42` for a Disk Copy 4.2 image; `null` if unknown |

## `macman disk info FILE`

```json
{"file": "System.img", "size": 819200, "layout": "volume", "volume": "hfs"}
```

`file` is the path as given, and `size` a number. `layout` says what else there is:

| `layout` | Other fields |
|---|---|
| `volume` | `volume`: `mfs` or `hfs` |
| `disk_copy42` | `data_offset` and `data_length`, numbers; `volume`: `mfs`, `hfs`, or `null` |
| `partitioned` | `partitions`, an array of objects, below |
| `blank` | none |
| `unsupported` | `description`, a string saying what it is |
| `unrecognized` | none |

Each partition has `index` (a number, from 1), `type` (a string, from the partition map),
`offset` and `length` (numbers), `in_bounds` (a boolean, whether it fits in the file), and
`volume` (`mfs`, `hfs`, or `null`).

## `macman config show [NAME]`

```json
{"machine": {"schema_version": 1, "name": "wombat-6", "emulator": {"backend": "minivmac", "revision": "36.04"}, "...": "..."}}
```

`machine` is the machine's definition, its `machine.toml`, as JSON, with the same tables and
keys. That's the format in [DESIGN.md](DESIGN.md), and it holds what the file does,
including any shared folder's host path.

## `macman current`

```json
{"current": "wombat-6"}
```

With no current machine, it fails, as without `--json`.

## `macman import FILE --dry-run`

```json
{
  "dry_run": true,
  "roms": [{"name": "plus-v3", "outcome": "unchanged", "local": "plus-v3", "reason": null}],
  "systems": [{"name": "system-6.0.8", "outcome": "imported", "reason": null}],
  "machines": [
    {"name": "plus-a", "outcome": "conflict", "reason": "a different machine is already named \"plus-a\"; import it under another name with --rename"},
    {"name": "plus-b", "outcome": "imported", "reason": null}
  ],
  "imported": 2,
  "unchanged": 0,
  "unresolved": 1
}
```

This is what an import would do now, not what one did, as `dry_run: true` says. The
outcomes use an import's own words, and [EXPORT_IMPORT.md](EXPORT_IMPORT.md#--dry-run)
explains them.

| Field | Type | |
|---|---|---|
| `dry_run` | boolean | Always `true` for now |
| `roms[].name` | string | The ROM's name in the archive |
| `roms[].outcome` | string | `imported`, `recovered`, `unchanged`, `reused`, `conflict`, or `failed` |
| `roms[].local` | string or null | The registered ROM the machines would use; `null` when `imported`, `conflict`, or `failed` |
| `roms[].reason` | string or null | Why, for `conflict` and `failed` |
| `systems[].name` | string | The system image's name, here and in the archive |
| `systems[].outcome` | string | `imported`, `recovered`, `unchanged`, `conflict`, or `failed` |
| `systems[].reason` | string or null | Why, for `conflict` and `failed` |
| `machines[].name` | string | The name it would have here, which `--rename` sets |
| `machines[].outcome` | string | `imported`, `unchanged`, `conflict`, or `failed` |
| `machines[].reason` | string or null | Why, for `conflict` and `failed` |
| `imported`, `unchanged`, `unresolved` | numbers | Machines and system images counted by outcome, with `recovered` images as `imported`; `unresolved` is conflicts and failures, and includes ROMs' |

`conflict` and `failed` ROMs, `reason`, and `systems` are new with system images in
archives. A ROM gets `conflict` or `failed` only when the archive asks for it to be
registered on its own, as `export --all` does, so for an archive of machines alone,
these fields and counts are as they were. A ROM that only machines with a conflict or
failure need may not appear in `roms`. The
exit status is 1 when `unresolved` isn't 0.
