//! C19 and C20: the ublk scripts under `scripts/` (contract section 12).
//! Their standard output becomes the runner's; exit 77 is an omission with
//! the reason of the script's `omitido: <razón>` line.

use std::process::{Command, Stdio};

use super::{Ctx, Outcome, SKIP_EXIT, Verdict, repo_root};

pub fn c19(ctx: &mut Ctx) -> Outcome {
    run_script(ctx, "scripts/c19-ublk.sh")
}

pub fn c20(ctx: &mut Ctx) -> Outcome {
    run_script(ctx, "scripts/c20-ublk-recovery.sh")
}

fn run_script(ctx: &mut Ctx, script: &str) -> Outcome {
    let root = repo_root();
    let output = Command::new("bash")
        .arg(root.join(script))
        .current_dir(&root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut reason = None;
    for line in stdout.lines() {
        match line.strip_prefix("omitido: ") {
            // `main.rs` prints the `omitido:` line of a skip itself.
            Some(r) => reason = Some(r.to_string()),
            None => ctx.line(line),
        }
    }
    Ok(match output.status.code() {
        Some(0) => Verdict::Pass,
        Some(c) if c == i32::from(SKIP_EXIT) => {
            Verdict::Skip(reason.unwrap_or_else(|| "sin razon registrada".to_string()))
        }
        _ => Verdict::Fail,
    })
}
