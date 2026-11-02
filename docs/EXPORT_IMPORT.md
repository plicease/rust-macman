# Export and import

`macman export` and `macman import` move machines between hosts. They transfer an
archive once; they aren't sync, and they never merge.

Apple ROMs and System Software are copyrighted, and an archive contains both. Archives
are for moving your own machines between your own computers, not for public
redistribution.

## Archive format

An archive is a gzipped tar. Its first entry is `manifest.toml`, and the rest are
`blobs/<sha256>`, one for each distinct ROM or disk, named after the SHA-256 of its
contents. Machines that share a ROM or an identical disk therefore share a blob.

```toml
format_version = 1

[[roms]]
name = "plus-v3"
sha256 = "..."
size = 131072

[[machines]]
rom_sha256 = "..."

[[machines.contents]]
path = "system.dsk"
sha256 = "..."
size = 819200

[machines.machine]
# the machine's definition, as in machine.toml
```

`format_version` changes whenever older versions of macman couldn't read an archive
correctly, and they refuse it and ask to be upgraded.

Archives hold the machines' current private disks, not the system images they were
created from, and the one ROM each machine uses. They leave out registered system
images, builds, source and build caches, logs, locks, and session data. Disk paths are
relative to the machine, so nothing in an archive depends on where macman stores files
on the source host. An imported machine builds or reuses its emulator on its first
`run`, and import itself compiles nothing.

## Identity

Each machine has an `id`, a random UUID in `machine.toml`. Unlike the name, it lasts
across export and import, so it says whether two machines are the same one. `create`
assigns a new one, and so does `clone`, because a clone goes its own way. Machines from
before `id` existed get one the first time they're exported, which is the only time
`export` changes a machine, and they keep it.

## What import does

ROMs are matched by SHA-256, so a ROM that's already registered is reused under its
local name, and the imported machine's definition refers to that name. A ROM with a
name in use but different contents is a conflict. It isn't renamed automatically.

If registering a ROM stopped after the ROM file but before its metadata, the next
import finishes the job and reports `recovered rom`. It does that only for a plain file
with no metadata and the archive's exact SHA-256, because the metadata can then be
rebuilt from the bytes. Anything else under that name is left alone as a conflict,
including metadata with no ROM file.

For each machine:

| Local state | Outcome |
| --- | --- |
| No machine with this `id` or name | `imported` |
| Same `id`, same configuration, ROM contents, and disk contents | `unchanged` |
| Same `id`, but anything else differs, whichever side changed | `conflict` |
| Same `id` and the local disks are in use | `conflict`; they can't be compared |
| Different machine with this name | `conflict` |
| The ROM's name is taken by a different ROM, or by an unclear partial registration | `conflict` |
| Backend, revision, hardware, or ROM unusable here | `failed` |

The comparison includes the emulator, hardware, display, and disk settings, and the
contents of every disk. It ignores file locations, timestamps, and which name the ROM
goes by. The comparison finds the local machine by `id`, even if it has been renamed.

Nothing is merged or overwritten, and there's no `--replace`. Conflicts and failures
affect only that machine: the others still import, the summary says what happened to
each, and the exit status is 1 if any machine is unresolved. An archive that's damaged,
hostile, or from a newer format is refused as a whole, before anything changes.

### `--rename`

`--rename NAME` imports an independent copy under `NAME`, and it needs an archive with
exactly one machine, so what it means is never ambiguous. The copy gets a new `id`, and
it doesn't look for an existing machine with the archive's `id`, so the original is not
involved at all. If `NAME` exists, that's a conflict. Importing the same archive with
`--rename` again is therefore refused, not `unchanged`.

## Safety

**Export** takes shared locks on all disks of all the machines before reading any of
them, and holds them until the archive is complete, so the archive is one coherent
snapshot. If any disk is in use, the whole export is refused. Every ROM is checked
against its registered digest, and each file's digest is computed again from the bytes
that went into the archive, which notices a writer that ignores macman's locks. An
export applies the same limits and path rules as import, so it won't write an archive
that a later import has to refuse. It writes to a temporary file, then publishes it
without overwriting anything. That needs hard links, so to put an archive on a FAT or
exFAT drive, export elsewhere and copy it.

**Import** treats the archive as hostile:

- Only `manifest.toml`, as the first entry, and `blobs/<sha256>` are accepted. Anything
  else, including `..` and absolute paths, and any entry that isn't a regular file,
  such as a link or device, refuses the archive. Entry names are never used as paths.
- Each blob must be listed in the manifest, appear once, and have the manifest's size
  and digest. Every listed blob must be present.
- The limits are 16 MiB for the manifest, 4 GiB for a blob, and 64 GiB in all. Blob
  sizes are checked against the manifest before any data is written, which bounds what
  a decompression bomb can do.
- Names in the manifest follow the rules for machine and ROM names. Disk paths must be
  relative, with no `..`, backslashes, or colons, and must not name `machine.toml` or
  collide with each other under case folding. On every host, they also can't use
  Windows device names or end in a dot or space, because an archive may reach Windows.

Everything unpacks into `staging/` and is verified before anything else is touched.
A machine is assembled there and moved into place with a single rename, and each copy
is verified again. If publishing fails, a ROM added for that machine is removed again.
A rename can replace an empty directory, and the standard library has no portable way
to refuse that, so `create`, `clone`, and `import` all hold `locks/machines.lock` across
the final existence check and the rename. A machine that appears during an import
is a conflict, and is left as it is. That lock only binds macman: an empty directory
another program creates in that instant could be replaced, though nothing in it can be
lost.
`staging/` is removed when an import finishes, and if an import was killed, the next
one clears what it left. A killed import can leave a ROM registered without a machine
that uses it, which is harmless, and the next import reuses or recovers it. One import
runs at a time, enforced by `import.lock`. File permissions from the archive are ignored.
