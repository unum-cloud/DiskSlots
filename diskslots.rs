//! `diskslots`: where each dataset lives across local folders, raw NVMe slots and S3, how much it
//! takes, and which places hold copies of it; then copy, free and regroup.
//!
//! With no verb on a terminal it opens the screens; every screen action is also a verb printing the
//! same lines.

#![deny(unsafe_code)]

use std::fmt;
use std::io::{self, IsTerminal};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod browse;
mod copy;
#[allow(unsafe_code)]
mod direct;
mod free;
mod list;
mod places;
mod regroup;
mod slots;

use places::{byte_count, parse_address, Address, ObjectAddress};

/// Every dataset on a raw drive starts on a slot boundary, a multiple of `2^SLOT_SHIFT` bytes.
pub const SLOT_SHIFT: u32 = 30;

/// Why a verb stopped, with the address or object it concerns.
#[derive(Debug)]
pub enum Error {
    Io {
        address: Address,
        source: io::Error,
    },
    /// The OS reports no answer to `query`, such as `BLKSSZGET`, so nothing about it is guessed.
    Geometry {
        address: Address,
        query: &'static str,
    },
    S3 {
        object: ObjectAddress,
        source: Box<aws_sdk_s3::Error>,
    },
    Refused(Refusal),
    /// Part `part` read back differs from what was written.
    Mismatch {
        address: Address,
        part: u16,
    },
    /// The progress callback asked to stop, and the job did after the group in flight.
    Stopped {
        address: Address,
    },
}

/// A change DiskSlots will not make, and the address it would have touched.
#[derive(Debug)]
pub enum Refusal {
    /// The address is not a drive, so it holds no slots and takes no byte base.
    NotDrive { address: Address },
    /// The base sits off a slot boundary.
    Misaligned { address: Address },
    /// The slots hold a dataset already.
    Owned { address: Address },
    /// The slots hold bytes no known format wrote, which are never touched.
    Foreign { address: Address },
    /// The slots are free already.
    Free { address: Address },
    /// The destination run overlaps the source.
    Overlap { address: Address },
    /// Process `pid` holds the drive open.
    Held { address: Address, pid: u32 },
    /// A drive takes one SafeGraphs file or UStore stretch and nothing else.
    NotGraph { address: Address },
    /// A UStore database names absolute offsets, so it copies only from and to its own base.
    StaysAtBase { address: Address },
    /// The destination has no room for `bytes`.
    Short { address: Address, bytes: u64 },
    /// The destination exists, and nothing is overwritten.
    Exists { address: Address },
    /// The source holds nothing to copy.
    Empty { address: Address },
    /// A destination folder or prefix needs a name for a source that carries none.
    Unnamed { address: Address },
    /// The object stores no SHA-256 per part, so nothing proves a download of it.
    Unverifiable { object: ObjectAddress },
    /// The object's parts are not one length of whole destination blocks, the last one shorter.
    UnevenParts { object: ObjectAddress },
    /// No local device sits between the two sides to hold the parts, as between two S3 places.
    Unsupported {
        source: ObjectAddress,
        destination: ObjectAddress,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { address, source } => write!(formatter, "{address}: {source}"),
            Error::Geometry { address, query } => {
                write!(formatter, "{address}: the OS reports no {query}")
            }
            Error::S3 { object, source } => {
                let context = aws_sdk_s3::error::DisplayErrorContext(source.as_ref());
                write!(formatter, "{object}: {context}")
            }
            Error::Refused(refusal) => refusal.fmt(formatter),
            Error::Mismatch { address, part } => {
                let part = u32::from(*part) + 1;
                write!(formatter, "{address} differs from its source in part {part}")
            }
            Error::Stopped { address } => {
                write!(formatter, "{address}: stopped after the group in flight")
            }
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slot = byte_count(1 << SLOT_SHIFT);
        match self {
            Refusal::NotDrive { address } => write!(formatter, "{address} is not a drive"),
            Refusal::Misaligned { address } => {
                write!(formatter, "{address} starts off a slot boundary, a multiple of {slot}")
            }
            Refusal::Owned { address } => write!(formatter, "{address} holds a dataset already"),
            Refusal::Foreign { address } => {
                write!(formatter, "{address} holds foreign slots, which are never touched")
            }
            Refusal::Free { address } => write!(formatter, "{address} is free already"),
            Refusal::Overlap { address } => write!(formatter, "{address} overlaps the source"),
            Refusal::Held { address, pid } => {
                write!(formatter, "{address} is held open by process {pid}")
            }
            Refusal::NotGraph { address } => write!(
                formatter,
                "{address} is not one SafeGraphs file or UStore database, and a drive takes nothing else"
            ),
            Refusal::StaysAtBase { address } => write!(
                formatter,
                "{address} is not the base of its UStore database, the one place a copy of it lands"
            ),
            Refusal::Short { address, bytes } => {
                write!(formatter, "{address} has no room for {}", byte_count(*bytes))
            }
            Refusal::Exists { address } => {
                write!(formatter, "{address} exists, and nothing is overwritten")
            }
            Refusal::Empty { address } => write!(formatter, "{address} holds nothing to copy"),
            Refusal::Unnamed { address } => write!(formatter, "name the copy in {address}"),
            Refusal::Unverifiable { object } => write!(
                formatter,
                "{object} stores no SHA-256 per part, so nothing proves a download of it"
            ),
            Refusal::UnevenParts { object } => {
                write!(
                    formatter,
                    "{object} has parts the destination cannot write in whole blocks"
                )
            }
            Refusal::Unsupported { source, destination } => write!(
                formatter,
                "{source} → {destination}: S3 to S3 goes through a local place, which holds the parts"
            ),
        }
    }
}

/// Prints one progress line and never stops, as the verbs report.
pub fn print_progress(progress: &copy::Progress) -> ControlFlow<()> {
    println!("{progress}");
    ControlFlow::Continue(())
}

#[derive(Parser)]
#[command(name = "diskslots", version, about, args_conflicts_with_subcommands = true)]
struct Arguments {
    /// Places to show beside ~/Data and the raw NVMe namespaces: folders, files, drives or s3:// prefixes.
    #[arg(value_parser = parse_address)]
    places: Vec<Address>,

    /// An S3-compatible endpoint URL, such as Nebius Object Storage, in place of AWS.
    #[arg(long, global = true)]
    endpoint: Option<String>,

    #[command(subcommand)]
    verb: Option<Verb>,
}

#[derive(Subcommand)]
enum Verb {
    /// Print every place and the datasets it holds.
    List {
        #[arg(value_parser = parse_address)]
        places: Vec<Address>,
    },
    /// Copy a dataset between places, with SHA-256 per part.
    Copy {
        #[arg(value_parser = parse_address)]
        source: Address,
        #[arg(value_parser = parse_address)]
        destination: Address,
    },
    /// Free the slot run starting at an address like /dev/nvme0n1@96GiB.
    Free {
        #[arg(value_parser = parse_address)]
        run: Address,
    },
    /// Pack a drive's slot runs towards its start.
    Regroup { drive: PathBuf },
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let endpoint = arguments.endpoint.as_deref();
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let result = match &arguments.verb {
        None if interactive => browse::browse(&arguments.places, endpoint),
        None => list::list(&arguments.places, endpoint),
        Some(Verb::List { places }) => list::list(places, endpoint),
        Some(Verb::Copy { source, destination }) => copy::copy(source, destination, endpoint),
        Some(Verb::Free { run }) => free::free(run),
        Some(Verb::Regroup { drive }) => regroup::regroup(drive),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diskslots: {error}");
            ExitCode::FAILURE
        }
    }
}
