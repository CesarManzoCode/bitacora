//! C8: writes with interleaved flushes and no orderly close; then each trial
//! truncates a copy of the file at a random point inside the last segment
//! and checks that recovery yields a prefix of the issued writes.
//!
//! With the GC off every write is one block and gets LSN = its position, so
//! the recovered LSN `k` names the prefix; it must hold at least every
//! flushed write whose batch ends before the cut. Batch boundaries come from
//! the locations the engine reports: consecutive writes of one batch sit in
//! consecutive blocks, a new batch starts after a summary block.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Arc;

use bitacora::format::{BLOCK_SIZE, DATA_OFFSET, segment_offset};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, scan};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub writes: u64,
    pub trials: u64,
}

pub const FULL: Params = Params {
    volume_size: 64 << 20,
    segment_size: 1 << 20,
    writes: 3_000,
    trials: 1_000,
};

/// One flush every this many writes on average.
const FLUSH_EVERY: u64 = 32;

impl Params {
    fn config(&self) -> VolumeConfig {
        VolumeConfig::new(self.volume_size)
            .with_segment_size(self.segment_size)
            .with_gc_mode(GcMode::Off)
    }

    fn blocks(&self) -> u64 {
        self.volume_size / BLOCK_SIZE as u64
    }
}

/// Whether the volume at `path` recovers to the state after the first `k`
/// writes of `lbas`, with `k_min <= k`.
fn valid_prefix(
    path: &Path,
    seed: u64,
    p: &Params,
    lbas: &[u64],
    k_min: usize,
) -> Result<bool, Box<dyn Error>> {
    let medium = Arc::new(FileMedium::open(path, false)?);
    let Ok(vol) = Volume::open(medium, p.config()) else {
        return Ok(false);
    };
    let k = vol.status().lsn as usize;
    if k < k_min || k > lbas.len() {
        return Ok(false);
    }
    let mut model = Model::new(seed, p.blocks());
    for &lba in &lbas[..k] {
        model.bump(lba);
    }
    Ok(scan(&vol, &model).diffs == 0)
}

/// Runs the workload and the trials; returns how many trials gave a valid
/// prefix.
pub fn exercise(ctx: &mut Ctx, p: &Params) -> Result<u64, Box<dyn Error>> {
    let base = ctx.dir.join("base.img");
    let trial = ctx.dir.join("trial.img");
    let vol = Volume::create(Arc::new(FileMedium::create(&base, false)?), p.config())?;
    let mut rng = Rng::stream(ctx.seed, 0);
    let mut model = Model::new(ctx.seed, p.blocks());
    let mut lbas = Vec::new();
    let mut offsets = Vec::new();
    let mut flushed = 0;
    for i in 0..p.writes {
        let lba = rng.below(p.blocks());
        vol.write(lba, &model.write(lba))?;
        lbas.push(lba);
        offsets.push(vol.locate(lba).ok_or("written block has no location")?);
        if rng.chance(1, FLUSH_EVERY) {
            vol.flush()?;
            flushed = i as usize + 1;
        }
    }
    // No orderly close: `vol` stays open, unflushed, while the copies are made.

    let block = BLOCK_SIZE as u64;
    let mut batch_end = vec![0u64; offsets.len()];
    for i in (0..offsets.len()).rev() {
        batch_end[i] = match offsets.get(i + 1) {
            Some(&next) if next == offsets[i] + block => batch_end[i + 1],
            _ => offsets[i] + block,
        };
    }
    let last = *offsets.last().ok_or("no writes")?;
    let seg_start = segment_offset((last - DATA_OFFSET) / p.segment_size, p.segment_size);
    let end = fs::metadata(&base)?.len().min(seg_start + p.segment_size);
    let span = end.saturating_sub(seg_start);

    let mut cuts = Rng::stream(ctx.seed, 1);
    let mut valid = 0;
    for t in 1..=p.trials {
        let cut = seg_start + cuts.below(span + 1);
        fs::copy(&base, &trial)?;
        OpenOptions::new().write(true).open(&trial)?.set_len(cut)?;
        let k_min = batch_end[..flushed]
            .iter()
            .take_while(|&&e| e <= cut)
            .count();
        if valid_prefix(&trial, ctx.seed, p, &lbas, k_min)? {
            valid += 1;
            ctx.line(format!("prueba {t}: prefijo: valido"));
        } else {
            ctx.line(format!("prueba {t}: prefijo: invalido"));
        }
    }
    drop(vol);
    let _ = fs::remove_file(&trial);
    Ok(valid)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let valid = exercise(ctx, &FULL)?;
    ctx.line(format!("prefijo: valido {valid} de {}", FULL.trials));
    Ok(Verdict::from_pass(valid == FULL.trials))
}
