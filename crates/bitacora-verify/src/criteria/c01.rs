//! C1: random 4 KiB reads and writes (50/50) against the model, then an
//! orderly close. Leaves `volume.img` and `seed` for c03.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use bitacora::format::{BLOCK_SIZE, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::FileMedium;
use bitacora::{Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub ops: u64,
}

pub const FULL: Params = Params {
    volume_size: 1 << 30,
    segment_size: DEFAULT_SEGMENT_SIZE,
    ops: 100_000,
};

impl Params {
    pub fn config(&self) -> VolumeConfig {
        VolumeConfig::new(self.volume_size).with_segment_size(self.segment_size)
    }

    pub fn blocks(&self) -> u64 {
        self.volume_size / BLOCK_SIZE as u64
    }
}

pub enum Op {
    Read(u64),
    Write(u64),
}

/// The operation sequence of `seed`.
pub fn ops(seed: u64, p: &Params) -> impl Iterator<Item = Op> {
    let mut rng = Rng::new(seed);
    let blocks = p.blocks();
    (0..p.ops).map(move |_| {
        let write = rng.chance(1, 2);
        let lba = rng.below(blocks);
        if write { Op::Write(lba) } else { Op::Read(lba) }
    })
}

/// The model after the whole sequence, rebuilt from the seed alone (c03).
pub fn model(seed: u64, p: &Params) -> Model {
    let mut model = Model::new(seed, p.blocks());
    for op in ops(seed, p) {
        if let Op::Write(lba) = op {
            model.bump(lba);
        }
    }
    model
}

/// Runs the sequence on a new volume at `path`, closes it in order and
/// returns how many reads differed from the model.
pub fn exercise(path: &Path, seed: u64, p: &Params) -> Result<u64, Box<dyn Error>> {
    let medium = Arc::new(FileMedium::create(path, false)?);
    let vol = Volume::create(medium, p.config())?;
    let mut model = Model::new(seed, p.blocks());
    let mut diffs = 0;
    for op in ops(seed, p) {
        match op {
            Op::Write(lba) => vol.write(lba, &model.write(lba))?,
            Op::Read(lba) => {
                if !vol.read(lba, 1).is_ok_and(|d| model.matches(lba, &d)) {
                    diffs += 1;
                }
            }
        }
    }
    vol.close()?;
    Ok(diffs)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let seed_file = ctx.dir.join("seed");
    let _ = fs::remove_file(&seed_file);
    let diffs = exercise(&ctx.dir.join("volume.img"), ctx.seed, &FULL)?;
    fs::write(&seed_file, format!("{}\n", ctx.seed))?;
    ctx.line(format!("operaciones: {}", FULL.ops));
    ctx.line(format!("diferencias: {diffs}"));
    Ok(Verdict::from_pass(diffs == 0))
}
