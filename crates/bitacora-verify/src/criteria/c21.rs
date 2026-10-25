//! C21: throughput and latency (contract section 11). A 4 GiB volume on a
//! `FileMedium` with `O_DIRECT` under `target/criteria/c21/` is filled
//! sequentially; then 8 threads run three phases of 30 s each:
//!
//! 1. random 4 KiB reads;
//! 2. random 4 KiB writes, each thread calling `flush` every 32 of its
//!    writes;
//! 3. the same writes once the volume has reached its GC trigger, so the
//!    GC cleans while they run (`p99_ms_durante_gc`).
//!
//! The latency of an operation is one `read`, or one `write` plus the
//! `flush` that follows it every 32 writes. `p50_ms`, `p99_ms` and
//! `p999_ms` cover phases 1 and 2 together; each phase is also reported
//! alone. Writes `results/c21.json`. A result below the public reference of
//! 3,000 IOPS is reported in `bajo_referencia`, not failed. The volume is
//! deleted at the end, also when the run fails.

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bitacora::format::{BLOCK_SIZE, DEFAULT_SEGMENT_SIZE};
use bitacora::gc::GC_RESERVE_SEGMENTS;
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::c06::{commit, cpu_model, disk_models, json_num, json_str, kernel, percentile, utc_now};
use super::{Ctx, Outcome, Verdict, repo_root};
use crate::rng::Rng;

/// Public Railway reference: 3,000 read and 3,000 write IOPS per volume.
pub const REFERENCE_IOPS: f64 = 3000.0;

pub struct Params {
    pub volume_size: u64,
    pub threads: u64,
    pub phase: Duration,
    /// Writes of one thread between two of its `flush` calls.
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 4 << 30,
    threads: 8,
    phase: Duration::from_secs(30),
    flush_every: 32,
};

/// Blocks per `write` while filling and while pushing to the GC trigger.
const CHUNK: u64 = 256;

/// Bound of the writes that push the volume to its GC trigger, in volumes.
const MAX_TRIGGER_PASSES: u64 = 2;

/// Deletes the volume file when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub struct Phase {
    pub ops: u64,
    pub secs: f64,
    /// Sorted latencies in milliseconds.
    pub latencies_ms: Vec<f64>,
    /// Segments the GC freed during the phase.
    pub gc_cleaned: u64,
}

impl Phase {
    pub fn iops(&self) -> f64 {
        if self.secs > 0.0 {
            self.ops as f64 / self.secs
        } else {
            0.0
        }
    }

    pub fn quantile(&mut self, q: f64) -> f64 {
        percentile(&mut self.latencies_ms, q)
    }
}

/// A block of seeded bytes; writers stamp a counter in its first 8 bytes.
fn random_block(rng: &mut Rng) -> Vec<u8> {
    (0..BLOCK_SIZE / 8)
        .flat_map(|_| rng.next_u64().to_le_bytes())
        .collect()
}

/// One thread of a phase: operations until `deadline`, with their latency.
fn worker(
    vol: &Volume,
    p: &Params,
    mut rng: Rng,
    write: bool,
    deadline: Instant,
) -> bitacora::Result<Vec<f64>> {
    let blocks = vol.blocks();
    let mut data = random_block(&mut rng);
    let mut latencies = Vec::new();
    let mut n = 0u64;
    while Instant::now() < deadline {
        let lba = rng.below(blocks);
        let start = Instant::now();
        if write {
            n += 1;
            data[..8].copy_from_slice(&n.to_le_bytes());
            vol.write(lba, &data)?;
            if n.is_multiple_of(p.flush_every) {
                vol.flush()?;
            }
        } else {
            vol.read(lba, 1)?;
        }
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(latencies)
}

/// Runs one phase on `p.threads` threads.
fn phase(vol: &Volume, p: &Params, seed: u64, id: u64, write: bool) -> bitacora::Result<Phase> {
    let cleaned_before = vol.status().gc.segments_cleaned;
    let start = Instant::now();
    let deadline = start + p.phase;
    let results: Vec<bitacora::Result<Vec<f64>>> = thread::scope(|s| {
        let handles: Vec<_> = (0..p.threads)
            .map(|t| {
                let rng = Rng::stream(seed, id * 1000 + t);
                s.spawn(move || worker(vol, p, rng, write, deadline))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(Err(bitacora::Error::Failed)))
            .collect()
    });
    let secs = start.elapsed().as_secs_f64();
    if write {
        vol.flush()?;
    }
    let mut latencies_ms = Vec::new();
    for r in results {
        latencies_ms.extend(r?);
    }
    latencies_ms.sort_by(f64::total_cmp);
    Ok(Phase {
        ops: latencies_ms.len() as u64,
        secs,
        latencies_ms,
        gc_cleaned: vol.status().gc.segments_cleaned - cleaned_before,
    })
}

fn fill(vol: &Volume, rng: &mut Rng) -> bitacora::Result<()> {
    let blocks = vol.blocks();
    let block = random_block(rng);
    let buf = block.repeat(CHUNK as usize);
    let mut lba = 0;
    while lba < blocks {
        let n = CHUNK.min(blocks - lba);
        vol.write(lba, &buf[..n as usize * BLOCK_SIZE])?;
        lba += n;
    }
    vol.flush()
}

/// Overwrites random aligned chunks until the GC has freed a segment or the
/// free segments are down to its trigger. Returns whether it got there.
fn reach_gc_trigger(vol: &Volume, rng: &mut Rng) -> bitacora::Result<bool> {
    let blocks = vol.blocks();
    let buf = random_block(rng).repeat(CHUNK as usize);
    let cleaned = vol.status().gc.segments_cleaned;
    let mut budget = MAX_TRIGGER_PASSES * blocks;
    loop {
        let status = vol.status();
        if status.gc.segments_cleaned > cleaned || status.segments_free <= GC_RESERVE_SEGMENTS {
            return Ok(true);
        }
        if budget < CHUNK {
            return Ok(false);
        }
        let lba = rng.below((blocks / CHUNK).max(1)) * CHUNK;
        let n = CHUNK.min(blocks - lba);
        vol.write(lba, &buf[..n as usize * BLOCK_SIZE])?;
        vol.flush()?;
        budget -= CHUNK;
    }
}

pub struct Report {
    pub read: Phase,
    pub write: Phase,
    pub gc: Phase,
    /// The GC trigger was reached before the third phase.
    pub gc_reached: bool,
}

pub fn measure(path: PathBuf, seed: u64, p: &Params) -> Result<Report, Box<dyn Error>> {
    let _ = fs::remove_file(&path);
    let scratch = Scratch(path);
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(DEFAULT_SEGMENT_SIZE)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(Arc::new(FileMedium::create(&scratch.0, true)?), config)?;
    let mut rng = Rng::stream(seed, 0);
    fill(&vol, &mut rng)?;
    let read = phase(&vol, p, seed, 1, false)?;
    let write = phase(&vol, p, seed, 2, true)?;
    let gc_reached = reach_gc_trigger(&vol, &mut rng)?;
    let gc = phase(&vol, p, seed, 3, true)?;
    vol.close()?;
    drop(vol);
    drop(scratch);
    Ok(Report {
        read,
        write,
        gc,
        gc_reached,
    })
}

/// `"name": {iops, p50_ms, p99_ms, p999_ms, operaciones, segundos}`.
fn phase_json(name: &str, ph: &mut Phase) -> String {
    format!(
        "  {}: {{\n    \"iops\": {},\n    \"p50_ms\": {},\n    \"p99_ms\": {},\n    \
         \"p999_ms\": {},\n    \"operaciones\": {},\n    \"segundos\": {},\n    \
         \"segmentos_liberados_por_gc\": {}\n  }}",
        json_str(name),
        json_num(ph.iops()),
        json_num(ph.quantile(0.50)),
        json_num(ph.quantile(0.99)),
        json_num(ph.quantile(0.999)),
        ph.ops,
        json_num(ph.secs),
        ph.gc_cleaned
    )
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let mut r = measure(ctx.dir.join("volume.img"), ctx.seed, &FULL)?;
    let mut both: Vec<f64> = r
        .read
        .latencies_ms
        .iter()
        .chain(&r.write.latencies_ms)
        .copied()
        .collect();
    let (iops_read, iops_write) = (r.read.iops(), r.write.iops());
    let p50 = percentile(&mut both, 0.50);
    let p99 = percentile(&mut both, 0.99);
    let p999 = percentile(&mut both, 0.999);
    let p99_gc = r.gc.quantile(0.99);
    let below = iops_read < REFERENCE_IOPS || iops_write < REFERENCE_IOPS;

    ctx.line(format!("iops_lectura: {iops_read:.1}"));
    ctx.line(format!("iops_escritura: {iops_write:.1}"));
    ctx.line(format!("p50_ms: {p50:.4}"));
    ctx.line(format!("p99_ms: {p99:.4}"));
    ctx.line(format!("p999_ms: {p999:.4}"));
    ctx.line(format!("p99_ms_durante_gc: {p99_gc:.4}"));
    ctx.line(format!(
        "segmentos_liberados_durante_gc: {}",
        r.gc.gc_cleaned
    ));
    ctx.line(format!("bajo_referencia: {below}"));

    let command = std::env::args().collect::<Vec<_>>().join(" ");
    let phases = [
        phase_json("lectura", &mut r.read),
        phase_json("escritura", &mut r.write),
        phase_json("sobrescritura_con_gc", &mut r.gc),
    ];
    let json = format!(
        "{{\n  \"criterio\": \"c21\",\n  \"iops_lectura\": {},\n  \"iops_escritura\": {},\n  \
         \"p50_ms\": {},\n  \"p99_ms\": {},\n  \"p999_ms\": {},\n  \"p99_ms_durante_gc\": {},\n  \
         \"bajo_referencia\": {below},\n  \"referencia_iops\": {},\n  \
         \"disparo_gc_alcanzado\": {},\n  \"volumen_bytes\": {},\n  \"hilos\": {},\n  \
         \"segundos_por_fase\": {},\n  \"flush_cada\": {},\n{},\n  \"maquina\": {{\n    \
         \"cpu\": {},\n    \"kernel\": {},\n    \"disco\": {}\n  }},\n  \"comando\": {},\n  \
         \"fecha\": {},\n  \"commit\": {}\n}}\n",
        json_num(iops_read),
        json_num(iops_write),
        json_num(p50),
        json_num(p99),
        json_num(p999),
        json_num(p99_gc),
        json_num(REFERENCE_IOPS),
        r.gc_reached,
        FULL.volume_size,
        FULL.threads,
        FULL.phase.as_secs(),
        FULL.flush_every,
        phases.join(",\n"),
        json_str(&cpu_model()),
        json_str(&kernel()),
        json_str(&disk_models()),
        json_str(&command),
        json_str(&utc_now()),
        json_str(&commit())
    );
    let results = repo_root().join("results");
    fs::create_dir_all(&results)?;
    fs::write(results.join("c21.json"), &json)?;
    fs::write(ctx.dir.join("c21.json"), &json)?;
    ctx.line("resultados: results/c21.json");
    Ok(Verdict::Pass)
}
