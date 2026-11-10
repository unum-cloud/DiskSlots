//! The `list` verb: every place, the datasets it holds and their sizes, which datasets are copies of
//! one another, and the health of each drive.
//!
//! Copies are matched without reading data: a SafeGraphs file or slot run by the SHA-256 of its
//! header bytes and its length, a UStore database by its ID, and a folder or S3 prefix by its
//! lowercase dash name, compared from its last component back as far as the shorter name goes.

use std::fs::{self, File};
use std::io::{self, Read};
use std::num::NonZeroU32;
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::direct::{self, Access, Device};
use crate::places::{rounded, size, Address, Location, ObjectAddress, Place, S3};
use crate::slots::{self, slot_extent, Graph, Slot, SpanOwner, SLOT_BYTES};
use crate::{Error, SLOT_SHIFT};

pub const SAFEGRAPHS_SUFFIX: &str = ".safegraphs";
const USTORE_MANIFEST_SUFFIX: &str = ".ustore.json";
const PARQUET_SUFFIX: &str = ".parquet";
/// The folder a Parquet subset keeps its files in.
const PARQUET_FOLDER: &str = "parquet";
/// The folder under the home folder that every survey includes.
const DATA_FOLDER: &str = "Data";

/// The magic and the header length opening a SafeGraphs file, which say where its header ends.
const GRAPH_LENGTH_PREFIX_BYTES: u64 = (safegraphs::MAGIC.len() + size_of::<u64>()) as u64;

/// The manifest fields naming a UStore database and the generation in force.
const MANIFEST_ID: &str = "id";
const MANIFEST_GENERATION: &str = "generation";
const HEX_RADIX: u32 = 16;
const HEX_DIGIT_BITS: u32 = 4;
/// The bytes of a UUID's first group in its 8-4-4-4-12 form, which names a database briefly.
const UUID_FIRST_GROUP_BYTES: usize = 4;

/// The units a count takes, each `COUNT_UNIT_BASE` times the one before.
const COUNT_UNITS: &[&str] = &["", "K", "M", "B", "T"];
const COUNT_UNIT_BASE: u64 = 1000;
/// The Kelvin reading of 0 °C, as the NVMe SMART log reports temperatures in whole Kelvin.
const KELVIN_AT_ZERO_CELSIUS: i32 = 273;
/// `TASK_COMM_LEN` in `<linux/sched.h>`: a command name and its terminating NUL.
const COMMAND_BYTES: usize = 16;
/// Bytes 24 through 63 of Identify Controller, the model number in ASCII padded with spaces.
const IDENTIFY_MODEL_NUMBER: Range<usize> = 24..64;
const MODEL_NUMBER_BYTES: usize = IDENTIFY_MODEL_NUMBER.end - IDENTIFY_MODEL_NUMBER.start;

const FREE_MARK: char = '·';
const GRAPH_MARK: char = '█';
const DATABASE_MARK: char = '▓';
const FOREIGN_MARK: char = '▒';
const FILES_MARK: char = '░';

pub use platform::holders;

/// Everything one refresh found, immutable until the next refresh.
pub struct Survey {
    pub places: Vec<Place>,
    /// What each place's medium reports.
    pub media: Vec<Medium>,
    /// Every dataset, kept together by place in place order.
    pub datasets: Vec<Dataset>,
    /// The first dataset sharing each dataset's identity, itself when none comes earlier.
    pub copy_of: Vec<u32>,
}

/// One dataset in one place.
pub struct Dataset {
    /// The index of its place in [`Survey::places`].
    pub place: u32,
    /// Its path under the place's own: a file or folder name, or the rest of an S3 key past the
    /// place's prefix; empty where the dataset is a slot run.
    pub name: String,
    /// The bytes it covers: on a drive, from its first slot; elsewhere, from zero.
    pub extent: Range<u64>,
    pub owner: Owner,
}

/// What wrote a dataset and what its header, manifest or files say, which decides how it is read,
/// what it is a copy of, and whether its slots may be freed.
// A survey holds few datasets, so their SafeGraphs headers stay inline rather than boxed.
#[allow(clippy::large_enum_variant)]
pub enum Owner {
    SafeGraphs(Graph),
    /// A UStore database whose manifest stands at `generation`.
    UStore {
        database: [u8; 16],
        generation: u64,
    },
    /// A UStore data directory holding no manifest, `medium` among the database's.
    Directory {
        database: [u8; 16],
        medium: u32,
    },
    Parquet {
        files: u64,
    },
    Files {
        files: u64,
    },
    /// Bytes no known format wrote, or a header or manifest that does not parse.
    Foreign,
    /// The header or manifest could not be read.
    Unreadable(Error),
}

/// What a place sits on, and what it reports beyond its datasets.
pub enum Medium {
    /// A folder or a file on a filesystem of `bytes`.
    Filesystem {
        bytes: u64,
    },
    Drive(Drive),
    /// An S3 prefix, which no capacity bounds.
    Bucket,
    /// A controller bound to `vfio-pci`, and the processes holding its IOMMU group.
    Bound {
        holders: Vec<Holder>,
    },
    /// The survey of the place failed.
    Unreachable(Error),
}

/// A raw drive, its slots and its health.
pub struct Drive {
    pub bytes: u64,
    pub slots: u32,
    pub health: Result<Health, Error>,
    /// Processes other than this one holding the drive open.
    pub holders: Vec<Holder>,
}

/// What the NVMe admin commands report about a drive.
pub struct Health {
    /// As Identify Controller reports it, padded with spaces.
    pub model: [u8; MODEL_NUMBER_BYTES],
    /// The logical block of the namespace's LBA format in force.
    pub logical_block: NonZeroU32,
    /// The SMART "percentage used": the share of rated endurance spent, which may pass 100.
    pub endurance_used: u8,
    /// In whole Kelvin.
    pub temperature: u16,
}

/// A process holding a drive or controller open.
pub struct Holder {
    pub pid: u32,
    /// Its command name, NUL-padded.
    pub command: [u8; COMMAND_BYTES],
}

/// One file under a place, by its path relative to the place with `/` between components.
pub struct Entry {
    pub name: String,
    pub bytes: u64,
}

/// Surveys the places at `addresses`, `~/Data`, every NVMe namespace with no mounted filesystem and
/// every NVMe controller bound to `vfio-pci`, each once.
pub fn survey(addresses: &[Address], endpoint: Option<&str>) -> Result<Survey, Error> {
    let s3 = S3::for_addresses(addresses, endpoint)?;
    let named: Vec<Place> = addresses.iter().map(Place::of).collect::<Result<_, _>>()?;
    let data = std::env::var_os("HOME").map(|home| Path::new(&home).join(DATA_FOLDER));
    let defaults = data
        .filter(|data| data.is_dir())
        .map(Place::Folder)
        .into_iter()
        .chain(platform::raw_namespaces().into_iter().map(Place::Drive))
        .chain(platform::bound_controllers().into_iter().map(Place::Bound));
    let mut places: Vec<Place> = Vec::new();
    for place in named.into_iter().chain(defaults) {
        if !places.contains(&place) {
            places.push(place);
        }
    }
    let mut datasets = Vec::new();
    let mut media = Vec::with_capacity(places.len());
    for (position, place) in places.iter().enumerate() {
        let first = datasets.len();
        let medium = survey_place(place, index(position), s3.as_ref(), &mut datasets);
        media.push(medium.unwrap_or_else(|error| {
            datasets.truncate(first);
            Medium::Unreachable(error)
        }));
    }
    let copy_of = match_copies(&datasets);
    Ok(Survey {
        places,
        media,
        datasets,
        copy_of,
    })
}

/// The `list` verb: one line per place, then one per dataset it holds.
pub fn list(places: &[Address], endpoint: Option<&str>) -> Result<(), Error> {
    let survey = survey(places, endpoint)?;
    println!("{}", survey.summary());
    for place in 0..index(survey.places.len()) {
        match &survey.media[place as usize] {
            Medium::Drive(drive) => println!("{}  {}", survey.place_line(place), drive_line(drive)),
            _ => println!("{}", survey.place_line(place)),
        }
        for row in survey.dataset_rows(survey.datasets_of(place)) {
            println!("  {row}");
        }
    }
    Ok(())
}

/// The index a survey gives the item at `position`.
pub fn index(position: usize) -> u32 {
    u32::try_from(position).expect("a survey holds fewer than 2^32 places and datasets")
}

fn survey_place(
    place: &Place,
    position: u32,
    s3: Option<&S3>,
    datasets: &mut Vec<Dataset>,
) -> Result<Medium, Error> {
    let io_failed = |path: &Path| {
        let address = Address::path(path);
        move |source| Error::Io { address, source }
    };
    let (location, entries, medium) = match place {
        Place::Drive(path) => return survey_drive(path, position, datasets),
        Place::Bound(device) => {
            let holders = platform::bound_holders(device);
            return Ok(Medium::Bound { holders });
        }
        Place::Bucket(object) => {
            let s3 = s3.expect("a survey with a bucket connects to S3 first");
            let entries = list_objects(s3, object)?;
            (Location::Bucket(object.clone()), entries, Medium::Bucket)
        }
        Place::Folder(path) => {
            let mut entries = Vec::new();
            walk(path, "", &mut Vec::new(), &mut entries)?;
            let medium = Medium::Filesystem {
                bytes: direct::filesystem_bytes(path)?,
            };
            (Location::Path(path.clone()), entries, medium)
        }
        // A file is one dataset, named under its folder.
        Place::File(path) => {
            let entry = Entry {
                name: Location::Path(path.clone()).name(),
                bytes: fs::metadata(path).map_err(io_failed(path))?.len(),
            };
            let folder = path.parent().unwrap_or(Path::new(""));
            let medium = Medium::Filesystem {
                bytes: direct::filesystem_bytes(path)?,
            };
            (Location::Path(folder.to_path_buf()), vec![entry], medium)
        }
    };
    group(&entries, &location, s3, position, datasets);
    Ok(medium)
}

/// Adds every file under `folder` to `entries`, named under `relative`, following symbolic links
/// as dataset folders use them, skipping hidden entries, and never entering a folder twice along
/// one path.
pub fn walk(
    folder: &Path,
    relative: &str,
    ancestors: &mut Vec<(u64, u64)>,
    entries: &mut Vec<Entry>,
) -> Result<(), Error> {
    let failed = |path: &Path| {
        let address = Address::path(path);
        move |source| Error::Io { address, source }
    };
    for entry in fs::read_dir(folder).map_err(failed(folder))? {
        let entry = entry.map_err(failed(folder))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            // A link to nothing holds no bytes.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(failed(&path)(source)),
        };
        let name = match relative {
            "" => file_name.into_owned(),
            _ => format!("{relative}/{file_name}"),
        };
        let identity = (metadata.dev(), metadata.ino());
        if !metadata.is_dir() {
            entries.push(Entry {
                name,
                bytes: metadata.len(),
            });
        } else if !ancestors.contains(&identity) {
            ancestors.push(identity);
            walk(&path, &name, ancestors, entries)?;
            ancestors.pop();
        }
    }
    Ok(())
}

/// The object `prefix` names itself, as an empty name, and every object under it as a folder,
/// named relative to it; folder markers are left out.
pub fn list_objects(s3: &S3, prefix: &ObjectAddress) -> Result<Vec<Entry>, Error> {
    let folder = !prefix.key.is_empty() && !prefix.key.ends_with('/');
    let mut pages = s3
        .client
        .list_objects_v2()
        .bucket(&prefix.bucket)
        .prefix(&prefix.key)
        .into_paginator()
        .send();
    let mut entries = Vec::new();
    while let Some(page) = s3.block_on(pages.next()) {
        let page = page.map_err(crate::places::s3_failed(prefix))?;
        for object in page.contents() {
            let (Some(key), Some(size)) = (object.key(), object.size()) else {
                continue;
            };
            let name = &key[prefix.key.len()..];
            let name = match name.strip_prefix('/') {
                Some(name) if folder => name,
                // A prefix without its trailing slash names its own object and the folder under it.
                _ if folder && !name.is_empty() => continue,
                _ => name,
            };
            if !key.ends_with('/') {
                entries.push(Entry {
                    name: name.to_owned(),
                    bytes: u64::try_from(size).unwrap_or_default(),
                });
            }
        }
    }
    Ok(entries)
}

/// The first `bytes` bytes of `entry` under `location`.
fn read_prefix(
    location: &Location,
    s3: Option<&S3>,
    entry: &Entry,
    bytes: u64,
) -> Result<Vec<u8>, Error> {
    match location.join(&entry.name) {
        Location::Path(path) => {
            let mut prefix = Vec::new();
            File::open(&path)
                .and_then(|file| file.take(bytes).read_to_end(&mut prefix))
                .map_err(|source| Error::Io {
                    address: Address::path(&path),
                    source,
                })?;
            Ok(prefix)
        }
        Location::Bucket(object) => s3
            .expect("a survey with a bucket connects to S3 first")
            .prefix(&object, bytes),
    }
}

/// The folder holding `name`, empty at the top.
fn parent(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Whether the folder `root` holds `name` or is it; the empty root holds everything.
fn holds(root: &str, name: &str) -> bool {
    root.is_empty()
        || name
            .strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// How the files of one gathered dataset are read.
enum Kind<'entries> {
    Manifest(&'entries Entry),
    Parquet,
    Files,
}

/// The UStore database or Parquet subset an entry itself declares, by the folder it roots.
fn declared_root(entry: &Entry) -> Option<(&str, Kind<'_>)> {
    let name = entry.name.as_str();
    if name.ends_with(USTORE_MANIFEST_SUFFIX) {
        return Some((parent(name), Kind::Manifest(entry)));
    }
    let mut start = 0;
    for component in name.split('/') {
        let end = start + component.len();
        if component == PARQUET_FOLDER && end < name.len() {
            return Some((name[..start].trim_end_matches('/'), Kind::Parquet));
        }
        start = end + '/'.len_utf8();
    }
    name.ends_with(PARQUET_SUFFIX)
        .then(|| (parent(name), Kind::Parquet))
}

/// One dataset gathered from entries: a database, a Parquet subset or a top-level file or folder.
struct Group<'entries> {
    name: &'entries str,
    kind: Kind<'entries>,
    bytes: u64,
    files: u64,
}

/// Turns the files under one place into datasets: each SafeGraphs file is one, each folder with a
/// UStore manifest or Parquet files is one, and every other file belongs to its top-level entry.
fn group(
    entries: &[Entry],
    location: &Location,
    s3: Option<&S3>,
    place: u32,
    datasets: &mut Vec<Dataset>,
) {
    let read = |entry: &Entry, bytes| read_prefix(location, s3, entry, bytes);
    let (graphs, files): (Vec<&Entry>, Vec<&Entry>) = entries
        .iter()
        .partition(|entry| entry.name.ends_with(SAFEGRAPHS_SUFFIX));
    let mut groups: Vec<Group> = Vec::new();
    for (root, kind) in files.iter().copied().filter_map(declared_root) {
        match groups.iter_mut().find(|group| group.name == root) {
            Some(group) if matches!(kind, Kind::Manifest(_)) => group.kind = kind,
            Some(_) => {}
            None => groups.push(Group {
                name: root,
                kind,
                bytes: 0,
                files: 0,
            }),
        }
    }
    let roots = groups.len();
    for entry in files {
        let deepest = (0..roots)
            .filter(|&position| holds(groups[position].name, &entry.name))
            .max_by_key(|&position| groups[position].name.len());
        let top = entry.name.split('/').next().unwrap_or_default();
        let position = deepest
            .or_else(|| (roots..groups.len()).find(|&position| groups[position].name == top))
            .unwrap_or_else(|| {
                groups.push(Group {
                    name: top,
                    kind: Kind::Files,
                    bytes: 0,
                    files: 0,
                });
                groups.len() - 1
            });
        groups[position].bytes += entry.bytes;
        groups[position].files += 1;
    }

    let first = datasets.len();
    for group in groups {
        let owner = match group.kind {
            Kind::Manifest(manifest) => read_manifest(read(manifest, manifest.bytes)),
            Kind::Parquet => Owner::Parquet { files: group.files },
            Kind::Files => Owner::Files { files: group.files },
        };
        datasets.push(Dataset {
            place,
            name: group.name.to_owned(),
            extent: 0..group.bytes,
            owner,
        });
    }
    for entry in graphs {
        datasets.push(Dataset {
            place,
            name: entry.name.clone(),
            extent: 0..entry.bytes,
            owner: read_graph(|bytes| read(entry, bytes)),
        });
    }
    datasets[first..].sort_by(|first, second| first.name.cmp(&second.name));
}

/// A SafeGraphs file's header, read from its first bytes alone; a header that does not end inside
/// one slot or does not parse makes the file foreign.
fn read_graph(read: impl Fn(u64) -> Result<Vec<u8>, Error>) -> Owner {
    let header = read(GRAPH_LENGTH_PREFIX_BYTES).and_then(|prefix| {
        match safegraphs::GraphHeader::segment_start_of(&prefix) {
            Ok(end) if end as u64 <= SLOT_BYTES => read(end as u64),
            _ => Ok(prefix),
        }
    });
    match header.map(|header| slots::classify(&header, u64::MAX)) {
        Ok(Slot::SafeGraphs(graph)) => Owner::SafeGraphs(graph),
        Ok(_) => Owner::Foreign,
        Err(error) => Owner::Unreadable(error),
    }
}

/// A UStore database's ID and generation, from its manifest; a manifest without them makes the
/// database foreign.
fn read_manifest(text: Result<Vec<u8>, Error>) -> Owner {
    let text = match text {
        Ok(text) => text,
        Err(error) => return Owner::Unreadable(error),
    };
    let parsed = jsontape::parse(&text).ok();
    let field = |name| parsed.as_ref().and_then(|manifest| manifest.get(name));
    let database = field(MANIFEST_ID)
        .and_then(|id| id.as_str())
        .and_then(uuid_bytes);
    match (
        database,
        field(MANIFEST_GENERATION).and_then(|value| value.as_u64()),
    ) {
        (Some(database), Some(generation)) => Owner::UStore {
            database,
            generation,
        },
        _ => Owner::Foreign,
    }
}

/// The bytes of a UUID written in hex, dashes anywhere.
fn uuid_bytes<const BYTES: usize>(text: &str) -> Option<[u8; BYTES]> {
    let mut bytes = [0; BYTES];
    let mut digits = text.chars().filter(|&character| character != '-');
    for byte in &mut bytes {
        let high = digits.next()?.to_digit(HEX_RADIX)?;
        let low = digits.next()?.to_digit(HEX_RADIX)?;
        *byte = u8::try_from(high << HEX_DIGIT_BITS | low).ok()?;
    }
    digits.next().is_none().then_some(bytes)
}

/// The datasets on a raw drive, from its slot map, and the drive's slots and health.
fn survey_drive(path: &Path, place: u32, datasets: &mut Vec<Dataset>) -> Result<Medium, Error> {
    let device = Device::open(path, Access::Read)?;
    let map = slots::map(&device)?;
    for span in map.spans {
        let extent = slot_extent(span.first_slot, span.slots);
        let (extent, owner) = match span.owner {
            SpanOwner::SafeGraphs(graph) => (
                extent.start..extent.start + graph.file_bytes,
                Owner::SafeGraphs(graph),
            ),
            SpanOwner::UStore { block, .. } if block.manifest_slot() == Some(span.first_slot) => {
                let (database, generation) = (block.database, block.manifest_generation);
                (
                    extent,
                    Owner::UStore {
                        database,
                        generation,
                    },
                )
            }
            SpanOwner::UStore { block, .. } => {
                let (database, medium) = (block.database, block.medium);
                (extent, Owner::Directory { database, medium })
            }
            SpanOwner::Foreign => (extent, Owner::Foreign),
        };
        datasets.push(Dataset {
            place,
            name: String::new(),
            extent,
            owner,
        });
    }
    Ok(Medium::Drive(Drive {
        bytes: device.bytes,
        slots: map.slots,
        health: platform::health(&device),
        holders: platform::holders(path),
    }))
}

/// For every dataset, the first one sharing its identity.
fn match_copies(datasets: &[Dataset]) -> Vec<u32> {
    let mut copy_of: Vec<u32> = Vec::with_capacity(datasets.len());
    for (position, dataset) in datasets.iter().enumerate() {
        let first = datasets[..position]
            .iter()
            .position(|earlier| same_dataset(earlier, dataset));
        copy_of.push(first.map_or(index(position), |first| copy_of[first]));
    }
    copy_of
}

/// Whether two datasets are copies of one another; a data directory without the manifest is a part
/// of its database, never a copy of it.
fn same_dataset(first: &Dataset, second: &Dataset) -> bool {
    match (&first.owner, &second.owner) {
        (Owner::SafeGraphs(first), Owner::SafeGraphs(second)) => {
            first.header_digest == second.header_digest && first.file_bytes == second.file_bytes
        }
        (
            Owner::UStore {
                database: first, ..
            },
            Owner::UStore {
                database: second, ..
            },
        ) => first == second,
        (
            Owner::Parquet { .. } | Owner::Files { .. },
            Owner::Parquet { .. } | Owner::Files { .. },
        ) => same_name(&first.name, &second.name),
        _ => false,
    }
}

/// Whether two dataset names match from their last component back, as far as the shorter goes,
/// ignoring case and taking spaces and underscores for dashes.
fn same_name(first: &str, second: &str) -> bool {
    let (first, second) = (first.trim_matches('/'), second.trim_matches('/'));
    let dashed = |character: char| match character {
        ' ' | '_' => '-',
        other => other.to_ascii_lowercase(),
    };
    !first.is_empty()
        && !second.is_empty()
        && first
            .rsplit('/')
            .zip(second.rsplit('/'))
            .all(|(first, second)| first.chars().map(dashed).eq(second.chars().map(dashed)))
}

/// Bytes a dataset covers.
pub fn dataset_bytes(dataset: &Dataset) -> u64 {
    dataset.extent.end - dataset.extent.start
}

/// The slots `extent` touches on a drive.
pub fn slot_range(extent: &Range<u64>) -> Range<u32> {
    let slot = |bytes: u64| u32::try_from(bytes).unwrap_or(u32::MAX);
    slot(extent.start >> SLOT_SHIFT)..slot(extent.end.div_ceil(SLOT_BYTES))
}

impl Owner {
    pub fn name(&self) -> &'static str {
        match self {
            Owner::SafeGraphs(_) => "SafeGraphs",
            Owner::UStore { .. } | Owner::Directory { .. } => "UStore",
            Owner::Parquet { .. } => "Parquet",
            Owner::Files { .. } => "files",
            Owner::Foreign | Owner::Unreadable(_) => "foreign",
        }
    }

    /// The character marking its slots on a slot bar.
    pub fn mark(&self) -> char {
        match self {
            Owner::SafeGraphs(_) => GRAPH_MARK,
            Owner::UStore { .. } | Owner::Directory { .. } => DATABASE_MARK,
            Owner::Parquet { .. } | Owner::Files { .. } => FILES_MARK,
            Owner::Foreign | Owner::Unreadable(_) => FOREIGN_MARK,
        }
    }

    /// What its header, manifest or files say.
    fn facts(&self) -> String {
        match self {
            Owner::SafeGraphs(graph) => {
                let field = |column: &str, field: &str| {
                    let column = graph.header.header.get(column)?;
                    column.get(field).and_then(|value| value.as_str())
                };
                let mut facts = count(graph.header.key.rows() as u64);
                if let Some(dtype) = field("value", "dtype") {
                    facts += &format!(" × {dtype}");
                }
                if let Some(metric) = field("__metadata__", "metric") {
                    facts += &format!(", {metric}");
                }
                facts
            }
            Owner::UStore { generation, .. } => format!("generation {generation}"),
            Owner::Directory { medium, .. } => format!("data directory {medium}"),
            Owner::Parquet { files } | Owner::Files { files } => plural(*files, "file"),
            Owner::Foreign => String::new(),
            Owner::Unreadable(error) => error.to_string(),
        }
    }
}

/// A count to three significant digits in decimal units, such as "1.2B".
fn count(value: u64) -> String {
    let unit = (0..COUNT_UNITS.len())
        .rev()
        .find(|&unit| value >= COUNT_UNIT_BASE.pow(unit as u32))
        .unwrap_or(0);
    let scale = COUNT_UNIT_BASE.pow(unit as u32);
    let value = rounded(value as f64 / scale as f64, value / scale, unit);
    format!("{value}{}", COUNT_UNITS[unit])
}

/// A count and its noun, plural unless the count is one.
pub fn plural(count: u64, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        count => format!("{count} {noun}s"),
    }
}

/// " · held by" and the processes holding a device open, or nothing when none does.
fn held_by(holders: &[Holder]) -> String {
    let mut text = String::new();
    for (position, holder) in holders.iter().enumerate() {
        let length = holder
            .command
            .iter()
            .position(|&byte| byte == 0 || byte == b'\n');
        let command = String::from_utf8_lossy(&holder.command[..length.unwrap_or(COMMAND_BYTES)]);
        let lead = if position == 0 { " · held by " } else { ", " };
        text += &format!("{lead}{command} {}", holder.pid);
    }
    text
}

/// A drive's model, logical block, wear, temperature and holders.
pub fn drive_line(drive: &Drive) -> String {
    let health = match &drive.health {
        Ok(health) => format!(
            "{} · {} blocks · {}% used · {} °C",
            String::from_utf8_lossy(&health.model).trim_end_matches([' ', '\0']),
            size(health.logical_block.get().into()),
            health.endurance_used,
            i32::from(health.temperature) - KELVIN_AT_ZERO_CELSIUS
        ),
        Err(error) => error.to_string(),
    };
    health + &held_by(&drive.holders)
}

/// A drive's slots drawn `columns` wide, each column marking what holds the slots it covers.
pub fn slot_bar(
    slots: u32,
    columns: u32,
    runs: impl Iterator<Item = (Range<u32>, char)> + Clone,
) -> String {
    let (slots, columns) = (u64::from(slots), u64::from(columns));
    (0..columns * u64::from(slots != 0))
        .map(|column| {
            let first = column * slots / columns;
            let end = ((column + 1) * slots / columns).max(first + 1);
            runs.clone()
                .find(|(run, _)| u64::from(run.start) < end && first < u64::from(run.end))
                .map_or(FREE_MARK, |(_, mark)| mark)
        })
        .collect()
}

impl Survey {
    /// The datasets of place `place`.
    pub fn datasets_of(&self, place: u32) -> Range<u32> {
        let start = self
            .datasets
            .partition_point(|dataset| dataset.place < place);
        let end = self
            .datasets
            .partition_point(|dataset| dataset.place <= place);
        index(start)..index(end)
    }

    pub fn dataset(&self, dataset: u32) -> &Dataset {
        &self.datasets[dataset as usize]
    }

    /// The address of `extent` in place `place`: the extent itself on a drive, the place
    /// elsewhere, and a file's folder, which its one dataset is named under.
    pub fn address(&self, place: u32, extent: Range<u64>) -> Address {
        match &self.places[place as usize] {
            Place::Drive(path) => Address::extent(path, extent),
            Place::File(path) => Address::path(path.parent().unwrap_or(Path::new(""))),
            Place::Folder(path) | Place::Bound(path) => Address::path(path),
            Place::Bucket(object) => Address::whole(Location::Bucket(object.clone())),
        }
    }

    /// The address `copy` and `free` take for dataset `dataset`.
    pub fn dataset_address(&self, dataset: u32) -> Address {
        let found = self.dataset(dataset);
        let address = self.address(found.place, found.extent.clone());
        Address {
            location: address.location.join(&found.name),
            ..address
        }
    }

    /// Bytes the place holds: on a drive, its slots that are not free.
    fn used_bytes(&self, place: u32) -> u64 {
        let datasets = self.datasets_of(place).map(|dataset| self.dataset(dataset));
        match &self.media[place as usize] {
            Medium::Drive(_) => datasets
                .map(|dataset| (slot_range(&dataset.extent).len() as u64) << SLOT_SHIFT)
                .sum(),
            _ => datasets.map(dataset_bytes).sum(),
        }
    }

    /// The other places holding a copy of `dataset`, each once, in place order.
    pub fn copies(&self, dataset: u32) -> Vec<u32> {
        let place = self.dataset(dataset).place;
        let mut places: Vec<u32> = (0..index(self.copy_of.len()))
            .filter(|&other| self.copy_of[other as usize] == self.copy_of[dataset as usize])
            .map(|other| self.dataset(other).place)
            .filter(|&other| other != place)
            .collect();
        places.dedup();
        places
    }

    /// "also" and the other places holding a copy of `dataset`, or "only here".
    pub fn copies_line(&self, dataset: u32) -> String {
        let copies = self.copies(dataset);
        let names: Vec<String> = copies
            .iter()
            .map(|&place| self.places[place as usize].to_string())
            .collect();
        match names.is_empty() {
            true => "only here".to_owned(),
            false => format!("also {}", names.join(", ")),
        }
    }

    /// How many places, and what they hold in all.
    pub fn summary(&self) -> String {
        let places = index(self.places.len());
        let bytes: u64 = (0..places).map(|place| self.used_bytes(place)).sum();
        let datasets = plural(self.datasets.len() as u64, "dataset");
        format!(
            "{} · {} in {datasets}",
            plural(places.into(), "place"),
            size(bytes)
        )
    }

    /// A place and how much it holds of how much it takes.
    pub fn place_line(&self, place: u32) -> String {
        let location = &self.places[place as usize];
        let used = size(self.used_bytes(place));
        match &self.media[place as usize] {
            Medium::Filesystem { bytes } => format!("{location}  {used} of {}", size(*bytes)),
            Medium::Drive(drive) => format!("{location}  {used} of {}", size(drive.bytes)),
            Medium::Bucket => format!("{location}  {used}"),
            Medium::Bound { holders } => format!("{location}{}", held_by(holders)),
            Medium::Unreachable(error) => format!("{location}  {error}"),
        }
    }

    /// A dataset's name: its own, else a named copy's, else a database's ID, its first slot or its
    /// place's name.
    pub fn dataset_name(&self, dataset: u32) -> String {
        let found = self.dataset(dataset);
        let class = self.copy_of[dataset as usize];
        let named = std::iter::once(dataset)
            .chain(
                (0..index(self.copy_of.len()))
                    .filter(|&other| self.copy_of[other as usize] == class),
            )
            .map(|other| &self.dataset(other).name)
            .find(|name| !name.trim_matches('/').is_empty());
        if let Some(name) = named {
            return name.trim_start_matches('/').to_owned();
        }
        match (&self.places[found.place as usize], &found.owner) {
            (
                Place::Drive(_),
                Owner::UStore { database, .. } | Owner::Directory { database, .. },
            ) => {
                let group = &database[..UUID_FIRST_GROUP_BYTES];
                group
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
                    + "…"
            }
            (Place::Drive(_), _) => format!("slot {}", slot_range(&found.extent).start),
            (Place::Folder(path) | Place::File(path) | Place::Bound(path), _) => {
                match path.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => path.display().to_string(),
                }
            }
            (Place::Bucket(object), _) => Location::Bucket(object.clone()).name(),
        }
    }

    /// One aligned row per dataset: its name, owner, size, facts and the other places holding a
    /// copy.
    pub fn dataset_rows(&self, datasets: impl Iterator<Item = u32>) -> Vec<String> {
        let cells: Vec<[String; 5]> = datasets
            .map(|dataset| {
                let found = self.dataset(dataset);
                [
                    self.dataset_name(dataset),
                    found.owner.name().to_owned(),
                    size(dataset_bytes(found)),
                    found.owner.facts(),
                    self.copies_line(dataset),
                ]
            })
            .collect();
        let mut widths = [0; 4];
        for row in &cells {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let [name, owner, size, facts] = widths;
        cells
            .iter()
            .map(|[n, o, s, f, copies]| {
                format!("{n:<name$}  {o:<owner$}  {s:>size$}  {f:<facts$}  {copies}")
            })
            .collect()
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{Health, Holder};
    use crate::direct::Device;
    use crate::Error;

    pub fn holders(_drive: &Path) -> Vec<Holder> {
        Vec::new()
    }

    pub fn health(device: &Device) -> Result<Health, Error> {
        Err(Error::Io {
            address: device.address(0..0),
            source: io::ErrorKind::Unsupported.into(),
        })
    }

    pub fn raw_namespaces() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn bound_controllers() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn bound_holders(_device: &Path) -> Vec<Holder> {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use std::fs;
    use std::io;
    use std::num::NonZeroU32;
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};

    use super::{Health, Holder, COMMAND_BYTES, IDENTIFY_MODEL_NUMBER};
    use crate::direct::Device;
    use crate::Error;

    const SYS_BLOCK: &str = "/sys/class/block";
    const DEV: &str = "/dev";
    const PROC: &str = "/proc";
    const VFIO_PCI_DRIVER: &str = "/sys/bus/pci/drivers/vfio-pci";
    const VFIO_GROUPS: &str = "/dev/vfio";
    /// The PCI class of an NVM Express controller, mass storage `01` and non-volatile memory `08`.
    const PCI_CLASS_NVME: &str = "0x0108";
    /// The device field of `/proc/self/mountinfo`, `major:minor`, counted from zero.
    const MOUNTINFO_DEVICE_FIELD: usize = 2;

    /// `_IO('N', 0x40)` in `<linux/nvme_ioctl.h>`, returning the namespace ID of the node.
    const NVME_IOCTL_ID: libc::Ioctl = 0x4E40;
    /// `_IOWR('N', 0x41, struct nvme_admin_cmd)` in `<linux/nvme_ioctl.h>`.
    const NVME_IOCTL_ADMIN_CMD: libc::Ioctl = 0xC048_4E41_u32 as libc::Ioctl;

    const OPCODE_GET_LOG_PAGE: u8 = 0x02;
    const OPCODE_IDENTIFY: u8 = 0x06;
    const IDENTIFY_NAMESPACE: u32 = 0x00;
    const IDENTIFY_CONTROLLER: u32 = 0x01;
    const IDENTIFY_BYTES: usize = 4096;
    /// Byte 25 of Identify Namespace: the LBA formats it lists, less one.
    const IDENTIFY_FORMATS_AT: usize = 25;
    /// Byte 26 of Identify Namespace: the LBA format in force.
    const IDENTIFY_FORMATTED_AT: usize = 26;
    /// Byte 128 of Identify Namespace: the LBA format table, 4 bytes per format.
    const IDENTIFY_FORMATS_TABLE_AT: usize = 128;
    const LBA_FORMAT_BYTES: usize = 4;
    /// Byte 2 of an LBA format: its data size as a power of two.
    const LBA_FORMAT_SHIFT_AT: usize = 2;
    /// The smallest data size shift the specification allows, 512 bytes.
    const LBA_FORMAT_SHIFT_MIN: u8 = 9;
    /// Bits 3:0 of the formatted LBA size field hold the format's low bits.
    const FORMATTED_LOW_MASK: u8 = 0x0F;
    /// Bits 6:5 hold its high bits once a namespace lists more than 16 formats.
    const FORMATTED_HIGH_MASK: u8 = 0x60;
    const FORMATTED_HIGH_SHIFT: u32 = 1;

    /// The SMART / Health Information log page and its length.
    const LOG_SMART: u32 = 0x02;
    const SMART_BYTES: usize = 512;
    /// Command dword 10 carries the dwords to return, less one, from bit 16.
    const LOG_DWORDS_SHIFT: u32 = 16;
    const NAMESPACE_ALL: u32 = u32::MAX;
    /// The namespace ID of a command that concerns the controller alone.
    const NAMESPACE_NONE: u32 = 0;
    /// Bytes 1 and 2 of the SMART log: the composite temperature in Kelvin.
    const SMART_TEMPERATURE_AT: usize = 1;
    /// Byte 5 of the SMART log: the percentage of rated endurance used.
    const SMART_PERCENTAGE_USED_AT: usize = 5;

    /// The kernel's `nvme_passthru_cmd`, field for field.
    #[repr(C)]
    #[derive(Default)]
    struct AdminCommand {
        opcode: u8,
        flags: u8,
        reserved: u16,
        namespace: u32,
        dword_2: u32,
        dword_3: u32,
        metadata: u64,
        address: u64,
        metadata_length: u32,
        data_length: u32,
        dword_10: u32,
        dword_11: u32,
        dword_12: u32,
        dword_13: u32,
        dword_14: u32,
        dword_15: u32,
        timeout_milliseconds: u32,
        result: u32,
    }

    /// Runs the admin command `query` against the drive, filling `data`; a controller that
    /// completes it with an error status answers nothing.
    fn admin(
        device: &Device,
        (opcode, namespace, dword_10): (u8, u32, u32),
        data: &mut [u8],
        query: &'static str,
    ) -> Result<(), Error> {
        let mut command = AdminCommand {
            opcode,
            namespace,
            address: data.as_mut_ptr() as u64,
            data_length: u32::try_from(data.len()).expect("admin buffers are short"),
            dword_10,
            ..AdminCommand::default()
        };
        // SAFETY: the command names `data` and its length, both valid across the call.
        match unsafe { libc::ioctl(device.as_raw_fd(), NVME_IOCTL_ADMIN_CMD, &mut command) } {
            0 => Ok(()),
            status if status < 0 => Err(Error::Io {
                address: device.address(0..0),
                source: io::Error::last_os_error(),
            }),
            _ => Err(Error::Geometry {
                address: device.address(0..0),
                query,
            }),
        }
    }

    pub fn health(device: &Device) -> Result<Health, Error> {
        // SAFETY: `NVME_IOCTL_ID` takes no argument and returns the namespace ID of the node.
        let namespace = unsafe { libc::ioctl(device.as_raw_fd(), NVME_IOCTL_ID) };
        let namespace = u32::try_from(namespace).map_err(|_| Error::Io {
            address: device.address(0..0),
            source: io::Error::last_os_error(),
        })?;
        let mut identify = [0; IDENTIFY_BYTES];
        let controller = (OPCODE_IDENTIFY, NAMESPACE_NONE, IDENTIFY_CONTROLLER);
        admin(device, controller, &mut identify, "Identify Controller")?;
        let model = identify[IDENTIFY_MODEL_NUMBER]
            .try_into()
            .expect("the model number lies inside Identify Controller");
        let formats = (OPCODE_IDENTIFY, namespace, IDENTIFY_NAMESPACE);
        admin(device, formats, &mut identify, "Identify Namespace")?;
        let logical_block = formatted_block(&identify).ok_or_else(|| Error::Geometry {
            address: device.address(0..0),
            query: "Identify Namespace LBA format",
        })?;
        let mut smart = [0; SMART_BYTES];
        let dwords = u32::try_from(SMART_BYTES / size_of::<u32>()).expect("a log page is short");
        let log = (
            OPCODE_GET_LOG_PAGE,
            NAMESPACE_ALL,
            (dwords - 1) << LOG_DWORDS_SHIFT | LOG_SMART,
        );
        admin(device, log, &mut smart, "SMART log")?;
        let temperature = *smart[SMART_TEMPERATURE_AT..]
            .first_chunk()
            .expect("the temperature lies inside the SMART log");
        Ok(Health {
            model,
            logical_block,
            endurance_used: smart[SMART_PERCENTAGE_USED_AT],
            temperature: u16::from_le_bytes(temperature),
        })
    }

    /// The logical block of the LBA format in force, from Identify Namespace.
    fn formatted_block(identify: &[u8; IDENTIFY_BYTES]) -> Option<NonZeroU32> {
        let (formats, formatted) = (
            identify[IDENTIFY_FORMATS_AT],
            identify[IDENTIFY_FORMATTED_AT],
        );
        let mut format = formatted & FORMATTED_LOW_MASK;
        if formats > FORMATTED_LOW_MASK {
            format |= (formatted & FORMATTED_HIGH_MASK) >> FORMATTED_HIGH_SHIFT;
        }
        let at = IDENTIFY_FORMATS_TABLE_AT + usize::from(format) * LBA_FORMAT_BYTES;
        let shift = identify[at + LBA_FORMAT_SHIFT_AT];
        if format > formats || shift < LBA_FORMAT_SHIFT_MIN {
            return None;
        }
        1u32.checked_shl(shift.into()).and_then(NonZeroU32::new)
    }

    /// Whether `name` is an NVMe namespace node, `nvme<controller>n<namespace>`.
    fn namespace_node(name: &str) -> bool {
        let numbered =
            |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
        name.strip_prefix("nvme")
            .and_then(|rest| rest.split_once('n'))
            .is_some_and(|(controller, namespace)| numbered(controller) && numbered(namespace))
    }

    /// The names under the sysfs folder of the block device `name` that are its partitions.
    fn partitions(name: &str) -> Vec<String> {
        let Ok(children) = fs::read_dir(Path::new(SYS_BLOCK).join(name)) else {
            return Vec::new();
        };
        children
            .flatten()
            .filter_map(|child| child.file_name().into_string().ok())
            .filter(|child| child.len() > name.len() && child.starts_with(name))
            .collect()
    }

    /// Processes other than this one holding `drive` open, by its block node, partitions or
    /// generic node.
    pub fn holders(drive: &Path) -> Vec<Holder> {
        let real = fs::canonicalize(drive).unwrap_or_else(|_| drive.to_path_buf());
        let Some(name) = real.file_name().and_then(|name| name.to_str()) else {
            return processes_holding(&[real]);
        };
        let mut nodes: Vec<PathBuf> = partitions(name)
            .iter()
            .map(|child| Path::new(DEV).join(child))
            .collect();
        nodes.push(Path::new(DEV).join(name));
        if let Some(rest) = name.strip_prefix("nvme") {
            nodes.push(Path::new(DEV).join(format!("ng{rest}")));
        }
        processes_holding(&nodes)
    }

    fn processes_holding(nodes: &[PathBuf]) -> Vec<Holder> {
        let own = std::process::id();
        let Ok(processes) = fs::read_dir(PROC) else {
            return Vec::new();
        };
        let mut holders = Vec::new();
        for process in processes.flatten() {
            let pid = process
                .file_name()
                .to_str()
                .and_then(|pid| pid.parse::<u32>().ok());
            let Some(pid) = pid.filter(|&pid| pid != own) else {
                continue;
            };
            let holds = fs::read_dir(process.path().join("fd")).is_ok_and(|descriptors| {
                descriptors
                    .flatten()
                    .filter_map(|descriptor| fs::read_link(descriptor.path()).ok())
                    .any(|target| nodes.contains(&target))
            });
            if holds {
                let mut command = [0; COMMAND_BYTES];
                if let Ok(name) = fs::read(process.path().join("comm")) {
                    let length = name.len().min(COMMAND_BYTES);
                    command[..length].copy_from_slice(&name[..length]);
                }
                holders.push(Holder { pid, command });
            }
        }
        holders
    }

    /// NVMe namespaces with no partitions, holders, mounts or swap on them.
    pub fn raw_namespaces() -> Vec<PathBuf> {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let mounted: Vec<&str> = mountinfo
            .lines()
            .filter_map(|line| line.split(' ').nth(MOUNTINFO_DEVICE_FIELD))
            .collect();
        let swaps = fs::read_to_string("/proc/swaps").unwrap_or_default();
        let Ok(entries) = fs::read_dir(SYS_BLOCK) else {
            return Vec::new();
        };
        let mut namespaces: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| namespace_node(name) && partitions(name).is_empty())
            .filter(|name| {
                let class = Path::new(SYS_BLOCK).join(name);
                let node = Path::new(DEV).join(name);
                let held = fs::read_dir(class.join("holders"))
                    .is_ok_and(|mut holders| holders.next().is_some());
                let device = fs::read_to_string(class.join("dev")).unwrap_or_default();
                let swapped = swaps
                    .lines()
                    .any(|line| line.split_whitespace().next() == node.to_str());
                !held && !swapped && !mounted.contains(&device.trim())
            })
            .map(|name| Path::new(DEV).join(name))
            .collect();
        namespaces.sort();
        namespaces
    }

    /// The sysfs device folders of NVMe controllers bound to `vfio-pci`.
    pub fn bound_controllers() -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(VFIO_PCI_DRIVER) else {
            return Vec::new();
        };
        let mut controllers: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(':'))
            .map(|entry| entry.path())
            .filter(|device| {
                fs::read_to_string(device.join("class"))
                    .is_ok_and(|class| class.trim().starts_with(PCI_CLASS_NVME))
            })
            .filter_map(|device| fs::canonicalize(device).ok())
            .collect();
        controllers.sort();
        controllers
    }

    /// Processes holding the IOMMU group of the controller at `device`.
    pub fn bound_holders(device: &Path) -> Vec<Holder> {
        let group = fs::read_link(device.join("iommu_group"));
        match group.ok().as_deref().and_then(Path::file_name) {
            Some(name) => processes_holding(&[Path::new(VFIO_GROUPS).join(name)]),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::tests::graph_prefix;

    fn dataset(place: u32, name: &str, owner: Owner) -> Dataset {
        Dataset {
            place,
            name: name.into(),
            extent: 0..1,
            owner,
        }
    }

    #[test]
    fn matches_copies_across_places() {
        let graph = || {
            let Slot::SafeGraphs(graph) = slots::classify(&graph_prefix(16_000), u64::MAX) else {
                panic!("the test header parses");
            };
            Owner::SafeGraphs(graph)
        };
        let database = [9; 16];
        let datasets = [
            dataset(0, "sift-1b/index.safegraphs", graph()),
            dataset(0, "usearch-wiki/data/en", Owner::Parquet { files: 1 }),
            dataset(
                0,
                "molecules",
                Owner::UStore {
                    database,
                    generation: 4,
                },
            ),
            dataset(1, "", graph()),
            dataset(
                1,
                "",
                Owner::Directory {
                    database,
                    medium: 1,
                },
            ),
            dataset(1, "", Owner::Foreign),
            dataset(2, "data/EN/", Owner::Parquet { files: 1 }),
            dataset(2, "data/de", Owner::Parquet { files: 1 }),
        ];
        // The graph by its header, the subset by its trailing name; a data directory is no copy.
        assert_eq!(match_copies(&datasets), [0, 1, 2, 0, 4, 5, 1, 7]);
    }

    #[test]
    fn draws_a_drive_with_scattered_runs() {
        let runs = [
            (0..2, GRAPH_MARK),
            (5..8, GRAPH_MARK),
            (9..10, FOREIGN_MARK),
            (12..14, DATABASE_MARK),
            (19..20, GRAPH_MARK),
        ];
        let bar = |columns| slot_bar(20, columns, runs.iter().cloned());
        assert_eq!(bar(20), "██···███·▒··▓▓·····█");
        // Narrower than the drive, a column shows the first run among the slots it covers.
        assert_eq!(bar(10), "█·██▒·▓··█");
    }
}
