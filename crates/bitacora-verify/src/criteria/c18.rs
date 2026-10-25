//! C18: seeds 1–200. Per seed, three replica processes of 64 MiB; a seeded
//! number of acknowledged writes; `SIGSTOP` to the primary; `bitacora
//! elect` among the other two; then the runner, in the name of the old
//! primary, sends each of the other replicas an `APPEND` with the old epoch
//! and blocks no client ever wrote; `SIGCONT` lets the old primary's own
//! threads go on with its epoch too. Once the old primary has rejoined (or
//! the wait ran out), the new primary exports its live volume.
//!
//! `escrituras_aceptadas_de_epoca_vieja` counts the `OK` answers to the
//! forged frames plus the forged blocks found in the new primary's export.

use std::error::Error;
use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use bitacora::format::{BLOCK_SIZE, BLOCK_SIZE_U64};
use bitacora::replica::PeerConn;
use bitacora::wire::{Op, Request, Status};
use bitacora::{Client, Version};

use super::cluster::{CONVERGE_TIMEOUT, Cluster, others};
use super::{Ctx, Outcome, Verdict};
use crate::model::Model;
use crate::rng::{Rng, block_data};

pub const SEEDS: RangeInclusive<u64> = 1..=200;

/// Versions of the forged blocks start here, above every `u32` version
/// the model can produce, so no client write has the same content.
const FORGED_VERSION: u64 = 1 << 32;
const PEER_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// The primary is stopped after `min_writes..=max_writes` acknowledged
    /// writes, chosen by the seed.
    pub min_writes: u64,
    pub max_writes: u64,
    /// Entries of each forged `APPEND`.
    pub forged_entries: u64,
}

pub const FULL: Params = Params {
    volume_size: 64 << 20,
    segment_size: 1 << 20,
    min_writes: 1,
    max_writes: 300,
    forged_entries: 16,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    /// `OK` answers to the forged frames.
    pub ok_answers: u64,
    /// Forged blocks present in the new primary's export.
    pub blocks_found: u64,
    /// The old primary rejoined the new one within the wait.
    pub rejoined: bool,
}

impl Trial {
    pub fn accepted(&self) -> u64 {
        self.ok_answers + self.blocks_found
    }
}

pub fn trial(dir: &Path, seed: u64, p: &Params) -> Result<Trial, Box<dyn Error>> {
    let cluster = Cluster::start(dir, p.volume_size, p.segment_size)?;
    let (old, old_epoch) = cluster.elect()?;
    if !cluster.converge(old, &others(old), CONVERGE_TIMEOUT)? {
        return Err("followers did not attach after the first election".into());
    }
    let blocks = p.volume_size / BLOCK_SIZE_U64;
    let mut choice = Rng::stream(seed, 1);
    let writes = p.min_writes + choice.below(p.max_writes - p.min_writes + 1);
    let mut model = Model::new(seed, blocks);
    let mut rng = Rng::stream(seed, 0);
    let mut client = Client::connect_replicas(&cluster.sockets())?;
    for _ in 0..writes {
        let lba = rng.below(blocks);
        client.write(lba, &model.write(lba))?;
    }
    drop(client);

    cluster.signal(old, "STOP")?;
    let (elected, new_epoch) = cluster.elect()?;
    if elected == old || new_epoch <= old_epoch {
        return Err(format!(
            "replica {elected} elected with epoch {new_epoch} while {old} (epoch {old_epoch}) was stopped"
        )
        .into());
    }

    let forged: Vec<(u64, [u8; BLOCK_SIZE])> = (0..p.forged_entries)
        .map(|i| {
            let lba = choice.below(blocks);
            (lba, block_data(seed, lba, FORGED_VERSION + i))
        })
        .collect();
    let mut payload = Vec::with_capacity(forged.len() * (8 + BLOCK_SIZE));
    for (lba, data) in &forged {
        payload.extend_from_slice(&lba.to_le_bytes());
        payload.extend_from_slice(data);
    }
    let mut ok_answers = 0;
    for target in others(old) {
        // `prev_lsn` matches the target's log, so only the epoch can
        // refuse the frame.
        let prev = Client::connect(cluster.socket(target))?
            .status()?
            .volume
            .last_lsn;
        let req = Request {
            op: Op::Append,
            req_id: 0,
            epoch: old_epoch,
            lba: prev,
            nblocks: forged.len() as u32,
            payload: payload.clone(),
        };
        let resp = PeerConn::open(&cluster.socket(target), PEER_TIMEOUT)?.call(req)?;
        if resp.status == Status::Ok {
            ok_answers += 1;
        }
    }
    cluster.signal(old, "CONT")?;
    let rejoined = cluster.converge(elected, &others(elected), CONVERGE_TIMEOUT)?;

    let path = cluster.dir().join("nuevo-principal.img");
    let _ = fs::remove_file(&path);
    Client::connect(cluster.socket(elected))?.export(Version::Live, &path)?;
    let image = fs::read(&path)?;
    if image.len() as u64 != blocks * BLOCK_SIZE_U64 {
        return Err(format!("export of {} bytes for {blocks} blocks", image.len()).into());
    }
    let blocks_found = forged
        .iter()
        .filter(|(lba, data)| {
            let at = *lba as usize * BLOCK_SIZE;
            image[at..at + BLOCK_SIZE] == data[..]
        })
        .count() as u64;
    Ok(Trial {
        ok_answers,
        blocks_found,
        rejoined,
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let dir = ctx.dir.join("cluster");
    let mut accepted = 0;
    let mut first_failing = None;
    for seed in SEEDS {
        // An error ends the run with this seed as the reproducible one.
        ctx.seed = seed;
        let t = trial(&dir, seed, &FULL)?;
        ctx.line(format!(
            "semilla {seed}: escrituras_aceptadas_de_epoca_vieja: {}",
            t.accepted()
        ));
        if !t.rejoined {
            ctx.line(format!(
                "semilla {seed}: principal viejo sin reincorporar en el plazo"
            ));
        }
        if t.accepted() > 0 && first_failing.is_none() {
            first_failing = Some(seed);
        }
        accepted += t.accepted();
    }
    ctx.line(format!("escrituras_aceptadas_de_epoca_vieja: {accepted}"));
    if let Some(seed) = first_failing {
        ctx.seed = seed;
    }
    Ok(Verdict::from_pass(accepted == 0))
}
