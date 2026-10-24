//! C17: seeds 1–200. Per seed, three replica processes of 64 MiB; a
//! writer thread sends seeded writes through a replicated `Client` and
//! records the version of every acknowledged one; after a seeded number of
//! acknowledged writes the primary gets `SIGKILL` while the writer goes on,
//! so a write may be in flight. `bitacora elect` picks a new primary among
//! the two live replicas; once the other live one is attached at its last
//! LSN, both export their live volume.
//!
//! A block of the new primary's export is correct when it holds a version
//! between the last acknowledged one and the last attempted one (zeros
//! count as version 0): an unacknowledged write may or may not have
//! survived. Every other block counts as a lost acknowledged write.

use std::error::Error;
use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use bitacora::Client;
use bitacora::format::{BLOCK_SIZE, BLOCK_SIZE_U64};

use super::cluster::{CONVERGE_TIMEOUT, Cluster, others};
use super::{Ctx, Outcome, Verdict, file_sha256, holds_version};
use crate::model::Model;
use crate::rng::Rng;

pub const SEEDS: RangeInclusive<u64> = 1..=200;

/// How long the writer looks for a primary after the kill before it stops.
const WRITER_FAILOVER: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(1);

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// The primary is killed after `min_writes..=max_writes` acknowledged
    /// writes, chosen by the seed.
    pub min_writes: u64,
    pub max_writes: u64,
}

pub const FULL: Params = Params {
    volume_size: 64 << 20,
    segment_size: 1 << 20,
    min_writes: 1,
    max_writes: 400,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    /// Blocks of the new primary older than their last acknowledged write
    /// (or holding no version of the model at all).
    pub lost: u64,
    /// The two live replicas exported the same SHA-256.
    pub live_identical: bool,
}

/// What the writer thread leaves: the model of every attempted write and
/// the last acknowledged version of each block.
struct Load {
    model: Model,
    acked: Vec<u32>,
}

fn writer(
    mut client: Client,
    seed: u64,
    blocks: u64,
    count: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Load {
    let mut model = Model::new(seed, blocks);
    let mut acked = vec![0u32; blocks as usize];
    let mut rng = Rng::stream(seed, 0);
    while !stop.load(Ordering::Acquire) {
        let lba = rng.below(blocks);
        let data = model.write(lba);
        if client.write(lba, &data).is_err() {
            break;
        }
        acked[lba as usize] = model.version(lba);
        count.fetch_add(1, Ordering::Release);
    }
    Load { model, acked }
}

pub fn trial(dir: &Path, seed: u64, p: &Params) -> Result<Trial, Box<dyn Error>> {
    let mut cluster = Cluster::start(dir, p.volume_size, p.segment_size)?;
    let (primary, _) = cluster.elect()?;
    if !cluster.converge(primary, &others(primary), CONVERGE_TIMEOUT)? {
        return Err("followers did not attach after the first election".into());
    }
    let blocks = p.volume_size / BLOCK_SIZE_U64;
    let kill_after = p.min_writes + Rng::stream(seed, 1).below(p.max_writes - p.min_writes + 1);

    let mut client = Client::connect_replicas(&cluster.sockets())?;
    client.set_failover_timeout(WRITER_FAILOVER);
    let count = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (count, stop) = (count.clone(), stop.clone());
        thread::spawn(move || writer(client, seed, blocks, count, stop))
    };
    while count.load(Ordering::Acquire) < kill_after && !handle.is_finished() {
        thread::sleep(POLL);
    }
    let early = count.load(Ordering::Acquire) < kill_after;
    cluster.kill(primary)?;
    stop.store(true, Ordering::Release);
    let load = handle.join().map_err(|_| "writer thread panicked")?;
    if early {
        return Err(format!(
            "a write failed before the kill, after {} of {kill_after}",
            count.load(Ordering::Acquire)
        )
        .into());
    }

    let (elected, _) = cluster.elect()?;
    if elected == primary {
        return Err(format!("replica {primary} was killed but elected").into());
    }
    let live: Vec<u64> = others(primary);
    let follower = live
        .iter()
        .copied()
        .find(|id| *id != elected)
        .expect("two live replicas");
    let converged = cluster.converge(elected, &[follower], CONVERGE_TIMEOUT)?;

    let new_image = cluster.dir().join("nuevo-principal.img");
    let follower_image = cluster.dir().join("seguidor-vivo.img");
    for (id, path) in [(elected, &new_image), (follower, &follower_image)] {
        let _ = fs::remove_file(path);
        Client::connect(cluster.socket(id))?.export(bitacora::Version::Live, path)?;
    }
    let live_identical = converged && file_sha256(&new_image)? == file_sha256(&follower_image)?;

    let image = fs::read(&new_image)?;
    if image.len() as u64 != blocks * BLOCK_SIZE_U64 {
        return Err(format!("export of {} bytes for {blocks} blocks", image.len()).into());
    }
    let lost = image
        .as_chunks::<BLOCK_SIZE>()
        .0
        .iter()
        .enumerate()
        .filter(|(lba, data)| {
            let min = u64::from(load.acked[*lba]);
            let max = u64::from(load.model.version(*lba as u64));
            !holds_version(seed, *lba as u64, *data, min, max)
        })
        .count() as u64;
    Ok(Trial {
        lost,
        live_identical,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let dir = ctx.dir.join("cluster");
    let mut lost = 0;
    let mut first_failing = None;
    for seed in SEEDS {
        // An error ends the run with this seed as the reproducible one.
        ctx.seed = seed;
        let t = trial(&dir, seed, &FULL)?;
        ctx.line(format!(
            "semilla {seed}: escrituras reconocidas perdidas: {}; hashes vivos identicos: {}",
            t.lost,
            if t.live_identical { "si" } else { "no" }
        ));
        if (t.lost > 0 || !t.live_identical) && first_failing.is_none() {
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
