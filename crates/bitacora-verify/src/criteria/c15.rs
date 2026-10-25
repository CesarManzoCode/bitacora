//! C15: the pause of the migrations of C14. Reads
//! `target/criteria/c14/seed-<n>/responses.log` for every seed of C14 (and
//! runs `bitacora-verify c14` first if any is missing), takes the largest
//! separation between two consecutive responses of any seed, prints it as
//! `pausa_max_ms` and passes only if it is at most `c15_max_pause_ms` of
//! `thresholds.json`. The pause of section 10 (freeze to the first answer
//! served by D) is one of those separations, so the largest one bounds it.

use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use super::c14::{SEEDS, seed_dir};
use super::{Ctx, Outcome, Verdict, threshold};

/// Largest separation, in nanoseconds, between consecutive instants of a
/// `responses.log`; `None` with fewer than two lines.
pub fn max_gap_ns(text: &str) -> Result<Option<u64>, Box<dyn Error>> {
    let mut prev: Option<u64> = None;
    let mut max = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let t: u64 = line.trim().parse()?;
        if let Some(p) = prev {
            let gap = t.checked_sub(p).ok_or("responses.log is not monotonic")?;
            max = Some(max.map_or(gap, |m: u64| m.max(gap)));
        }
        prev = Some(t);
    }
    Ok(max)
}

fn logs_present(c14: &Path) -> bool {
    SEEDS
        .into_iter()
        .all(|seed| seed_dir(c14, seed).join("responses.log").is_file())
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let c14 = ctx
        .dir
        .parent()
        .ok_or("the runner directory has no parent")?
        .join("c14");
    if !logs_present(&c14) {
        let status = Command::new(env::current_exe()?).arg("c14").status()?;
        if !status.success() {
            ctx.line(format!("c14 fallo ({status})"));
        }
        if !logs_present(&c14) {
            return Err("c14 did not leave every responses.log".into());
        }
    }
    let mut max_ns = 0;
    for seed in SEEDS {
        let text = fs::read_to_string(seed_dir(&c14, seed).join("responses.log"))?;
        let gap =
            max_gap_ns(&text)?.ok_or_else(|| format!("seed {seed}: fewer than two responses"))?;
        max_ns = max_ns.max(gap);
    }
    let limit = threshold(&["c15_max_pause_ms"])?;
    let pause_ms = max_ns as f64 / 1e6;
    ctx.line(format!("pausa_max_ms: {pause_ms:.3}"));
    ctx.line(format!("umbral_ms: {limit}"));
    Ok(Verdict::from_pass(pause_ms <= limit))
}
