//! C14: seeds 1–20. Per seed, two real `bitacora serve` processes of
//! 64 MiB, the source S and the destination D (`--migrate-target`), in
//! `seed-<n>/`. A writer thread sends seeded writes to S through a `Client`
//! without pause; once it has `warmup` acknowledged writes, `bitacora
//! migrate --from S --to D` runs while the writer goes on, follows `MOVED`
//! to D and writes `after` more blocks there. Then D exports its live
//! volume and the export must hash like the model of the acknowledged
//! writes; a block that differs counts as a lost acknowledged write. S must
//! answer `MOVED(D)` afterwards.
//!
//! The writer stops at its first error, which fails the seed: with one
//! writer and no error, every write attempted was acknowledged, so the
//! model is exact. Every response it receives (`OK` or after following
//! `MOVED`) is timed in nanoseconds of the monotonic clock (`Instant`, from
//! the start of the seed) and written to `seed-<n>/responses.log`, one per
//! line, which C15 reads. Standard output carries no time, so a seed always
//! prints the same line.

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::ops::RangeInclusive;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bitacora::format::BLOCK_SIZE_U64;
use bitacora::wire::{self, Op, Request, Status};
use bitacora::{Client, Version};

use super::cluster::bitacora_bin;
use super::{Ctx, Outcome, Verdict, file_sha256, model_sha256};
use crate::model::Model;
use crate::rng::Rng;

pub const SEEDS: RangeInclusive<u64> = 1..=20;

/// How long a started server may take to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(1);

pub struct Params {
    pub volume_size: u64,
    /// Acknowledged writes before the migration starts.
    pub warmup: u64,
    /// Acknowledged writes after the migration ends.
    pub after: u64,
    /// Writes between two `flush` calls of the writer.
    pub flush_every: u64,
}

pub const FULL: Params = Params {
    volume_size: 64 << 20,
    warmup: 8_000,
    after: 2_000,
    flush_every: 64,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    /// Blocks of D's export that differ from the model.
    pub lost: u64,
    pub dest_sha: String,
    pub model_sha: String,
    /// S answered `MOVED(D)` after the migration.
    pub source_moved: bool,
    /// The writer ended connected to D.
    pub writer_on_dest: bool,
    /// Responses timed in `responses.log`.
    pub responses: u64,
}

impl Trial {
    pub fn passed(&self) -> bool {
        self.lost == 0
            && self.dest_sha == self.model_sha
            && self.source_moved
            && self.writer_on_dest
    }
}

/// Two `bitacora serve` processes, killed and reaped on drop.
struct Servers {
    children: Vec<Child>,
}

impl Drop for Servers {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_serve(bin: &Path, dir: &Path, name: &str, size: u64, target: bool) -> Res<Child> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{name}.log")))?;
    let mut cmd = Command::new(bin);
    cmd.arg("serve")
        .arg("--volume")
        .arg(dir.join(format!("{name}.img")))
        .arg("--socket")
        .arg(dir.join(format!("{name}.sock")))
        .args(["--create", &size.to_string()]);
    if target {
        cmd.arg("--migrate-target");
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    Ok(child)
}

type Res<T> = Result<T, Box<dyn Error>>;

fn wait_ready(socket: &Path, child: &mut Child) -> Res<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("server of {} exited: {status}", socket.display()).into());
        }
        if Instant::now() >= deadline {
            return Err(format!("server did not open {}", socket.display()).into());
        }
        thread::sleep(POLL);
    }
}

/// What the writer thread hands back.
struct Load {
    model: Model,
    times_ns: Vec<u64>,
    final_path: PathBuf,
}

/// Writes seeded blocks to `socket` until `stop`, counting acknowledged
/// writes in `acked`.
fn writer(
    socket: PathBuf,
    seed: u64,
    blocks: u64,
    flush_every: u64,
    start: Instant,
    acked: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Result<Load, String> {
    let mut client = Client::connect(&socket).map_err(|e| e.to_string())?;
    let mut model = Model::new(seed, blocks);
    let mut rng = Rng::stream(seed, 0);
    let mut times_ns = Vec::new();
    let mut n = 0u64;
    while !stop.load(Ordering::SeqCst) {
        let lba = rng.below(blocks);
        let data = model.write(lba);
        client
            .write(lba, &data)
            .map_err(|e| format!("write {n} of lba {lba}: {e}"))?;
        times_ns.push(start.elapsed().as_nanos() as u64);
        n += 1;
        acked.store(n, Ordering::SeqCst);
        if n.is_multiple_of(flush_every) {
            client
                .flush()
                .map_err(|e| format!("flush after {n}: {e}"))?;
            times_ns.push(start.elapsed().as_nanos() as u64);
        }
    }
    client.flush().map_err(|e| format!("last flush: {e}"))?;
    times_ns.push(start.elapsed().as_nanos() as u64);
    Ok(Load {
        model,
        times_ns,
        final_path: client.path().to_path_buf(),
    })
}

fn wait_acked(acked: &AtomicU64, count: u64, writer: &thread::JoinHandle<Result<Load, String>>) {
    while acked.load(Ordering::SeqCst) < count && !writer.is_finished() {
        thread::sleep(POLL);
    }
}

/// Whether the server at `socket` answers a `STATUS` with `MOVED(dest)`.
fn answers_moved(socket: &Path, dest: &Path) -> Res<bool> {
    let mut stream = UnixStream::connect(socket)?;
    wire::write_request(&mut stream, &Request::new(Op::Status, 1))?;
    let resp = wire::read_response(&mut stream)?.ok_or("source closed the connection")?;
    Ok(resp.status == Status::Moved && Path::new(&resp.text()) == dest)
}

pub fn trial(dir: &Path, seed: u64, p: &Params) -> Res<Trial> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir)?;
    let dir = fs::canonicalize(dir)?;
    let bin = bitacora_bin()?;
    let src = dir.join("origen.sock");
    let dst = dir.join("destino.sock");
    let mut servers = Servers {
        children: Vec::new(),
    };
    servers
        .children
        .push(spawn_serve(&bin, &dir, "origen", p.volume_size, false)?);
    servers
        .children
        .push(spawn_serve(&bin, &dir, "destino", p.volume_size, true)?);
    wait_ready(&src, &mut servers.children[0])?;
    wait_ready(&dst, &mut servers.children[1])?;

    let blocks = p.volume_size / BLOCK_SIZE_U64;
    let start = Instant::now();
    let acked = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (src, acked, stop) = (src.clone(), acked.clone(), stop.clone());
        let flush_every = p.flush_every;
        thread::spawn(move || writer(src, seed, blocks, flush_every, start, acked, stop))
    };
    wait_acked(&acked, p.warmup, &handle);

    let out = Command::new(&bin)
        .arg("migrate")
        .arg("--from")
        .arg(&src)
        .arg("--to")
        .arg(&dst)
        .stdin(Stdio::null())
        .output();
    let migrated_at = acked.load(Ordering::SeqCst);
    if out.as_ref().is_ok_and(|o| o.status.success()) {
        wait_acked(&acked, migrated_at + p.after, &handle);
    }
    stop.store(true, Ordering::SeqCst);
    let load = handle.join().map_err(|_| "the writer panicked")??;
    let out = out?;
    fs::write(dir.join("migrate.log"), &out.stdout)?;
    if !out.status.success() {
        return Err(format!(
            "migrate failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }

    let mut log = BufWriter::new(File::create(dir.join("responses.log"))?);
    for t in &load.times_ns {
        writeln!(log, "{t}")?;
    }
    log.flush()?;

    let source_moved = answers_moved(&src, &dst)?;
    let image = dir.join("destino-export.img");
    Client::connect(&dst)?.export(Version::Live, &image)?;
    let dest_sha = file_sha256(&image)?;
    let lost = load.model.diff_image(&fs::read(&image)?);
    drop(servers);
    Ok(Trial {
        lost,
        dest_sha,
        model_sha: model_sha256(&load.model),
        source_moved,
        writer_on_dest: load.final_path == dst,
        responses: load.times_ns.len() as u64,
    })
}

/// Directory of seed `seed` under the runner's directory.
pub fn seed_dir(dir: &Path, seed: u64) -> PathBuf {
    dir.join(format!("seed-{seed}"))
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let mut lost = 0;
    let mut first_failing = None;
    for seed in SEEDS {
        // An error ends the run with this seed as the reproducible one.
        ctx.seed = seed;
        let t = trial(&seed_dir(&ctx.dir, seed), seed, &FULL)?;
        ctx.line(format!(
            "semilla {seed}: escrituras reconocidas perdidas: {}; sha256 destino={} modelo={}",
            t.lost, t.dest_sha, t.model_sha
        ));
        if !t.source_moved {
            ctx.line(format!("semilla {seed}: el origen no responde MOVED"));
        }
        if !t.writer_on_dest {
            ctx.line(format!(
                "semilla {seed}: el escritor no termino en el destino"
            ));
        }
        if !t.passed() && first_failing.is_none() {
            first_failing = Some(seed);
        }
        lost += t.lost;
    }
    ctx.line(format!("escrituras reconocidas perdidas: {lost}"));
    if let Some(seed) = first_failing {
        ctx.seed = seed;
    }
    Ok(Verdict::from_pass(first_failing.is_none()))
}
