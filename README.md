# DiskSlots

DiskSlots answers one question first: where does each dataset live, and how much does it take?
It looks at local folders, raw NVMe drives cut into 1 GiB slots, and S3 buckets, and groups the copies of one dataset across them.
Then it copies datasets between those places with SHA-256 per part, frees slot runs on drives, and regroups a drive's runs so its free slots join up.

## The Screen

Run `diskslots` in a terminal, naming any buckets or folders beside the default places:

```sh
diskslots s3://bigger-ann s3://usearch-wiki
```

__Places__, on the left, lists `~/Data`, every NVMe namespace with no filesystem, every NVMe controller bound to `vfio-pci`, and whatever you name.
A drive adds its model, LBA size, wear, temperature, queues, NUMA node and the processes holding it.
__Datasets__, on the right, lists what the selected place holds: the format, what its header says, the size, and the other places holding a copy.
A drive adds its slot bar, with `█` for SafeGraphs files, `▓` for UStore databases, `▒` for foreign slots and `·` for free ones.

Every action is one bare key.
`c` opens __Copy__: pick a destination with `↑↓`, step through a drive's free runs with `←→`, `Enter` shows the plan, `y` starts it, and `Esc` stops it after the group in flight.
`f` opens __Free__ on a slot run, naming its owner and its other copies, and `y` frees it.
`r` opens __Regroup__ on a drive, showing its slot bar before and after and every move, and `y` runs the moves one by one.

## Commands

Every screen action is also a command printing the same lines, and without a terminal `diskslots` behaves like `diskslots list`.

```sh
diskslots list ~/Data /dev/nvme0n1 s3://bigger-ann
diskslots copy /dev/nvme0n1@0GiB s3://bigger-ann/sift-1b/index.safegraphs
diskslots free /dev/nvme0n1@96GiB
diskslots regroup /dev/nvme0n1
```

- `list` prints every place, its datasets and, for a drive, its slot bar.
- `copy` copies a file, a folder, a slot run, an S3 object or an S3 prefix to another place.
- `free` frees the SafeGraphs file or UStore database starting at a slot.
- `regroup` packs a drive's SafeGraphs files towards slot 0.

S3 uses the AWS SDK's default credentials, profile and region, so `AWS_PROFILE` and `AWS_REGION` work as they do for the AWS CLI.
`--endpoint https://storage.eu-north1.nebius.cloud` points it at an S3-compatible store instead.

## Addresses

```text
~/Data/sift-1b                    a folder or a file
/dev/nvme1n1@64GiB                a drive from byte 64 GiB
/dev/nvme1n1@64GiB+16GiB          the 16 GiB of it from byte 64 GiB on
s3://bigger-ann/sift-1b/          an S3 prefix
s3://bigger-ann/sift-1b/x.bin     an S3 object
```

A base and a length are whole numbers of `B`, `KiB`, `MiB`, `GiB` or `TiB`, bytes when bare.
A file whose name ends like an extent takes an explicit `@0`, as in `/data/backup@2GiB@0`.
A destination ending in `/`, an existing folder, or a bare bucket is a place to copy into, by the source's own name; a bare bucket keeps its datasets under `data/`.
A drive destination without a base takes its first free run long enough.

## Slot Rules

A raw drive is cut into 1 GiB slots from byte 0, and every file or database on it starts on a slot boundary.
DiskSlots reads the first page of each slot and checks it against each owner's own magic:

- a SafeGraphs file whose header parses and whose extent fits the drive covers as many slots as its length needs;
- a UStore owner block, `USTORESL` with layout version 1, names the database holding that slot;
- an all-zero page is free;
- anything else is foreign, one slot long, and never written.

A copy lands only on slots that are all free, and writes the first page of every slot it covers last, so a copy that stops early leaves them free.
Freeing zeroes the first page of every slot of the run, the one that makes it live first, then discards the rest of each slot.
Regrouping moves each SafeGraphs file into a free run strictly before it, as a verified copy followed by a free, so no move overlaps its source.
UStore databases and foreign slots stay where they are, and the files pack around them.

## Integrity

Every part of a copy is hashed with SHA-256, sixteen parts per batched call.
A part bound for S3 carries its digest, and S3 refuses a part whose bytes do not match it.
A part coming from S3 is compared with the digest S3 stores for it before a byte of it is written.
A part written to a file or drive is read back and hashed again.
Local files and S3 objects are never deleted or overwritten, and nothing is stored inside a data file to check it.

## Install

```sh
cargo install --git https://github.com/unum-cloud/DiskSlots
```

DiskSlots runs on Linux and macOS.
On Linux it reads and writes drives with `O_DIRECT`, discards freed slots, and asks drives for their health through NVMe admin commands, which may need `CAP_SYS_ADMIN`.

## License

Apache-2.0, see [LICENSE](https://github.com/unum-cloud/DiskSlots/blob/main/LICENSE).
