//! C9: per seed, single-block writes with random flushes on a `SimDisk`
//! with the GC in `Auto`; a power cut at a point chosen by the seed; then
//! recovery.
//!
//! Each client write gets LSN = its position (GC relocations keep `birth`
//! and take no LSN), so the recovered LSN `k` names a prefix. Two checks:
//! the recovered volume must read exactly as the first `k` issued writes
//! (prefix), and every write acknowledged by a completed `flush` must be
//! present: its block holds that version or a later one (no acknowledged
//! write lost). The standard output carries only counts, never times.

use std::error::Error;
use std::sync::Arc;

use bitacora::format::BLOCK_SIZE;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, holds_version, scan};
use crate::model::Model;
use crate::rng::Rng;
use crate::simdisk::SimDisk;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub ops: u64,
}

pub const FULL: Params = Params {
    volume_size: 16 << 20,
    segment_size: 1 << 20,
    ops: 20_000,
};

pub const SEEDS: std::ops::RangeInclusive<u64> = 1..=1000;

/// One flush every this many writes, on average.
const FLUSH_EVERY: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedResult {
    /// Writes acknowledged by a completed `flush` missing after recovery.
    pub lost: u64,
    /// Whether the recovered state is a prefix of the issued writes.
    pub prefix: bool,
}

impl Params {
    fn config(&self) -> VolumeConfig {
        VolumeConfig::new(self.volume_size)
            .with_segment_size(self.segment_size)
            .with_gc_mode(GcMode::Auto)
    }

    fn blocks(&self) -> u64 {
        self.volume_size / BLOCK_SIZE as u64
    }
}

fn prefix_model(seed: u64, blocks: u64, lbas: &[u64]) -> Model {
    let mut model = Model::new(seed, blocks);
    for &lba in lbas {
        model.bump(lba);
    }
    model
}

/// Runs one seed: workload, cut and recovery.
pub fn trial(seed: u64, p: &Params) -> Result<SeedResult, Box<dyn Error>> {
    let blocks = p.blocks();
    let disk = Arc::new(SimDisk::new(seed));
    let vol = Volume::create(disk.clone(), p.config())?;
    let mut rng = Rng::stream(seed, 0);
    let cut = Rng::stream(seed, 1).below(p.ops) + 1;
    let mut model = Model::new(seed, blocks);
    let mut lbas = Vec::with_capacity(cut as usize);
    let mut flushed = 0;
    for _ in 0..cut {
        let lba = rng.below(blocks);
        vol.write(lba, &model.write(lba))?;
        lbas.push(lba);
        if rng.chance(1, FLUSH_EVERY) {
            vol.flush()?;
            flushed = lbas.len();
        }
    }
    // The cut happens with the volume open and the GC thread possibly busy.
    let crashed = Arc::new(disk.crash(seed));
    drop(vol);

    let required = prefix_model(seed, blocks, &lbas[..flushed]);
    let count_lost = |vol: Option<&Volume>| -> u64 {
        (0..blocks)
            .filter(|&lba| {
                let need = u64::from(required.version(lba));
                if need == 0 {
                    return false;
                }
                let have = u64::from(model.version(lba));
                !vol.and_then(|v| v.read(lba, 1).ok())
                    .is_some_and(|d| holds_version(seed, lba, &d, need, have))
            })
            .count() as u64
    };

    let Ok(recovered) = Volume::open(crashed, p.config()) else {
        return Ok(SeedResult {
            lost: count_lost(None),
            prefix: false,
        });
    };
    let k = recovered.status().lsn as usize;
    let prefix =
        k <= lbas.len() && scan(&recovered, &prefix_model(seed, blocks, &lbas[..k])).diffs == 0;
    let lost = if prefix && k >= flushed {
        0
    } else {
        count_lost(Some(&recovered))
    };
    Ok(SeedResult { lost, prefix })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let mut lost = 0;
    let mut valid = 0;
    let mut first_failing = None;
    for seed in SEEDS {
        let r = trial(seed, &FULL)?;
        ctx.line(format!(
            "semilla {seed}: escrituras reconocidas perdidas: {}",
            r.lost
        ));
        if r.prefix {
            valid += 1;
        } else {
            ctx.line(format!("semilla {seed}: prefijo: invalido"));
        }
        if (r.lost > 0 || !r.prefix) && first_failing.is_none() {
            first_failing = Some(seed);
        }
        lost += r.lost;
    }
    let total = SEEDS.count();
    ctx.line(format!("prefijo: valido {valid} de {total}"));
    ctx.line(format!("escrituras reconocidas perdidas: {lost}"));
    if let Some(seed) = first_failing {
        ctx.seed = seed;
    }
    Ok(Verdict::from_pass(lost == 0 && valid == total))
}
