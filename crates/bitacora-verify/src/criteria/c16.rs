//! C16: three replica processes of 64 MiB; seeded writes through a
//! replicated `Client`; `SIGKILL` to a follower; the writes go on (the
//! primary and the other follower are a majority) and are counted; the
//! follower is started again on its volume while more writes arrive; once
//! the primary reports it `activa` at its last LSN, every replica exports
//! its live volume and the three images must hash alike and like the model.

use std::error::Error;

use bitacora::Client;
use bitacora::format::BLOCK_SIZE_U64;

use super::cluster::{CONVERGE_TIMEOUT, Cluster, IDS, others};
use super::{Ctx, Outcome, Verdict, model_sha256};
use crate::model::Model;
use crate::rng::Rng;

pub struct Params {
    pub volume_size: u64,
    pub segment_size: u64,
    /// Writes before the kill.
    pub before: u64,
    /// Writes while the follower is down.
    pub during: u64,
    /// Writes after the restart, while it rejoins.
    pub after: u64,
}

pub const FULL: Params = Params {
    volume_size: 64 << 20,
    segment_size: 1 << 20,
    before: 1_000,
    during: 1_000,
    after: 500,
};

pub struct Report {
    /// Writes acknowledged while the follower was down.
    pub writes_after_kill: u64,
    pub converged: bool,
    /// SHA-256 of the export of replica 1, 2 and 3.
    pub replica_sha: Vec<String>,
    pub model_sha: String,
}

impl Report {
    pub fn identical(&self) -> bool {
        self.converged && self.replica_sha.iter().all(|h| *h == self.model_sha)
    }
}

fn write_random(
    client: &mut Client,
    model: &mut Model,
    rng: &mut Rng,
    count: u64,
) -> Result<u64, Box<dyn Error>> {
    for _ in 0..count {
        let lba = rng.below(model.blocks());
        client.write(lba, &model.write(lba))?;
    }
    Ok(count)
}

pub fn exercise(ctx: &Ctx, p: &Params) -> Result<Report, Box<dyn Error>> {
    let mut cluster = Cluster::start(&ctx.dir, p.volume_size, p.segment_size)?;
    let (primary, _) = cluster.elect()?;
    if !cluster.converge(primary, &others(primary), CONVERGE_TIMEOUT)? {
        return Err("followers did not attach after the election".into());
    }
    let mut model = Model::new(ctx.seed, p.volume_size / BLOCK_SIZE_U64);
    let mut rng = Rng::stream(ctx.seed, 0);
    let mut client = Client::connect_replicas(&cluster.sockets())?;
    write_random(&mut client, &mut model, &mut rng, p.before)?;

    let follower = others(primary)[Rng::stream(ctx.seed, 1).below(2) as usize];
    cluster.kill(follower)?;
    let writes_after_kill = write_random(&mut client, &mut model, &mut rng, p.during)?;
    cluster.restart(follower)?;
    write_random(&mut client, &mut model, &mut rng, p.after)?;
    drop(client);

    let converged = cluster.converge(primary, &others(primary), CONVERGE_TIMEOUT)?;
    let replica_sha = IDS
        .iter()
        .map(|id| cluster.export_sha256(*id, &format!("replica{id}.img")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Report {
        writes_after_kill,
        converged,
        replica_sha,
        model_sha: model_sha256(&model),
    })
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let r = exercise(ctx, &FULL)?;
    ctx.line(format!("escrituras_tras_caida: {}", r.writes_after_kill));
    for (i, sha) in r.replica_sha.iter().enumerate() {
        ctx.line(format!("sha256 replica{}: {sha}", i + 1));
    }
    ctx.line(format!("sha256 modelo: {}", r.model_sha));
    if !r.converged {
        ctx.line("convergencia: sin alcanzar en el plazo");
    }
    let identical = r.identical();
    ctx.line(format!(
        "convergencia: {}",
        if identical { "identica" } else { "distinta" }
    ));
    Ok(Verdict::from_pass(r.writes_after_kill > 0 && identical))
}
