//! C6: write amplification and write latency under two loads. A 1 GiB volume
//! is filled sequentially and flushed; then 3 × volume of random 4 KiB
//! writes follow, with a `flush` every 256 writes. The amplification is
//! (bytes written to the medium) / (bytes written by the client) during the
//! random phase, counted by `CountingMedium`. `p99_ms` is the 99th percentile
//! of the time of one operation of that phase: a `write`, plus the `flush`
//! that follows it every 256 writes. Writes `results/c06.json`.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bitacora::format::{BLOCK_SIZE, BLOCK_SIZE_U64, DEFAULT_SEGMENT_SIZE};
use bitacora::medium::{FileMedium, Medium};
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict, repo_root, threshold};
use crate::model::Model;
use crate::rng::Rng;

/// A medium that adds up the bytes passed to `write_at` of the medium it
/// wraps. Holes and syncs are not writes and are not counted.
pub struct CountingMedium<M: Medium = FileMedium> {
    inner: M,
    written: AtomicU64,
}

impl<M: Medium> CountingMedium<M> {
    pub fn new(inner: M) -> Self {
        CountingMedium {
            inner,
            written: AtomicU64::new(0),
        }
    }

    /// Bytes written since creation or the last `reset`.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.written.store(0, Ordering::SeqCst);
    }
}

impl<M: Medium> Medium for CountingMedium<M> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> bitacora::Result<usize> {
        self.inner.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> bitacora::Result<()> {
        self.inner.write_at(buf, offset)?;
        self.written.fetch_add(buf.len() as u64, Ordering::SeqCst);
        Ok(())
    }

    fn sync(&self) -> bitacora::Result<()> {
        self.inner.sync()
    }

    fn len(&self) -> bitacora::Result<u64> {
        self.inner.len()
    }

    fn punch_hole(&self, offset: u64, len: u64) -> bitacora::Result<()> {
        self.inner.punch_hole(offset, len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// 20 % of the blocks receive 80 % of the writes.
    HotCold,
    Uniform,
}

impl Load {
    pub fn name(self) -> &'static str {
        match self {
            Load::HotCold => "caliente_fria",
            Load::Uniform => "uniforme",
        }
    }
}

pub const LOADS: [Load; 2] = [Load::HotCold, Load::Uniform];

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// Random writes = `passes` × volume blocks.
    pub passes: u64,
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 1 << 30,
    segment_size: DEFAULT_SEGMENT_SIZE,
    passes: 3,
    flush_every: 256,
};

/// Blocks per `write` call while filling.
const FILL_CHUNK: u64 = 256;

pub struct LoadResult {
    pub client_bytes: u64,
    pub medium_bytes: u64,
    pub write_amplification: f64,
    pub p99_ms: f64,
}

/// Runs `load` on a new volume at `path`.
pub fn measure(
    path: &Path,
    load: Load,
    seed: u64,
    p: &Params,
) -> Result<LoadResult, Box<dyn Error>> {
    let medium = Arc::new(CountingMedium::new(FileMedium::create(path, false)?));
    let config = VolumeConfig::new(p.volume_size)
        .with_segment_size(p.segment_size)
        .with_gc_mode(GcMode::Auto);
    let vol = Volume::create(medium.clone(), config)?;
    let blocks = vol.blocks();
    let mut model = Model::new(seed, blocks);

    let mut buf = vec![0u8; FILL_CHUNK as usize * BLOCK_SIZE];
    let mut lba = 0;
    while lba < blocks {
        let n = FILL_CHUNK.min(blocks - lba);
        for i in 0..n {
            let start = i as usize * BLOCK_SIZE;
            buf[start..start + BLOCK_SIZE].copy_from_slice(&model.write(lba + i));
        }
        vol.write(lba, &buf[..n as usize * BLOCK_SIZE])?;
        lba += n;
    }
    vol.flush()?;

    medium.reset();
    let hot = (blocks / 5).max(1);
    let mut rng = Rng::stream(seed, load as u64);
    let writes = p.passes * blocks;
    let mut latencies = Vec::with_capacity(writes as usize);
    for i in 1..=writes {
        let lba = match load {
            Load::Uniform => rng.below(blocks),
            Load::HotCold if rng.chance(4, 5) => rng.below(hot),
            Load::HotCold => hot + rng.below(blocks - hot),
        };
        let data = model.write(lba);
        let start = Instant::now();
        vol.write(lba, &data)?;
        if i % p.flush_every == 0 {
            vol.flush()?;
        }
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    vol.flush()?;
    let medium_bytes = medium.written();
    let client_bytes = writes * BLOCK_SIZE_U64;
    vol.close()?;
    Ok(LoadResult {
        client_bytes,
        medium_bytes,
        write_amplification: medium_bytes as f64 / client_bytes as f64,
        p99_ms: percentile(&mut latencies, 0.99),
    })
}

/// Nearest-rank percentile; 0 for an empty sample.
fn percentile(sample: &mut [f64], q: f64) -> f64 {
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_by(f64::total_cmp);
    let rank = (q * sample.len() as f64).ceil() as usize;
    sample[rank.clamp(1, sample.len()) - 1]
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_num(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.4}")
    } else {
        "null".to_string()
    }
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "desconocido".to_string())
}

fn kernel() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "desconocido".to_string())
}

/// Models of every block device that reports one, sorted and joined.
fn disk_models() -> String {
    let mut models: Vec<String> = fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|dev| fs::read_to_string(dev.path().join("device/model")).ok())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    models.sort();
    models.dedup();
    if models.is_empty() {
        "desconocido".to_string()
    } else {
        models.join(", ")
    }
}

fn commit() -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "desconocido".to_string())
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let mut pass = true;
    let mut loads_json = Vec::new();
    for load in LOADS {
        let max = threshold(&["c06_max_write_amplification", load.name()])?;
        let path = ctx.dir.join(format!("{}.img", load.name()));
        let r = measure(&path, load, ctx.seed, &FULL)?;
        let _ = fs::remove_file(&path);
        ctx.line(format!(
            "carga {}: amplificacion_escritura: {:.4}, p99_ms: {:.4}, umbral: {max}",
            load.name(),
            r.write_amplification,
            r.p99_ms
        ));
        pass &= r.write_amplification <= max;
        loads_json.push(format!(
            "    {}: {{\n      \"amplificacion_escritura\": {},\n      \"p99_ms\": {},\n      \
             \"umbral\": {},\n      \"bytes_cliente\": {},\n      \"bytes_medio\": {}\n    }}",
            json_str(load.name()),
            json_num(r.write_amplification),
            json_num(r.p99_ms),
            json_num(max),
            r.client_bytes,
            r.medium_bytes
        ));
    }
    let command = std::env::args().collect::<Vec<_>>().join(" ");
    let json = format!(
        "{{\n  \"criterio\": \"c06\",\n  \"cargas\": {{\n{}\n  }},\n  \"maquina\": {{\n    \
         \"cpu\": {},\n    \"kernel\": {},\n    \"disco\": {}\n  }},\n  \"comando\": {},\n  \
         \"fecha\": {},\n  \"commit\": {}\n}}\n",
        loads_json.join(",\n"),
        json_str(&cpu_model()),
        json_str(&kernel()),
        json_str(&disk_models()),
        json_str(&command),
        json_str(&utc_now()),
        json_str(&commit())
    );
    let results = repo_root().join("results");
    fs::create_dir_all(&results)?;
    fs::write(results.join("c06.json"), &json)?;
    fs::write(ctx.dir.join("c06.json"), &json)?;
    ctx.line("resultados: results/c06.json");
    Ok(Verdict::from_pass(pass))
}
