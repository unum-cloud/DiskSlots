//! The `free` verb: a slot run stops being live, then goes back to the drive.
//!
//! Freeing zeroes the first logical block of every slot the run covers, the run's first slot
//! first, so the run stops being live before anything else changes, and then discards the rest of
//! each slot. Only SafeGraphs files and UStore databases are freed; foreign slots are never touched.

use std::fmt;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::copy::Progress;
use crate::direct::{Access, AlignedBuffer, Device};
use crate::list;
use crate::places::{Address, Location};
use crate::slots::{self, slot_at, slot_extent, SpanOwner, SLOT_BYTES};
use crate::{Error, Refusal};

/// The slots one free zeroes, the one that makes the run live first.
pub struct FreePlan {
    pub run: Address,
    drive: PathBuf,
    held: Vec<u32>,
}

/// Refuses when a process other than this one holds `drive` open.
pub fn refuse_held(drive: &Path, address: &Address) -> Result<(), Error> {
    match list::holders(drive).first() {
        Some(holder) => Err(Error::Refused(Refusal::Held {
            address: address.clone(),
            pid: holder.pid,
        })),
        None => Ok(()),
    }
}

impl FreePlan {
    /// The SafeGraphs file or UStore data directory starting at `run`, such as
    /// `/dev/nvme0n1@96GiB`.
    pub fn new(run: &Address) -> Result<FreePlan, Error> {
        let Location::Path(drive) = &run.location else {
            return Err(Error::Refused(Refusal::NotDrive {
                address: run.clone(),
            }));
        };
        let first = slot_at(run)?;
        refuse_held(drive, run)?;
        let map = slots::map(&Device::open(drive, Access::Read)?)?;
        let refused = |refusal: fn(Address) -> Refusal| Err(Error::Refused(refusal(run.clone())));
        let held = match map.covering(first) {
            Some(span) if span.first_slot == first => match &span.owner {
                SpanOwner::SafeGraphs(_) => (first..span.end()).collect(),
                SpanOwner::UStore { held, .. } => held.clone(),
                SpanOwner::Foreign => return refused(|address| Refusal::Foreign { address }),
            },
            Some(_) => return refused(|address| Refusal::Foreign { address }),
            None => return refused(|address| Refusal::Free { address }),
        };
        Ok(FreePlan {
            run: run.clone(),
            drive: drive.clone(),
            held,
        })
    }

    /// Zeroes every held slot's first block, then discards the rest of each slot, calling `report`
    /// after each and stopping the discards once it breaks; the run is free by then.
    pub fn run(&self, mut report: impl FnMut(&Progress) -> ControlFlow<()>) -> Result<(), Error> {
        // Planned again on the open drive, so nothing that changed since is touched.
        let held = FreePlan::new(&self.run)?.held;
        let device = Device::open(&self.drive, Access::Write)?;
        // Every slot's first block, not just the run's: a discard need not read back as zeros, and a
        // slot whose first block holds old bytes would read as foreign forever.
        let block = u64::from(device.logical_block.get());
        let zeros = AlignedBuffer::zeroed(&device, block)?;
        for &slot in &held {
            device.write_at(&zeros, slot_extent(slot, 0).start)?;
        }
        device.sync()?;
        let mut progress = Progress::new(held.len() as u64 * SLOT_BYTES);
        for &slot in &held {
            let extent = slot_extent(slot, 1);
            let rest = extent.start + block..extent.end.min(device.bytes);
            if !rest.is_empty() {
                device.discard(rest)?;
            }
            progress.bytes_done += SLOT_BYTES;
            if report(&progress).is_break() {
                return Err(Error::Stopped {
                    address: self.run.clone(),
                });
            }
        }
        Ok(())
    }
}

impl fmt::Display for FreePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slots = list::plural(self.held.len() as u64, "slot");
        write!(formatter, "free {}: {slots}", self.run)
    }
}

/// The `free` verb: the plan, one progress line per slot, and the freed run.
pub fn free(run: &Address) -> Result<(), Error> {
    let plan = FreePlan::new(run)?;
    println!("{plan}");
    plan.run(crate::print_progress)?;
    println!("freed {run}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frees_a_database_and_refuses_free_and_foreign_slots() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let device = Device::open(file.path(), Access::Write).unwrap();
        device.set_len(3 * SLOT_BYTES).unwrap();
        let mut owner = b"USTORESL".to_vec();
        owner.extend_from_slice(&1u32.to_le_bytes());
        owner.resize(device.logical_block.get() as usize, 0);
        let mut foreign = owner.clone();
        foreign[0] = b'X';
        let mut block = AlignedBuffer::zeroed(&device, owner.len() as u64).unwrap();
        for (slot, bytes) in [(0, &owner), (1, &foreign)] {
            block.copy_from_slice(bytes);
            device.write_at(&block, slot_extent(slot, 0).start).unwrap();
        }
        drop(device);

        let address = |slot| Address::extent(file.path(), slot_extent(slot, 0));
        let plan = FreePlan::new(&address(0)).unwrap();
        plan.run(|_| ControlFlow::Continue(())).unwrap();
        let map = slots::map(&Device::open(file.path(), Access::Read).unwrap()).unwrap();
        assert_eq!(map.spans.len(), 1);
        assert!(matches!(map.spans[0].owner, SpanOwner::Foreign));

        let refusal = |slot| FreePlan::new(&address(slot)).err();
        assert!(matches!(
            refusal(0),
            Some(Error::Refused(Refusal::Free { .. }))
        ));
        assert!(matches!(
            refusal(1),
            Some(Error::Refused(Refusal::Foreign { .. }))
        ));
    }
}
