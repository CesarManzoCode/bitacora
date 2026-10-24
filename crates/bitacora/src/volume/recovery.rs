//! Recovery of contract section 3.4.
//!
//! 1. Accept every segment header with a valid CRC and `volume_id`.
//! 2. Follow each segment's chain of batch summaries up to the first invalid
//!    one (CRC, `volume_id`, `seg_gen`, stream, a batch past the end of the
//!    segment or of the file, and in the hot stream an unexpected LSN).
//! 3. In the last segment of each stream, check the data of every batch. With
//!    D = max(superblock `durable_lsn`, greatest hot `synced_through_lsn`), a
//!    hot batch with a bad block ends the log if it holds an LSN > D (torn
//!    write) and is kept otherwise (corruption, reported on read). A bad cold
//!    or resync batch ends its segment.
//! 4. A valid summary of the same `seg_gen` after the end of a hot chain whose
//!    `synced_through_lsn` covers a lost LSN fails the open with `Corrupt`.
//!    Every hot segment is checked, not only the last one: older segments were
//!    fully synced, so a loss there is just as real.
//! 5. Rebuild by sorting `(lba, birth)`: the live index takes the greatest
//!    `birth` of each LBA.
//! 6. The caller opens a new hot segment on the first write.
//!
//! Before returning, the block where the chain of the last segment of each
//! stream ended is zeroed and the medium is synced. Otherwise, once a newer
//! segment exists, the leftovers of a torn batch would be read back as a
//! valid batch of a non-last segment (whose data is not checked) and collide
//! with the LSNs the new segment reuses.

use crate::error::{Error, Result};
use crate::format::{
    BLOCK_SIZE, BLOCK_SIZE_U64, BatchSummary, SegmentHeader, Stream, data_crc, segment_offset,
};
use crate::index::{Index, RecordTable};
use crate::medium::{AlignedBuf, Medium};

use super::{Geometry, SegmentInfo};

/// Blocks read per call when looking for summaries past the end of a chain.
const SCAN_CHUNK_BLOCKS: u64 = 256;

/// State rebuilt from the medium.
pub(super) struct Recovered {
    pub index: Index,
    pub records: RecordTable,
    pub segments: Vec<Option<SegmentInfo>>,
    /// Greatest LSN found in the log (0 for an empty volume).
    pub last_lsn: u64,
    pub next_seg_gen: u64,
}

struct ScannedBatch {
    /// Block of the summary within the segment.
    block: u64,
    summary: BatchSummary,
}

struct ScannedSegment {
    index: u64,
    header: SegmentHeader,
    batches: Vec<ScannedBatch>,
    /// Block where the chain ends.
    stop_block: u64,
    /// Hot stream: first LSN not in the chain.
    next_lsn: u64,
}

/// One block version found in the log.
struct Found {
    lba: u64,
    birth: u64,
    seg_gen: u64,
    segment: u32,
    block: u32,
    data_crc: u32,
}

pub(super) fn recover(medium: &dyn Medium, geo: &Geometry, durable_lsn: u64) -> Result<Recovered> {
    let len = medium.len()?;
    let data_start = segment_offset(0, geo.segment_size);
    let slots = if len <= data_start {
        0
    } else {
        (len - data_start)
            .div_ceil(geo.segment_size)
            .min(geo.max_segments)
    };

    // Points 1 and 2.
    let mut segs = Vec::new();
    for index in 0..slots {
        let offset = segment_offset(index, geo.segment_size);
        let Some(buf) = read_exact_at(medium, offset, BLOCK_SIZE)? else {
            continue;
        };
        let Ok(header) = SegmentHeader::decode(&buf, &geo.volume_id) else {
            continue;
        };
        segs.push(scan_chain(medium, geo, len, index, header)?);
    }

    // The last segment of each stream is the one with the greatest `seg_gen`.
    let mut last: [Option<usize>; 3] = [None; 3];
    for (i, s) in segs.iter().enumerate() {
        let k = s.header.stream as usize;
        if last[k].is_none_or(|j| segs[j].header.seg_gen < s.header.seg_gen) {
            last[k] = Some(i);
        }
    }

    // Point 3.
    let d = segs
        .iter()
        .filter(|s| s.header.stream == Stream::Hot)
        .flat_map(|s| s.batches.iter())
        .map(|b| b.summary.synced_through_lsn)
        .fold(durable_lsn, u64::max);
    for i in last.iter().flatten() {
        verify_tail(medium, geo, &mut segs[*i], d)?;
    }

    // Point 4.
    for s in segs.iter().filter(|s| s.header.stream == Stream::Hot) {
        check_lost(medium, geo, len, s)?;
    }

    for i in last.iter().flatten() {
        clear_stop(medium, geo, len, &segs[*i])?;
    }
    medium.sync()?;

    rebuild(geo, &segs)
}

/// Follows the chain of batch summaries of one segment (point 2).
fn scan_chain(
    medium: &dyn Medium,
    geo: &Geometry,
    len: u64,
    index: u64,
    header: SegmentHeader,
) -> Result<ScannedSegment> {
    let hot = header.stream == Stream::Hot;
    let mut block = 1;
    let mut next_lsn = header.first_lsn;
    let mut batches = Vec::new();
    while block + 2 <= geo.seg_blocks {
        let offset = geo.block_offset(index, block);
        let Some(buf) = read_exact_at(medium, offset, BLOCK_SIZE)? else {
            break;
        };
        let Ok(summary) = BatchSummary::decode(&buf, &geo.volume_id) else {
            break;
        };
        if summary.seg_gen != header.seg_gen || summary.stream != header.stream {
            break;
        }
        let count = summary.entries.len() as u64;
        // A batch cut by the end of the file ends the log: a prefix.
        if block + 1 + count > geo.seg_blocks || offset + (1 + count) * BLOCK_SIZE_U64 > len {
            break;
        }
        if hot
            && (summary.first_lsn != next_lsn
                || summary
                    .entries
                    .iter()
                    .zip(next_lsn..)
                    .any(|(e, lsn)| e.birth != lsn))
        {
            break;
        }
        if let Some(e) = summary
            .entries
            .iter()
            .find(|e| e.lba >= geo.blocks || e.birth == 0)
        {
            return Err(Error::Corrupt(format!(
                "segment {index}, block {block}: entry with lba {} and birth {} in a volume of {} blocks",
                e.lba, e.birth, geo.blocks
            )));
        }
        if hot {
            next_lsn += count;
        }
        batches.push(ScannedBatch { block, summary });
        block += 1 + count;
    }
    Ok(ScannedSegment {
        index,
        header,
        batches,
        stop_block: block,
        next_lsn,
    })
}

/// Checks the data of every batch of the last segment of a stream (point 3)
/// and cuts the chain at the first batch that ends the log.
fn verify_tail(
    medium: &dyn Medium,
    geo: &Geometry,
    seg: &mut ScannedSegment,
    d: u64,
) -> Result<()> {
    let hot = seg.header.stream == Stream::Hot;
    let mut cut_at = None;
    for (i, b) in seg.batches.iter().enumerate() {
        let count = b.summary.entries.len();
        let offset = geo.block_offset(seg.index, b.block + 1);
        let cut = match read_exact_at(medium, offset, count * BLOCK_SIZE)? {
            // End of file in the middle of the batch: a prefix.
            None => true,
            Some(data) => {
                let bad = b.summary.entries.iter().enumerate().any(|(j, e)| {
                    data_crc(e.lba, e.birth, &data[j * BLOCK_SIZE..(j + 1) * BLOCK_SIZE])
                        != e.data_crc
                });
                let last_lsn = b.summary.first_lsn + count as u64 - 1;
                bad && (!hot || last_lsn > d)
            }
        };
        if cut {
            cut_at = Some(i);
            break;
        }
    }
    if let Some(i) = cut_at {
        let b = &seg.batches[i];
        seg.stop_block = b.block;
        if hot {
            seg.next_lsn = b.summary.first_lsn;
        }
        seg.batches.truncate(i);
    }
    Ok(())
}

/// Point 4: a summary of this segment past the end of its chain that reports
/// a lost LSN as synced means a durable write was lost.
fn check_lost(medium: &dyn Medium, geo: &Geometry, len: u64, seg: &ScannedSegment) -> Result<()> {
    let mut block = seg.stop_block;
    while block < geo.seg_blocks {
        let offset = geo.block_offset(seg.index, block);
        if offset >= len {
            break;
        }
        let n = (geo.seg_blocks - block).min(SCAN_CHUNK_BLOCKS);
        let mut buf = AlignedBuf::zeroed(n as usize * BLOCK_SIZE);
        let whole = (medium.read_at(&mut buf, offset)? / BLOCK_SIZE) as u64;
        for j in 0..whole {
            let start = j as usize * BLOCK_SIZE;
            if let Ok(s) = BatchSummary::decode(&buf[start..start + BLOCK_SIZE], &geo.volume_id)
                && s.seg_gen == seg.header.seg_gen
                && s.stream == Stream::Hot
                && s.synced_through_lsn >= seg.next_lsn
            {
                return Err(Error::Corrupt(format!(
                    "segment {}, block {}: a batch reports LSN {} synced, but the log ends before LSN {}",
                    seg.index,
                    block + j,
                    s.synced_through_lsn,
                    seg.next_lsn
                )));
            }
        }
        if whole < n {
            break;
        }
        block += n;
    }
    Ok(())
}

/// Zeroes the block where the chain of `seg` ends, if it holds anything.
fn clear_stop(medium: &dyn Medium, geo: &Geometry, len: u64, seg: &ScannedSegment) -> Result<()> {
    if seg.stop_block >= geo.seg_blocks {
        return Ok(());
    }
    let offset = geo.block_offset(seg.index, seg.stop_block);
    if offset >= len {
        return Ok(());
    }
    let mut buf = AlignedBuf::zeroed(BLOCK_SIZE);
    let got = medium.read_at(&mut buf, offset)?;
    if buf[..got].iter().all(|&b| b == 0) {
        return Ok(());
    }
    medium.write_at(&AlignedBuf::zeroed(BLOCK_SIZE), offset)
}

/// Point 5: the live index takes the greatest `birth` of each LBA. A birth
/// found twice (an original and its relocation) keeps the newest location.
fn rebuild(geo: &Geometry, segs: &[ScannedSegment]) -> Result<Recovered> {
    let mut found = Vec::new();
    let mut segments = vec![None; geo.max_segments as usize];
    let mut last_lsn = 0;
    let mut max_gen = 0;
    for s in segs {
        let mut max_birth = 0;
        for b in &s.batches {
            for (j, e) in b.summary.entries.iter().enumerate() {
                found.push(Found {
                    lba: e.lba,
                    birth: e.birth,
                    seg_gen: s.header.seg_gen,
                    segment: s.index as u32,
                    block: (b.block + 1 + j as u64) as u32,
                    data_crc: e.data_crc,
                });
                max_birth = max_birth.max(e.birth);
            }
        }
        if s.header.stream == Stream::Hot {
            last_lsn = last_lsn.max(s.next_lsn.saturating_sub(1));
        }
        last_lsn = last_lsn.max(max_birth);
        max_gen = max_gen.max(s.header.seg_gen);
        segments[s.index as usize] = Some(SegmentInfo {
            stream: s.header.stream,
            seg_gen: s.header.seg_gen,
            sealed: true,
            max_birth,
        });
    }

    found.sort_unstable_by_key(|f| (f.lba, f.birth, f.seg_gen));
    let mut index = Index::new(geo.blocks);
    let mut records = RecordTable::new(geo.max_segments as usize);
    // K2: each live snapshot S also takes the greatest birth ≤ S.lsn here.
    for (i, f) in found.iter().enumerate() {
        if found.get(i + 1).is_some_and(|next| next.lba == f.lba) {
            continue;
        }
        if !records.insert(f.birth, f.segment, f.block, f.data_crc) {
            return Err(Error::Corrupt(format!(
                "birth {} is the current version of more than one lba",
                f.birth
            )));
        }
        index.set(f.lba, f.birth);
    }

    Ok(Recovered {
        index,
        records,
        segments,
        last_lsn,
        next_seg_gen: max_gen + 1,
    })
}

/// Reads exactly `len` bytes at `offset`; `None` if the medium ends first.
fn read_exact_at(medium: &dyn Medium, offset: u64, len: usize) -> Result<Option<AlignedBuf>> {
    let mut buf = AlignedBuf::zeroed(len);
    if medium.read_at(&mut buf, offset)? < len {
        return Ok(None);
    }
    Ok(Some(buf))
}
