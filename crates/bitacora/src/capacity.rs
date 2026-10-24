//! Capacity projection (contract section 11, C22).
//!
//! `bitacora capacity --history H.json --days N` reads a `History` and
//! prints `{"projected_physical_bytes": n}`: the bytes the volume file is
//! expected to occupy on its backing store (`st_blocks × 512`) after `N`
//! days of that history.
//!
//! The history is replayed day by day with the rules the engine applies:
//!
//! - A day is `rounds_per_day` rounds (one per snapshot, at least one).
//!   Each round writes its share of `write_blocks_per_day`: a fraction
//!   `overwrite_fraction` goes to blocks already written, the rest to new
//!   blocks until the volume is full (`History::split`). A round ends with
//!   a `flush`, and with a snapshot when the history takes them.
//! - Snapshots taken on day `d` are deleted at the end of day `d + r`, with
//!   `r = snapshot_retention_days`.
//! - Hot stream (section 3.3): every round's writes are packed in batches of
//!   up to 128 blocks behind a summary block, inside segments that start
//!   with a header block; a new segment opens when the open one has no room
//!   for a summary and one data block. Segments are taken lowest first and
//!   the file grows lazily (section 4), so until the GC runs the file holds
//!   exactly the blocks appended plus the two superblock slots.
//! - GC release rule (section 6, trigger of ADR 0001): `GcMode::Auto` only
//!   cleans when `GC_RESERVE_SEGMENTS` or fewer segments are free, and only
//!   the GC releases space (`punch_hole`). Once the hot stream opens the
//!   segment that leaves that many free, the GC keeps one more segment
//!   free than the reserve: `max_segments − GC_RESERVE_SEGMENTS − 1`
//!   segments in use, the open hot and cold segments among them. If the
//!   versions still referenced (live blocks plus dead versions that a
//!   retained snapshot holds) need more sealed segments than that level
//!   leaves, the sealed segments are the ones they need. The open hot
//!   segment holds what the replay put in it; the open cold segment is
//!   taken as half full.
//! - Space rule (section 4): the hot stream only takes a segment while more
//!   than `GC_RESERVE_SEGMENTS` are free, so the referenced versions must
//!   fit in `max_segments − GC_RESERVE_SEGMENTS − 2` sealed segments. If
//!   they do not, a write gets `NoSpace` and, from that day on, the
//!   projection is the cap: the file at `max_segments` segments.
//!
//! [hipótesis] Overwrites hit the written blocks uniformly, so a snapshot
//! keeps an expected `held × (1 − (1 − 1/N)^o)` dead versions after `o`
//! overwrites over `N` written blocks. The projection never exceeds
//! `physical_cap`.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE_U64, DATA_OFFSET, MAX_BATCH_BLOCKS, MAX_SNAPSHOTS};
use crate::gc::GC_RESERVE_SEGMENTS;
use crate::volume::{VolumeConfig, max_segments};

/// Bytes of the file prefix that hold data: the two superblock slots. The
/// rest of the first MiB is never written and stays a hole.
pub const PREFIX_ALLOCATED_BYTES: u64 = 2 * BLOCK_SIZE_U64;

/// Write history of a volume (input of `bitacora capacity`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub volume_size: u64,
    pub segment_size: u64,
    pub write_blocks_per_day: u64,
    /// Fraction of each round's writes that overwrite written blocks.
    pub overwrite_fraction: f64,
    pub snapshots_per_day: u64,
    pub snapshot_retention_days: u64,
}

/// Output of `bitacora capacity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub projected_physical_bytes: u64,
}

impl Projection {
    pub fn from_json(text: &str) -> Result<Projection> {
        serde_json::from_str(text)
            .map_err(|e| Error::InvalidArgument(format!("projection: {e}")))
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| Error::InvalidArgument(format!("projection: {e}")))
    }
}

impl History {
    /// Parses and validates a history.
    pub fn from_json(text: &str) -> Result<History> {
        let history: History = serde_json::from_str(text)
            .map_err(|e| Error::InvalidArgument(format!("history: {e}")))?;
        history.validate()?;
        Ok(history)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::InvalidArgument(format!("history: {e}")))
    }

    /// The geometry must be one `Volume::create` accepts, the fraction must
    /// be in `[0, 1]` and the retained snapshots must fit in the superblock.
    pub fn validate(&self) -> Result<()> {
        VolumeConfig::new(self.volume_size)
            .with_segment_size(self.segment_size)
            .validate()?;
        if !(0.0..=1.0).contains(&self.overwrite_fraction) {
            return Err(Error::InvalidArgument(format!(
                "overwrite_fraction {} is not in [0, 1]",
                self.overwrite_fraction
            )));
        }
        let retained = self
            .snapshot_retention_days
            .checked_add(1)
            .and_then(|days| days.checked_mul(self.snapshots_per_day));
        if retained.is_none_or(|n| n > MAX_SNAPSHOTS as u64) {
            return Err(Error::InvalidArgument(format!(
                "{} snapshots per day kept {} days exceed the {MAX_SNAPSHOTS} superblock entries",
                self.snapshots_per_day, self.snapshot_retention_days
            )));
        }
        Ok(())
    }

    pub fn blocks(&self) -> u64 {
        self.volume_size / BLOCK_SIZE_U64
    }

    pub fn max_segments(&self) -> u64 {
        max_segments(self.volume_size, self.segment_size)
    }

    /// One round per snapshot, at least one per day.
    pub fn rounds_per_day(&self) -> u64 {
        self.snapshots_per_day.max(1)
    }

    /// Writes of round `round` (from 0) of a day: the day's writes split as
    /// evenly as possible, the first rounds taking the remainder.
    pub fn round_writes(&self, round: u64) -> u64 {
        let rounds = self.rounds_per_day();
        self.write_blocks_per_day / rounds
            + u64::from(round < self.write_blocks_per_day % rounds)
    }

    /// `(new, overwrites)` for `writes` blocks when `written` distinct
    /// blocks were written before. With nothing written yet every write is
    /// new; writes that do not fit as new blocks are overwrites. The new
    /// blocks come first in the round.
    pub fn split(&self, written: u64, writes: u64) -> (u64, u64) {
        let wanted = if written == 0 {
            0
        } else {
            ((writes as f64 * self.overwrite_fraction).round() as u64).min(writes)
        };
        let new = (writes - wanted).min(self.blocks().saturating_sub(written));
        (new, writes - new)
    }
}

/// Data blocks a full hot segment holds: every block but the header and
/// one summary per batch of up to 128 blocks.
pub fn segment_payload_blocks(segment_size: u64) -> u64 {
    let after_header = segment_size / BLOCK_SIZE_U64 - 1;
    after_header - after_header.div_ceil(MAX_BATCH_BLOCKS as u64 + 1)
}

/// The largest file the space rule allows: the prefix plus `max_segments`
/// segments. No projection exceeds it.
pub fn physical_cap(volume_size: u64, segment_size: u64) -> u64 {
    DATA_OFFSET + max_segments(volume_size, segment_size) * segment_size
}

/// State of the replay at the end of a day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Day {
    /// From 1.
    pub day: u64,
    /// Distinct blocks written so far (the live blocks).
    pub written_blocks: u64,
    /// Expected dead versions held by retained snapshots.
    pub pinned_blocks: f64,
    /// Blocks appended to the hot stream so far: headers, summaries, data.
    pub hot_blocks: u64,
    /// The GC trigger was reached: the file stays at its GC level.
    pub gc_active: bool,
    /// A write would have received `NoSpace` on this day or before.
    pub no_space: bool,
    pub physical_bytes: u64,
}

/// The hot stream at block granularity.
struct HotStream {
    seg_blocks: u64,
    /// Next block of the open segment; 0 before the first segment.
    pos: u64,
    opened: u64,
    appended: u64,
}

impl HotStream {
    /// Appends `n` client blocks followed by a `flush`.
    fn append(&mut self, mut n: u64) {
        while n > 0 {
            if self.pos == 0 || self.seg_blocks - self.pos < 2 {
                self.opened += 1;
                self.pos = 1;
                self.appended += 1;
            }
            let k = n.min((self.seg_blocks - self.pos - 1).min(MAX_BATCH_BLOCKS as u64));
            self.pos += 1 + k;
            self.appended += 1 + k;
            n -= k;
        }
    }
}

/// A retained snapshot: the blocks it held when taken and the expected
/// fraction of them still live. Only the newest one's fraction changes;
/// see `pinned`.
struct Held {
    day: u64,
    blocks: f64,
    surviving: f64,
}

/// Dead versions held by snapshots. Each such version belongs to the
/// newest retained snapshot taken while it was live, so they add up
/// without double counting; retention deletes the oldest first.
fn pinned(snapshots: &[Held]) -> f64 {
    snapshots
        .iter()
        .map(|s| s.blocks * (1.0 - s.surviving))
        .sum()
}

/// Replays `days` days of `history` and returns the state after each one.
pub fn simulate(history: &History, days: u64) -> Result<Vec<Day>> {
    history.validate()?;
    let segment = history.segment_size;
    let max = history.max_segments();
    let payload = segment_payload_blocks(segment);
    // Sealed segments at the level the GC keeps (the open hot and cold
    // segments are the other two), and the most the space rule allows.
    let gc_sealed = max.saturating_sub(GC_RESERVE_SEGMENTS + 3);
    let sealed_room = max.saturating_sub(GC_RESERVE_SEGMENTS + 2);
    let cap = physical_cap(history.volume_size, segment);

    let mut hot = HotStream {
        seg_blocks: segment / BLOCK_SIZE_U64,
        pos: 0,
        opened: 0,
        appended: 0,
    };
    let mut written = 0u64;
    let mut snapshots: Vec<Held> = Vec::new();
    let mut gc_active = false;
    let mut no_space = false;
    let mut sealed = gc_sealed;
    let mut out = Vec::new();
    for day in 1..=days {
        for round in 0..history.rounds_per_day() {
            let (new, overwrites) = history.split(written, history.round_writes(round));
            written += new;
            if overwrites > 0
                && let Some(last) = snapshots.last_mut()
            {
                let miss = 1.0 - 1.0 / written as f64;
                last.surviving *= miss.powf(overwrites as f64);
            }
            hot.append(new + overwrites);
            gc_active |= hot.opened >= max.saturating_sub(GC_RESERVE_SEGMENTS);
            if gc_active {
                let referenced = written as f64 + pinned(&snapshots);
                let needed = (referenced / payload as f64).ceil() as u64;
                no_space |= needed > sealed_room;
                sealed = gc_sealed.max(needed);
            }
            if history.snapshots_per_day > 0 {
                snapshots.push(Held {
                    day,
                    blocks: written as f64,
                    surviving: 1.0,
                });
            }
        }
        snapshots.retain(|s| s.day + history.snapshot_retention_days > day);

        let physical = if no_space {
            cap
        } else if gc_active {
            PREFIX_ALLOCATED_BYTES + sealed * segment + hot.pos * BLOCK_SIZE_U64 + segment / 2
        } else {
            PREFIX_ALLOCATED_BYTES + hot.appended * BLOCK_SIZE_U64
        };
        out.push(Day {
            day,
            written_blocks: written,
            pinned_blocks: pinned(&snapshots),
            hot_blocks: hot.appended,
            gc_active,
            no_space,
            physical_bytes: physical.min(cap),
        });
    }
    Ok(out)
}

/// Physical bytes of the volume file after `days` days of `history`.
pub fn project(history: &History, days: u64) -> Result<Projection> {
    let days = simulate(history, days)?;
    Ok(Projection {
        projected_physical_bytes: days
            .last()
            .map_or(PREFIX_ALLOCATED_BYTES, |d| d.physical_bytes),
    })
}
