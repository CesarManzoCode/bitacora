//! C10: fail-stop under injected I/O errors. For each N, case `write` makes
//! the N-th `write_at` of the medium fail and case `sync` the N-th `sync`.
//! After the first error the workload keeps issuing writes and flushes; then
//! the power is cut and the volume recovered.
//!
//! A violation is a write acknowledged by a `flush` that returned `Ok` after
//! the error and whose version (or a later one) is not in the recovered
//! volume: once the medium failed nothing may be acknowledged as durable.
//! The GC is off so the medium call numbering depends only on N.

use std::error::Error;
use std::sync::Arc;

use bitacora::format::BLOCK_SIZE;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, holds_version};
use crate::model::Model;
use crate::rng::Rng;
use crate::simdisk::SimDisk;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub writes: u64,
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 16 << 20,
    segment_size: 1 << 20,
    writes: 4_000,
    flush_every: 16,
};

pub const CASES: std::ops::RangeInclusive<u64> = 1..=200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Write,
    Sync,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Case::Write => "write",
            Case::Sync => "sync",
        }
    }
}

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

/// Runs case `case` with failure at call `n`; returns the violations.
pub fn trial(case: Case, n: u64, seed: u64, p: &Params) -> Result<u64, Box<dyn Error>> {
    let blocks = p.blocks();
    let disk = Arc::new(SimDisk::new(seed ^ n));
    match case {
        Case::Write => disk.fail_write_at(n),
        Case::Sync => disk.fail_sync_at(n),
    }
    let Ok(vol) = Volume::create(disk.clone(), p.config()) else {
        // The failure hit the first superblock: nothing was acknowledged.
        return Ok(0);
    };
    let mut rng = Rng::stream(seed, n);
    let mut model = Model::new(seed, blocks);
    let mut errored = false;
    // Writes acknowledged by `write` and not yet covered by a `flush`.
    let mut unflushed: Vec<(u64, u64)> = Vec::new();
    // Writes acknowledged by a `flush` that returned `Ok` after the error.
    let mut after_error: Vec<(u64, u64)> = Vec::new();
    for i in 1..=p.writes {
        let lba = rng.below(blocks);
        let data = model.write(lba);
        match vol.write(lba, &data) {
            Ok(()) => unflushed.push((lba, u64::from(model.version(lba)))),
            Err(_) => errored = true,
        }
        if i % p.flush_every == 0 {
            match vol.flush() {
                Ok(()) if errored => after_error.append(&mut unflushed),
                Ok(()) => unflushed.clear(),
                Err(_) => errored = true,
            }
        }
    }
    let crashed = Arc::new(disk.crash(seed ^ n));
    drop(vol);
    if after_error.is_empty() {
        return Ok(0);
    }

    let recovered = Volume::open(crashed, p.config()).ok();
    let violations = after_error
        .iter()
        .filter(|&&(lba, version)| {
            let latest = u64::from(model.version(lba));
            !recovered
                .as_ref()
                .and_then(|v| v.read(lba, 1).ok())
                .is_some_and(|d| holds_version(seed, lba, &d, version, latest))
        })
        .count() as u64;
    Ok(violations)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let mut total = 0;
    for n in CASES {
        for case in [Case::Write, Case::Sync] {
            let k = trial(case, n, ctx.seed, &FULL)?;
            ctx.line(format!(
                "caso {} {n}: reconocidas_tras_error_sin_sync: {k}",
                case.name()
            ));
            total += k;
        }
    }
    ctx.line(format!("reconocidas_tras_error_sin_sync: {total}"));
    Ok(Verdict::from_pass(total == 0))
}
