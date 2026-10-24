//! On-disk format `bitacora-volume/1` (contract section 3).
//!
//! Every structure occupies one 4096-byte block, integers are little-endian
//! and the CRC32C of bytes `0..4092` is stored in the last 4 bytes.
//!
//! Superblock layout:
//!
//! | offset | field |
//! | --- | --- |
//! | 0 | magic `BITACSB1` |
//! | 8 | `format_version` u32, then 4 zero bytes |
//! | 16 | `sb_seq` u64 |
//! | 24 | `volume_id` 16 bytes |
//! | 40 | `volume_size` u64 |
//! | 48 | `segment_size` u64 |
//! | 56 | `max_segments` u64 |
//! | 64 | `durable_lsn` u64 |
//! | 72 | `clean_shutdown` u8, then 7 zero bytes |
//! | 80 | `current_epoch` u64 |
//! | 88 | epoch history count u32, snapshot count u32 |
//! | 96 | 64 × `(epoch u64, start_lsn u64)` |
//! | 1120 | 64 × `(snapshot_id u64, lsn u64, flags u32)` |
//! | 4092 | `crc32c` |
//!
//! Segment header layout: magic `BITACSG1` (0), `volume_id` (8), `seg_gen`
//! (24), `stream` u8 (32), `first_lsn` (40), `crc32c` (4092).
//!
//! Batch summary layout: magic `BITACBT1` (0), `volume_id` (8), `seg_gen`
//! (24), `stream` u8 (32), `epoch` (40), `first_lsn` (48), `count` u32 (56),
//! `synced_through_lsn` (64), `count` × `(lba u64, birth u64, data_crc u32,
//! flags u32)` from 72, `crc32c` (4092).

use std::fs::File;
use std::io::Read;

use crate::error::{Error, Result};

/// Block size in bytes.
pub const BLOCK_SIZE: usize = 4096;
/// Block size in bytes, as a medium offset.
pub const BLOCK_SIZE_U64: u64 = BLOCK_SIZE as u64;
/// Offset of segment 0; everything before it is superblocks and zeros.
pub const DATA_OFFSET: u64 = 1 << 20;
/// Maximum data blocks per batch.
pub const MAX_BATCH_BLOCKS: usize = 128;
/// Maximum snapshot table entries in the superblock.
pub const MAX_SNAPSHOTS: usize = 64;
/// Maximum epoch history entries in the superblock.
pub const MAX_EPOCH_HISTORY: usize = 64;
/// Format version stored in the superblock.
pub const FORMAT_VERSION: u32 = 1;
/// Default segment size (2048 blocks).
pub const DEFAULT_SEGMENT_SIZE: u64 = 8 << 20;
/// Smallest configurable segment size.
pub const MIN_SEGMENT_SIZE: u64 = 256 << 10;
/// Largest configurable segment size.
pub const MAX_SEGMENT_SIZE: u64 = 64 << 20;
/// Snapshot table flag: the snapshot was deleted.
pub const SNAPSHOT_FLAG_DELETED: u32 = 1;

pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"BITACSB1";
pub const SEGMENT_MAGIC: [u8; 8] = *b"BITACSG1";
pub const BATCH_MAGIC: [u8; 8] = *b"BITACBT1";

const CRC_OFFSET: usize = BLOCK_SIZE - 4;

const SB_EPOCHS_OFFSET: usize = 96;
const SB_EPOCH_ENTRY: usize = 16;
const SB_SNAPSHOTS_OFFSET: usize = SB_EPOCHS_OFFSET + MAX_EPOCH_HISTORY * SB_EPOCH_ENTRY;
const SB_SNAPSHOT_ENTRY: usize = 20;
const _: () = assert!(SB_SNAPSHOTS_OFFSET + MAX_SNAPSHOTS * SB_SNAPSHOT_ENTRY <= CRC_OFFSET);

const BATCH_ENTRIES_OFFSET: usize = 72;
const BATCH_ENTRY: usize = 24;
const _: () = assert!(BATCH_ENTRIES_OFFSET + MAX_BATCH_BLOCKS * BATCH_ENTRY <= CRC_OFFSET);

/// Random identifier of a volume, chosen at creation.
pub type VolumeId = [u8; 16];

/// Reads a fresh volume id from `/dev/urandom`.
pub fn new_volume_id() -> Result<VolumeId> {
    let mut id = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

/// Byte offset of segment `index` for the given segment size.
pub fn segment_offset(index: u64, segment_size: u64) -> u64 {
    DATA_OFFSET + index * segment_size
}

/// Whether `segment_size` is a power of two within the configurable range.
pub fn valid_segment_size(segment_size: u64) -> bool {
    segment_size.is_power_of_two()
        && (MIN_SEGMENT_SIZE..=MAX_SEGMENT_SIZE).contains(&segment_size)
}

/// `crc32c(lba ‖ birth ‖ data)`: a misdirected block or a block of another
/// version fails the check too.
pub fn data_crc(lba: u64, birth: u64, data: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&lba.to_le_bytes());
    let crc = crc32c::crc32c_append(crc, &birth.to_le_bytes());
    crc32c::crc32c_append(crc, data)
}

/// Log stream a segment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Stream {
    /// Client writes.
    Hot = 0,
    /// GC relocations.
    Cold = 1,
    /// Replica resynchronisation.
    Resync = 2,
}

impl Stream {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Stream::Hot),
            1 => Ok(Stream::Cold),
            2 => Ok(Stream::Resync),
            _ => Err(Error::Corrupt(format!("unknown stream {v}"))),
        }
    }
}

/// One `(epoch, start_lsn)` pair of the epoch history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochEntry {
    pub epoch: u64,
    pub start_lsn: u64,
}

/// One snapshot table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub snapshot_id: u64,
    pub lsn: u64,
    pub flags: u32,
}

impl SnapshotEntry {
    pub fn is_deleted(&self) -> bool {
        self.flags & SNAPSHOT_FLAG_DELETED != 0
    }
}

/// Volume superblock, stored twice (slots A and B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub sb_seq: u64,
    pub volume_id: VolumeId,
    pub volume_size: u64,
    pub segment_size: u64,
    pub max_segments: u64,
    pub durable_lsn: u64,
    pub clean_shutdown: bool,
    pub current_epoch: u64,
    pub epoch_history: Vec<EpochEntry>,
    pub snapshots: Vec<SnapshotEntry>,
}

impl Superblock {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.epoch_history.len() > MAX_EPOCH_HISTORY {
            return Err(Error::InvalidArgument(format!(
                "epoch history has {} entries, maximum {MAX_EPOCH_HISTORY}",
                self.epoch_history.len()
            )));
        }
        if self.snapshots.len() > MAX_SNAPSHOTS {
            return Err(Error::InvalidArgument(format!(
                "snapshot table has {} entries, maximum {MAX_SNAPSHOTS}",
                self.snapshots.len()
            )));
        }
        let mut b = vec![0u8; BLOCK_SIZE];
        b[0..8].copy_from_slice(&SUPERBLOCK_MAGIC);
        put_u32(&mut b, 8, FORMAT_VERSION);
        put_u64(&mut b, 16, self.sb_seq);
        b[24..40].copy_from_slice(&self.volume_id);
        put_u64(&mut b, 40, self.volume_size);
        put_u64(&mut b, 48, self.segment_size);
        put_u64(&mut b, 56, self.max_segments);
        put_u64(&mut b, 64, self.durable_lsn);
        b[72] = u8::from(self.clean_shutdown);
        put_u64(&mut b, 80, self.current_epoch);
        put_u32(&mut b, 88, self.epoch_history.len() as u32);
        put_u32(&mut b, 92, self.snapshots.len() as u32);
        for (i, e) in self.epoch_history.iter().enumerate() {
            let o = SB_EPOCHS_OFFSET + i * SB_EPOCH_ENTRY;
            put_u64(&mut b, o, e.epoch);
            put_u64(&mut b, o + 8, e.start_lsn);
        }
        for (i, s) in self.snapshots.iter().enumerate() {
            let o = SB_SNAPSHOTS_OFFSET + i * SB_SNAPSHOT_ENTRY;
            put_u64(&mut b, o, s.snapshot_id);
            put_u64(&mut b, o + 8, s.lsn);
            put_u32(&mut b, o + 16, s.flags);
        }
        seal(&mut b);
        Ok(b)
    }

    /// Decodes a superblock slot. With `volume_id`, a superblock of another
    /// volume is rejected.
    pub fn decode(buf: &[u8], volume_id: Option<&VolumeId>) -> Result<Self> {
        check_block(buf, &SUPERBLOCK_MAGIC, "superblock")?;
        let version = get_u32(buf, 8);
        if version != FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "superblock format version {version}, expected {FORMAT_VERSION}"
            )));
        }
        let id = get_id(buf, 24);
        if let Some(expected) = volume_id {
            check_volume_id(&id, expected, "superblock")?;
        }
        let clean_shutdown = match buf[72] {
            0 => false,
            1 => true,
            v => return Err(Error::Corrupt(format!("superblock clean_shutdown {v}"))),
        };
        let epochs = get_u32(buf, 88) as usize;
        let snaps = get_u32(buf, 92) as usize;
        if epochs > MAX_EPOCH_HISTORY || snaps > MAX_SNAPSHOTS {
            return Err(Error::Corrupt(format!(
                "superblock has {epochs} epoch entries and {snaps} snapshots"
            )));
        }
        let epoch_history = (0..epochs)
            .map(|i| {
                let o = SB_EPOCHS_OFFSET + i * SB_EPOCH_ENTRY;
                EpochEntry {
                    epoch: get_u64(buf, o),
                    start_lsn: get_u64(buf, o + 8),
                }
            })
            .collect();
        let snapshots = (0..snaps)
            .map(|i| {
                let o = SB_SNAPSHOTS_OFFSET + i * SB_SNAPSHOT_ENTRY;
                SnapshotEntry {
                    snapshot_id: get_u64(buf, o),
                    lsn: get_u64(buf, o + 8),
                    flags: get_u32(buf, o + 16),
                }
            })
            .collect();
        Ok(Superblock {
            sb_seq: get_u64(buf, 16),
            volume_id: id,
            volume_size: get_u64(buf, 40),
            segment_size: get_u64(buf, 48),
            max_segments: get_u64(buf, 56),
            durable_lsn: get_u64(buf, 64),
            clean_shutdown,
            current_epoch: get_u64(buf, 80),
            epoch_history,
            snapshots,
        })
    }
}

/// One of the two superblock slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperblockSlot {
    A,
    B,
}

impl SuperblockSlot {
    pub fn offset(self) -> u64 {
        match self {
            SuperblockSlot::A => 0,
            SuperblockSlot::B => BLOCK_SIZE_U64,
        }
    }

    /// The slot the next superblock write goes to.
    pub fn other(self) -> Self {
        match self {
            SuperblockSlot::A => SuperblockSlot::B,
            SuperblockSlot::B => SuperblockSlot::A,
        }
    }
}

/// Picks the current superblock: the slot with a valid CRC and the greater
/// `sb_seq`. Fails only if neither slot is valid.
pub fn select_superblock(slot_a: &[u8], slot_b: &[u8]) -> Result<(SuperblockSlot, Superblock)> {
    match (
        Superblock::decode(slot_a, None),
        Superblock::decode(slot_b, None),
    ) {
        (Ok(a), Ok(b)) => {
            if a.volume_id != b.volume_id {
                return Err(Error::Corrupt(
                    "superblock slots belong to different volumes".into(),
                ));
            }
            if b.sb_seq > a.sb_seq {
                Ok((SuperblockSlot::B, b))
            } else {
                Ok((SuperblockSlot::A, a))
            }
        }
        (Ok(a), Err(_)) => Ok((SuperblockSlot::A, a)),
        (Err(_), Ok(b)) => Ok((SuperblockSlot::B, b)),
        (Err(ea), Err(eb)) => Err(Error::Corrupt(format!(
            "no valid superblock: slot A: {ea}; slot B: {eb}"
        ))),
    }
}

/// Header in block 0 of every segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub volume_id: VolumeId,
    pub seg_gen: u64,
    pub stream: Stream,
    pub first_lsn: u64,
}

impl SegmentHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_SIZE];
        b[0..8].copy_from_slice(&SEGMENT_MAGIC);
        b[8..24].copy_from_slice(&self.volume_id);
        put_u64(&mut b, 24, self.seg_gen);
        b[32] = self.stream as u8;
        put_u64(&mut b, 40, self.first_lsn);
        seal(&mut b);
        b
    }

    pub fn decode(buf: &[u8], volume_id: &VolumeId) -> Result<Self> {
        check_block(buf, &SEGMENT_MAGIC, "segment header")?;
        let id = get_id(buf, 8);
        check_volume_id(&id, volume_id, "segment header")?;
        Ok(SegmentHeader {
            volume_id: id,
            seg_gen: get_u64(buf, 24),
            stream: Stream::from_u8(buf[32])?,
            first_lsn: get_u64(buf, 40),
        })
    }
}

/// One data block of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchEntry {
    pub lba: u64,
    pub birth: u64,
    pub data_crc: u32,
    pub flags: u32,
}

/// Summary block that precedes the data blocks of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSummary {
    pub volume_id: VolumeId,
    pub seg_gen: u64,
    pub stream: Stream,
    pub epoch: u64,
    pub first_lsn: u64,
    pub synced_through_lsn: u64,
    pub entries: Vec<BatchEntry>,
}

impl BatchSummary {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let count = self.entries.len();
        if count == 0 || count > MAX_BATCH_BLOCKS {
            return Err(Error::InvalidArgument(format!(
                "batch has {count} blocks, expected 1 to {MAX_BATCH_BLOCKS}"
            )));
        }
        let mut b = vec![0u8; BLOCK_SIZE];
        b[0..8].copy_from_slice(&BATCH_MAGIC);
        b[8..24].copy_from_slice(&self.volume_id);
        put_u64(&mut b, 24, self.seg_gen);
        b[32] = self.stream as u8;
        put_u64(&mut b, 40, self.epoch);
        put_u64(&mut b, 48, self.first_lsn);
        put_u32(&mut b, 56, count as u32);
        put_u64(&mut b, 64, self.synced_through_lsn);
        for (i, e) in self.entries.iter().enumerate() {
            let o = BATCH_ENTRIES_OFFSET + i * BATCH_ENTRY;
            put_u64(&mut b, o, e.lba);
            put_u64(&mut b, o + 8, e.birth);
            put_u32(&mut b, o + 16, e.data_crc);
            put_u32(&mut b, o + 20, e.flags);
        }
        seal(&mut b);
        Ok(b)
    }

    pub fn decode(buf: &[u8], volume_id: &VolumeId) -> Result<Self> {
        check_block(buf, &BATCH_MAGIC, "batch summary")?;
        let id = get_id(buf, 8);
        check_volume_id(&id, volume_id, "batch summary")?;
        let count = get_u32(buf, 56) as usize;
        if count == 0 || count > MAX_BATCH_BLOCKS {
            return Err(Error::Corrupt(format!("batch summary count {count}")));
        }
        let entries = (0..count)
            .map(|i| {
                let o = BATCH_ENTRIES_OFFSET + i * BATCH_ENTRY;
                BatchEntry {
                    lba: get_u64(buf, o),
                    birth: get_u64(buf, o + 8),
                    data_crc: get_u32(buf, o + 16),
                    flags: get_u32(buf, o + 20),
                }
            })
            .collect();
        Ok(BatchSummary {
            volume_id: id,
            seg_gen: get_u64(buf, 24),
            stream: Stream::from_u8(buf[32])?,
            epoch: get_u64(buf, 40),
            first_lsn: get_u64(buf, 48),
            synced_through_lsn: get_u64(buf, 64),
            entries,
        })
    }
}

fn seal(b: &mut [u8]) {
    let crc = crc32c::crc32c(&b[..CRC_OFFSET]);
    put_u32(b, CRC_OFFSET, crc);
}

/// Checks length, magic and CRC of an encoded block.
fn check_block(buf: &[u8], magic: &[u8; 8], what: &str) -> Result<()> {
    if buf.len() != BLOCK_SIZE {
        return Err(Error::InvalidArgument(format!(
            "{what} buffer is {} bytes, expected {BLOCK_SIZE}",
            buf.len()
        )));
    }
    if buf[0..8] != magic[..] {
        return Err(Error::Corrupt(format!("{what}: bad magic")));
    }
    let stored = get_u32(buf, CRC_OFFSET);
    let computed = crc32c::crc32c(&buf[..CRC_OFFSET]);
    if stored != computed {
        return Err(Error::Corrupt(format!(
            "{what}: crc32c {stored:#010x}, computed {computed:#010x}"
        )));
    }
    Ok(())
}

fn check_volume_id(found: &VolumeId, expected: &VolumeId, what: &str) -> Result<()> {
    if found != expected {
        return Err(Error::Corrupt(format!("{what}: volume_id mismatch")));
    }
    Ok(())
}

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(a)
}

fn get_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn get_id(b: &[u8], o: usize) -> VolumeId {
    let mut id = [0u8; 16];
    id.copy_from_slice(&b[o..o + 16]);
    id
}
