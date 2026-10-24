//! Engine side of replication (contract section 9).
//!
//! - `write_lsn` is `write` that returns the LSN of its last block: the
//!   primary forwards each block to the followers with that LSN, and a
//!   follower applies an `APPEND` with plain writes, which receive the same
//!   LSNs because its log ends where the primary's `prev_lsn` says.
//! - `set_epoch` persists `current_epoch` and `epoch_history` in the
//!   superblock with `fdatasync` before returning.
//! - A `Pin` is a frozen copy of the live index kept only in memory. It
//!   holds a reference to every `birth` it sees, so the GC cannot reclaim
//!   them while the primary reads a consistent version for a resync, and it
//!   leaves nothing in the superblock: after a crash there is nothing to
//!   clean up.
//! - `apply_resync` writes blocks received from the primary to the resync
//!   stream (stream 2) with their original `birth`, and points the live
//!   index at each one newer than what the LBA holds. `finish_resync` syncs
//!   them and moves the next LSN past the primary's: the hot segment open at
//!   that point is sealed, so the next client block opens a new hot segment
//!   whose `first_lsn` follows the resync, as recovery expects.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE, BatchEntry, EpochEntry, MAX_EPOCH_HISTORY, Stream, data_crc};
use crate::gc::GC_RESERVE_SEGMENTS;
use crate::snapshot::Frozen;

use super::{Core, Inner, Volume};

/// Ids from here up name pins in the snapshot map; the superblock table
/// never reaches them.
pub(crate) const PIN_BASE: u64 = 1 << 63;

/// A version of the volume frozen in memory (see the module documentation).
/// Dropping it releases its references.
pub struct Pin {
    core: Arc<Core>,
    id: u64,
    lsn: u64,
}

impl Pin {
    /// Last LSN the pinned version contains.
    pub fn lsn(&self) -> u64 {
        self.lsn
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut inner = match self.core.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(frozen) = inner.snapshots.remove(&self.id) else {
            return;
        };
        for (_, birth) in frozen.index.written() {
            if inner.records.decref(birth) {
                inner.changes += 1;
            }
        }
        inner.gc.kick = true;
        self.core.kick_gc(&mut inner);
    }
}

impl Volume {
    /// `write` that returns the LSN of the last block written. The blocks
    /// of one call receive consecutive LSNs as long as no other thread
    /// writes to the volume at the same time (the replica serialises its
    /// writes).
    pub fn write_lsn(&self, lba: u64, data: &[u8]) -> Result<u64> {
        let mut inner = self.core.lock()?;
        inner.usable()?;
        inner = self.core.append_waiting(inner, lba, data)?;
        self.core.kick_gc(&mut inner);
        Ok(inner.next_lsn - 1)
    }

    /// `epoch_history` of the superblock.
    pub fn epoch_history(&self) -> Vec<EpochEntry> {
        match self.core.inner.lock() {
            Ok(inner) => inner.sb.epoch_history.clone(),
            Err(poisoned) => poisoned.into_inner().sb.epoch_history.clone(),
        }
    }

    /// Flushes, then sets `current_epoch` (and `epoch_history` when given)
    /// and writes the superblock with `fdatasync`. Durable when it returns.
    pub fn set_epoch(&self, epoch: u64, history: Option<&[EpochEntry]>) -> Result<()> {
        if history.is_some_and(|h| h.len() > MAX_EPOCH_HISTORY) {
            return Err(Error::InvalidArgument(format!(
                "epoch history longer than {MAX_EPOCH_HISTORY} entries"
            )));
        }
        let mut inner = self.core.lock()?;
        inner.usable()?;
        inner.flush()?;
        inner.sb.current_epoch = epoch;
        if let Some(h) = history {
            inner.sb.epoch_history = h.to_vec();
        }
        inner.sb.durable_lsn = inner.synced_lsn;
        inner.write_superblock()
    }

    /// Freezes the live index in memory (see `Pin`).
    pub fn pin(&self) -> Result<Pin> {
        let mut inner = self.core.lock()?;
        inner.usable()?;
        let id = inner
            .snapshots
            .range(PIN_BASE..)
            .next_back()
            .map_or(PIN_BASE, |(id, _)| id + 1);
        let lsn = inner.next_lsn - 1;
        let index = inner.index.clone();
        for (_, birth) in index.written() {
            inner.records.incref(birth);
        }
        inner.snapshots.insert(id, Frozen { lsn, index });
        Ok(Pin {
            core: self.core.clone(),
            id,
            lsn,
        })
    }

    /// `(lba, birth)` of every block of the pinned version whose `birth` is
    /// greater than `after`, in LBA order.
    pub fn pinned_births_after(&self, pin: &Pin, after: u64) -> Result<Vec<(u64, u64)>> {
        let inner = self.core.lock()?;
        inner.usable()?;
        let frozen = inner
            .snapshots
            .get(&pin.id)
            .ok_or_else(|| Error::InvalidArgument("pin of another volume".into()))?;
        Ok(frozen
            .index
            .written()
            .filter(|(_, birth)| *birth > after)
            .collect())
    }

    /// Reads `nblocks` blocks of the pinned version from `lba`.
    pub fn read_pinned(&self, pin: &Pin, lba: u64, nblocks: u64) -> Result<Vec<u8>> {
        self.read_version(Some(pin.id), lba, nblocks)
    }

    /// Writes `(lba, birth, data)` blocks received from the primary to the
    /// resync stream. A block whose `birth` is not newer than the one its
    /// LBA holds, or that the volume already has, is skipped. Durable only
    /// after `finish_resync` (or `flush`).
    pub fn apply_resync(&self, items: &[(u64, u64, &[u8])]) -> Result<()> {
        let core = &*self.core;
        let mut inner = core.lock()?;
        inner.usable()?;
        for (lba, birth, data) in items {
            if data.len() != BLOCK_SIZE || *birth == 0 {
                return Err(Error::InvalidArgument(format!(
                    "resync block for lba {lba} with birth {birth} and {} bytes",
                    data.len()
                )));
            }
            core.geo.check_range(*lba, 1)?;
        }
        // The open hot batch goes to the medium first: its blocks are read
        // from memory only while it is open.
        inner.emit_batch()?;
        let fresh: Vec<(BatchEntry, &[u8])> = items
            .iter()
            .filter(|(lba, birth, _)| {
                *birth > inner.index.get(*lba) && !inner.records.contains(*birth)
            })
            .map(|(lba, birth, data)| {
                let entry = BatchEntry {
                    lba: *lba,
                    birth: *birth,
                    data_crc: data_crc(*lba, *birth, data),
                    flags: 0,
                };
                (entry, *data)
            })
            .collect();
        let mut done = 0;
        while done < fresh.len() {
            inner = wait_for_resync_room(core, inner)?;
            let Some((k, segment, first)) = inner.append_kept(Stream::Resync, &fresh[done..])?
            else {
                return Err(Error::NoSpace);
            };
            let seg = segment as u32;
            for (j, (e, _)) in fresh[done..done + k].iter().enumerate() {
                let block = (first + j as u64) as u32;
                inner.records.insert(e.birth, seg, block, e.data_crc);
                let old = inner.index.set(e.lba, e.birth);
                if old != 0 && inner.records.decref(old) {
                    inner.changes += 1;
                }
            }
            done += k;
        }
        core.kick_gc(&mut inner);
        Ok(())
    }

    /// Syncs every block applied by `apply_resync` and makes `target` (the
    /// primary's last LSN) the last LSN of this volume if it is ahead.
    pub fn finish_resync(&self, target: u64) -> Result<()> {
        let mut inner = self.core.lock()?;
        inner.usable()?;
        inner.flush()?;
        if let Some(open) = inner.resync.take() {
            inner.seal(open.index);
        }
        if target >= inner.next_lsn {
            if let Some(open) = inner.hot.take() {
                inner.seal(open.index);
            }
            inner.next_lsn = target + 1;
            inner.emitted_lsn = target;
            inner.synced_lsn = target;
        }
        Ok(())
    }
}

impl Inner {
    /// Marks a synced segment as sealed: no more batches go to it.
    fn seal(&mut self, segment: u64) {
        if let Some(info) = self.segments[segment as usize].as_mut() {
            info.sealed = true;
            self.changes += 1;
        }
    }
}

/// Before the resync stream takes a new segment, waits for the GC while
/// only the reserve of the cold stream is free, as client writes do.
fn wait_for_resync_room<'a>(
    core: &'a Core,
    mut inner: std::sync::MutexGuard<'a, Inner>,
) -> Result<std::sync::MutexGuard<'a, Inner>> {
    loop {
        let room = inner
            .resync
            .map_or(0, |r| inner.geo.seg_blocks - r.next_block);
        if room >= 2 || inner.free_segments > GC_RESERVE_SEGMENTS || !inner.gc_can_help() {
            return Ok(inner);
        }
        inner.gc.waiters += 1;
        core.gc_wake.notify_all();
        inner = core.space.wait(inner).map_err(|_| Error::Failed)?;
        inner.gc.waiters -= 1;
        inner.usable()?;
    }
}
