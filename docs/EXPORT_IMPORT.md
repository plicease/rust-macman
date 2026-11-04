# Export and import

`macman export` and `macman import` move machines, ROMs, system images, and media between
hosts. They transfer an archive once; they aren't sync, and they never merge.

Apple ROMs, System Software, and installation media are copyrighted, and an archive
contains them. Archives are for moving your own machines between your own computers, not
for public redistribution.

## Archive format

An archive is a gzipped tar. Its first entry is `manifest.toml`, and the rest are
`blobs/<sha256>`, one for each distinct ROM, system image, medium, or disk, named after the
SHA-256 of its contents. Anything with the same bytes therefore shares a blob: machines
on one ROM, identical disks, two names for one system image, and a fresh machine's disk
and the image it was created from.

```toml
format_version = 3

[[roms]]
name = "plus-v3"
sha256 = "..."
size = 131072
register = true      # only with --all

[[systems]]
name = "system-6.0.8"
sha256 = "..."
size = 819200
version = "6.0.8"    # what `system add` recorded, when it did
extracted_from = "partition 3 (Apple_HFS)"
source_sha256 = "..."

[[media]]
name = "macos-9.0.4"
sha256 = "..."
size = 681574400
kind = "cdrom"       # cdrom, floppy, or disk
format = "partitioned"
bootable = true
source = "Mac OS 9.0.4.toast"
version = "Mac OS 9.0.4"   # only if given to `media add`

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
correctly, and they refuse it and ask to be upgraded. Format 2 adds `systems` and
`register`, which format 1 readers would quietly drop, and format 3 adds `media`. Each
archive is written in the oldest format that holds it: one without media is format 2,
and one of machines alone, with only the ROMs they use, is format 1, so older versions of
macman import them as before. That includes `--all-machines`; a ROM registered for its
own sake, by `--rom`, `--all-roms`, or `--all`, needs format 2. This version reads all three.

## What an archive holds

A machine brings its current private disks and the one ROM it uses. Its definition
doesn't record which system image it was created from, and its disk has changed since,
so no system image comes with it. Those are chosen separately:

| Command | Machines | ROMs | System images | Media |
| --- | --- | --- | --- | --- |
| `export NAME...` | Those named | The ones they use | None | None |
| `export --all-machines` | All | The ones they use | None | None |
| `export --rom ROM...` | None | Those named | None | None |
| `export --all-roms` | None | All, used or not | None | None |
| `export --all-machines --all-roms` | All | All, used or not | None | None |
| `export NAME... --rom ROM...` | Those named | The ones they use, and those named | None | None |
| `export NAME... --system SYS...` | Those named | The ones they use | Those named | None |
| `export --all-machines --system SYS...` | All | The ones they use | Those named | None |
| `export --system SYS...` | None | None | Those named | None |
| `export [NAME...] --all-systems` | Those named, if any | The ones they use | All | None |
| `export --all-roms --all-systems` | None | All, used or not | All | None |
| `export --media MEDIA...` | None | None | None | Those named |
| `export [NAME...] --all-media` | Those named, if any | The ones they use | None | All |
| `export --all` | All | All, used or not | All | All |

The selectors are additive: an archive holds what any of them selects, and a file
selected more than once, such as a ROM two machines use, is stored once. Machine names
and `--all-machines`, `--rom` and `--all-roms`, `--system` and `--all-systems`, and
`--media` and `--all-media` select from the same four registries, so the rows above
combine freely. Only these combinations are refused: machine names with
`--all-machines`, `--system` with `--all-systems`, `--media` with `--all-media`, and
`--all` with anything, since it already takes everything. `--rom` with `--all-roms` is
allowed, and is every ROM.

A name that isn't registered stops the export, and so does a selection with nothing in
it, such as `--all-machines` on a host with no machines. `--all-machines` with no
machines is fine alongside a selector that does find something.

A ROM named with `--rom`, or taken by `--all-roms` or `--all`, is registered for its own
sake: it has an entry of its own under its registered name, and import registers it
under that name whether or not a machine uses it. A ROM registered under two names from
the same bytes arrives under both, sharing one blob. A ROM that comes only because a
machine uses it is, as in format 1, one entry for its contents, which import reuses where
an identical ROM is already registered under another name.

Archives leave out builds, source and build caches, logs, locks, session data,
[aliases](../README.md#aliases), which are names on one host, and
[checkpoints](../README.md#checkpoints): a machine travels with its current disks only,
and import, which never replaces a local machine, leaves the checkpoints of one that's
already here as they are. Disk paths are
relative to the machine, so nothing in an archive depends on where macman stores files
on the source host. An imported machine builds or reuses its emulator on its first
`run`, and import itself compiles nothing. Hosts can build different Mini vMac revisions,
so that's whichever revision the destination builds; see
[PLATFORMS.md](PLATFORMS.md#mini-vmac-revisions). The definition keeps the revision it
arrived with.

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

A ROM marked `register`, as all are with `--all`, is registered under its own name,
even if no machine that uses it is imported. Like a system image, it's matched by name:
`unchanged` if this name has these contents, `recovered` as below, `imported` if the
name is free, even when an identical ROM is registered under another name, and
otherwise `conflict rom NAME`, which leaves the import unresolved. Machines are decided
first, and still reuse an identical ROM that's already here, as described above.
Without `register`, a ROM comes in only with a machine, as it always has.

If registering a ROM stopped after the ROM file but before its metadata, the next
import finishes the job and reports `recovered rom`. It does that only for a plain file
with no metadata and the archive's exact SHA-256, because the metadata can then be
rebuilt from the bytes. Anything else under that name is left alone as a conflict,
including metadata with no ROM file.

System images are matched by name, since that's what `create --system` uses, and
compared by contents, read in full:

| Local state | Outcome |
| --- | --- |
| No system image with this name | `imported` |
| Same name and contents | `unchanged` |
| Same name, other contents, or a damaged local copy | `conflict` |
| The image alone, with these contents and no metadata | `recovered` |
| The metadata alone, or the image is a link or in use | `conflict` |

An image the same as one registered here under another name is still imported under its
own name, so each name in the archive can be used afterwards. Its metadata comes from
the archive, so `system list` shows what it did on the source host. A conflict affects
only that image.

Media are matched the same way, by name and then contents, with the same outcomes:
`imported`, `unchanged`, `recovered`, or `conflict media NAME`, and never overwritten.
An imported medium is the archive's bytes exactly, stored read-only like one registered
here, with the metadata `media add` recorded on the source host. A dry run decides
media from their digests without unpacking them.

For each machine:

| Local state | Outcome |
| --- | --- |
| No machine with this `id` or name | `imported` |
| Same `id`, same configuration, ROM contents, and disk contents | `unchanged` |
| Same `id`, but anything else differs, whichever side changed | `conflict` |
| Same `id` and the local disks are in use | `conflict`; they can't be compared |
| Different machine with this name | `conflict` |
| The name, or the `--rename` name, is an alias of a local machine | `conflict` |
| The ROM's name is taken by a different ROM, or by an unclear partial registration | `conflict` |
| Backend, hardware, or ROM unusable here, or a revision not known to be interchangeable with this host's | `failed` |

The comparison includes the backend, hardware, display, disk, and `ephemeral` settings, and the
contents of every disk. It ignores file locations, timestamps, which name the ROM goes
by, and which revision a copy records, provided both run as the same one here. The comparison finds the local machine by `id`, even if it has been renamed.

Nothing is merged or overwritten, and there's no `--replace`. Conflicts and failures
affect only that machine, image, or ROM: the others still import, the summary says
what happened to each, and the exit status is 1 if anything is unresolved. The summary
counts machines, system images, and media; its unresolved count also includes ROMs
registered for their own sake. An archive that's damaged,
hostile, or from a newer format is refused as a whole, before anything changes.

### `--rename`

`--rename NAME` imports an independent copy under `NAME`, and it needs an archive with
exactly one machine, so what it means is never ambiguous. The copy gets a new `id`, and
it doesn't look for an existing machine with the archive's `id`, so the original is not
involved at all. If `NAME` exists, that's a conflict. Importing the same archive with
`--rename` again is therefore refused, not `unchanged`.

### `--dry-run`

`macman import FILE --dry-run` shows what an import would do, and changes nothing. It
reads and checks the whole archive the way an import does, including every blob's size
and digest. It then decides each ROM and machine with the same code and rules, and
stops short of writing anything. With `--rename`, it previews that copy.

Each line says what an import would do: `would import`, `would recover`, `would
reuse`, `conflict`, or `failed` for ROMs, then `would import`, `would recover`,
`unchanged`, `conflict`, or `failed` for system images and machines, with the reason. All of them are reported, not just the first problem. A
rejected archive gives the same error as a real import.

A dry run doesn't register ROMs or machines, finish a partial ROM registration, or
clear an abandoned `staging/`. It also creates no directories or lock files in macman's
data directory. To decide, it needs the ROMs' and system images' contents. It unpacks only those,
into a private temporary directory that it removes before exiting, so that needs room
for the archive's system images. Disks are verified as they
stream past, and then discarded. If an import has ever run, the dry run holds a
shared lock on `import.lock` while it reads, so it doesn't overlap a real import.
Comparing a machine with its local copy takes the same shared locks on the local disks,
but only through lock files that already exist. A disk with no lock file has no holder.

The result is how an import would go against macman's data as it is now. Something can
change before you import: another import, a machine started or edited, or a ROM
registered. If it does, the real import decides afresh, and its outcome can differ.

Exit status is the same as an import's:
- 0 when nothing would be left unresolved.
- 1 when any machine would conflict or fail, or the archive is refused.
- 2 for arguments that can't be parsed.

`--json` works with `--dry-run`, and its fields are in [JSON.md](JSON.md). An import
without `--dry-run` refuses `--json`.

## Safety

**Export** takes shared locks on all disks of all the machines, and on the system
images, before reading any of them, and holds them until the archive is complete, so
the archive is one coherent snapshot. If any of them is in use, the whole export is
refused. Every ROM and system image is checked against its registered digest, and
each system image must still be a volume `system add` would take. Each file's digest
is computed again from the bytes that went into the archive, which notices a writer
that ignores macman's locks. An export applies the same limits and path rules as import, so it won't write an archive
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
- Names in the manifest follow the rules for machine, ROM, and system names, and a
  system image's version and origin can't hold control characters.
- Each system image must be a volume that `system add` would accept. Disk paths must be
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
that uses it, which is harmless, and the next import reuses or recovers it. A system
image is copied and verified before its metadata is written, and is removed again if
that fails, so a killed import leaves at most an image without metadata, which the
next import recovers. One import
runs at a time, enforced by `import.lock`. File permissions from the archive are ignored.

## Basilisk II machines

These export like any other, and build lazily on the destination. A machine's shared
folder is a path on its host, so archives leave it out, and it doesn't count when
import compares two copies. Importing onto a host that can't run Basilisk II works, and
`macman doctor` and `macman run` then say why the machine can't start.

SheepShaver machines work the same way, under the same rules, and may be imported onto
hosts that can't run them.
