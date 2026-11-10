# DiskSlots

DiskSlots shows where every dataset lives and how much it takes, across local folders, raw NVMe drives and S3 buckets, and moves datasets between them.
It reads raw drives the way USearch and UStore lay them out, so one screen names SafeGraphs files and UStore databases side by side.

## What It Does

- __Map__ every place named on the command line, plus the local dataset store and every NVMe namespace with no mounted filesystem, with the datasets each holds and the other places holding the same dataset.
- __Copy__ a dataset between a folder, a raw drive and an S3 bucket, with SHA-256 checked for every part.
- __Free__ a run of slots on a raw drive.
- __Regroup__ a raw drive, so its free space becomes one run.

## Addresses

- `/path` names a file or a folder.
- `/dev/nvme1n1@64GiB` names a raw drive from a byte base, and `/dev/nvme1n1@64GiB+16GiB` bounds it to a length.
- `s3://bucket/prefix` names an S3 prefix.

## Raw Drive Slots

- A raw drive is cut into slots of 1 GiB from byte 0, with no filesystem and no label.
- Every file or database starts on a slot boundary and covers whole slots.
- A slot is free when its first logical block is all zeros, owned when that block opens a SafeGraphs file or a UStore database, and foreign otherwise.
- Owned and foreign slots are never written, a copy's first block is written last, and a source is freed only after its copy is verified.

## License

Apache 2.0, see [LICENSE](https://github.com/unum-cloud/DiskSlots/blob/main/LICENSE).
