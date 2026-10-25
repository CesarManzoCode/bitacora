//! `bitacora-verify <cNN> [--seed N]` runs one criterion; `bitacora-verify all`
//! runs every registered one, each in its own process, and writes
//! `target/criteria/report.txt` (contract section 12).

use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bitacora_verify::criteria::{self, Ctx, RUNNERS, SKIP_EXIT, Verdict};

const USAGE: &str = "usage: bitacora-verify <cNN|all> [--seed N]";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let (id, seed) = match parse(&args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("{msg}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let root = criteria_root();
    let result = if id == "all" {
        run_all(&root)
    } else {
        run_one(&root, &id, seed)
    };
    result.unwrap_or_else(|e| {
        eprintln!("bitacora-verify: {e}");
        ExitCode::from(2)
    })
}

fn parse(args: &[String]) -> Result<(String, Option<u64>), String> {
    let mut id = None;
    let mut seed = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seed" => {
                let value = it.next().ok_or("--seed needs a value")?;
                seed = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid seed: {value}"))?,
                );
            }
            _ if id.is_none() => id = Some(arg.clone()),
            _ => return Err(format!("unexpected argument: {arg}")),
        }
    }
    Ok((id.ok_or("missing criterion")?, seed))
}

/// `target/criteria`, taking `target` from the binary's location
/// (`target/<profile>/bitacora-verify`) and falling back to `./target`.
fn criteria_root() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| {
            let target = exe.parent()?.parent()?;
            (target.file_name()? == "target").then(|| target.to_path_buf())
        })
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("criteria")
}

fn run_one(root: &Path, id: &str, seed: Option<u64>) -> std::io::Result<ExitCode> {
    let Some(runner) = criteria::find(id) else {
        eprintln!("unknown criterion: {id}\n{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let dir = root.join(id);
    fs::create_dir_all(&dir)?;
    let log = File::create(dir.join("stdout.log"))?;
    let mut ctx = Ctx::new(dir, seed.unwrap_or(runner.default_seed), Some(log));
    let verdict = (runner.run)(&mut ctx).unwrap_or_else(|e| {
        ctx.line(format!("error: {e}"));
        Verdict::Fail
    });
    Ok(match verdict {
        Verdict::Pass => ExitCode::SUCCESS,
        Verdict::Skip(reason) => {
            ctx.line(format!("omitido: {reason}"));
            ExitCode::from(SKIP_EXIT)
        }
        Verdict::Fail => {
            ctx.line(format!("falla reproducible: semilla {}", ctx.seed));
            ExitCode::FAILURE
        }
    })
}

fn run_all(root: &Path) -> std::io::Result<ExitCode> {
    let exe = env::current_exe()?;
    let mut report = String::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for runner in RUNNERS {
        let code = Command::new(&exe).arg(runner.id).status()?.code();
        let verdict = match code {
            Some(0) => "PASS".to_string(),
            Some(c) if c == i32::from(SKIP_EXIT) => {
                let reason = skip_reason(&root.join(runner.id));
                skipped.push(format!("{}: {reason}", runner.id));
                format!("OMITIDO {reason}")
            }
            _ => {
                failed = true;
                "FAIL".to_string()
            }
        };
        report.push_str(&format!("{}: {verdict}\n", runner.id));
    }
    // C23 asks for the list of omitted criteria; it is empty when none was.
    report.push_str("\nOmitidos:\n");
    for line in &skipped {
        report.push_str(&format!("- {line}\n"));
    }
    fs::create_dir_all(root)?;
    fs::write(root.join("report.txt"), &report)?;
    print!("{report}");
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The reason from the last `omitido: <razón>` line of a runner's log.
fn skip_reason(dir: &Path) -> String {
    fs::read_to_string(dir.join("stdout.log"))
        .ok()
        .and_then(|log| {
            log.lines()
                .rev()
                .find_map(|l| l.strip_prefix("omitido: ").map(str::to_string))
        })
        .unwrap_or_else(|| "sin razon registrada".to_string())
}
