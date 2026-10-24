//! Criteria runners (contract section 12). `main.rs` parses the command
//! line, sets up `target/criteria/<cNN>/` and maps the verdict to the exit
//! code; each runner only does its criterion.

pub mod c01;
pub mod c02;
pub mod c03;
pub mod c07;
pub mod c08;

use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use bitacora::Volume;
use bitacora::format::BLOCK_SIZE;

use crate::model::Model;

/// Exit code of an omitted criterion.
pub const SKIP_EXIT: u8 = 77;

pub type Outcome = Result<Verdict, Box<dyn std::error::Error>>;

pub enum Verdict {
    Pass,
    Fail,
    /// Omitted with a reason (exit code 77); never counts as a pass.
    Skip(String),
}

impl Verdict {
    pub fn from_pass(pass: bool) -> Verdict {
        if pass { Verdict::Pass } else { Verdict::Fail }
    }
}

pub struct Runner {
    pub id: &'static str,
    pub default_seed: u64,
    pub run: fn(&mut Ctx) -> Outcome,
}

/// Registered runners, in criterion order. Only existing ones are listed.
pub const RUNNERS: &[Runner] = &[
    Runner {
        id: "c01",
        default_seed: 1,
        run: c01::run,
    },
    Runner {
        id: "c02",
        default_seed: 1,
        run: c02::run,
    },
    Runner {
        id: "c03",
        default_seed: 1,
        run: c03::run,
    },
    Runner {
        id: "c07",
        default_seed: 1,
        run: c07::run,
    },
    Runner {
        id: "c08",
        default_seed: 1,
        run: c08::run,
    },
];

pub fn find(id: &str) -> Option<&'static Runner> {
    RUNNERS.iter().find(|r| r.id == id)
}

/// What a runner gets: its artifact directory, its seed and its output.
pub struct Ctx {
    pub dir: PathBuf,
    /// Seed of the run. A runner that sweeps seeds sets it to the first
    /// failing one before returning `Verdict::Fail`.
    pub seed: u64,
    log: Option<File>,
    echo: bool,
}

impl Ctx {
    /// Prints to standard output and copies every line to `log`.
    pub fn new(dir: PathBuf, seed: u64, log: Option<File>) -> Ctx {
        Ctx {
            dir,
            seed,
            log,
            echo: true,
        }
    }

    /// Discards the output (tests).
    pub fn quiet(dir: PathBuf, seed: u64) -> Ctx {
        Ctx {
            dir,
            seed,
            log: None,
            echo: false,
        }
    }

    pub fn line(&mut self, text: impl Display) {
        if self.echo {
            println!("{text}");
        }
        if let Some(log) = &mut self.log {
            // Losing the copy must not hide the verdict on standard output.
            let _ = writeln!(log, "{text}");
        }
    }
}

/// Result of reading a whole volume against the model.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    /// Blocks that did not match the model (read errors included).
    pub diffs: u64,
    /// The subset of `diffs` never written that did not read as zeros.
    pub unwritten_nonzero: u64,
}

/// Blocks per read when scanning a volume.
const SCAN_CHUNK: u64 = 256;

/// Reads every block of `vol` and compares it with `model`. A chunk that
/// fails is reread block by block so each error counts once.
pub fn scan(vol: &Volume, model: &Model) -> Scan {
    let mut scan = Scan::default();
    let check = |scan: &mut Scan, lba: u64, data: Option<&[u8]>| {
        if !data.is_some_and(|d| model.matches(lba, d)) {
            scan.diffs += 1;
            if model.version(lba) == 0 {
                scan.unwritten_nonzero += 1;
            }
        }
    };
    let blocks = vol.blocks();
    let mut lba = 0;
    while lba < blocks {
        let n = SCAN_CHUNK.min(blocks - lba);
        match vol.read(lba, n) {
            Ok(buf) => {
                for (i, block) in buf.chunks_exact(BLOCK_SIZE).enumerate() {
                    check(&mut scan, lba + i as u64, Some(block));
                }
            }
            Err(_) => {
                for l in lba..lba + n {
                    let one = vol.read(l, 1).ok();
                    check(&mut scan, l, one.as_deref());
                }
            }
        }
        lba += n;
    }
    scan
}
