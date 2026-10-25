//! C25: three replica processes of 64 MiB; `bitacora status --json` of the
//! primary to `status-antes.json`; `SIGKILL` to a follower; `bitacora
//! status --json` every 50 ms until the primary reports that follower
//! `caida`, whose answer goes to `status-despues.json`. `events.log` holds
//! `kill <id> <t_ms>` and `caida <id> <t_ms>`, both in milliseconds of one
//! monotonic clock started with the runner; `caida` is taken when the
//! answer that shows it arrives. Passes only if the detection takes at
//! most `c25_max_detection_ms`.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use bitacora::StatusReport;
use bitacora::status::ReplicaState;

use super::cluster::{CONVERGE_TIMEOUT, Cluster, others};
use super::{Ctx, Outcome, Verdict, threshold};
use crate::rng::Rng;

const VOLUME_SIZE: u64 = 64 << 20;
const SEGMENT_SIZE: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(50);
/// The poll gives up after this many times the threshold.
const GIVE_UP_FACTOR: u32 = 10;

pub fn run(ctx: &mut Ctx) -> Outcome {
    let limit_ms = threshold(&["c25_max_detection_ms"])?;
    let clock = Instant::now();
    let ms = |at: Instant| at.duration_since(clock).as_millis() as u64;

    let mut cluster = Cluster::start(&ctx.dir, VOLUME_SIZE, SEGMENT_SIZE)?;
    let (primary, _) = cluster.elect()?;
    if !cluster.converge(primary, &others(primary), CONVERGE_TIMEOUT)? {
        return Err("followers did not attach after the election".into());
    }
    fs::write(
        ctx.dir.join("status-antes.json"),
        cluster.status_json(primary)?,
    )?;

    let follower = others(primary)[Rng::stream(ctx.seed, 1).below(2) as usize];
    cluster.kill(follower)?;
    let killed_at = Instant::now();
    let mut events = format!("kill {follower} {}\n", ms(killed_at));

    let give_up = Duration::from_millis(limit_ms as u64) * GIVE_UP_FACTOR;
    let mut next = killed_at;
    let detected_at = loop {
        let json = cluster.status_json(primary)?;
        let seen = Instant::now();
        let report = StatusReport::from_json(json.as_bytes())?;
        let down = report
            .replicas
            .iter()
            .any(|r| r.id == follower && r.state == ReplicaState::Caida);
        if down {
            fs::write(ctx.dir.join("status-despues.json"), &json)?;
            break Some(seen);
        }
        if seen.duration_since(killed_at) >= give_up {
            fs::write(ctx.dir.join("status-despues.json"), &json)?;
            break None;
        }
        next += POLL;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
    };

    let Some(detected_at) = detected_at else {
        fs::write(ctx.dir.join("events.log"), &events)?;
        ctx.line(format!(
            "caida: replica {follower} sin detectar en {} ms",
            give_up.as_millis()
        ));
        return Ok(Verdict::Fail);
    };
    events.push_str(&format!("caida {follower} {}\n", ms(detected_at)));
    fs::write(ctx.dir.join("events.log"), &events)?;
    let detection_ms = ms(detected_at) - ms(killed_at);
    ctx.line(format!("kill {follower} {}", ms(killed_at)));
    ctx.line(format!("caida {follower} {}", ms(detected_at)));
    ctx.line(format!("deteccion_ms: {detection_ms}"));
    ctx.line(format!("umbral_ms: {limit_ms}"));
    Ok(Verdict::from_pass(detection_ms as f64 <= limit_ms))
}
