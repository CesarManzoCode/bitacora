//! Volume: write path, read path, durability and recovery (contract sections
//! 3.4, 4 and 5).
//!
//! All mutable state lives in `Inner` behind one `Mutex`. Writes, `flush` and
//! superblock updates run under that lock; reads resolve their locations under
//! it and then `pread` from the medium after releasing it.
//!
//! The volume behaves like a disk with a volatile write cache: `write` is
//! acknowledged once the block is in the open batch and in the index; `flush`
//! emits the open batch and calls `fdatasync`. Every block of the open batch
//! already has its final place in the hot segment, so its record is inserted
//! at write time and only the bytes wait in memory.

mod recovery;

use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::error::{Error, Result};
use crate::format::{
    BLOCK_SIZE, BLOCK_SIZE_U64, BatchEntry, BatchSummary, DEFAULT_SEGMENT_SIZE, MAX_BATCH_BLOCKS,
    SegmentHeader, Stream, Superblock, SuperblockSlot, VolumeId, data_crc, new_volume_id,
    segment_offset, select_superblock, valid_segment_size,
};
use crate::index::{Index, RecordTable};
use crate::medium::{AlignedBuf, Medium};

/// Garbage collection mode (contract section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GcMode {
    /// Collect when free segments drop below 10 % of `max_segments`.
    #[default]
    Auto,
    /// Collect without pause even when space is left (tests).
    Continuous,
    /// Never collect.
    Off,
}

/// Parameters of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeConfig {
    /// Logical size in bytes, a multiple of 4096.
    pub volume_size: u64,
    /// Segment size in bytes, a power of two between 256 KiB and 64 MiB.
    pub segment_size: u64,
    pub gc_mode: GcMode,
}

impl VolumeConfig {
    /// A configuration with the default 8 MiB segments and `GcMode::Auto`.
    pub fn new(volume_size: u64) -> Self {
        VolumeConfig {
            volume_size,
            segment_size: DEFAULT_SEGMENT_SIZE,
            gc_mode: GcMode::Auto,
        }
    }

    pub fn with_segment_size(mut self, segment_size: u64) -> Self {
        self.segment_size = segment_size;
        self
    }

    pub fn with_gc_mode(mut self, gc_mode: GcMode) -> Self {
        self.gc_mode = gc_mode;
        self
    }

    /// `ceil(volume_size × 1.25 / segment_size) + 4`.
    pub fn max_segments(&self) -> u64 {
        max_segments(self.volume_size, self.segment_size)
    }

    fn validate(&self) -> Result<()> {
        if self.volume_size == 0 || !self.volume_size.is_multiple_of(BLOCK_SIZE_U64) {
            return Err(Error::InvalidArgument(format!(
                "volume size {} is not a positive multiple of {BLOCK_SIZE}",
                self.volume_size
            )));
        }
        if !valid_segment_size(self.segment_size) {
            return Err(Error::InvalidArgument(format!(
                "segment size {} is not a power of two between 256 KiB and 64 MiB",
                self.segment_size
            )));
        }
        if self.max_segments() > u64::from(u32::MAX) {
            return Err(Error::InvalidArgument(format!(
                "volume size {} needs too many segments",
                self.volume_size
            )));
        }
        Ok(())
    }
}

/// Space rule of contract section 4: `ceil(volume_size × 1.25 / segment_size) + 4`.
pub fn max_segments(volume_size: u64, segment_size: u64) -> u64 {
    let n = (u128::from(volume_size) * 5).div_ceil(u128::from(segment_size) * 4);
    u64::try_from(n).unwrap_or(u64::MAX - 4) + 4
}

/// Lifecycle state of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeState {
    Open,
    /// Fail-stop: a write, `sync` or `punch_hole` of the medium failed.
    Failed,
    /// `close` finished.
    Closed,
}

/// Snapshot of the volume counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeStatus {
    pub state: VolumeState,
    /// Last LSN assigned (0 if nothing was ever written).
    pub lsn: u64,
    /// Last LSN covered by a completed `fdatasync`.
    pub durable_lsn: u64,
    pub max_segments: u64,
    pub segments_used: u64,
    pub segments_free: u64,
    /// Bytes of block versions still referenced by the live volume.
    pub live_bytes: u64,
}

/// Fixed geometry of an open volume.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    volume_id: VolumeId,
    volume_size: u64,
    segment_size: u64,
    max_segments: u64,
    /// Logical blocks of the volume.
    blocks: u64,
    /// Blocks per segment, header included.
    seg_blocks: u64,
}

impl Geometry {
    fn new(volume_id: VolumeId, volume_size: u64, segment_size: u64, max_segments: u64) -> Self {
        Geometry {
            volume_id,
            volume_size,
            segment_size,
            max_segments,
            blocks: volume_size / BLOCK_SIZE_U64,
            seg_blocks: segment_size / BLOCK_SIZE_U64,
        }
    }

    fn block_offset(&self, segment: u64, block: u64) -> u64 {
        segment_offset(segment, self.segment_size) + block * BLOCK_SIZE_U64
    }

    fn check_range(&self, lba: u64, nblocks: u64) -> Result<()> {
        match lba.checked_add(nblocks) {
            Some(end) if end <= self.blocks => Ok(()),
            _ => Err(Error::InvalidArgument(format!(
                "blocks {lba}..+{nblocks} outside a volume of {} blocks",
                self.blocks
            ))),
        }
    }
}

/// Bookkeeping of a segment in use. The GC (k1-03) reads `stream`,
/// `sealed` and `max_birth`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct SegmentInfo {
    stream: Stream,
    seg_gen: u64,
    /// No more batches are appended and its data is synced.
    sealed: bool,
    /// Greatest `birth` stored in the segment (age for the GC policy).
    max_birth: u64,
}

/// The hot segment receiving client batches.
#[derive(Debug, Clone, Copy)]
struct HotSegment {
    index: u64,
    seg_gen: u64,
    /// Block where the next batch summary goes.
    next_block: u64,
}

/// The batch being filled in memory. Its summary goes at `block` of
/// `segment` and its data right after.
struct OpenBatch {
    segment: u64,
    block: u64,
    first_lsn: u64,
    capacity: usize,
    entries: Vec<BatchEntry>,
    /// Summary block followed by `capacity` data blocks, aligned for `O_DIRECT`.
    buf: AlignedBuf,
}

struct Inner {
    medium: Arc<dyn Medium>,
    geo: Geometry,
    state: VolumeState,
    sb: Superblock,
    /// Slot holding the current superblock.
    sb_slot: SuperblockSlot,
    index: Index,
    records: RecordTable,
    segments: Vec<Option<SegmentInfo>>,
    /// LSN the next client block receives.
    next_lsn: u64,
    /// Last LSN written to the medium.
    emitted_lsn: u64,
    /// Last LSN covered by a completed `fdatasync`.
    synced_lsn: u64,
    next_seg_gen: u64,
    hot: Option<HotSegment>,
    batch: Option<OpenBatch>,
    /// Read by the GC (k1-03).
    #[allow(dead_code)]
    gc_mode: GcMode,
}

/// A block volume over a `Medium`. `Send + Sync`: share it with `Arc`.
pub struct Volume {
    medium: Arc<dyn Medium>,
    geo: Geometry,
    /// Segment reuse lock: reads hold it shared while they `pread`, so a
    /// segment is never reused under a read in flight. The GC (k1-03) takes
    /// it exclusively, before `inner`, to free a segment.
    reuse: RwLock<()>,
    inner: Mutex<Inner>,
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Volume>();
}

/// A block of a read that must come from the medium.
struct PendingRead {
    /// Position of the block within the request.
    slot: u64,
    lba: u64,
    birth: u64,
    offset: u64,
    data_crc: u32,
}

impl Volume {
    /// Creates a volume on an empty medium and writes its first superblock.
    pub fn create(medium: Arc<dyn Medium>, config: VolumeConfig) -> Result<Volume> {
        config.validate()?;
        if !medium.is_empty()? {
            return Err(Error::InvalidArgument(
                "a volume can only be created on an empty medium".into(),
            ));
        }
        let geo = Geometry::new(
            new_volume_id()?,
            config.volume_size,
            config.segment_size,
            config.max_segments(),
        );
        let sb = Superblock {
            sb_seq: 0,
            volume_id: geo.volume_id,
            volume_size: geo.volume_size,
            segment_size: geo.segment_size,
            max_segments: geo.max_segments,
            durable_lsn: 0,
            clean_shutdown: false,
            current_epoch: 0,
            epoch_history: Vec::new(),
            snapshots: Vec::new(),
        };
        // The first write goes to slot A.
        let mut inner = Inner::new(medium.clone(), geo, sb, SuperblockSlot::B, config.gc_mode);
        inner.write_superblock()?;
        Ok(Volume::assemble(medium, geo, inner))
    }

    /// Opens an existing volume: recovery of contract section 3.4, then a
    /// superblock with `clean_shutdown = 0`. `volume_size` and `segment_size`
    /// must match the ones the volume was created with.
    pub fn open(medium: Arc<dyn Medium>, config: VolumeConfig) -> Result<Volume> {
        config.validate()?;
        let mut slots = vec![0u8; 2 * BLOCK_SIZE];
        medium.read_at(&mut slots, 0)?;
        let (slot, mut sb) = select_superblock(&slots[..BLOCK_SIZE], &slots[BLOCK_SIZE..])?;
        if sb.volume_size != config.volume_size || sb.segment_size != config.segment_size {
            return Err(Error::InvalidArgument(format!(
                "volume was created with size {} and segment size {}, opened with {} and {}",
                sb.volume_size, sb.segment_size, config.volume_size, config.segment_size
            )));
        }
        if !valid_segment_size(sb.segment_size)
            || sb.max_segments != max_segments(sb.volume_size, sb.segment_size)
        {
            return Err(Error::Corrupt(format!(
                "superblock geometry: segment size {}, max_segments {}",
                sb.segment_size, sb.max_segments
            )));
        }
        let geo = Geometry::new(
            sb.volume_id,
            sb.volume_size,
            sb.segment_size,
            sb.max_segments,
        );
        let rec = recovery::recover(&*medium, &geo, sb.durable_lsn)?;

        // Recovery synced the medium, so everything recovered is durable.
        sb.durable_lsn = rec.last_lsn;
        sb.clean_shutdown = false;
        let mut inner = Inner::new(medium.clone(), geo, sb, slot, config.gc_mode);
        inner.index = rec.index;
        inner.records = rec.records;
        inner.segments = rec.segments;
        inner.next_lsn = rec.last_lsn + 1;
        inner.emitted_lsn = rec.last_lsn;
        inner.synced_lsn = rec.last_lsn;
        inner.next_seg_gen = rec.next_seg_gen;
        // `hot` stays empty: the first write opens a new hot segment with
        // `first_lsn = last_lsn + 1`, never appending to a recovered one.
        inner.write_superblock()?;
        Ok(Volume::assemble(medium, geo, inner))
    }

    fn assemble(medium: Arc<dyn Medium>, geo: Geometry, inner: Inner) -> Volume {
        Volume {
            medium,
            geo,
            reuse: RwLock::new(()),
            inner: Mutex::new(inner),
        }
    }

    /// Writes `data` (a multiple of 4096 bytes) starting at `lba`. Returns once
    /// the blocks are in the open batch and in the index; they are durable
    /// only after `flush`.
    pub fn write(&self, lba: u64, data: &[u8]) -> Result<()> {
        let mut inner = self.lock()?;
        inner.usable()?;
        inner.append_all(lba, data)
    }

    /// `write` followed by `flush` of that write.
    pub fn write_fua(&self, lba: u64, data: &[u8]) -> Result<()> {
        let mut inner = self.lock()?;
        inner.usable()?;
        inner.append_all(lba, data)?;
        inner.flush()
    }

    /// Reads `nblocks` blocks from `lba`. Never-written blocks read as zeros;
    /// a block whose `data_crc` fails returns `Error::Integrity`, never data.
    pub fn read(&self, lba: u64, nblocks: u64) -> Result<Vec<u8>> {
        let _reuse = self.reuse.read().map_err(|_| Error::Failed)?;
        let len = usize::try_from(nblocks)
            .ok()
            .and_then(|n| n.checked_mul(BLOCK_SIZE))
            .ok_or_else(|| Error::InvalidArgument(format!("cannot read {nblocks} blocks")))?;
        let mut out = vec![0u8; len];
        let mut pending = Vec::new();
        {
            let inner = self.lock()?;
            inner.usable()?;
            self.geo.check_range(lba, nblocks)?;
            for slot in 0..nblocks {
                let birth = inner.index.get(lba + slot);
                if birth == 0 {
                    continue;
                }
                if let Some(data) = inner.open_batch_block(birth) {
                    block_mut(&mut out, slot).copy_from_slice(data);
                    continue;
                }
                let record = inner.records.get(birth).ok_or_else(|| {
                    Error::Corrupt(format!(
                        "lba {} points at birth {birth} with no record",
                        lba + slot
                    ))
                })?;
                pending.push(PendingRead {
                    slot,
                    lba: lba + slot,
                    birth,
                    offset: self
                        .geo
                        .block_offset(u64::from(record.segment), u64::from(record.block)),
                    data_crc: record.data_crc,
                });
            }
        }

        // Out of the lock: one `pread` per run of blocks contiguous both in the
        // request and on the medium.
        let mut start = 0;
        while start < pending.len() {
            let mut end = start + 1;
            while end < pending.len()
                && pending[end].slot == pending[end - 1].slot + 1
                && pending[end].offset == pending[end - 1].offset + BLOCK_SIZE_U64
            {
                end += 1;
            }
            let first = pending[start].slot as usize * BLOCK_SIZE;
            let run = &mut out[first..first + (end - start) * BLOCK_SIZE];
            let got = self.medium.read_at(run, pending[start].offset)?;
            for (k, p) in pending[start..end].iter().enumerate() {
                let block = &run[k * BLOCK_SIZE..(k + 1) * BLOCK_SIZE];
                if got < (k + 1) * BLOCK_SIZE || data_crc(p.lba, p.birth, block) != p.data_crc {
                    return Err(Error::Integrity { lba: p.lba });
                }
            }
            start = end;
        }
        Ok(out)
    }

    /// Emits the open batch and calls `fdatasync`: every write acknowledged
    /// before the call is durable when it returns.
    pub fn flush(&self) -> Result<()> {
        let mut inner = self.lock()?;
        inner.usable()?;
        inner.flush()
    }

    /// Flushes and writes a superblock with `durable_lsn` and
    /// `clean_shutdown = 1`. Every later operation returns `Error::Closed`.
    pub fn close(&self) -> Result<()> {
        let mut inner = self.lock()?;
        inner.usable()?;
        inner.flush()?;
        inner.sb.durable_lsn = inner.synced_lsn;
        inner.sb.clean_shutdown = true;
        inner.write_superblock()?;
        inner.state = VolumeState::Closed;
        Ok(())
    }

    /// Current counters. Observational: it answers in every state.
    pub fn status(&self) -> VolumeStatus {
        match self.inner.lock() {
            Ok(inner) => inner.status(),
            Err(poisoned) => {
                let mut status = poisoned.into_inner().status();
                status.state = VolumeState::Failed;
                status
            }
        }
    }

    /// Byte offset in the medium of the current block of `lba`, or `None` if
    /// it was never written. A block still in the open batch already has its
    /// offset, but its bytes reach the medium only when the batch is emitted.
    pub fn locate(&self, lba: u64) -> Option<u64> {
        if lba >= self.geo.blocks {
            return None;
        }
        let inner = self.inner.lock().ok()?;
        let birth = inner.index.get(lba);
        if birth == 0 {
            return None;
        }
        let record = inner.records.get(birth)?;
        Some(
            self.geo
                .block_offset(u64::from(record.segment), u64::from(record.block)),
        )
    }

    pub fn volume_id(&self) -> VolumeId {
        self.geo.volume_id
    }

    pub fn volume_size(&self) -> u64 {
        self.geo.volume_size
    }

    pub fn segment_size(&self) -> u64 {
        self.geo.segment_size
    }

    /// Logical blocks of the volume.
    pub fn blocks(&self) -> u64 {
        self.geo.blocks
    }

    /// A poisoned lock means a thread panicked mid-update: fail-stop.
    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| Error::Failed)
    }
}

fn block_mut(buf: &mut [u8], slot: u64) -> &mut [u8] {
    let start = slot as usize * BLOCK_SIZE;
    &mut buf[start..start + BLOCK_SIZE]
}

impl Inner {
    fn new(
        medium: Arc<dyn Medium>,
        geo: Geometry,
        sb: Superblock,
        sb_slot: SuperblockSlot,
        gc_mode: GcMode,
    ) -> Self {
        Inner {
            medium,
            geo,
            state: VolumeState::Open,
            sb,
            sb_slot,
            index: Index::new(geo.blocks),
            records: RecordTable::new(geo.max_segments as usize),
            segments: vec![None; geo.max_segments as usize],
            next_lsn: 1,
            emitted_lsn: 0,
            synced_lsn: 0,
            next_seg_gen: 1,
            hot: None,
            batch: None,
            gc_mode,
        }
    }

    fn usable(&self) -> Result<()> {
        match self.state {
            VolumeState::Open => Ok(()),
            VolumeState::Failed => Err(Error::Failed),
            VolumeState::Closed => Err(Error::Closed),
        }
    }

    /// Fail-stop: the first error of a medium write, `sync` or `punch_hole`
    /// moves the volume to `Failed`. There is no retry (fsyncgate).
    fn fail_on<T>(&mut self, r: Result<T>) -> Result<T> {
        if r.is_err() {
            self.state = VolumeState::Failed;
        }
        r
    }

    fn status(&self) -> VolumeStatus {
        let used = self.segments.iter().flatten().count() as u64;
        VolumeStatus {
            state: self.state,
            lsn: self.next_lsn - 1,
            durable_lsn: self.synced_lsn,
            max_segments: self.geo.max_segments,
            segments_used: used,
            segments_free: self.geo.max_segments - used,
            live_bytes: self.records.len() as u64 * BLOCK_SIZE_U64,
        }
    }

    /// Writes the superblock to the slot that does not hold the current one
    /// and syncs it.
    fn write_superblock(&mut self) -> Result<()> {
        self.sb.sb_seq += 1;
        let slot = self.sb_slot.other();
        let buf = self.sb.encode()?;
        let r = self
            .medium
            .write_at(&buf, slot.offset())
            .and_then(|()| self.medium.sync());
        self.fail_on(r)?;
        self.sb_slot = slot;
        Ok(())
    }

    fn append_all(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        if !data.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Error::InvalidArgument(format!(
                "write of {} bytes is not a multiple of {BLOCK_SIZE}",
                data.len()
            )));
        }
        self.geo
            .check_range(lba, (data.len() / BLOCK_SIZE) as u64)?;
        for (i, block) in data.chunks_exact(BLOCK_SIZE).enumerate() {
            self.append(lba + i as u64, block)?;
        }
        Ok(())
    }

    /// Adds one block to the open batch with the next LSN as its `birth`.
    fn append(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        if self.batch.is_none() {
            self.open_batch()?;
        }
        let lsn = self.next_lsn;
        let crc = data_crc(lba, lsn, data);
        let batch = self.batch.as_mut().expect("open_batch left a batch");
        let slot = batch.entries.len();
        batch.buf[(1 + slot) * BLOCK_SIZE..(2 + slot) * BLOCK_SIZE].copy_from_slice(data);
        batch.entries.push(BatchEntry {
            lba,
            birth: lsn,
            data_crc: crc,
            flags: 0,
        });
        let segment = batch.segment;
        let block = batch.block + 1 + slot as u64;
        let full = batch.entries.len() == batch.capacity;

        self.next_lsn += 1;
        self.records.insert(lsn, segment as u32, block as u32, crc);
        let old = self.index.set(lba, lsn);
        if old != 0 {
            self.records.decref(old);
        }
        if let Some(info) = self.segments[segment as usize].as_mut() {
            info.max_birth = lsn;
        }
        if full {
            self.emit_batch()?;
        }
        Ok(())
    }

    /// The bytes of `birth` if it is still in the open batch.
    fn open_batch_block(&self, birth: u64) -> Option<&[u8]> {
        let batch = self.batch.as_ref()?;
        let i = birth.checked_sub(batch.first_lsn)? as usize;
        if i >= batch.entries.len() {
            return None;
        }
        Some(&batch.buf[(1 + i) * BLOCK_SIZE..(2 + i) * BLOCK_SIZE])
    }

    /// Opens a batch in the hot segment, opening a new hot segment if the
    /// current one has no room for a summary and one data block.
    fn open_batch(&mut self) -> Result<()> {
        let room = self.hot.map_or(0, |h| self.geo.seg_blocks - h.next_block);
        if room < 2 {
            self.open_hot_segment()?;
        }
        let hot = self.hot.expect("open_hot_segment left a hot segment");
        let capacity = ((self.geo.seg_blocks - hot.next_block - 1) as usize).min(MAX_BATCH_BLOCKS);
        self.batch = Some(OpenBatch {
            segment: hot.index,
            block: hot.next_block,
            first_lsn: self.next_lsn,
            capacity,
            entries: Vec::with_capacity(capacity),
            buf: AlignedBuf::zeroed((1 + capacity) * BLOCK_SIZE),
        });
        Ok(())
    }

    /// Takes the free segment with the lowest number for the hot stream. The
    /// previous hot segment is synced before the new header is written, so
    /// only the last segment of the stream can hold unsynced batches.
    fn open_hot_segment(&mut self) -> Result<()> {
        // k1-03 hook: with no free segment, the GC frees one here (or the
        // writer waits under backpressure) before giving up with `NoSpace`.
        let Some(index) = self.segments.iter().position(Option::is_none) else {
            return Err(Error::NoSpace);
        };
        if self.emitted_lsn > self.synced_lsn {
            self.sync()?;
        }
        if let Some(prev) = self.hot.take()
            && let Some(info) = self.segments[prev.index as usize].as_mut()
        {
            info.sealed = true;
        }
        let seg_gen = self.next_seg_gen;
        self.next_seg_gen += 1;
        let header = SegmentHeader {
            volume_id: self.geo.volume_id,
            seg_gen,
            stream: Stream::Hot,
            first_lsn: self.next_lsn,
        };
        let r = self.medium.write_at(
            &header.encode(),
            segment_offset(index as u64, self.geo.segment_size),
        );
        self.fail_on(r)?;
        self.segments[index] = Some(SegmentInfo {
            stream: Stream::Hot,
            seg_gen,
            sealed: false,
            max_birth: 0,
        });
        self.hot = Some(HotSegment {
            index: index as u64,
            seg_gen,
            next_block: 1,
        });
        // k1-03 hook: wake the GC when free segments drop below 10 %.
        Ok(())
    }

    /// Writes the open batch (summary and data in one `pwrite`).
    fn emit_batch(&mut self) -> Result<()> {
        let Some(mut batch) = self.batch.take() else {
            return Ok(());
        };
        let count = batch.entries.len();
        if count == 0 {
            return Ok(());
        }
        let hot = self.hot.expect("an open batch lives in the hot segment");
        let summary = BatchSummary {
            volume_id: self.geo.volume_id,
            seg_gen: hot.seg_gen,
            stream: Stream::Hot,
            epoch: self.sb.current_epoch,
            first_lsn: batch.first_lsn,
            synced_through_lsn: self.synced_lsn,
            entries: std::mem::take(&mut batch.entries),
        };
        let encoded = match summary.encode() {
            Ok(b) => b,
            Err(e) => return self.fail_on(Err(e)),
        };
        batch.buf[..BLOCK_SIZE].copy_from_slice(&encoded);
        let offset = self.geo.block_offset(batch.segment, batch.block);
        let r = self
            .medium
            .write_at(&batch.buf[..(1 + count) * BLOCK_SIZE], offset);
        self.fail_on(r)?;
        if let Some(h) = self.hot.as_mut() {
            h.next_block = batch.block + 1 + count as u64;
        }
        self.emitted_lsn = batch.first_lsn + count as u64 - 1;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let r = self.medium.sync();
        self.fail_on(r)?;
        self.synced_lsn = self.emitted_lsn;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.emit_batch()?;
        if self.emitted_lsn > self.synced_lsn {
            self.sync()?;
        }
        Ok(())
    }
}
