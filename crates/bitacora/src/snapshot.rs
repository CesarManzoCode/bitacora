//! Copy-on-write snapshots and raw export (contract sections 5 and 7).
//!
//! A snapshot is a frozen clone of the paged index: pages are shared with
//! the live index until a write copies one. Creating it flushes first, so
//! every `birth` it references is durable, then adds one reference to each
//! of those births and records `(id, lsn)` in the superblock, which is
//! synced before returning. Deleting it sets the deleted flag in the
//! superblock, syncs it, and only then removes the references; a record
//! left with `refcount = 0` is dead and the GC (or `compact`) reclaims it.
//! The flag is synced first so that after a crash the snapshot never comes
//! back pointing at space the GC already freed.
//!
//! Recovery rebuilds each snapshot from the log (`volume/recovery.rs`).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE_U64, MAX_SNAPSHOTS, SNAPSHOT_FLAG_DELETED, SnapshotEntry};
use crate::index::Index;
use crate::volume::Volume;

/// Blocks per read when exporting.
const EXPORT_CHUNK: u64 = 256;

/// A version of the volume: the live one or a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Live,
    Snapshot(u64),
}

/// A snapshot that was not deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: u64,
    /// Last LSN the snapshot contains.
    pub lsn: u64,
}

/// In-memory state of one snapshot.
pub(crate) struct Frozen {
    pub(crate) lsn: u64,
    pub(crate) index: Index,
}

impl Volume {
    /// Freezes the current contents and returns the new snapshot id. Every
    /// write acknowledged before the call is in the snapshot and durable.
    pub fn snapshot_create(&self) -> Result<u64> {
        let mut inner = self.core.lock()?;
        inner.usable()?;
        inner.flush()?;
        let table = &mut inner.sb.snapshots;
        let id = table.iter().map(|s| s.snapshot_id).max().unwrap_or(0) + 1;
        if table.len() >= MAX_SNAPSHOTS {
            // Deleted entries can go; the newest one stays so ids are never
            // reused.
            table.retain(|s| !s.is_deleted() || s.snapshot_id == id - 1);
        }
        if table.len() >= MAX_SNAPSHOTS {
            return Err(Error::InvalidArgument(format!(
                "the snapshot table is full ({MAX_SNAPSHOTS} snapshots)"
            )));
        }
        let lsn = inner.next_lsn - 1;
        let index = inner.index.clone();
        for (_, birth) in index.written() {
            inner.records.incref(birth);
        }
        inner.snapshots.insert(id, Frozen { lsn, index });
        inner.sb.snapshots.push(SnapshotEntry {
            snapshot_id: id,
            lsn,
            flags: 0,
        });
        inner.sb.durable_lsn = inner.synced_lsn;
        inner.write_superblock()?;
        Ok(id)
    }

    /// Deletes snapshot `id`. The space only it held becomes reclaimable.
    pub fn snapshot_delete(&self, id: u64) -> Result<()> {
        let mut inner = self.core.lock()?;
        inner.usable()?;
        let Some(entry) = inner
            .sb
            .snapshots
            .iter_mut()
            .find(|s| s.snapshot_id == id && !s.is_deleted())
        else {
            return Err(Error::InvalidArgument(format!("no snapshot {id}")));
        };
        entry.flags |= SNAPSHOT_FLAG_DELETED;
        inner.sb.durable_lsn = inner.synced_lsn;
        inner.write_superblock()?;
        let frozen = inner
            .snapshots
            .remove(&id)
            .ok_or_else(|| Error::Corrupt(format!("snapshot {id} has no index")))?;
        for (_, birth) in frozen.index.written() {
            if inner.records.decref(birth) {
                inner.changes += 1;
            }
        }
        inner.gc.kick = true;
        self.core.kick_gc(&mut inner);
        Ok(())
    }

    /// Snapshots not deleted, by id.
    pub fn snapshots(&self) -> Vec<SnapshotInfo> {
        let inner = match self.core.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner
            .snapshots
            .iter()
            .map(|(id, f)| SnapshotInfo { id: *id, lsn: f.lsn })
            .collect()
    }

    /// Reads `nblocks` blocks of snapshot `id` from `lba`, with the same
    /// rules as `read`.
    pub fn read_snapshot(&self, id: u64, lba: u64, nblocks: u64) -> Result<Vec<u8>> {
        self.read_version(Some(id), lba, nblocks)
    }

    /// Reads `nblocks` blocks of `version` from `lba`.
    pub fn read_at_version(&self, version: Version, lba: u64, nblocks: u64) -> Result<Vec<u8>> {
        match version {
            Version::Live => self.read(lba, nblocks),
            Version::Snapshot(id) => self.read_snapshot(id, lba, nblocks),
        }
    }

    /// Writes the raw image of `version` (`volume_size` bytes) to a new file
    /// at `path`; blocks never written are left as holes. A snapshot gives a
    /// point-in-time image; the live volume is read chunk by chunk, so writes
    /// running during the export may land in some chunks and not others.
    pub fn export(&self, version: Version, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        file.set_len(self.volume_size())?;
        let blocks = self.blocks();
        let mut lba = 0;
        while lba < blocks {
            let n = EXPORT_CHUNK.min(blocks - lba);
            let data = self.read_at_version(version, lba, n)?;
            if data.iter().any(|&b| b != 0) {
                file.write_all_at(&data, lba * BLOCK_SIZE_U64)?;
            }
            lba += n;
        }
        file.sync_all()?;
        Ok(())
    }
}
