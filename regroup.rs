//! The `regroup` verb: packs a drive's runs towards its start, so its free slots join into one
//! long run.
//!
//! Each move is a verified copy into a free run that ends by the source's first slot, then a free
//! of the source, so no move overlaps what it copies and a crash leaves an extra copy, never a
//! missing one. UStore databases and foreign slots stay where they are, and runs pack around them.

use std::fmt;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};

use crate::copy::{CopyPlan, Progress};
use crate::direct::{Access, Device};
use crate::free::{refuse_held, FreePlan};
use crate::list::plural;
use crate::places::Address;
use crate::slots::{self, first_free_run, slot_extent, SpanOwner};
use crate::{Error, SLOT_SHIFT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub from_slot: u32,
    pub to_slot: u32,
    pub slots: u32,
}

/// Every move packing one drive, in order, and the runs that find no room before them.
pub struct RegroupPlan {
    pub drive: PathBuf,
    pub moves: Vec<Move>,
    pub stays: Vec<Range<u32>>,
}

/// The moves packing `spans`, each a run of slots and whether it may move, towards slot 0, and the
/// movable runs that find no room before them.
pub fn plan(spans: &[(Range<u32>, bool)]) -> (Vec<Move>, Vec<Range<u32>>) {
    let mut occupied: Vec<Range<u32>> = spans.iter().map(|(run, _)| run.clone()).collect();
    let (mut moves, mut stays) = (Vec::new(), Vec::new());
    let mut target = 0;
    for (index, (run, _)) in spans
        .iter()
        .enumerate()
        .filter(|(_, (_, movable))| *movable)
    {
        let slots = run.end - run.start;
        if run.start <= target {
            target = target.max(run.end);
            continue;
        }
        match first_free_run(&occupied, slots, target, run.start) {
            Some(to_slot) => {
                moves.push(Move {
                    from_slot: run.start,
                    to_slot,
                    slots,
                });
                occupied[index] = to_slot..to_slot + slots;
                target = to_slot + slots;
            }
            None => {
                stays.push(run.clone());
                target = run.end;
            }
        }
    }
    (moves, stays)
}

impl RegroupPlan {
    /// Maps `drive` and plans its moves; only SafeGraphs files move.
    pub fn new(drive: &Path) -> Result<RegroupPlan, Error> {
        refuse_held(drive, &Address::path(drive))?;
        let map = slots::map(&Device::open(drive, Access::Read)?)?;
        let spans: Vec<(Range<u32>, bool)> = map
            .spans
            .iter()
            .map(|span| {
                (
                    span.first_slot..span.end(),
                    matches!(span.owner, SpanOwner::SafeGraphs(_)),
                )
            })
            .collect();
        let (moves, stays) = plan(&spans);
        Ok(RegroupPlan {
            drive: drive.to_path_buf(),
            moves,
            stays,
        })
    }

    /// Copies each run to its new place, then frees where it was, calling `report` after every
    /// group of parts and stopping after the group in flight once it breaks.
    pub fn run(&self, mut report: impl FnMut(&Progress) -> ControlFlow<()>) -> Result<(), Error> {
        let slots: u64 = self.moves.iter().map(|step| u64::from(step.slots)).sum();
        let mut progress = Progress::new(slots << SLOT_SHIFT);
        for step in &self.moves {
            let run = |first| Address::extent(&self.drive, slot_extent(first, step.slots));
            let source = run(step.from_slot);
            let done = progress.bytes_done;
            CopyPlan::new(&source, &run(step.to_slot), None)?.run(None, |copied| {
                progress.bytes_done = done + copied.bytes_done;
                report(&progress)
            })?;
            FreePlan::new(&source)?.run(|_| report(&progress))?;
        }
        Ok(())
    }
}

impl fmt::Display for Move {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let last = |first: u32| first + self.slots - 1;
        let (from, to) = (self.from_slot, self.to_slot);
        write!(formatter, "slots {from}–{} → {to}–{}", last(from), last(to))
    }
}

impl fmt::Display for RegroupPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let moves = plural(self.moves.len() as u64, "move");
        write!(formatter, "regroup {}: {moves}", self.drive.display())?;
        match self.stays.len() {
            0 => Ok(()),
            stays => write!(
                formatter,
                ", {} with no room before",
                plural(stays as u64, "run")
            ),
        }
    }
}

/// The `regroup` verb: the plan, every move and every run that stays, one progress line per group
/// of parts, and the regrouped drive.
pub fn regroup(drive: &Path) -> Result<(), Error> {
    let plan = RegroupPlan::new(drive)?;
    println!("{plan}");
    for step in &plan.moves {
        println!("move {step}");
    }
    for run in &plan.stays {
        println!(
            "stays at slot {}: no free run of {} slots before it",
            run.start,
            run.len()
        );
    }
    plan.run(crate::print_progress)?;
    println!("regrouped {}", drive.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(from_slot: u32, to_slot: u32, slots: u32) -> Move {
        Move {
            from_slot,
            to_slot,
            slots,
        }
    }

    #[test]
    fn packs_runs_around_pinned_ones_without_overlapping_sources() {
        // SafeGraphs files move; a foreign slot at 6 and a UStore stretch at 18 stay.
        let spans = [
            (2..4, true),
            (6..7, false),
            (9..12, true),
            (13..17, true),
            (18..20, false),
            (21..22, true),
        ];
        let (moves, stays) = plan(&spans);
        assert_eq!(
            moves,
            [
                step(2, 0, 2),
                step(9, 2, 3),
                step(13, 7, 4),
                step(21, 11, 1)
            ]
        );
        assert!(stays.is_empty());
        assert!(moves
            .iter()
            .all(|step| step.to_slot + step.slots <= step.from_slot));

        // A run that finds no room before it stays.
        let (moves, stays) = plan(&[(0..2, true), (3..4, false), (4..7, true)]);
        assert!(moves.is_empty());
        assert_eq!(stays, vec![(4..7)]);
    }
}
