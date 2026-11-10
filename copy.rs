//! The `copy` verb: one dataset between any two places, every part checked by SHA-256.
//!
//! A transfer splits into parts of one size, and up to one part per pool thread moves as a group
//! through buffers lent from one pool. A part bound for S3 is hashed by the SDK and checked by S3; a
//! part coming from S3 is checked by the SDK against the SHA-256 S3 stores for it; every part
//! written to a file or drive is hashed before it is written and again after it is read back. The
//! first logical block of every slot a copy covers is written last, the first slot's after all the
//! others, so a copy that stops early leaves its slots free.

use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroU16;
use std::ops::{ControlFlow, Range};
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    ChecksumAlgorithm, ChecksumMode, CompletedMultipartUpload, CompletedPart, StorageClass,
};
use bytes::Bytes;
use forkunion as fu;
use futures_util::future::try_join_all;
use stringzilla as sz;

use crate::direct::{self, part_bytes, Access, AlignedBuffer, BufferPool, Device, PooledBuffer};
use crate::list::{self, Entry};
use crate::places::{s3_failed, size, Address, Location, ObjectAddress, Placement, S3};
use crate::slots::{self, slot_at, slot_extent, Slot, SpanOwner, UstoreOwnerBlock, SLOT_BYTES};
use crate::{Error, Refusal, SLOT_SHIFT};

/// The folder a copy into a bare bucket lands under, per the address rules.
const BUCKET_DATA_FOLDER: &str = "data";

/// The `If-None-Match` value that completes an upload only where no object exists yet.
const NO_OBJECT: &str = "*";

/// The index of an object's first part.
const FIRST_PART: u16 = 0;

/// Where Linux lists block devices, each linking into its device's sysfs folder.
const SYSFS_BLOCK: &str = "/sys/class/block";

/// The sysfs attribute naming a device's NUMA node.
const NUMA_NODE: &str = "numa_node";

/// Part buffers take at most `1 / 2^MEMORY_SHARE_SHIFT` of the memory the process may use, leaving
/// the rest to the page cache, the S3 client and the other processes on the machine.
const MEMORY_SHARE_SHIFT: u32 = 2;

/// One side of a transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A file, from its first byte.
    File(PathBuf),
    /// A slot run on a drive, from byte `base`.
    Drive {
        path: PathBuf,
        base: u64,
    },
    Object(ObjectAddress),
}

impl Endpoint {
    /// The address of this endpoint's first `bytes`, which errors and refusals carry.
    fn address(&self, bytes: u64) -> Address {
        match self {
            Endpoint::File(path) => Address::path(path),
            Endpoint::Drive { path, base } => Address::extent(path, *base..base + bytes),
            Endpoint::Object(object) => Address::whole(Location::Bucket(object.clone())),
        }
    }
}

/// What one transfer moves, fixed before any byte moves.
pub struct Transfer {
    pub source: Endpoint,
    pub destination: Endpoint,
    pub bytes: u64,
    pub part_bytes: u64,
    pub parts: NonZeroU16,
}

impl Transfer {
    /// An empty source still takes one empty part, as S3 completes no upload without one.
    fn new(source: Endpoint, destination: Endpoint, bytes: u64, part_bytes: u64) -> Transfer {
        let parts = u16::try_from(bytes.div_ceil(part_bytes))
            .expect("part_bytes keeps a transfer within S3_PARTS_MAX parts");
        Transfer {
            source,
            destination,
            bytes,
            part_bytes,
            parts: NonZeroU16::new(parts).unwrap_or(NonZeroU16::MIN),
        }
    }

    /// The byte range of part `index`, the last one shorter.
    pub fn part(&self, index: u16) -> Range<u64> {
        let start = u64::from(index) * self.part_bytes;
        start..self.bytes.min(start + self.part_bytes)
    }

    /// The slots whose first blocks the destination holds back: every slot the bytes reach.
    fn slots(&self) -> usize {
        usize::try_from(self.bytes.div_ceil(SLOT_BYTES))
            .expect("a transfer covers fewer slots than memory has bytes")
            .max(1)
    }
}

/// How far a job has come.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub started: Instant,
}

impl Progress {
    pub fn new(bytes_total: u64) -> Progress {
        Progress {
            bytes_done: 0,
            bytes_total,
            started: Instant::now(),
        }
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (done, total) = (size(self.bytes_done), size(self.bytes_total));
        write!(formatter, "{done} of {total}")?;
        let seconds = self.started.elapsed().as_secs_f64();
        match seconds > 0.0 {
            true => write!(
                formatter,
                ", {}/s",
                size((self.bytes_done as f64 / seconds) as u64)
            ),
            false => Ok(()),
        }
    }
}

/// Everything one copy moves: a file, a slot run or an object, or every file of a folder or every
/// object of a prefix.
pub struct CopyPlan {
    pub source: Address,
    /// Where the copy lands: a drive's run, or a folder, file, prefix or object by its own name.
    pub destination: Address,
    pub transfers: Vec<Transfer>,
    pub bytes: u64,
}

impl fmt::Display for CopyPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: u64 = self
            .transfers
            .iter()
            .map(|transfer| u64::from(transfer.parts.get()))
            .sum();
        let (source, destination, bytes) = (&self.source, &self.destination, size(self.bytes));
        write!(
            formatter,
            "copy {source} → {destination}: {bytes} in {parts} parts"
        )
    }
}

/// What a source destined for a drive is, which decides where on the drive it may land.
#[derive(Clone, Copy)]
enum Payload {
    /// A SafeGraphs file, which lands on any free run.
    Graph,
    /// A UStore stretch, whose owner blocks name absolute offsets, so it lands only at slot `slot`.
    Pinned { slot: u32 },
}

/// One file, run or object a source address resolves to.
struct Item {
    endpoint: Endpoint,
    bytes: u64,
    part_bytes: u64,
    /// Its path under the source folder or prefix, empty for a single source.
    relative: String,
}

/// What a source address resolves to.
struct Sources {
    /// The source's own name, which a copy into a folder or prefix takes; empty for a slot run.
    name: String,
    items: Vec<Item>,
}

impl CopyPlan {
    /// Resolves both addresses and checks every rule before any byte moves.
    pub fn new(
        source: &Address,
        destination: &Address,
        s3: Option<&S3>,
    ) -> Result<CopyPlan, Error> {
        if let (Location::Bucket(from), Location::Bucket(to)) =
            (&source.location, &destination.location)
        {
            return Err(Error::Refused(Refusal::Unsupported {
                source: from.clone(),
                destination: to.clone(),
            }));
        }
        let sources = sources(source, s3)?;
        let (resolved, transfers) = match &destination.location {
            Location::Path(path) if is_drive(path) => onto_drive(path, destination, sources, s3)?,
            _ => into_place(destination, sources, s3)?,
        };
        Ok(CopyPlan {
            source: source.clone(),
            destination: resolved,
            bytes: transfers.iter().map(|transfer| transfer.bytes).sum(),
            transfers,
        })
    }

    /// Moves every transfer, calling `report` after every group of parts and stopping after the
    /// group in flight once it breaks.
    pub fn run(
        &self,
        s3: Option<&S3>,
        mut report: impl FnMut(&Progress) -> ControlFlow<()>,
    ) -> Result<(), Error> {
        let mut progress = Progress::new(self.bytes);
        let (first, rest) = self
            .transfers
            .split_first()
            .expect("a plan holds a transfer");
        let sides = Sides::open(first, s3)?;
        let mut engine = Engine::new(&self.transfers, sides.aligned())?;
        engine.transfer(first, sides, &mut progress, &mut report)?;
        for transfer in rest {
            let sides = Sides::open(transfer, s3)?;
            engine.transfer(transfer, sides, &mut progress, &mut report)?;
        }
        Ok(())
    }
}

/// The `copy` verb: the plan, one progress line per group of parts, and where the copy landed.
pub fn copy(source: &Address, destination: &Address, endpoint: Option<&str>) -> Result<(), Error> {
    let s3 = S3::for_addresses([source, destination], endpoint)?;
    let plan = CopyPlan::new(source, destination, s3.as_ref())?;
    println!("{plan}");
    plan.run(s3.as_ref(), crate::print_progress)?;
    println!(
        "{} holds a checked copy of {}",
        plan.destination, plan.source
    );
    Ok(())
}

fn is_drive(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_block_device())
}

fn io_failed(address: &Address) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        address: address.clone(),
        source,
    }
}

fn refused(refusal: fn(Address) -> Refusal, address: &Address) -> Error {
    Error::Refused(refusal(address.clone()))
}

fn connected<'s3>(s3: Option<&'s3 S3>, address: &Address) -> Result<&'s3 S3, Error> {
    s3.ok_or_else(|| io_failed(address)(io::ErrorKind::NotConnected.into()))
}

fn sources(address: &Address, s3: Option<&S3>) -> Result<Sources, Error> {
    let path = match &address.location {
        Location::Bucket(object) => return bucket_sources(connected(s3, address)?, object),
        Location::Path(path) => path,
    };
    let metadata = fs::metadata(path).map_err(io_failed(address))?;
    if metadata.file_type().is_block_device() {
        return drive_source(path, address);
    }
    if address.base != 0 || address.limit != 0 {
        return Err(refused(|address| Refusal::NotDrive { address }, address));
    }
    let mut entries = Vec::new();
    match metadata.is_dir() {
        true => list::walk(path, "", &mut Vec::new(), &mut entries)?,
        false => entries.push(Entry {
            name: String::new(),
            bytes: metadata.len(),
        }),
    }
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let Location::Path(file) = address.location.join(&entry.name) else {
            unreachable!("a path joins into a path");
        };
        let device = Device::open(&file, Access::Read)?;
        items.push(Item {
            bytes: device.bytes,
            part_bytes: part_bytes(device.bytes, &device),
            endpoint: Endpoint::File(file),
            relative: entry.name,
        });
    }
    if items.is_empty() {
        return Err(refused(|address| Refusal::Empty { address }, address));
    }
    items.sort_by(|first, second| first.relative.cmp(&second.relative));
    Ok(Sources {
        name: address.location.name(),
        items,
    })
}

/// The SafeGraphs file or UStore stretch whose first slot sits at the address's base.
fn drive_source(path: &Path, address: &Address) -> Result<Sources, Error> {
    let device = Device::open(path, Access::Read)?;
    let slot = slot_at(address)?;
    let map = slots::map(&device)?;
    let bytes = match map.covering(slot) {
        Some(span) if span.first_slot != slot => {
            return Err(refused(|address| Refusal::StaysAtBase { address }, address))
        }
        Some(span) => match &span.owner {
            SpanOwner::SafeGraphs(graph) => graph.file_bytes,
            SpanOwner::UStore { .. } => u64::from(span.slots) << SLOT_SHIFT,
            SpanOwner::Foreign => {
                return Err(refused(|address| Refusal::Foreign { address }, address))
            }
        },
        None => return Err(refused(|address| Refusal::Empty { address }, address)),
    };
    let item = Item {
        endpoint: Endpoint::Drive {
            path: path.to_path_buf(),
            base: address.base,
        },
        bytes,
        part_bytes: part_bytes(bytes, &device),
        relative: String::new(),
    };
    Ok(Sources {
        name: String::new(),
        items: vec![item],
    })
}

/// One object, or every object under a prefix.
fn bucket_sources(s3: &S3, prefix: &ObjectAddress) -> Result<Sources, Error> {
    let location = Location::Bucket(prefix.clone());
    let mut entries = list::list_objects(s3, prefix)?;
    if let Some(object) = entries.iter().position(|entry| entry.name.is_empty()) {
        entries.swap_remove(object);
        entries.truncate(1);
    }
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let Location::Bucket(object) = location.join(&entry.name) else {
            unreachable!("a bucket joins into a bucket");
        };
        items.push(object_item(s3, object, entry.name)?);
    }
    if items.is_empty() {
        return Err(refused(
            |address| Refusal::Empty { address },
            &Address::whole(location),
        ));
    }
    Ok(Sources {
        name: location.name(),
        items,
    })
}

fn object_exists(s3: &S3, object: &ObjectAddress) -> Result<bool, Error> {
    let head = s3
        .client
        .head_object()
        .bucket(&object.bucket)
        .key(&object.key);
    match s3.block_on(head.send()) {
        Ok(_) => Ok(true),
        Err(error)
            if error
                .as_service_error()
                .is_some_and(|error| error.is_not_found()) =>
        {
            Ok(false)
        }
        Err(error) => Err(s3_failed(object)(error)),
    }
}

/// An object's length and part size, from its first and last parts, refusing an object whose parts
/// carry no SHA-256.
fn object_item(s3: &S3, object: ObjectAddress, relative: String) -> Result<Item, Error> {
    let uneven = || {
        Error::Refused(Refusal::UnevenParts {
            object: object.clone(),
        })
    };
    let part_length = |index: u16| -> Result<(u64, NonZeroU16), Error> {
        let head = s3
            .client
            .head_object()
            .bucket(&object.bucket)
            .key(&object.key)
            .part_number(i32::from(index) + 1)
            .checksum_mode(ChecksumMode::Enabled);
        let output = s3.block_on(head.send()).map_err(s3_failed(&object))?;
        if output.checksum_sha256().is_none() {
            return Err(Error::Refused(Refusal::Unverifiable {
                object: object.clone(),
            }));
        }
        let length = output
            .content_length()
            .and_then(|length| u64::try_from(length).ok());
        // An object uploaded whole reports no part count, and is its own one part.
        let parts = match output.parts_count() {
            Some(count) => u16::try_from(count).ok().and_then(NonZeroU16::new),
            None => Some(NonZeroU16::MIN),
        };
        Ok((length.ok_or_else(uneven)?, parts.ok_or_else(uneven)?))
    };
    let (part_bytes, parts) = part_length(FIRST_PART)?;
    let address = Address::whole(Location::Bucket(object.clone()));
    if part_bytes == 0 {
        return Err(refused(|address| Refusal::Empty { address }, &address));
    }
    let last = parts.get() - 1;
    let last_bytes = match last {
        FIRST_PART => part_bytes,
        last => part_length(last)?.0,
    };
    if last_bytes > part_bytes {
        return Err(uneven());
    }
    Ok(Item {
        bytes: u64::from(last) * part_bytes + last_bytes,
        part_bytes,
        endpoint: Endpoint::Object(object),
        relative,
    })
}

/// Reads what a source bound for a drive opens with, refusing anything a drive does not take; the
/// whole first part is checked again once it is read.
fn payload(item: &Item, destination: &Device, s3: Option<&S3>) -> Result<Payload, Error> {
    let address = item.endpoint.address(item.bytes);
    let block = u64::from(destination.logical_block.get());
    let (local, remote);
    let head: &[u8] = match &item.endpoint {
        Endpoint::File(path) | Endpoint::Drive { path, .. } => {
            let device = Device::open(path, Access::Read)?;
            let mut buffer =
                AlignedBuffer::zeroed(&device, block.max(device.logical_block.get().into()))?;
            let filled = device.read_at(&mut buffer, address.base)?;
            local = buffer;
            &local[..filled]
        }
        Endpoint::Object(object) => {
            remote = connected(s3, &address)?.prefix(object, block)?;
            &remote
        }
    };
    match (UstoreOwnerBlock::read(head), &item.endpoint) {
        // A stretch read off a drive lands back where it was, a data directory's included.
        (Some(_), Endpoint::Drive { .. }) => Ok(Payload::Pinned {
            slot: slot_at(&address)?,
        }),
        (Some(owner), _) => owner
            .manifest_slot()
            .map(|slot| Payload::Pinned { slot })
            .ok_or_else(|| refused(|address| Refusal::StaysAtBase { address }, &address)),
        (None, _) if head.starts_with(&safegraphs::MAGIC) => Ok(Payload::Graph),
        (None, _) => Err(refused(|address| Refusal::NotGraph { address }, &address)),
    }
}

/// Refuses unless slots `first..first + slots` exist and no dataset or foreign slot touches them.
fn require_free(device: &Device, first: u32, slots: u32, bytes: u64) -> Result<(), Error> {
    let map = slots::map(device)?;
    let run = device.address(slot_extent(first, slots));
    if first.checked_add(slots).is_none_or(|end| end > map.slots) {
        return Err(Error::Refused(Refusal::Short {
            address: run,
            bytes,
        }));
    }
    let overlapping = map
        .spans
        .iter()
        .find(|span| span.first_slot < first + slots && first < span.end());
    match overlapping.map(|span| &span.owner) {
        None => Ok(()),
        Some(SpanOwner::Foreign) => Err(Error::Refused(Refusal::Foreign { address: run })),
        Some(_) => Err(Error::Refused(Refusal::Owned { address: run })),
    }
}

/// A drive takes one SafeGraphs file on any free run, or a UStore stretch back at its own slot.
fn onto_drive(
    path: &Path,
    address: &Address,
    sources: Sources,
    s3: Option<&S3>,
) -> Result<(Address, Vec<Transfer>), Error> {
    let [item] = <[Item; 1]>::try_from(sources.items)
        .map_err(|_| refused(|address| Refusal::NotGraph { address }, address))?;
    let device = Device::open(path, Access::Read)?;
    let short = || {
        Error::Refused(Refusal::Short {
            address: address.clone(),
            bytes: item.bytes,
        })
    };
    if address.limit != 0 && address.limit < item.bytes {
        return Err(short());
    }
    let needed = u32::try_from(item.bytes.div_ceil(SLOT_BYTES).max(1)).map_err(|_| short())?;
    let named = slot_at(address)?;
    let first = match (payload(&item, &device, s3)?, address.placement) {
        (Payload::Pinned { slot }, Placement::FirstFree) => slot,
        (Payload::Pinned { slot }, Placement::Exact) if slot == named => slot,
        (Payload::Pinned { .. }, Placement::Exact) => {
            return Err(refused(|address| Refusal::StaysAtBase { address }, address))
        }
        (Payload::Graph, Placement::Exact) => named,
        (Payload::Graph, Placement::FirstFree) => {
            let map = slots::map(&device)?;
            slots::first_free_run(&map.occupied(), needed, 0, map.slots).ok_or_else(short)?
        }
    };
    let run = device.address(slot_extent(first, needed));
    if let Endpoint::Drive { path: source, base } = &item.endpoint {
        let source_run = *base..base + item.bytes;
        if source == path && source_run.start < run.base + run.limit && run.base < source_run.end {
            return Err(Error::Refused(Refusal::Overlap { address: run }));
        }
    }
    require_free(&device, first, needed, item.bytes)?;
    let part_bytes = match item.endpoint {
        Endpoint::Object(_) => item.part_bytes,
        _ => item.part_bytes.max(part_bytes(item.bytes, &device)),
    };
    let destination = Endpoint::Drive {
        path: path.to_path_buf(),
        base: run.base,
    };
    let transfer = Transfer::new(item.endpoint, destination, item.bytes, part_bytes);
    Ok((run, vec![transfer]))
}

/// An existing folder, a path ending in `/`, a bare bucket or a prefix ending in `/` takes the copy
/// by the source's name, a bare bucket under `data/`; any other path or prefix is the copy itself.
fn into_place(
    address: &Address,
    sources: Sources,
    s3: Option<&S3>,
) -> Result<(Address, Vec<Transfer>), Error> {
    if address.base != 0 || address.limit != 0 {
        return Err(refused(|address| Refusal::NotDrive { address }, address));
    }
    let into = match &address.location {
        Location::Path(path) => {
            path.is_dir() || path.as_os_str().as_encoded_bytes().ends_with(b"/")
        }
        Location::Bucket(object) => object.key.is_empty() || object.key.ends_with('/'),
    };
    let root = match &address.location {
        _ if into && sources.name.is_empty() => {
            return Err(refused(|address| Refusal::Unnamed { address }, address))
        }
        Location::Bucket(object) if object.key.is_empty() => address
            .location
            .join(BUCKET_DATA_FOLDER)
            .join(&sources.name),
        location if into => location.join(&sources.name),
        location => location.clone(),
    };
    let mut transfers = Vec::with_capacity(sources.items.len());
    for item in sources.items {
        let target = root.join(&item.relative);
        let exists = match &target {
            Location::Path(path) => {
                let mut relative = Path::new(&item.relative).components();
                if !relative.all(|component| matches!(component, Component::Normal(_))) {
                    return Err(io_failed(&item.endpoint.address(item.bytes))(
                        io::ErrorKind::InvalidInput.into(),
                    ));
                }
                path.symlink_metadata().is_ok()
            }
            Location::Bucket(object) => object_exists(connected(s3, address)?, object)?,
        };
        if exists {
            return Err(refused(
                |address| Refusal::Exists { address },
                &Address::whole(target),
            ));
        }
        let destination = match target {
            Location::Path(path) => Endpoint::File(path),
            Location::Bucket(object) => Endpoint::Object(object),
        };
        transfers.push(Transfer::new(
            item.endpoint,
            destination,
            item.bytes,
            item.part_bytes,
        ));
    }
    Ok((Address::whole(root), transfers))
}

/// A file or drive open for a transfer, and the byte its extent starts at.
struct Local {
    device: Device,
    base: u64,
}

/// Where the parts of a transfer written to a file or drive come from.
enum Source<'plan> {
    Local(Local),
    Object {
        s3: &'plan S3,
        object: &'plan ObjectAddress,
    },
}

/// Both sides of a transfer, open.
enum Sides<'plan> {
    Upload {
        source: Local,
        upload: Upload<'plan>,
    },
    Write {
        source: Source<'plan>,
        destination: Local,
        /// The first logical block of every slot the copy covers, written once all else is.
        first_blocks: AlignedBuffer,
    },
}

impl<'plan> Sides<'plan> {
    /// Opens both sides of `transfer`, rechecking that a destination run is still free.
    fn open(transfer: &'plan Transfer, s3: Option<&'plan S3>) -> Result<Sides<'plan>, Error> {
        let local = |path: &Path, base: u64, access| -> Result<Local, Error> {
            let device = Device::open(path, access)?;
            Ok(Local { device, base })
        };
        let source = match &transfer.source {
            Endpoint::File(path) => Source::Local(local(path, 0, Access::Read)?),
            Endpoint::Drive { path, base } => Source::Local(local(path, *base, Access::Read)?),
            Endpoint::Object(object) => Source::Object {
                s3: connected(s3, &transfer.source.address(transfer.bytes))?,
                object,
            },
        };
        let destination = match &transfer.destination {
            Endpoint::Object(object) => {
                let Source::Local(source) = source else {
                    unreachable!("planning refuses S3 to S3");
                };
                let s3 = connected(s3, &transfer.destination.address(transfer.bytes))?;
                let upload = Upload::create(s3, object, transfer)?;
                return Ok(Sides::Upload { source, upload });
            }
            Endpoint::File(path) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(io_failed(&Address::path(parent)))?;
                }
                Local {
                    device: Device::create(path)?,
                    base: 0,
                }
            }
            Endpoint::Drive { path, base } => {
                let destination = local(path, *base, Access::Write)?;
                let first = slot_at(&transfer.destination.address(transfer.bytes))?;
                let slots = u32::try_from(transfer.slots()).expect("planning sized the run in u32");
                require_free(&destination.device, first, slots, transfer.bytes)?;
                destination
            }
        };
        let block = u64::from(destination.device.logical_block.get());
        if let Source::Object { object, .. } = &source {
            if !transfer.part_bytes.is_multiple_of(block) {
                return Err(Error::Refused(Refusal::UnevenParts {
                    object: (*object).clone(),
                }));
            }
        }
        let first_blocks =
            AlignedBuffer::zeroed(&destination.device, transfer.slots() as u64 * block)?;
        Ok(Sides::Write {
            source,
            destination,
            first_blocks,
        })
    }

    /// The device whose logical block every pooled buffer aligns to: the side with the wider block.
    fn aligned(&self) -> &Device {
        match self {
            Sides::Upload { source, .. } => &source.device,
            Sides::Write {
                source: Source::Local(source),
                destination,
                ..
            } => [&source.device, &destination.device]
                .into_iter()
                .max_by_key(|device| device.logical_block)
                .expect("two sides"),
            Sides::Write { destination, .. } => &destination.device,
        }
    }
}

/// One multipart upload in progress.
struct Upload<'plan> {
    s3: &'plan S3,
    object: &'plan ObjectAddress,
    upload_id: String,
    completed: Vec<CompletedPart>,
}

impl<'plan> Upload<'plan> {
    fn create(
        s3: &'plan S3,
        object: &'plan ObjectAddress,
        transfer: &Transfer,
    ) -> Result<Upload<'plan>, Error> {
        let create = s3
            .client
            .create_multipart_upload()
            .bucket(&object.bucket)
            .key(&object.key)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .storage_class(StorageClass::IntelligentTiering);
        let output = s3.block_on(create.send()).map_err(s3_failed(object))?;
        let address = Address::whole(Location::Bucket(object.clone()));
        let upload_id = output
            .upload_id
            .ok_or_else(|| io_failed(&address)(io::ErrorKind::InvalidData.into()))?;
        Ok(Upload {
            s3,
            object,
            upload_id,
            completed: Vec::with_capacity(transfer.parts.get().into()),
        })
    }

    async fn part(&self, lane: Lane) -> Result<CompletedPart, Error> {
        let Lane {
            index,
            length,
            mut buffer,
            ..
        } = lane;
        buffer.set_len(length);
        let output = self
            .s3
            .client
            .upload_part()
            .bucket(&self.object.bucket)
            .key(&self.object.key)
            .upload_id(&self.upload_id)
            .part_number(i32::from(index) + 1)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .body(ByteStream::from(Bytes::from_owner(buffer)))
            .send()
            .await
            .map_err(s3_failed(self.object))?;
        Ok(CompletedPart::builder()
            .part_number(i32::from(index) + 1)
            .set_checksum_sha256(output.checksum_sha256)
            .set_e_tag(output.e_tag)
            .build())
    }

    fn complete(&mut self) -> Result<(), Error> {
        let parts = CompletedMultipartUpload::builder()
            .set_parts(Some(std::mem::take(&mut self.completed)))
            .build();
        let complete = self
            .s3
            .client
            .complete_multipart_upload()
            .bucket(&self.object.bucket)
            .key(&self.object.key)
            .upload_id(&self.upload_id)
            .multipart_upload(parts)
            .if_none_match(NO_OBJECT);
        self.s3
            .block_on(complete.send())
            .map(|_| ())
            .map_err(s3_failed(self.object))
    }

    /// Discards the uploaded parts; a failed abort leaves only parts S3's lifecycle rules expire.
    fn abort(&self) {
        let abort = self
            .s3
            .client
            .abort_multipart_upload()
            .bucket(&self.object.bucket)
            .key(&self.object.key)
            .upload_id(&self.upload_id);
        let _ = self.s3.block_on(abort.send());
    }
}

async fn download_part(s3: &S3, object: &ObjectAddress, lane: &mut Lane) -> Result<(), Error> {
    let uneven = || {
        Error::Refused(Refusal::UnevenParts {
            object: object.clone(),
        })
    };
    let output = s3
        .client
        .get_object()
        .bucket(&object.bucket)
        .key(&object.key)
        .part_number(i32::from(lane.index) + 1)
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .map_err(s3_failed(object))?;
    if output.checksum_sha256().is_none() {
        return Err(Error::Refused(Refusal::Unverifiable {
            object: object.clone(),
        }));
    }
    let address = Address::whole(Location::Bucket(object.clone()));
    let mut body = output.body;
    let mut filled = 0;
    while let Some(chunk) = body
        .try_next()
        .await
        .map_err(|source| io_failed(&address)(source.into()))?
    {
        let end = filled + chunk.len();
        if end > lane.length {
            return Err(uneven());
        }
        lane.buffer.as_mut()[filled..end].copy_from_slice(&chunk);
        filled = end;
    }
    match filled == lane.length {
        true => Ok(()),
        false => Err(uneven()),
    }
}

/// One part of the group in flight and the buffer holding it.
struct Lane {
    index: u16,
    /// The part's length, which hashes and uploads take; the buffer shows it rounded up to whole
    /// blocks, the tail zeroed.
    length: usize,
    buffer: PooledBuffer,
    /// The SHA-256 of the part as written, compared with the one read back.
    digest: sz::Sha256Digest,
    /// What the last parallel step on this part returned.
    outcome: Result<(), Error>,
}

fn buffer_length(bytes: u64) -> usize {
    usize::try_from(bytes).expect("a part fits its pooled buffer")
}

fn lane_bytes(lane: &Lane) -> &[u8] {
    &lane.buffer.as_ref()[..lane.length]
}

/// Reads part `lane.index` of `local` into the lane, the bytes past the part zeroed.
fn read_part(local: &Local, transfer: &Transfer, lane: &mut Lane) -> Result<(), Error> {
    let part = transfer.part(lane.index);
    let filled = local
        .device
        .read_at(lane.buffer.as_mut(), local.base + part.start)?;
    if filled < lane.length {
        let address = local
            .device
            .address(local.base + part.start..local.base + part.end);
        return Err(io_failed(&address)(io::ErrorKind::UnexpectedEof.into()));
    }
    lane.buffer.as_mut()[lane.length..].fill(0);
    Ok(())
}

/// The buffer offset and the slot of every slot's first block part `index` covers.
fn first_blocks_of(transfer: &Transfer, index: u16) -> impl Iterator<Item = (usize, usize)> {
    let part = transfer.part(index);
    (part.start.next_multiple_of(SLOT_BYTES)..part.end)
        .step_by(SLOT_BYTES as usize)
        .map(move |boundary| {
            let slot = usize::try_from(boundary >> SLOT_SHIFT).expect("held slots fit memory");
            (buffer_length(boundary - part.start), slot)
        })
}

/// Refuses part 0 bound for a drive unless it opens what planning took it for: a SafeGraphs file
/// of the transfer's length, or a UStore stretch, which planning pinned to its slot.
fn check_landing(lane: &Lane, transfer: &Transfer) -> Result<(), Error> {
    match slots::classify(lane_bytes(lane), transfer.bytes) {
        Slot::SafeGraphs(graph) if graph.file_bytes == transfer.bytes => Ok(()),
        Slot::UStore(_) => Ok(()),
        _ => Err(Error::Refused(Refusal::NotGraph {
            address: transfer.source.address(transfer.bytes),
        })),
    }
}

/// The NUMA node of the drive at `path`, from the nearest sysfs folder above it that names one.
fn numa_node(path: &Path) -> Option<fu::MemoryDomainId> {
    let name = fs::canonicalize(path).ok()?.file_name()?.to_owned();
    let folder = fs::canonicalize(Path::new(SYSFS_BLOCK).join(name)).ok()?;
    folder.ancestors().find_map(|folder| {
        let node: u16 = fs::read_to_string(folder.join(NUMA_NODE))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(fu::MemoryDomainId(i32::from(node)))
    })
}

/// A pool over the cores nearest `device`, or over every core when no NUMA node is known for it.
fn spawn_threads(topology: &fu::Topology, device: &Device) -> fu::Result<fu::ThreadPool> {
    let node = numa_node(&device.path);
    let local = (0..topology.compute_domains_count()?)
        .map(fu::ComputeDomain)
        .find(|&domain| {
            let memory = topology.local_memory_of(domain);
            let id = memory.and_then(|memory| topology.memory_domain_id_at_index(memory));
            node.is_some_and(|node| id.is_ok_and(|id| id == node))
        });
    match local {
        Some(domain) => fu::ThreadPool::spawn_on(
            topology,
            domain.get(),
            topology.logical_cores_count_in(domain)?,
            fu::CallerExclusivity::Inclusive,
        ),
        None => fu::ThreadPool::spawn(topology, topology.logical_cores_count()?),
    }
}

/// The threads, buffers and hash states every transfer of one plan reuses.
struct Engine {
    threads: fu::ThreadPool,
    /// Outlives `threads`, which were spawned over it.
    _topology: fu::Topology,
    buffers: BufferPool,
    in_flight: NonZeroU16,
    lanes: Vec<Lane>,
    states: Vec<sz::Sha256>,
    digests: Vec<sz::Sha256Digest>,
}

impl Engine {
    /// Sizes the pool for the longest part and the most parts of any transfer: at most one part
    /// per thread, and no more parts than the memory budget holds.
    fn new(transfers: &[Transfer], device: &Device) -> Result<Engine, Error> {
        let address = device.address(0..0);
        let failed = |error: fu::Error| io_failed(&address)(io::Error::other(error));
        let topology = fu::Topology::new().map_err(failed)?;
        let threads = spawn_threads(&topology, device).map_err(failed)?;
        let memory = direct::memory_bytes().map_err(|query| Error::Geometry {
            address: address.clone(),
            query,
        })?;
        let parts = transfers.iter().map(|transfer| transfer.parts.get()).max();
        let part_bytes = transfers.iter().map(|transfer| transfer.part_bytes).max();
        let part_bytes = part_bytes.expect("a plan holds a transfer");
        let in_flight = [
            u64::from(parts.expect("a plan holds a transfer")),
            threads.threads_count() as u64,
            (memory >> MEMORY_SHARE_SHIFT) / part_bytes,
        ];
        let in_flight = in_flight.into_iter().min().expect("three bounds").max(1);
        let in_flight = u16::try_from(in_flight).ok().and_then(NonZeroU16::new);
        let in_flight = in_flight.expect("at most the parts of one transfer and at least one");
        let buffers = BufferPool::new(device, in_flight, part_bytes)?;
        let lanes = usize::from(in_flight.get());
        Ok(Engine {
            threads,
            _topology: topology,
            buffers,
            in_flight,
            lanes: Vec::with_capacity(lanes),
            states: vec![sz::Sha256::new(); lanes],
            digests: vec![sz::Sha256Digest::default(); lanes],
        })
    }

    fn transfer(
        &mut self,
        transfer: &Transfer,
        sides: Sides,
        progress: &mut Progress,
        report: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
    ) -> Result<(), Error> {
        let block = u64::from(sides.aligned().logical_block.get());
        match sides {
            Sides::Upload { source, mut upload } => {
                let moved = self.groups(transfer, block, progress, report, |engine| {
                    engine.dispatch(transfer, |lane| read_part(&source, transfer, lane))?;
                    let parts = engine.lanes.drain(..).map(|lane| upload.part(lane));
                    let completed = upload.s3.block_on(try_join_all(parts))?;
                    upload.completed.extend(completed);
                    Ok(())
                });
                match moved {
                    Ok(()) => upload.complete().inspect_err(|_| upload.abort()),
                    Err(error) => {
                        upload.abort();
                        Err(error)
                    }
                }
            }
            Sides::Write {
                source,
                destination,
                mut first_blocks,
            } => {
                self.groups(transfer, block, progress, report, |engine| {
                    engine.write_group(transfer, &source, &destination, &mut first_blocks)
                })?;
                self.finish(transfer, &destination, &first_blocks)
            }
        }
    }

    /// Runs `group` over every group of up to `in_flight` parts, each in its own lane, and reports
    /// after each; a report that breaks stops the transfer after the group in flight.
    fn groups(
        &mut self,
        transfer: &Transfer,
        block: u64,
        progress: &mut Progress,
        report: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
        mut group: impl FnMut(&mut Engine) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (parts, in_flight) = (transfer.parts.get(), self.in_flight.get());
        for first in (0..parts).step_by(in_flight.into()) {
            let end = parts.min(first.saturating_add(in_flight));
            for index in first..end {
                let mut buffer = self.buffers.take();
                let part = transfer.part(index);
                let length = part.end - part.start;
                buffer.set_len(buffer_length(length.next_multiple_of(block)));
                let length = buffer_length(length);
                buffer.as_mut()[length..].fill(0);
                self.lanes.push(Lane {
                    index,
                    length,
                    buffer,
                    digest: sz::Sha256Digest::default(),
                    outcome: Ok(()),
                });
            }
            let moved = group(self);
            self.lanes.clear();
            moved?;
            progress.bytes_done += transfer.part(end - 1).end - transfer.part(first).start;
            if report(progress).is_break() {
                return Err(Error::Stopped {
                    address: transfer.destination.address(transfer.bytes),
                });
            }
        }
        Ok(())
    }

    /// Reads or downloads the group, holds back each slot's first block, hashes, writes, reads back
    /// and hashes again, refusing a part whose digests differ.
    fn write_group(
        &mut self,
        transfer: &Transfer,
        source: &Source,
        destination: &Local,
        first_blocks: &mut [u8],
    ) -> Result<(), Error> {
        match source {
            Source::Local(source) => {
                self.dispatch(transfer, |lane| read_part(source, transfer, lane))?
            }
            Source::Object { s3, object } => {
                let downloads = self
                    .lanes
                    .iter_mut()
                    .map(|lane| download_part(s3, object, lane));
                s3.block_on(try_join_all(downloads))?;
            }
        }
        // A drive source was checked against its slot map when planned.
        if let (Endpoint::Drive { .. }, Endpoint::File(_) | Endpoint::Object(_)) =
            (&transfer.destination, &transfer.source)
        {
            if let Some(lane) = self.lanes.first().filter(|lane| lane.index == FIRST_PART) {
                check_landing(lane, transfer)?;
            }
        }
        let block = destination.device.logical_block.get() as usize;
        for lane in &mut self.lanes {
            for (at, slot) in first_blocks_of(transfer, lane.index) {
                let bytes = &mut lane.buffer.as_mut()[at..at + block];
                first_blocks[slot * block..][..block].copy_from_slice(bytes);
                bytes.fill(0);
            }
        }
        self.digest();
        for (lane, digest) in self.lanes.iter_mut().zip(&self.digests) {
            lane.digest = *digest;
        }
        self.dispatch(transfer, |lane| {
            let at = destination.base + transfer.part(lane.index).start;
            destination.device.write_at(lane.buffer.as_ref(), at)
        })?;
        self.dispatch(transfer, |lane| read_part(destination, transfer, lane))?;
        self.digest();
        let differing = self
            .lanes
            .iter()
            .zip(&self.digests)
            .find(|(lane, digest)| lane.digest != **digest);
        match differing {
            None => Ok(()),
            Some((lane, _)) => {
                let part = transfer.part(lane.index);
                Err(Error::Mismatch {
                    address: destination
                        .device
                        .address(destination.base + part.start..destination.base + part.end),
                    part: lane.index,
                })
            }
        }
    }

    /// Runs `task` on every lane across the pool, returning the first failure.
    fn dispatch(
        &mut self,
        transfer: &Transfer,
        task: impl Fn(&mut Lane) -> Result<(), Error> + Sync,
    ) -> Result<(), Error> {
        self.threads
            .for_slices_mut(&mut self.lanes, |lanes, _| {
                for lane in lanes {
                    lane.outcome = task(lane);
                }
            })
            .map_err(|error| {
                io_failed(&transfer.destination.address(transfer.bytes))(io::Error::other(error))
            })?;
        self.lanes
            .iter_mut()
            .find_map(|lane| std::mem::replace(&mut lane.outcome, Ok(())).err())
            .map_or(Ok(()), Err)
    }

    /// Hashes every lane's part in one batched pass into `digests`.
    fn digest(&mut self) {
        let lanes = self.lanes.len();
        let states = &mut self.states[..lanes];
        states.fill(sz::Sha256::new());
        sz::sha256_multistate_update_by(states, &self.lanes, lane_bytes)
            .expect("one state per lane");
        sz::sha256_multistate_digest(states, &mut self.digests[..lanes])
            .expect("one digest per lane");
    }

    /// Writes the held first blocks in slot order, the first slot's last once all else is durable,
    /// reads each back, and trims a file to its length.
    fn finish(
        &mut self,
        transfer: &Transfer,
        destination: &Local,
        first_blocks: &[u8],
    ) -> Result<(), Error> {
        let device = &destination.device;
        let block = device.logical_block.get() as usize;
        let held = |slot: usize| &first_blocks[slot * block..][..block];
        let at = |slot: usize| destination.base + ((slot as u64) << SLOT_SHIFT);
        device.sync()?;
        for slot in 1..transfer.slots() {
            device.write_at(held(slot), at(slot))?;
        }
        device.sync()?;
        device.write_at(held(0), at(0))?;
        device.sync()?;
        let mut scratch = self.buffers.take();
        for slot in 0..transfer.slots() {
            let read_back = &mut scratch.as_mut()[..block];
            device.read_at(read_back, at(slot))?;
            if read_back != held(slot) {
                let part = (at(slot) - destination.base) / transfer.part_bytes;
                return Err(Error::Mismatch {
                    address: device.address(at(slot)..at(slot) + block as u64),
                    part: u16::try_from(part).expect("a held block lies inside a part"),
                });
            }
        }
        if let Endpoint::File(_) = transfer.destination {
            device.set_len(transfer.bytes)?;
            device.sync()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    /// Parts of 8 MiB over one slot and a bit, so the copy covers two slots and ends mid-block.
    const BYTES: u64 = SLOT_BYTES + (4 << 20) + 1234;
    const PART_BYTES: u64 = 8 << 20;
    /// Marked bytes inside the first part but past its held first block.
    const MARK: u64 = 1 << 20;

    /// A source file standing in for a drive, sparse but for marked bytes in both slots and at the
    /// end, and an empty destination three slots long.
    fn stand_ins(folder: &Path) -> (PathBuf, PathBuf) {
        let (source, destination) = (folder.join("source.img"), folder.join("destination.img"));
        let file = fs::File::create(&source).unwrap();
        file.set_len(BYTES).unwrap();
        for (at, mark) in [
            (0, 0xA5),
            (SLOT_BYTES, 0x5A),
            (MARK, 0x33),
            (BYTES - 100, 0xC3),
        ] {
            file.write_all_at(&[mark; 100], at).unwrap();
        }
        fs::File::create(&destination)
            .unwrap()
            .set_len(3 * SLOT_BYTES)
            .unwrap();
        (source, destination)
    }

    fn read(path: &Path, range: Range<u64>) -> Vec<u8> {
        let mut bytes = vec![0; (range.end - range.start) as usize];
        fs::File::open(path)
            .unwrap()
            .read_exact_at(&mut bytes, range.start)
            .unwrap();
        bytes
    }

    #[test]
    fn last_part_ends_at_the_last_byte() {
        let transfer = |bytes| {
            Transfer::new(
                Endpoint::File("a".into()),
                Endpoint::File("b".into()),
                bytes,
                PART_BYTES,
            )
        };
        let shorter = transfer(BYTES);
        let last = shorter.parts.get() - 1;
        assert_eq!(u64::from(shorter.parts.get()), BYTES.div_ceil(PART_BYTES));
        assert_eq!(shorter.part(last), u64::from(last) * PART_BYTES..BYTES);
        assert_eq!(
            transfer(4 * PART_BYTES).part(3),
            3 * PART_BYTES..4 * PART_BYTES
        );
    }

    #[test]
    fn writes_every_first_block_last() {
        let folder = tempfile::tempdir().unwrap();
        let (source, destination) = stand_ins(folder.path());
        let block = Device::open(&destination, Access::Read)
            .unwrap()
            .logical_block
            .get() as u64;
        let transfer = Transfer::new(
            Endpoint::Drive {
                path: source.clone(),
                base: 0,
            },
            Endpoint::Drive {
                path: destination.clone(),
                base: SLOT_BYTES,
            },
            BYTES,
            PART_BYTES,
        );
        let plan = CopyPlan {
            source: transfer.source.address(BYTES),
            destination: transfer.destination.address(BYTES),
            transfers: vec![transfer],
            bytes: BYTES,
        };
        let mut groups = 0;
        plan.run(None, |_| {
            groups += 1;
            for slot in [1, 2] {
                let start = slot_extent(slot, 0).start;
                let first = read(&destination, start..start + block);
                assert!(
                    first.iter().all(|&byte| byte == 0),
                    "slot {slot} waits for the last part"
                );
            }
            ControlFlow::Continue(())
        })
        .unwrap();
        assert!(groups > 0);
        let chunk = 64 << 20;
        for start in (0..BYTES).step_by(chunk as usize) {
            let end = BYTES.min(start + chunk);
            assert!(
                read(&source, start..end)
                    == read(&destination, SLOT_BYTES + start..SLOT_BYTES + end),
                "bytes {start}..{end} differ"
            );
        }
        let after = read(
            &destination,
            SLOT_BYTES + BYTES..SLOT_BYTES + BYTES.next_multiple_of(block),
        );
        assert!(
            after.iter().all(|&byte| byte == 0),
            "the copy ends at its last byte"
        );
    }

    #[test]
    fn a_file_copied_into_a_folder_and_stopped_leaves_its_slots_free() {
        let folder = tempfile::tempdir().unwrap();
        let (source, _) = stand_ins(folder.path());
        let into = folder.path().join("into");
        fs::create_dir(&into).unwrap();
        let plan = CopyPlan::new(&Address::path(&source), &Address::path(&into), None).unwrap();
        let target = into.join("source.img");
        assert_eq!(
            plan.destination,
            Address::path(&target),
            "the copy takes the file's name"
        );
        let stopped = plan.run(None, |_| ControlFlow::Break(()));
        assert!(matches!(stopped, Err(Error::Stopped { .. })));
        let block = Device::open(&target, Access::Read)
            .unwrap()
            .logical_block
            .get() as u64;
        assert!(
            read(&target, 0..block).iter().all(|&byte| byte == 0),
            "the first block waits"
        );
        assert_eq!(read(&target, MARK..MARK + 100), [0x33; 100]);
    }
}
