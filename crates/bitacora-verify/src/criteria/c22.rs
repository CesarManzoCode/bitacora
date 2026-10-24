//! C22: capacity projection against a real run (contract section 11). A
//! fixed history of 30 accelerated days (1 day = one round of load per
//! snapshot) over 256 MiB is written to `history.json`; the `bitacora
//! capacity` binary projects it into `projection.json` before the run; then
//! the same load is applied to `volume.img` with `GcMode::Auto`. The error
//! is |projected − real| / real, with real = `st_blocks × 512` of
//! `volume.img` after `close`, and must not exceed `c22_max_relative_error`.
//!
//! The load follows the replay of `bitacora::capacity`: each round writes
//! `History::round_writes` blocks split by `History::split`, the new blocks
//! first (sequential LBAs after the written ones) and then the overwrites
//! (uniform over the written blocks); it ends with a snapshot, or a `flush`
//! without snapshots. Snapshots of day `d` are deleted at the end of day
//! `d + snapshot_retention_days`.

use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bitacora::format::{BLOCK_SIZE, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, History, Projection, Volume, VolumeConfig};

use super::cluster::bitacora_bin;
use super::{Ctx, Outcome, Verdict, threshold};
use crate::model::Model;
use crate::rng::Rng;

/// 5,000 blocks a day, 70 % of them overwrites, one snapshot a day kept 2
/// days: about 586 MiB written in 30 days, enough to reach the GC trigger
/// near day 17 while the referenced versions stay well below the space
/// the GC keeps.
pub const HISTORY: History = History {
    volume_size: 256 << 20,
    segment_size: DEFAULT_SEGMENT_SIZE,
    write_blocks_per_day: 5000,
    overwrite_fraction: 0.7,
    snapshots_per_day: 1,
    snapshot_retention_days: 2,
};

pub const DAYS: u64 = 30;

/// Blocks per `write` for the new blocks of a round.
const CHUNK: u64 = 128;

/// Applies `days` days of `history` to a new volume at `path` and returns
/// its allocated bytes (`st_blocks × 512`) after `close`.
pub fn apply(path: &Path, history: &History, days: u64, seed: u64) -> Result<u64, Box<dyn Error>> {
    let _ = fs::remove_file(path);
    let config = VolumeConfig::new(history.volume_size)
        .with_segment_size(history.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(path, false)?), config)?;
    let mut model = Model::new(seed, vol.blocks());
    let mut rng = Rng::new(seed);
    let mut written = 0u64;
    // (day taken, snapshot id), oldest first.
    let mut snapshots: VecDeque<(u64, u64)> = VecDeque::new();
    let mut buf = vec![0u8; CHUNK as usize * BLOCK_SIZE];
    for day in 1..=days {
        for round in 0..history.rounds_per_day() {
            let (new, overwrites) = history.split(written, history.round_writes(round));
            let end = written + new;
            while written < end {
                let n = CHUNK.min(end - written);
                for i in 0..n {
                    let at = i as usize * BLOCK_SIZE;
                    buf[at..at + BLOCK_SIZE].copy_from_slice(&model.write(written + i));
                }
                vol.write(written, &buf[..n as usize * BLOCK_SIZE])?;
                written += n;
            }
            for _ in 0..overwrites {
                let lba = rng.below(written);
                vol.write(lba, &model.write(lba))?;
            }
            if history.snapshots_per_day > 0 {
                snapshots.push_back((day, vol.snapshot_create()?));
            } else {
                vol.flush()?;
            }
        }
        while let Some(&(taken, id)) = snapshots.front()
            && taken + history.snapshot_retention_days <= day
        {
            vol.snapshot_delete(id)?;
            snapshots.pop_front();
        }
    }
    vol.close()?;
    drop(vol);
    Ok(fs::metadata(path)?.blocks() * 512)
}

/// Runs `bitacora capacity` on `history_path` and saves its output.
fn project(history_path: &Path, out: &Path) -> Result<Projection, Box<dyn Error>> {
    let output = Command::new(bitacora_bin()?)
        .arg("capacity")
        .arg("--history")
        .arg(history_path)
        .arg("--days")
        .arg(DAYS.to_string())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "bitacora capacity: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    fs::write(out, &output.stdout)?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(Projection::from_json(&text)?)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let max_error = threshold(&["c22_max_relative_error"])?;
    let history_path = ctx.dir.join("history.json");
    fs::write(&history_path, HISTORY.to_json()?)?;
    let projected = project(&history_path, &ctx.dir.join("projection.json"))?
        .projected_physical_bytes;
    let real = apply(&ctx.dir.join("volume.img"), &HISTORY, DAYS, ctx.seed)?;
    let error = (projected as f64 - real as f64).abs() / real as f64;
    ctx.line(format!("proyectado_bytes: {projected}"));
    ctx.line(format!("real_bytes: {real}"));
    ctx.line(format!("error_relativo: {error:.4}"));
    ctx.line(format!("umbral: {max_error}"));
    Ok(Verdict::from_pass(real > 0 && error <= max_error))
}
