//! C11: a 256 MiB volume receives seeded writes, then a snapshot is taken
//! and exported; 100,000 more writes run (with the GC in `Auto`) and the
//! snapshot is exported again. Both images must hash like the model frozen
//! at the snapshot. Leaves `snapshot-antes.img`, `snapshot-despues.img` and
//! `modelo.sha256`.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use bitacora::format::DEFAULT_SEGMENT_SIZE;
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, file_sha256, model_sha256};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// Random block writes before the snapshot.
    pub before: u64,
    /// Random block writes after the snapshot.
    pub after: u64,
    /// Writes between two `flush` calls.
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 256 << 20,
    segment_size: DEFAULT_SEGMENT_SIZE,
    before: 16_384,
    after: 100_000,
    flush_every: 256,
};

pub struct Report {
    pub model_sha: String,
    pub before_sha: String,
    pub after_sha: String,
}

fn write_random(
    vol: &Volume,
    model: &mut Model,
    rng: &mut Rng,
    count: u64,
    flush_every: u64,
) -> Result<(), Box<dyn Error>> {
    for i in 1..=count {
        let lba = rng.below(model.blocks());
        vol.write(lba, &model.write(lba))?;
        if i % flush_every == 0 {
            vol.flush()?;
        }
    }
    vol.flush()?;
    Ok(())
}

/// Runs the scenario in `dir` and hashes the model and both exports.
pub fn exercise(dir: &Path, seed: u64, p: &Params) -> Result<Report, Box<dyn Error>> {
    let path = dir.join("volume.img");
    let _ = fs::remove_file(&path);
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(&path, false)?), config)?;
    let mut model = Model::new(seed, vol.blocks());
    let mut rng = Rng::new(seed);

    write_random(&vol, &mut model, &mut rng, p.before, p.flush_every)?;
    let id = vol.snapshot_create()?;
    let model_sha = model_sha256(&model);
    fs::write(dir.join("modelo.sha256"), format!("{model_sha}\n"))?;
    let before = dir.join("snapshot-antes.img");
    vol.export(Version::Snapshot(id), &before)?;

    write_random(&vol, &mut model, &mut rng, p.after, p.flush_every)?;
    let after = dir.join("snapshot-despues.img");
    vol.export(Version::Snapshot(id), &after)?;
    vol.close()?;
    drop(vol);
    // The volume itself is not an artifact of C11.
    fs::remove_file(&path)?;

    Ok(Report {
        model_sha,
        before_sha: file_sha256(&before)?,
        after_sha: file_sha256(&after)?,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let r = exercise(&ctx.dir.clone(), ctx.seed, &FULL)?;
    let intact = r.before_sha == r.model_sha && r.after_sha == r.model_sha;
    ctx.line(format!("sha256_modelo: {}", r.model_sha));
    ctx.line(format!("sha256_antes: {}", r.before_sha));
    ctx.line(format!("sha256_despues: {}", r.after_sha));
    ctx.line(if intact {
        "snapshot: inmutable"
    } else {
        "snapshot: modificado"
    });
    Ok(Verdict::from_pass(intact))
}
