# Disk image formats

What Mini vMac 36.04 can attach, and what macman does with each kind of image.
macman always identifies images by content. File extensions such as `.dsk`, `.img`,
and `.image` are not reliable: a `.image` file may be a Disk Copy image or a raw
partitioned disk.

## How Mini vMac sees a disk

Mini vMac has one disk path: its replacement `.Sony` driver (`src/SONYEMDV.c`). The
SCSI chip in `src/SCSIEMDV.c` is a stub with no disks behind it, so nothing
reads a partition map. The driver makes one check when a disk is inserted, for a Disk
Copy 4.2 header. Anything else is presented to the guest as a single drive starting at
byte 0, and the guest looks for boot blocks at block 0 and a volume at block 2.

## Formats

| Image | `system add` | `run --disk` | Why |
|---|---|---|---|
| Raw MFS or HFS volume | Stored as is | Attached, writable | Mini vMac's native format |
| Disk Copy 4.2 | Volume unwrapped, then writable | Attached, **read-only in the guest** | See below |
| Apple partition map | Volume extracted | Refused | Mini vMac can't mount it |
| Blank image | Refused | Attached | The guest can initialize it |
| Disk Copy holding a partitioned disk | Refused | Refused | Would need both steps |
| Old-style (`TS`) partition map | Refused | Refused | Can't be mounted |
| HFS+ volume | Refused | Refused | Needs Mac OS 8.1 or later |
| UDIF `.dmg` | Refused | Refused | Modern compressed container |
| gzip, bzip2, zip, StuffIt, BinHex, MacBinary | Refused | Refused | Must be unpacked first |
| Anything else | Refused | Refused unless `--any-format` | Unrecognized |

The last row is deliberately separate from the ones above it. Those are known not to
work, so no flag overrides them. An unrecognized image might work, so `--any-format`
attaches it, ideally with `--readonly`, because a guest may offer to erase a disk it
can't read. NDIF images, from Disk Copy 6, keep their structure in a resource fork
and usually land here. They have no reliable signature in the data fork.

## Partitioned disks

Zip and hard-disk images start with a driver descriptor (`ER`), followed by a
partition map (`PM`). Block 0 therefore isn't boot blocks and block 2 isn't a volume.

`macman system add` reads the map and stores the partition holding an MFS or HFS
volume, confirming that by the volume's signature rather than the partition's type
name. If there are several, it lists them and asks for `--partition N`. The source
file is never changed. The sidecar records `extracted_from` and `source_sha256`.

`macman disk extract` does the same without registering anything, and `macman disk
info` shows the map. With `--raw --partition N`, it extracts a partition whatever it
holds, without checking that it's a volume. Partitions that are empty or run past the
end of the image are always refused.

The result is what you'd get by hand. For a 96 MB Iomega Zip image, whose `Apple_HFS`
partition starts at block 491:

```sh
dd if=system710-zip.image of=system710-hfs.img bs=512 skip=491 count=196106
```

Only the volume is kept, so the partition map, Zip driver, and any other partitions
are dropped. A guest doesn't need them, because Mini vMac provides its own driver.

`run --disk` won't do this, because it attaches your actual file, and writes to an
extracted copy would never reach it. Extract the volume first, or register the image.

## Disk Copy 4.2

Mini vMac reads Disk Copy 4.2 images directly, with no conversion. macman applies the
same validity rules as the emulator (`vSonyNextPendingInsert`):

- signature `0x0100` at offset 82
- data size a multiple of 512, and at least 4 blocks
- disk name under 64 bytes
- tag data either empty or 12 bytes per block
- the file at least as long as those parts; trailing bytes are ignored

An image failing these rules would be treated as raw by the emulator.

In a default build, `-sony-sum` is off, so Mini vMac won't keep the image's checksums
current. It revokes write access instead. That would leave a machine with a locked
boot disk, so `system add` stores the bare volume. Tag data is dropped, which classic
Mac OS doesn't need. `run --disk` passes the image through unchanged and
prints a note, as does `run` for a machine whose disk was registered by an older macman.

## Limitations

- Findings here come from reading the 36.04 source. Raw and extracted volumes have
  been booted for real, but a partitioned image has not been tried in the emulator.
- macman only checks signatures. It doesn't verify a volume's structure or that a
  System is compatible with the model. It warns if a volume has no boot blocks.
- Models with 64K ROMs read only MFS, and macman warns when given HFS.
