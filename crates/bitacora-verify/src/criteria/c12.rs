//! C12: five chained snapshots of a 256 MiB volume with seeded writes before
//! each one and after the last; the 2nd is deleted and `compact()` runs.
//! The four remaining snapshots must hash like the model frozen at each one,
//! and the allocated size of `volume.img` (`st_blocks × 512`) must stay
//! within `c12_max_allocated_over_justified × bytes_justificados +
//! c12_slack_bytes`. `bytes_justificados` comes from the model alone:
//! 4096 × distinct `(lba, version)` pairs referenced by the live volume and
//! the four snapshots.

use std::error::Error;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use bitacora::format::{BLOCK_SIZE_U64, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, model_sha256, threshold, version_sha256};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub snapshots: usize,
    /// Position (from 1) of the snapshot that is deleted.
    pub deleted: usize,
    /// Random block writes before each snapshot and after the last one.
    pub writes_per_round: u64,
}

pub const FULL: Params = Params {
    volume_size: 256 << 20,
    segment_size: DEFAULT_SEGMENT_SIZE,
    snapshots: 5,
    deleted: 2,
    writes_per_round: 8192,
};

pub struct Report {
    /// Remaining snapshots whose content hashes like the model.
    pub intact: usize,
    pub remaining: usize,
    pub justified_bytes: u64,
    pub allocated_bytes: u64,
}

/// 4096 × distinct `(lba, version)` with version > 0 over `versions`.
pub fn justified_bytes(versions: &[&Model]) -> u64 {
    let Some(first) = versions.first() else {
        return 0;
    };
    let mut seen: Vec<u32> = Vec::with_capacity(versions.len());
    let mut pairs = 0u64;
    for lba in 0..first.blocks() as usize {
        seen.clear();
        seen.extend(
            versions
                .iter()
                .map(|m| m.versions()[lba])
                .filter(|&v| v > 0),
        );
        seen.sort_unstable();
        seen.dedup();
        pairs += seen.len() as u64;
    }
    pairs * BLOCK_SIZE_U64
}

fn write_round(
    vol: &Volume,
    model: &mut Model,
    rng: &mut Rng,
    count: u64,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..count {
        let lba = rng.below(model.blocks());
        vol.write(lba, &model.write(lba))?;
    }
    vol.flush()?;
    Ok(())
}

pub fn exercise(path: &Path, seed: u64, p: &Params) -> Result<Report, Box<dyn Error>> {
    let _ = fs::remove_file(path);
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(path, false)?), config)?;
    let mut model = Model::new(seed, vol.blocks());
    let mut rng = Rng::new(seed);

    // (snapshot id, model frozen at the snapshot)
    let mut snaps: Vec<(u64, Model)> = Vec::with_capacity(p.snapshots);
    for _ in 0..p.snapshots {
        write_round(&vol, &mut model, &mut rng, p.writes_per_round)?;
        let id = vol.snapshot_create()?;
        snaps.push((id, model.clone()));
    }
    write_round(&vol, &mut model, &mut rng, p.writes_per_round)?;

    let (deleted, _) = snaps.remove(p.deleted - 1);
    vol.snapshot_delete(deleted)?;
    vol.compact()?;

    let mut intact = 0;
    for (id, frozen) in &snaps {
        if version_sha256(&vol, Version::Snapshot(*id))? == model_sha256(frozen) {
            intact += 1;
        }
    }
    vol.close()?;
    drop(vol);

    let mut kept: Vec<&Model> = vec![&model];
    kept.extend(snaps.iter().map(|(_, m)| m));
    Ok(Report {
        intact,
        remaining: snaps.len(),
        justified_bytes: justified_bytes(&kept),
        allocated_bytes: fs::metadata(path)?.blocks() * 512,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let ratio = threshold(&["c12_max_allocated_over_justified"])?;
    let slack = threshold(&["c12_slack_bytes"])?;
    let r = exercise(&ctx.dir.join("volume.img"), ctx.seed, &FULL)?;
    let limit = r.justified_bytes as f64 * ratio + slack;
    ctx.line(format!(
        "hashes_snapshots_intactos: {} de {}",
        r.intact, r.remaining
    ));
    ctx.line(format!("bytes_justificados: {}", r.justified_bytes));
    ctx.line(format!("bytes_asignados: {}", r.allocated_bytes));
    Ok(Verdict::from_pass(
        r.intact == r.remaining
            && r.remaining == FULL.snapshots - 1
            && r.allocated_bytes as f64 <= limit,
    ))
}
