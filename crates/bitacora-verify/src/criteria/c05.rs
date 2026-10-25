//! C5: four threads write the same 256 blocks while the GC runs in
//! `Continuous`. Every write records its start and its acknowledgement on a
//! shared monotonic clock and carries a version unique within the run.
//!
//! Rule: a lost update is a block whose final value is a write `w` such that
//! another acknowledged write of the same block started after `w` was
//! acknowledged (it follows `w` in real time, so it must win), or whose final
//! value is none of the writes issued to it.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict};
use crate::rng::{Rng, block_data};

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub threads: u64,
    /// Shared blocks: lba `0..blocks`.
    pub blocks: u64,
    pub writes_per_thread: u64,
}

pub const FULL: Params = Params {
    volume_size: 16 << 20,
    segment_size: 256 << 10,
    threads: 4,
    blocks: 256,
    writes_per_thread: 5_000,
};

pub const SEEDS: std::ops::RangeInclusive<u64> = 1..=20;

/// One flush every this many writes of a thread, on average.
const FLUSH_EVERY: u64 = 64;

struct WriteRec {
    lba: u64,
    version: u64,
    /// Nanoseconds since the shared start, before calling `write`.
    start: u64,
    /// Nanoseconds since the shared start, after `write` returned `Ok`.
    ack: u64,
}

fn writer(
    vol: &Volume,
    clock: Instant,
    seed: u64,
    t: u64,
    p: &Params,
) -> bitacora::Result<Vec<WriteRec>> {
    let mut rng = Rng::stream(seed, t);
    let mut recs = Vec::with_capacity(p.writes_per_thread as usize);
    for i in 0..p.writes_per_thread {
        let lba = rng.below(p.blocks);
        let version = t * p.writes_per_thread + i + 1;
        let data = block_data(seed, lba, version);
        let start = clock.elapsed().as_nanos() as u64;
        vol.write(lba, &data)?;
        let ack = clock.elapsed().as_nanos() as u64;
        recs.push(WriteRec {
            lba,
            version,
            start,
            ack,
        });
        if rng.chance(1, FLUSH_EVERY) {
            vol.flush()?;
        }
    }
    Ok(recs)
}

/// Runs seed `seed` on a new volume at `path` and returns the number of
/// blocks with a lost update.
pub fn lost_updates(path: &Path, seed: u64, p: &Params) -> Result<u64, Box<dyn Error>> {
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Continuous);
    let vol = Volume::create(Arc::new(FileMedium::create(path, false)?), config)?;
    if p.blocks > vol.blocks() {
        return Err("shared blocks exceed the volume".into());
    }
    let clock = Instant::now();
    let results: Vec<bitacora::Result<Vec<WriteRec>>> = thread::scope(|s| {
        let handles: Vec<_> = (0..p.threads)
            .map(|t| {
                let vol = &vol;
                s.spawn(move || writer(vol, clock, seed, t, p))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("writer thread panicked"))
            .collect()
    });
    let mut per_block: Vec<Vec<WriteRec>> = (0..p.blocks).map(|_| Vec::new()).collect();
    for result in results {
        for rec in result? {
            per_block[rec.lba as usize].push(rec);
        }
    }

    let mut lost = 0;
    for (lba, recs) in per_block.iter().enumerate() {
        let lba = lba as u64;
        let data = vol.read(lba, 1)?;
        let winner = recs
            .iter()
            .find(|r| data[..] == block_data(seed, lba, r.version)[..]);
        let is_lost = match winner {
            Some(w) => recs.iter().any(|other| other.start > w.ack),
            None => !recs.is_empty() || data.iter().any(|&b| b != 0),
        };
        if is_lost {
            lost += 1;
        }
    }
    vol.close()?;
    Ok(lost)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let path = ctx.dir.join("volume.img");
    let mut total = 0;
    let mut first_failing = None;
    for seed in SEEDS {
        let lost = lost_updates(&path, seed, &FULL)?;
        ctx.line(format!("semilla {seed}: actualizaciones perdidas: {lost}"));
        if lost > 0 && first_failing.is_none() {
            first_failing = Some(seed);
        }
        total += lost;
    }
    let _ = fs::remove_file(&path);
    ctx.line(format!("actualizaciones perdidas: {total}"));
    if let Some(seed) = first_failing {
        ctx.seed = seed;
    }
    Ok(Verdict::from_pass(total == 0))
}
