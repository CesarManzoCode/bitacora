//! Garbage collection (contract section 6).
//!
//! Policy: LFS cost-benefit, `score = (1 − u) × age / (1 + u)` with `u` =
//! live blocks / blocks of the segment and `age` = current LSN − greatest
//! `birth` of the segment. Only sealed (and therefore synced) segments are
//! candidates, and only if they hold dead data. A segment with no live block
//! is freed first, since it costs nothing.
//!
//! Cleaning cycle of a victim:
//! 1. read the whole victim outside the lock and follow its batch summaries;
//! 2. copy the blocks still live (record at that exact segment and block) to
//!    the cold stream, keeping `lba`, `birth` and `data_crc`;
//! 3. under the lock, emit the open hot batch and `fdatasync` once: that
//!    makes the cold copies durable and also every write that killed a
//!    record of the victim, so a freed version can never be needed again
//!    after a crash;
//! 4. move each record to its copy only if the `birth` is still live at the
//!    victim; the index is never touched;
//! 5. take the reuse lock exclusively (no read in flight on the victim),
//!    `punch_hole` the victim and mark it free.
//!
//! One GC thread per volume. `GcMode::Auto` runs while free segments are
//! below 10 % of `max_segments`, limited by a 256 MiB/s token bucket; the
//! limit is ignored when `GC_RESERVE_SEGMENTS` or fewer segments are free,
//! and then client writes that need a segment wait (backpressure).
//! `GcMode::Continuous` cleans without pause while there are victims.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE, BLOCK_SIZE_U64, BatchEntry, BatchSummary, data_crc, segment_offset};
use crate::medium::AlignedBuf;
use crate::volume::{Core, GcMode, Inner, VolumeState};

/// Free segments kept for the cold stream; at this level or below, the
/// bandwidth limit is ignored and client writes wait for the GC.
pub const GC_RESERVE_SEGMENTS: u64 = 2;

/// Token bucket rate: 256 MiB/s.
pub const GC_RATE_BYTES_PER_SEC: u64 = 256 << 20;

/// Token bucket capacity (burst).
const GC_BURST_BYTES: f64 = (16 << 20) as f64;

/// GC counters reported by `Volume::status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStatus {
    pub mode: GcMode,
    /// A cleaning cycle (GC thread or `compact`) is in progress.
    pub running: bool,
    /// Segments freed since the volume was opened.
    pub segments_cleaned: u64,
    /// Bytes copied to the cold stream and adopted by the record table.
    pub bytes_relocated: u64,
}

/// GC bookkeeping kept in `Inner`.
pub(crate) struct GcState {
    pub(crate) mode: GcMode,
    /// `close` or drop asked the GC thread to stop.
    pub(crate) stop: bool,
    pub(crate) running: bool,
    pub(crate) segments_cleaned: u64,
    pub(crate) bytes_relocated: u64,
    /// Value of `Inner::changes` when the GC last found nothing it could
    /// free. While it stays equal, the GC cannot help.
    pub(crate) stuck_at: Option<u64>,
    /// Client writes waiting for a free segment.
    pub(crate) waiters: u64,
    /// Wake the GC thread after the current operation.
    pub(crate) kick: bool,
}

impl GcState {
    pub(crate) fn new(mode: GcMode) -> Self {
        GcState {
            mode,
            stop: false,
            running: false,
            segments_cleaned: 0,
            bytes_relocated: 0,
            stuck_at: None,
            waiters: 0,
            kick: false,
        }
    }

    pub(crate) fn status(&self) -> GcStatus {
        GcStatus {
            mode: self.mode,
            running: self.running,
            segments_cleaned: self.segments_cleaned,
            bytes_relocated: self.bytes_relocated,
        }
    }
}

/// Cost-benefit score of contract section 6.
pub fn cost_benefit(live_blocks: u64, segment_blocks: u64, age: u64) -> f64 {
    let u = live_blocks as f64 / segment_blocks as f64;
    (1.0 - u) * age as f64 / (1.0 + u)
}

impl Inner {
    /// Whether waiting for the GC can produce a free segment.
    pub(crate) fn gc_can_help(&self) -> bool {
        self.gc.mode != GcMode::Off
            && !self.gc.stop
            && self.state == VolumeState::Open
            && self.gc.stuck_at != Some(self.changes)
    }

    /// Free segments are at or below the reserve.
    pub(crate) fn gc_critical(&self) -> bool {
        self.free_segments <= GC_RESERVE_SEGMENTS
    }

    /// The trigger of the mode holds (ignoring whether the GC is stuck).
    pub(crate) fn gc_wanted(&self) -> bool {
        match self.gc.mode {
            GcMode::Off => false,
            GcMode::Continuous => true,
            GcMode::Auto => self.free_segments * 10 < self.geo.max_segments,
        }
    }

    /// The GC thread has work to do now.
    fn gc_should_run(&self) -> bool {
        self.gc_can_help() && (self.gc.waiters > 0 || self.gc_wanted())
    }

    /// Whether `birth` is live and its record is at `segment`, `block`.
    fn live_at(&self, birth: u64, segment: u64, block: u64) -> bool {
        self.records.get(birth).is_some_and(|r| {
            u64::from(r.segment) == segment && u64::from(r.block) == block
        })
    }

    /// The best victim by cost-benefit among sealed segments with dead data.
    pub(crate) fn pick_victim(&self) -> Option<u64> {
        let now = self.next_lsn - 1;
        let mut best: Option<(f64, u64)> = None;
        for (i, s) in self.segments.iter().enumerate() {
            let Some(info) = s else {
                continue;
            };
            let i = i as u64;
            if !info.sealed || info.damaged {
                continue;
            }
            let live = self.records.live_blocks(i);
            if live == 0 {
                return Some(i);
            }
            if live >= info.data_blocks {
                continue;
            }
            let score = cost_benefit(
                live,
                self.geo.seg_blocks,
                now.saturating_sub(info.max_birth),
            );
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, i));
            }
        }
        best.map(|(_, i)| i)
    }

    fn mark_damaged(&mut self, segment: u64) {
        if let Some(info) = self.segments[segment as usize].as_mut() {
            info.damaged = true;
        }
    }
}

/// Result of one cleaning cycle.
enum Outcome {
    /// The victim was freed.
    Freed,
    /// The victim cannot be cleaned (no longer sealed, or damaged).
    Skipped,
    /// No free segment is left for the cold stream.
    NoRoom,
    /// The GC was asked to stop mid-cycle.
    Stopped,
}

/// 256 MiB/s token bucket. Waiting is interruptible: it ends early on stop
/// and the limit is dropped while free segments are at the reserve.
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new() -> Self {
        TokenBucket {
            tokens: GC_BURST_BYTES,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens =
            (self.tokens + elapsed * GC_RATE_BYTES_PER_SEC as f64).min(GC_BURST_BYTES);
    }

    /// Spends `bytes` tokens, waiting for the deficit if there is one.
    fn charge(&mut self, core: &Core, bytes: u64) -> Result<()> {
        self.refill();
        self.tokens -= bytes as f64;
        while self.tokens < 0.0 {
            let wait = Duration::from_secs_f64(-self.tokens / GC_RATE_BYTES_PER_SEC as f64);
            let inner = core.lock()?;
            if inner.gc.stop || inner.state != VolumeState::Open || inner.gc_critical() {
                self.tokens = 0.0;
                return Ok(());
            }
            let _ = core
                .gc_wake
                .wait_timeout(inner, wait)
                .map_err(|_| Error::Failed)?;
            self.refill();
        }
        Ok(())
    }
}

/// Starts the GC thread of a volume.
pub(crate) fn spawn(core: Arc<Core>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("bitacora-gc".into())
        .spawn(move || run(&core))
}

/// Body of the GC thread: wait for work, clean one victim, repeat until
/// stopped or the volume leaves the `Open` state.
fn run(core: &Core) {
    let mut bucket = TokenBucket::new();
    loop {
        {
            let Ok(mut inner) = core.inner.lock() else {
                return;
            };
            loop {
                if inner.gc.stop || inner.state != VolumeState::Open {
                    drop(inner);
                    core.space.notify_all();
                    return;
                }
                if inner.gc_should_run() {
                    break;
                }
                inner = match core.gc_wake.wait(inner) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
        }
        let limited = core
            .inner
            .lock()
            .is_ok_and(|inner| inner.gc.mode == GcMode::Auto);
        let limit = if limited { Some(&mut bucket) } else { None };
        let outcome = cycle(core, limit);
        if let Ok(mut inner) = core.inner.lock()
            && !matches!(outcome, Ok(Some(Outcome::Freed | Outcome::Skipped)))
        {
            inner.gc.stuck_at = Some(inner.changes);
        }
        core.space.notify_all();
    }
}

/// Picks a victim and cleans it. `None` if there was no victim.
fn cycle(core: &Core, bucket: Option<&mut TokenBucket>) -> Result<Option<Outcome>> {
    let _cycle = core.gc_lock.lock().map_err(|_| Error::Failed)?;
    let victim = {
        let inner = core.lock()?;
        inner.usable()?;
        inner.pick_victim()
    };
    match victim {
        None => Ok(None),
        Some(v) => clean_marked(core, v, bucket).map(Some),
    }
}

/// `Volume::compact`: cleans every sealed segment with dead data, without
/// the bandwidth limit.
pub(crate) fn compact(core: &Core) -> Result<()> {
    let _cycle = core.gc_lock.lock().map_err(|_| Error::Failed)?;
    // Bounded so that concurrent writers cannot keep it running forever.
    let limit = 2 * core.geo.max_segments + 8;
    for _ in 0..limit {
        let victim = {
            let inner = core.lock()?;
            inner.usable()?;
            inner.pick_victim()
        };
        let Some(v) = victim else {
            return Ok(());
        };
        match clean_marked(core, v, None)? {
            Outcome::Freed | Outcome::Skipped => {}
            Outcome::NoRoom => return Err(Error::NoSpace),
            Outcome::Stopped => return core.lock()?.usable(),
        }
        core.space.notify_all();
    }
    Ok(())
}

/// `clean` with the `running` flag set around it.
fn clean_marked(core: &Core, victim: u64, bucket: Option<&mut TokenBucket>) -> Result<Outcome> {
    core.lock()?.gc.running = true;
    let r = clean(core, victim, bucket);
    match core.inner.lock() {
        Ok(mut inner) => inner.gc.running = false,
        Err(poisoned) => poisoned.into_inner().gc.running = false,
    }
    r
}

/// One cleaning cycle of `victim` (see the module documentation).
fn clean(core: &Core, victim: u64, mut bucket: Option<&mut TokenBucket>) -> Result<Outcome> {
    let geo = core.geo;
    let seg_gen = {
        let inner = core.lock()?;
        inner.usable()?;
        match inner.segments[victim as usize] {
            Some(info) if info.sealed && !info.damaged => info.seg_gen,
            _ => return Ok(Outcome::Skipped),
        }
    };

    // 1. Read the victim and follow its chain of summaries. Only the GC
    // frees segments and cycles are serialised, so the victim stays put.
    let mut buf = AlignedBuf::zeroed(geo.segment_size as usize);
    let got = match core
        .medium
        .read_at(&mut buf, segment_offset(victim, geo.segment_size))
    {
        Ok(n) => n,
        Err(_) => {
            core.lock()?.mark_damaged(victim);
            return Ok(Outcome::Skipped);
        }
    };
    let mut found: Vec<(BatchEntry, u64)> = Vec::new();
    let mut block = 1u64;
    while block + 2 <= geo.seg_blocks {
        let start = block as usize * BLOCK_SIZE;
        if start + BLOCK_SIZE > got {
            break;
        }
        let Ok(summary) = BatchSummary::decode(&buf[start..start + BLOCK_SIZE], &geo.volume_id)
        else {
            break;
        };
        let count = summary.entries.len() as u64;
        if summary.seg_gen != seg_gen || block + 1 + count > geo.seg_blocks {
            break;
        }
        for (j, e) in summary.entries.iter().enumerate() {
            found.push((*e, block + 1 + j as u64));
        }
        block += 1 + count;
    }

    // 2. Copy the live blocks to the cold stream, one batch at a time.
    let mut moved: Vec<(u64, u64, u32, u32)> = Vec::new();
    let mut pos = 0;
    while pos < found.len() {
        let mut inner = core.lock()?;
        inner.usable()?;
        if inner.gc.stop {
            return Ok(Outcome::Stopped);
        }
        let mut chunk: Vec<(BatchEntry, u64)> = Vec::new();
        while pos < found.len() && chunk.len() < crate::format::MAX_BATCH_BLOCKS {
            let (e, b) = found[pos];
            pos += 1;
            if inner.live_at(e.birth, victim, b) {
                chunk.push((e, b));
            }
        }
        let mut done = 0;
        while done < chunk.len() {
            let mut items = Vec::with_capacity(chunk.len() - done);
            for (e, b) in &chunk[done..] {
                let start = *b as usize * BLOCK_SIZE;
                let data = buf.get(start..start + BLOCK_SIZE).filter(|_| start + BLOCK_SIZE <= got);
                match data {
                    Some(d) if data_crc(e.lba, e.birth, d) == e.data_crc => items.push((*e, d)),
                    _ => {
                        // A live block that fails its check stays where it
                        // is (reads report it); the victim is not reclaimed.
                        inner.mark_damaged(victim);
                        return Ok(Outcome::Skipped);
                    }
                }
            }
            let Some((k, segment, first)) = inner.append_cold(&items)? else {
                return Ok(Outcome::NoRoom);
            };
            for (j, (e, b)) in chunk[done..done + k].iter().enumerate() {
                moved.push((e.birth, *b, segment as u32, (first + j as u64) as u32));
            }
            done += k;
        }
        drop(inner);
        if let Some(bucket) = bucket.as_deref_mut() {
            bucket.charge(core, chunk.len() as u64 * BLOCK_SIZE_U64)?;
        }
    }

    // 3 and 4. One `fdatasync` covers the cold copies and the hot writes
    // that killed the victim's records; then move the records still live.
    let mut inner = core.lock()?;
    inner.usable()?;
    inner.flush()?;
    let mut relocated = 0u64;
    for (birth, old_block, segment, block) in moved {
        if inner.live_at(birth, victim, old_block) && inner.records.relocate(birth, segment, block)
        {
            relocated += 1;
        }
    }
    inner.gc.bytes_relocated += relocated * BLOCK_SIZE_U64;
    if inner.records.live_blocks(victim) != 0 {
        // A live record the summaries do not list: never free it.
        inner.mark_damaged(victim);
        return Ok(Outcome::Skipped);
    }
    drop(inner);

    // 5. Wait for reads in flight on the victim, then free it.
    let _reuse = core.reuse.write().map_err(|_| Error::Failed)?;
    let mut inner = core.lock()?;
    inner.usable()?;
    inner.free_segment(victim)?;
    inner.gc.segments_cleaned += 1;
    Ok(Outcome::Freed)
}
