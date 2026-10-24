//! C4: ten complete overwrites of a 1 GiB volume, each with different data,
//! with the GC in `Auto`; then the whole volume is checked against the model
//! and the file size is compared with the volume size. Leaves `volume.img`.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use bitacora::format::{BLOCK_SIZE, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, scan, threshold};
use crate::model::Model;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    pub passes: u64,
}

pub const FULL: Params = Params {
    volume_size: 1 << 30,
    segment_size: DEFAULT_SEGMENT_SIZE,
    passes: 10,
};

/// Blocks per `write` call.
const CHUNK: u64 = 256;

pub struct Report {
    /// `st_size` of the file divided by the volume size.
    pub physical_factor: f64,
    pub diffs: u64,
}

/// Writes every block `p.passes` times (version = pass), closes the volume
/// in order and measures the file.
pub fn exercise(path: &Path, seed: u64, p: &Params) -> Result<Report, Box<dyn Error>> {
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(path, false)?), config)?;
    let blocks = vol.blocks();
    let mut model = Model::new(seed, blocks);
    let mut buf = vec![0u8; CHUNK as usize * BLOCK_SIZE];
    for _ in 0..p.passes {
        let mut lba = 0;
        while lba < blocks {
            let n = CHUNK.min(blocks - lba);
            for i in 0..n {
                let start = i as usize * BLOCK_SIZE;
                buf[start..start + BLOCK_SIZE].copy_from_slice(&model.write(lba + i));
            }
            vol.write(lba, &buf[..n as usize * BLOCK_SIZE])?;
            lba += n;
        }
        vol.flush()?;
    }
    let diffs = scan(&vol, &model).diffs;
    vol.close()?;
    drop(vol);
    let size = fs::metadata(path)?.len();
    Ok(Report {
        physical_factor: size as f64 / p.volume_size as f64,
        diffs,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let max_factor = threshold(&["c04_max_physical_factor"])?;
    let report = exercise(&ctx.dir.join("volume.img"), ctx.seed, &FULL)?;
    ctx.line(format!("factor_fisico: {:.4}", report.physical_factor));
    ctx.line(format!("diferencias: {}", report.diffs));
    Ok(Verdict::from_pass(
        report.physical_factor <= max_factor && report.diffs == 0,
    ))
}
