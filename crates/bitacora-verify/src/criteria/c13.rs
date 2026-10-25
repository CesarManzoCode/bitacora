//! C13: a 256 MiB volume receives seeded writes and a full backup of a
//! snapshot; then N = 5,000 block writes, a second snapshot and an
//! incremental backup between the two, taken while another thread keeps
//! writing to the volume. Both backups are restored into a new volume and
//! its export must hash like the export of the incremental's snapshot (and
//! like the model frozen there); the incremental may not exceed
//! 1.1 × n × 4096 bytes, n being the blocks the model saw change. Leaves
//! `incremental.bkp`, `restaurado.img` and `snapshot.sha256`.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use bitacora::backup;
use bitacora::format::{BLOCK_SIZE_U64, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, file_sha256, model_sha256};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// Random block writes before the full backup.
    pub before: u64,
    /// Random block writes between the two snapshots.
    pub changed: u64,
    /// Writes between two `flush` calls.
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 256 << 20,
    segment_size: DEFAULT_SEGMENT_SIZE,
    before: 16_384,
    changed: 5_000,
    flush_every: 256,
};

/// Largest incremental allowed, as a multiple of `n × 4096` bytes.
const MAX_SIZE_FACTOR: f64 = 1.1;

pub struct Report {
    /// Distinct blocks whose model version changed between the snapshots.
    pub modified: u64,
    /// Entries of the incremental backup.
    pub entries: u64,
    pub incremental_bytes: u64,
    /// Writes the concurrent writer completed during the incremental.
    pub concurrent_writes: u64,
    pub model_sha: String,
    pub snapshot_sha: String,
    pub restored_sha: String,
}

impl Report {
    pub fn identical(&self) -> bool {
        self.restored_sha == self.snapshot_sha && self.snapshot_sha == self.model_sha
    }

    pub fn size_ok(&self) -> bool {
        self.incremental_bytes as f64 <= MAX_SIZE_FACTOR * (self.modified * BLOCK_SIZE_U64) as f64
    }
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

/// Writes random blocks until `stop` is set; signals `started` after the
/// first one. Returns the number of writes.
fn writer(
    vol: &Volume,
    model: &mut Model,
    rng: &mut Rng,
    flush_every: u64,
    stop: &AtomicBool,
    started: mpsc::Sender<()>,
) -> bitacora::Result<u64> {
    let mut writes = 0;
    while writes == 0 || !stop.load(Ordering::Acquire) {
        let lba = rng.below(model.blocks());
        vol.write(lba, &model.write(lba))?;
        writes += 1;
        if writes == 1 {
            let _ = started.send(());
        }
        if writes % flush_every == 0 {
            vol.flush()?;
        }
    }
    vol.flush()?;
    Ok(writes)
}

/// Runs the scenario in `dir`.
pub fn exercise(dir: &Path, seed: u64, p: &Params) -> Result<Report, Box<dyn Error>> {
    let path = dir.join("volume.img");
    let restored_path = dir.join("restaurado-volumen.img");
    let full_bkp = dir.join("completo.bkp");
    let incr_bkp = dir.join("incremental.bkp");
    let snap_img = dir.join("snapshot.img");
    let restored_img = dir.join("restaurado.img");
    for f in [
        &path,
        &restored_path,
        &full_bkp,
        &incr_bkp,
        &snap_img,
        &restored_img,
    ] {
        let _ = fs::remove_file(f);
    }
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(&path, false)?), config)?;
    let mut model = Model::new(seed, vol.blocks());
    let mut rng = Rng::new(seed);

    write_random(&vol, &mut model, &mut rng, p.before, p.flush_every)?;
    let base = vol.snapshot_create()?;
    vol.backup_full(base, &full_bkp)?;
    let base_versions = model.versions().to_vec();

    write_random(&vol, &mut model, &mut rng, p.changed, p.flush_every)?;
    let modified = base_versions
        .iter()
        .zip(model.versions())
        .filter(|(a, b)| a != b)
        .count() as u64;
    let target = vol.snapshot_create()?;
    let frozen = model.clone();

    // The incremental runs while another thread keeps writing.
    let stop = AtomicBool::new(false);
    let (header, concurrent_writes) = thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let mut wrng = Rng::stream(seed, 1);
        let (vol, stop, model) = (&vol, &stop, &mut model);
        let handle = s.spawn(move || writer(vol, model, &mut wrng, p.flush_every, stop, tx));
        let header: Result<_, Box<dyn Error>> = match rx.recv() {
            Ok(()) => vol
                .backup_incremental(base, target, &incr_bkp)
                .map_err(Box::from),
            Err(_) => Err("the concurrent writer stopped before its first write".into()),
        };
        stop.store(true, Ordering::Release);
        let writes = handle
            .join()
            .map_err(|_| "the concurrent writer panicked")?;
        Ok::<_, Box<dyn Error>>((header?, writes?))
    })?;

    vol.export(Version::Snapshot(target), &snap_img)?;
    let snapshot_sha = file_sha256(&snap_img)?;
    fs::write(dir.join("snapshot.sha256"), format!("{snapshot_sha}\n"))?;
    vol.close()?;
    drop(vol);
    // Neither the source volume nor its export is an artifact of C13.
    fs::remove_file(&snap_img)?;
    fs::remove_file(&path)?;

    let medium = Arc::new(FileMedium::create(&restored_path, false)?);
    let restored = backup::restore(&[&full_bkp, &incr_bkp], medium, config)?;
    restored.export(Version::Live, &restored_img)?;
    restored.close()?;
    drop(restored);
    fs::remove_file(&restored_path)?;

    Ok(Report {
        modified,
        entries: header.count,
        incremental_bytes: fs::metadata(&incr_bkp)?.len(),
        concurrent_writes,
        model_sha: model_sha256(&frozen),
        snapshot_sha,
        restored_sha: file_sha256(&restored_img)?,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let r = exercise(&ctx.dir.clone(), ctx.seed, &FULL)?;
    let limit = MAX_SIZE_FACTOR * (r.modified * BLOCK_SIZE_U64) as f64;
    // The concurrent write count depends on timing, so it is not printed:
    // the output of a seed stays the same from run to run.
    ctx.line(format!("bloques_modificados: {}", r.modified));
    ctx.line(format!("entradas_incremental: {}", r.entries));
    ctx.line(format!("bytes_incremental: {}", r.incremental_bytes));
    ctx.line(format!("bytes_maximos: {limit:.0}"));
    ctx.line(format!("sha256_modelo: {}", r.model_sha));
    ctx.line(format!("sha256_snapshot: {}", r.snapshot_sha));
    ctx.line(format!("sha256_restaurado: {}", r.restored_sha));
    ctx.line(if r.identical() {
        "restauracion: identica"
    } else {
        "restauracion: distinta"
    });
    if !r.size_ok() {
        ctx.line("tamano_incremental: excedido");
    }
    Ok(Verdict::from_pass(
        r.identical() && r.size_ok() && r.concurrent_writes > 0,
    ))
}
