//! Paged copy-on-write index and record table (contract section 5).
//!
//! The index maps every LBA to the `birth` of its current block (0 = never
//! written). It is a vector of shared pages, so a snapshot is a clone of the
//! vector and a write copies only the page it touches (`Arc::make_mut`).
//!
//! The record table maps a `birth` to where its block lives on the medium and
//! how many versions (live volume plus snapshots) reference it. The GC and
//! the replication move records; they never touch the index.

use std::collections::HashMap;
use std::sync::Arc;

/// Entries per index page.
pub const PAGE_ENTRIES: usize = 512;

/// One index page: the `birth` of `PAGE_ENTRIES` consecutive LBAs.
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    births: [u64; PAGE_ENTRIES],
}

impl Page {
    fn zeroed() -> Self {
        Page {
            births: [0; PAGE_ENTRIES],
        }
    }

    /// The `birth` stored at `slot` (0 = never written).
    pub fn get(&self, slot: usize) -> u64 {
        self.births[slot]
    }

    /// All entries of the page.
    pub fn births(&self) -> &[u64; PAGE_ENTRIES] {
        &self.births
    }
}

/// Paged copy-on-write index: LBA → `birth`.
#[derive(Clone)]
pub struct Index {
    pages: Vec<Option<Arc<Page>>>,
    blocks: u64,
}

impl Index {
    /// An index for `blocks` LBAs, all never written.
    pub fn new(blocks: u64) -> Self {
        let pages = blocks.div_ceil(PAGE_ENTRIES as u64) as usize;
        Index {
            pages: vec![None; pages],
            blocks,
        }
    }

    /// Number of LBAs covered.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// The `birth` of the current block of `lba`; 0 if never written.
    pub fn get(&self, lba: u64) -> u64 {
        let (page, slot) = split(lba);
        match &self.pages[page] {
            Some(p) => p.get(slot),
            None => 0,
        }
    }

    /// Points `lba` at `birth` and returns the previous `birth` (0 if none).
    /// A page shared with a snapshot is copied first.
    pub fn set(&mut self, lba: u64, birth: u64) -> u64 {
        let (page, slot) = split(lba);
        let p = self.pages[page].get_or_insert_with(|| Arc::new(Page::zeroed()));
        let p = Arc::make_mut(p);
        std::mem::replace(&mut p.births[slot], birth)
    }

    /// The pages, `None` where no LBA of the page was ever written. Two
    /// versions share a page exactly when `Arc::ptr_eq` holds.
    pub fn pages(&self) -> &[Option<Arc<Page>>] {
        &self.pages
    }

    /// Replaces every page equal to the same page of `other` with a pointer
    /// to it, so that both versions share it (used after recovery, which
    /// builds each version separately).
    pub fn share_with(&mut self, other: &Index) {
        for (mine, theirs) in self.pages.iter_mut().zip(&other.pages) {
            let Some(b) = theirs else {
                continue;
            };
            let equal = mine.as_ref().is_some_and(|a| !Arc::ptr_eq(a, b) && **a == **b);
            if equal {
                *mine = Some(b.clone());
            }
        }
    }

    /// Every written LBA with its `birth`, in LBA order.
    pub fn written(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.pages.iter().enumerate().flat_map(|(pi, p)| {
            p.iter().flat_map(move |page| {
                page.births
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b != 0)
                    .map(move |(slot, b)| ((pi * PAGE_ENTRIES + slot) as u64, *b))
            })
        })
    }
}

fn split(lba: u64) -> (usize, usize) {
    let lba = lba as usize;
    (lba / PAGE_ENTRIES, lba % PAGE_ENTRIES)
}

/// Location and reference count of one written block version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Segment number.
    pub segment: u32,
    /// Block number within the segment.
    pub block: u32,
    /// `data_crc` of the block, checked on every read from the medium.
    pub data_crc: u32,
    /// Versions (live volume and snapshots) whose index points at this birth.
    pub refcount: u32,
}

/// `birth` → `Record`, with the count of live blocks per segment.
pub struct RecordTable {
    records: HashMap<u64, Record>,
    live: Vec<u64>,
}

impl RecordTable {
    /// An empty table for a volume of `segments` segments.
    pub fn new(segments: usize) -> Self {
        RecordTable {
            records: HashMap::new(),
            live: vec![0; segments],
        }
    }

    /// The record of `birth`, if it is live.
    pub fn get(&self, birth: u64) -> Option<&Record> {
        self.records.get(&birth)
    }

    /// Whether `birth` has a live record.
    pub fn contains(&self, birth: u64) -> bool {
        self.records.contains_key(&birth)
    }

    /// Adds a live record with `refcount = 1`. Returns `false` and changes
    /// nothing if `birth` already has a record.
    pub fn insert(&mut self, birth: u64, segment: u32, block: u32, data_crc: u32) -> bool {
        if self.records.contains_key(&birth) {
            return false;
        }
        self.records.insert(
            birth,
            Record {
                segment,
                block,
                data_crc,
                refcount: 1,
            },
        );
        self.live[segment as usize] += 1;
        true
    }

    /// One more version references `birth`.
    pub fn incref(&mut self, birth: u64) {
        if let Some(r) = self.records.get_mut(&birth) {
            r.refcount += 1;
        }
    }

    /// One version less references `birth`. Returns `true` if the record
    /// died: it is removed and stops counting as live in its segment.
    pub fn decref(&mut self, birth: u64) -> bool {
        let Some(r) = self.records.get_mut(&birth) else {
            return false;
        };
        r.refcount -= 1;
        if r.refcount > 0 {
            return false;
        }
        let segment = r.segment as usize;
        self.records.remove(&birth);
        self.live[segment] -= 1;
        true
    }

    /// Moves a live record to a new location (GC relocation, resync).
    /// Returns `false` if `birth` is no longer live.
    pub fn relocate(&mut self, birth: u64, segment: u32, block: u32) -> bool {
        let Some(r) = self.records.get_mut(&birth) else {
            return false;
        };
        self.live[r.segment as usize] -= 1;
        self.live[segment as usize] += 1;
        r.segment = segment;
        r.block = block;
        true
    }

    /// Live blocks stored in `segment`.
    pub fn live_blocks(&self, segment: u64) -> u64 {
        self.live[segment as usize]
    }

    /// Number of live records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether no record is live.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every live `(birth, record)`, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Record)> + '_ {
        self.records.iter().map(|(b, r)| (*b, r))
    }
}
