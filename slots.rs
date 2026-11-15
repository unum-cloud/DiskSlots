//! The 1 GiB slots of a raw drive and who owns them.
//!
//! A drive is cut into slots from byte 0, and every file or database on it starts on a slot
//! boundary. Mapping reads the first logical block of each slot and checks it against each owner's
//! own magic alone: a SafeGraphs file whose header ends inside its slot, parses and whose extent
//! fits the drive covers as many slots as its length needs; a UStore owner block names the
//! database holding that one slot; a dataset exchange file whose head declares its length covers
//! as many slots as that length needs, and one whose length sits in a footer covers one slot and is
//! never written; an all-zero block is free; anything else is foreign, one slot long, and never
//! written.

use std::ops::Range;

use allocator_api2::alloc::Global;
use safegraphs::GraphHeader;
use stringzilla as sz;

use crate::direct::{AlignedBuffer, Device};
use crate::places::Address;
use crate::{Error, Refusal, SLOT_SHIFT};

/// Bytes in one slot.
pub const SLOT_BYTES: u64 = 1 << SLOT_SHIFT;

/// The bytes of slots `first..first + slots`.
pub fn slot_extent(first: u32, slots: u32) -> Range<u64> {
    u64::from(first) << SLOT_SHIFT..u64::from(first + slots) << SLOT_SHIFT
}

/// The slot starting at the address's base, refusing a base off a slot boundary.
pub fn slot_at(address: &Address) -> Result<u32, Error> {
    let misaligned = || {
        Error::Refused(Refusal::Misaligned {
            address: address.clone(),
        })
    };
    if !address.base.is_multiple_of(SLOT_BYTES) {
        return Err(misaligned());
    }
    u32::try_from(address.base >> SLOT_SHIFT).map_err(|_| misaligned())
}

const OWNER_MAGIC: [u8; 8] = *b"USTORESL";
const OWNER_FORMAT_VERSION: u32 = 1;
const OWNER_BYTES: usize = 56;
const OWNER_VERSION_AT: usize = 8;
const OWNER_MEDIUM_AT: usize = 12;
const OWNER_DATABASE_AT: usize = 16;
const OWNER_GENERATION_AT: usize = 32;
const OWNER_OFFSET_AT: usize = 40;
const OWNER_LENGTH_AT: usize = 48;

/// The first logical block of every slot a UStore database holds, little-endian, as the C
/// `slot_owner_block_t` lays it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UstoreOwnerBlock {
    /// The slot's data directory among the database's.
    pub medium: u32,
    pub database: [u8; 16],
    /// The generation of the manifest copy in force, on the manifest slot alone.
    pub manifest_generation: u64,
    /// Where on the drive that copy begins.
    pub manifest_offset: u64,
    pub manifest_bytes: u64,
}

impl UstoreOwnerBlock {
    /// The owner block opening `bytes`, if it carries the magic and this layout's version.
    pub fn read(bytes: &[u8]) -> Option<Self> {
        let block: &[u8; OWNER_BYTES] = bytes.first_chunk()?;
        fn field<const WIDTH: usize>(block: &[u8; OWNER_BYTES], at: usize) -> [u8; WIDTH] {
            block[at..at + WIDTH]
                .try_into()
                .expect("every field lies inside the block")
        }
        let version = u32::from_le_bytes(field(block, OWNER_VERSION_AT));
        (block.starts_with(&OWNER_MAGIC) && version == OWNER_FORMAT_VERSION).then(|| Self {
            medium: u32::from_le_bytes(field(block, OWNER_MEDIUM_AT)),
            database: field(block, OWNER_DATABASE_AT),
            manifest_generation: u64::from_le_bytes(field(block, OWNER_GENERATION_AT)),
            manifest_offset: u64::from_le_bytes(field(block, OWNER_OFFSET_AT)),
            manifest_bytes: u64::from_le_bytes(field(block, OWNER_LENGTH_AT)),
        })
    }

    /// The slot holding the manifest copy in force, which only the manifest slot's block names;
    /// the database lands back there and nowhere else.
    pub fn manifest_slot(&self) -> Option<u32> {
        (self.manifest_bytes != 0)
            .then(|| u32::try_from(self.manifest_offset >> SLOT_SHIFT).ok())
            .flatten()
    }
}

/// A SafeGraphs file opening a slot or a file.
pub struct Graph {
    pub header: GraphHeader,
    pub file_bytes: u64,
    /// SHA-256 of the header bytes, which tells copies of one file apart without reading data.
    pub header_digest: sz::Sha256Digest,
}

/// Each format's magic and the byte it starts at; safetensors carries none and is tried last.
const TABLE_MAGICS: [(TableFormat, usize, &[u8]); 8] = [
    (TableFormat::Parquet, 0, b"PAR1"),
    (TableFormat::ArrowFile, 0, b"ARROW1\0\0"),
    (TableFormat::Orc, 0, b"ORC"),
    (TableFormat::Numpy, 0, b"\x93NUMPY"),
    (TableFormat::Sqlite, 0, b"SQLite format 3\0"),
    (TableFormat::Hdf5, 0, b"\x89HDF\r\n\x1a\n"),
    (TableFormat::DuckDb, 8, b"DUCK"),
    (TableFormat::Gguf, 0, b"GGUF"),
];
const NUMPY_VERSION_AT: usize = 6;
const NUMPY_LENGTH_AT: usize = 8;
/// The item kinds whose descr digits count bytes, unlike `U`, which counts 4-byte characters.
const NUMPY_BYTE_KINDS: [char; 7] = ['b', 'i', 'u', 'f', 'c', 'S', 'V'];
const SAFETENSORS_JSON_AT: usize = 8;
/// A longer safetensors header length is taken for other bytes, as no real header grows so long.
const SAFETENSORS_HEADER_MAX: u64 = 100 << 20;
const SQLITE_PAGE_BYTES_AT: usize = 16;
const SQLITE_PAGES_AT: usize = 28;
/// What a page size field of 1 stands for, as 65536 does not fit its two bytes.
const SQLITE_PAGE_BYTES_MAX: u64 = 1 << 16;
const HDF5_VERSION_AT: usize = 8;
const HDF5_OFFSETS_AT: usize = 9;
/// Where the base address starts; the extension address and then the end-of-file address follow.
const HDF5_ADDRESSES_AT: usize = 12;

/// A dataset exchange file recognized by its first page, with its length when the head declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub format: TableFormat,
    pub bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableFormat {
    Parquet,
    ArrowFile,
    Orc,
    Numpy,
    Safetensors,
    Sqlite,
    Hdf5,
    DuckDb,
    Gguf,
}

impl TableFormat {
    pub fn name(self) -> &'static str {
        match self {
            TableFormat::Parquet => "Parquet",
            TableFormat::ArrowFile => "Arrow",
            TableFormat::Orc => "ORC",
            TableFormat::Numpy => "NumPy",
            TableFormat::Safetensors => "safetensors",
            TableFormat::Sqlite => "SQLite",
            TableFormat::Hdf5 => "HDF5",
            TableFormat::DuckDb => "DuckDB",
            TableFormat::Gguf => "GGUF",
        }
    }
}

impl Table {
    /// The file opening `head`, if a format's magic or a whole safetensors header opens it.
    pub fn read(head: &[u8]) -> Option<Self> {
        let magic = TABLE_MAGICS
            .iter()
            .find(|(_, at, magic)| head.get(*at..).is_some_and(|rest| rest.starts_with(magic)));
        let Some(&(format, ..)) = magic else {
            let bytes = Some(safetensors_bytes(head)?);
            return Some(Table {
                format: TableFormat::Safetensors,
                bytes,
            });
        };
        let bytes = match format {
            TableFormat::Numpy => numpy_bytes(head),
            TableFormat::Sqlite => sqlite_bytes(head),
            TableFormat::Hdf5 => hdf5_bytes(head),
            _ => None,
        };
        // A zero length would cover no slot at all.
        let bytes = bytes.filter(|&bytes| bytes != 0);
        Some(Table { format, bytes })
    }
}

/// Where the safetensors header opening `head` ends, if its length is plausible and JSON follows.
pub fn safetensors_end(head: &[u8]) -> Option<usize> {
    let length = u64::from_le_bytes(*head.first_chunk()?);
    (length < SAFETENSORS_HEADER_MAX && head.get(SAFETENSORS_JSON_AT) == Some(&b'{'))
        .then(|| SAFETENSORS_JSON_AT + length as usize)
}

/// The header, then the bytes up to the furthest tensor end.
fn safetensors_bytes(head: &[u8]) -> Option<u64> {
    let end = safetensors_end(head)?;
    let header = jsontape::parse(head.get(SAFETENSORS_JSON_AT..end)?).ok()?;
    let tensor_ends =
        header
            .as_object()?
            .iter()
            .filter_map(|(_, tensor)| match tensor.get("data_offsets")?.as_array()? {
                [_, tensor_end] => tensor_end.as_u64(),
                _ => None,
            });
    Some(end as u64 + tensor_ends.max()?)
}

/// The header, then the shape's items at the descr's item size.
fn numpy_bytes(head: &[u8]) -> Option<u64> {
    let (length_bytes, header_at) = match head.get(NUMPY_VERSION_AT)? {
        1 => (size_of::<u16>(), NUMPY_LENGTH_AT + size_of::<u16>()),
        2 | 3 => (size_of::<u32>(), NUMPY_LENGTH_AT + size_of::<u32>()),
        _ => return None,
    };
    let mut length = [0; size_of::<u32>()];
    length[..length_bytes].copy_from_slice(head.get(NUMPY_LENGTH_AT..header_at)?);
    let end = header_at + u32::from_le_bytes(length) as usize;
    let header = std::str::from_utf8(head.get(header_at..end)?).ok()?;
    let value = |key: &str| Some(header.split_once(key)?.1.trim_start());
    let descr = value("'descr':")?.strip_prefix('\'')?.split_once('\'')?.0;
    let item_bytes: u64 = descr
        .trim_start_matches(['<', '>', '|', '='])
        .strip_prefix(NUMPY_BYTE_KINDS)?
        .parse()
        .ok()?;
    let shape = value("'shape':")?.strip_prefix('(')?.split_once(')')?.0;
    let items = shape
        .split(',')
        .map(str::trim)
        .filter(|extent| !extent.is_empty())
        .try_fold(1u64, |items, extent| items.checked_mul(extent.parse().ok()?))?;
    items.checked_mul(item_bytes)?.checked_add(end as u64)
}

/// The page size times the page count, which a database that predates the count leaves at zero.
fn sqlite_bytes(head: &[u8]) -> Option<u64> {
    let page_bytes = u16::from_be_bytes(*head.get(SQLITE_PAGE_BYTES_AT..)?.first_chunk()?);
    let pages = u32::from_be_bytes(*head.get(SQLITE_PAGES_AT..)?.first_chunk()?);
    let page_bytes = match page_bytes {
        1 => SQLITE_PAGE_BYTES_MAX,
        page_bytes => u64::from(page_bytes),
    };
    (pages != 0).then(|| page_bytes * u64::from(pages))
}

/// The end-of-file address of a version 2 or 3 superblock.
fn hdf5_bytes(head: &[u8]) -> Option<u64> {
    let offset_bytes = usize::from(*head.get(HDF5_OFFSETS_AT)?);
    if !matches!(head.get(HDF5_VERSION_AT)?, 2 | 3) || offset_bytes > size_of::<u64>() {
        return None;
    }
    let at = HDF5_ADDRESSES_AT + 2 * offset_bytes;
    let mut address = [0; size_of::<u64>()];
    address[..offset_bytes].copy_from_slice(head.get(at..at + offset_bytes)?);
    Some(u64::from_le_bytes(address))
}

/// What one slot's first bytes say.
// A map holds few runs, so its SafeGraphs headers stay inline rather than boxed.
#[allow(clippy::large_enum_variant)]
pub enum Slot {
    SafeGraphs(Graph),
    UStore(UstoreOwnerBlock),
    Table(Table),
    /// An all-zero first block.
    Free,
    Foreign,
}

/// Classifies a slot from its first bytes, `prefix`, given the `room` from the slot to the drive's
/// end.
pub fn classify(prefix: &[u8], room: u64) -> Slot {
    if prefix.starts_with(&safegraphs::MAGIC) {
        return match GraphHeader::read_in(prefix, Global) {
            Ok(header) if header.file_bytes() as u64 <= room && header.segment_start as u64 <= SLOT_BYTES => {
                Slot::SafeGraphs(Graph {
                    header_digest: sz::Sha256::hash(&prefix[..header.segment_start]),
                    file_bytes: header.file_bytes() as u64,
                    header,
                })
            }
            _ => Slot::Foreign,
        };
    }
    if let Some(block) = UstoreOwnerBlock::read(prefix) {
        return Slot::UStore(block);
    }
    if let Some(table) = Table::read(prefix).filter(|table| table.bytes.is_none_or(|bytes| bytes <= room)) {
        return Slot::Table(table);
    }
    match prefix.iter().all(|&byte| byte == 0) {
        true => Slot::Free,
        false => Slot::Foreign,
    }
}

/// Reads slot `slot` of `device` through `scratch` and classifies it: its first logical block
/// alone, then up to the end of a SafeGraphs or safetensors header only when that header says it
/// is longer.
pub fn probe(device: &Device, slot: u32, scratch: &mut AlignedBuffer) -> Result<Slot, Error> {
    let offset = slot_extent(slot, 0).start;
    let room = device.bytes.saturating_sub(offset);
    let block = device.logical_block.get() as usize;
    if scratch.len() < block {
        *scratch = AlignedBuffer::zeroed(device, block as u64)?;
    }
    let read = device.read_at(&mut scratch[..block], offset)?;
    let head = &scratch[..read];
    let header_bytes = GraphHeader::segment_start_of(head)
        .ok()
        .or_else(|| safetensors_end(head))
        .filter(|&end| end > block && end as u64 <= room.min(SLOT_BYTES));
    let Some(header_bytes) = header_bytes else {
        return Ok(classify(&scratch[..read], room));
    };
    *scratch = AlignedBuffer::zeroed(device, header_bytes.next_multiple_of(block) as u64)?;
    let read = device.read_at(scratch, offset)?;
    let owner = classify(&scratch[..read], room);
    *scratch = AlignedBuffer::default();
    Ok(owner)
}

/// Who holds a stretch of slots.
#[allow(clippy::large_enum_variant)]
pub enum SpanOwner {
    SafeGraphs(Graph),
    /// One data directory of a UStore database, by its first slot's owner block, and the slots
    /// it holds in slot order, the free ones between them left out.
    UStore {
        block: UstoreOwnerBlock,
        held: Vec<u32>,
    },
    /// A dataset exchange file, covering its declared length or else its one slot.
    Table(Table),
    /// Consecutive foreign slots.
    Foreign,
}

/// A stretch of slots that is not free.
pub struct Span {
    pub first_slot: u32,
    pub slots: u32,
    pub owner: SpanOwner,
}

impl Span {
    pub fn end(&self) -> u32 {
        self.first_slot + self.slots
    }
}

/// Every stretch on a drive that is not free, by first slot.
pub struct SlotMap {
    pub slots: u32,
    pub spans: Vec<Span>,
}

impl SlotMap {
    /// The span covering `slot`.
    pub fn covering(&self, slot: u32) -> Option<&Span> {
        self.spans
            .iter()
            .find(|span| span.first_slot <= slot && slot < span.end())
    }

    pub fn occupied(&self) -> Vec<Range<u32>> {
        self.spans.iter().map(|span| span.first_slot..span.end()).collect()
    }
}

/// Probes every slot of `device`, skipping the slots each SafeGraphs file covers.
pub fn map(device: &Device) -> Result<SlotMap, Error> {
    let slots = u32::try_from(device.bytes >> SLOT_SHIFT).unwrap_or(u32::MAX);
    let mut spans: Vec<Span> = Vec::new();
    let mut scratch = AlignedBuffer::default();
    let mut slot = 0;
    while slot < slots {
        let mut covered = 1;
        match probe(device, slot, &mut scratch)? {
            Slot::Free => {}
            Slot::SafeGraphs(graph) => {
                covered = u32::try_from(graph.file_bytes.div_ceil(SLOT_BYTES).max(1))
                    .expect("a file that fits its drive covers fewer slots than the drive has");
                spans.push(Span {
                    first_slot: slot,
                    slots: covered,
                    owner: SpanOwner::SafeGraphs(graph),
                });
            }
            Slot::Table(table) => {
                covered = u32::try_from(table.bytes.unwrap_or_default().div_ceil(SLOT_BYTES).max(1))
                    .expect("a file that fits its drive covers fewer slots than the drive has");
                spans.push(Span {
                    first_slot: slot,
                    slots: covered,
                    owner: SpanOwner::Table(table),
                });
            }
            Slot::Foreign => match spans.last_mut() {
                Some(last) if matches!(last.owner, SpanOwner::Foreign) && last.end() == slot => last.slots += 1,
                _ => spans.push(Span {
                    first_slot: slot,
                    slots: 1,
                    owner: SpanOwner::Foreign,
                }),
            },
            Slot::UStore(block) => {
                let directory = spans.iter_mut().find(|span| {
                    matches!(&span.owner, SpanOwner::UStore { block: known, .. }
                        if known.database == block.database && known.medium == block.medium)
                });
                match directory {
                    Some(span) => {
                        span.slots = slot + 1 - span.first_slot;
                        if let SpanOwner::UStore { held, .. } = &mut span.owner {
                            held.push(slot);
                        }
                    }
                    None => spans.push(Span {
                        first_slot: slot,
                        slots: 1,
                        owner: SpanOwner::UStore {
                            block,
                            held: vec![slot],
                        },
                    }),
                }
            }
        }
        slot += covered;
    }
    spans.sort_by_key(|span| span.first_slot);
    Ok(SlotMap { slots, spans })
}

/// The first slot of the first run of `count` slots at or after `from` that no occupied range
/// touches and that ends by `end`.
pub fn first_free_run(occupied: &[Range<u32>], count: u32, from: u32, end: u32) -> Option<u32> {
    let mut start = from;
    while let Some(blocking) = occupied
        .iter()
        .find(|range| range.start < start + count && start < range.end)
    {
        start = blocking.end;
    }
    (start + count <= end).then_some(start)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const BLOCK: usize = 4096;
    const DATABASE: [u8; 16] = [7; 16];

    fn block_with(head: &[u8]) -> Vec<u8> {
        let mut block = vec![0; BLOCK];
        block[..head.len()].copy_from_slice(head);
        block
    }

    pub fn graph_prefix(data_bytes: u64) -> Vec<u8> {
        let header = format!(
            r#"{{"__metadata__":{{"metric":"l2sq"}},"key":{{"dtype":"u32","data":[0,{keys}]}},"value":{{"dtype":"u8","shape":4,"data":[{keys},{values}]}},"neighbors":{{"dtype":"u32","shape":2,"data":[{values},{data_bytes}]}}}}"#,
            keys = 4 * 1000,
            values = 4 * 1000 + 4 * 1000,
        );
        let mut prefix = safegraphs::MAGIC.to_vec();
        prefix.extend_from_slice(&(header.len() as u64).to_le_bytes());
        prefix.extend_from_slice(header.as_bytes());
        block_with(&prefix)
    }

    pub fn owner_block(version: u32, medium: u32, manifest_offset: u64, manifest_bytes: u64) -> Vec<u8> {
        let mut block = OWNER_MAGIC.to_vec();
        block.extend_from_slice(&version.to_le_bytes());
        block.extend_from_slice(&medium.to_le_bytes());
        block.extend_from_slice(&DATABASE);
        block.extend_from_slice(&9u64.to_le_bytes());
        block.extend_from_slice(&manifest_offset.to_le_bytes());
        block.extend_from_slice(&manifest_bytes.to_le_bytes());
        block_with(&block)
    }

    #[test]
    fn reads_owner_blocks_and_classifies_first_blocks() {
        let block = UstoreOwnerBlock::read(&owner_block(1, 3, 5 << 20, 100)).unwrap();
        assert_eq!(block.medium, 3);
        assert_eq!(block.database, DATABASE);
        assert_eq!(block.manifest_generation, 9);
        assert_eq!(block.manifest_offset, 5 << 20);
        assert_eq!(block.manifest_bytes, 100);
        assert_eq!(block.manifest_slot(), Some(0));
        assert_eq!(UstoreOwnerBlock::read(&owner_block(2, 0, 0, 0)), None);
        assert_eq!(
            UstoreOwnerBlock::read(&owner_block(1, 0, 0, 0)[..OWNER_BYTES - 1]),
            None
        );

        let room = 3 * SLOT_BYTES;
        let Slot::SafeGraphs(graph) = classify(&graph_prefix(16_000), room) else {
            panic!("a SafeGraphs header that parses and fits is live");
        };
        assert!(graph.file_bytes > 16_000 && graph.file_bytes < BLOCK as u64 + 16_000);
        // A header claiming more than the drive holds past its slot is not trusted.
        assert!(matches!(classify(&graph_prefix(4 * SLOT_BYTES), room), Slot::Foreign));
        assert!(matches!(
            classify(&owner_block(1, 0, 0, 0), SLOT_BYTES),
            Slot::UStore(block) if block.database == DATABASE
        ));
        assert!(matches!(classify(&owner_block(2, 0, 0, 0), SLOT_BYTES), Slot::Foreign));
        assert!(matches!(classify(&block_with(&[]), SLOT_BYTES), Slot::Free));
        // One stray byte anywhere in the first block makes the slot foreign, never free.
        let mut stray = block_with(&[]);
        stray[BLOCK - 1] = 1;
        assert!(matches!(classify(&stray, SLOT_BYTES), Slot::Foreign));
    }

    fn table(head: &[u8], room: u64) -> Option<Table> {
        match classify(&block_with(head), room) {
            Slot::Table(table) => Some(table),
            _ => None,
        }
    }

    fn safetensors(header: &str) -> Vec<u8> {
        let mut head = (header.len() as u64).to_le_bytes().to_vec();
        head.extend_from_slice(header.as_bytes());
        head
    }

    #[test]
    fn recognizes_dataset_exchange_files_by_their_magic() {
        let tensor = safetensors(r#"{"t":{"dtype":"U8","shape":[4],"data_offsets":[0,4]}}"#);
        let heads: [(&[u8], TableFormat); 9] = [
            (b"PAR1", TableFormat::Parquet),
            (b"ARROW1\0\0", TableFormat::ArrowFile),
            (b"ORC", TableFormat::Orc),
            (b"\x93NUMPY", TableFormat::Numpy),
            (b"SQLite format 3\0", TableFormat::Sqlite),
            (b"\x89HDF\r\n\x1a\n", TableFormat::Hdf5),
            (b"\x01\0\0\0\0\0\0\0DUCK", TableFormat::DuckDb),
            (b"GGUF", TableFormat::Gguf),
            (&tensor, TableFormat::Safetensors),
        ];
        for (head, format) in heads {
            assert_eq!(table(head, SLOT_BYTES).map(|table| table.format), Some(format));
        }
        // Safetensors carries no magic, so a header naming no tensor is no table.
        assert_eq!(table(&safetensors(r#"{"__metadata__":{}}"#), SLOT_BYTES), None);
        assert_eq!(table(b"\x02\0\0\0\0\0\0\0[]", SLOT_BYTES), None);
    }

    #[test]
    fn reads_the_lengths_heads_declare() {
        let numpy = |descr: &str| {
            let header = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': (3, 4), }}\n");
            let mut head = b"\x93NUMPY\x01\x00".to_vec();
            head.extend_from_slice(&(header.len() as u16).to_le_bytes());
            head.extend_from_slice(header.as_bytes());
            (head, 10 + header.len() as u64)
        };
        let (head, header_end) = numpy("<f4");
        assert_eq!(table(&head, SLOT_BYTES).unwrap().bytes, Some(header_end + 3 * 4 * 4));
        assert_eq!(table(&numpy("|O").0, SLOT_BYTES).unwrap().bytes, None);

        let header = r#"{"__metadata__":{"a":"b"},"x":{"dtype":"F32","shape":[6],"data_offsets":[8,32]},"y":{"dtype":"F32","shape":[2],"data_offsets":[0,8]}}"#;
        let declared = table(&safetensors(header), SLOT_BYTES).unwrap().bytes;
        assert_eq!(declared, Some(8 + header.len() as u64 + 32));

        let sqlite = |page_bytes: u16, pages: u32| {
            let mut head = b"SQLite format 3\0".to_vec();
            head.extend_from_slice(&page_bytes.to_be_bytes());
            head.resize(28, 0);
            head.extend_from_slice(&pages.to_be_bytes());
            head
        };
        assert_eq!(table(&sqlite(4096, 5), SLOT_BYTES).unwrap().bytes, Some(5 * 4096));
        assert_eq!(table(&sqlite(1, 3), SLOT_BYTES).unwrap().bytes, Some(3 << 16));
        assert_eq!(table(&sqlite(4096, 0), SLOT_BYTES).unwrap().bytes, None);
        // A length past the drive's end is not trusted.
        assert!(matches!(classify(&block_with(&sqlite(1, 3)), 3 << 15), Slot::Foreign));

        let hdf5 = |version: u8| {
            let mut head = b"\x89HDF\r\n\x1a\n".to_vec();
            head.extend_from_slice(&[version, 8, 8, 0]);
            head.resize(12 + 2 * 8, 0);
            head.extend_from_slice(&123_456u64.to_le_bytes());
            head
        };
        assert_eq!(table(&hdf5(2), SLOT_BYTES).unwrap().bytes, Some(123_456));
        assert_eq!(table(&hdf5(0), SLOT_BYTES).unwrap().bytes, None);
    }
}
