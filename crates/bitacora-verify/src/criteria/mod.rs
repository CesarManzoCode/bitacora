//! Criteria runners (contract section 12). `main.rs` parses the command
//! line, sets up `target/criteria/<cNN>/` and maps the verdict to the exit
//! code; each runner only does its criterion.

pub mod c01;
pub mod c02;
pub mod c03;
pub mod c04;
pub mod c05;
pub mod c06;
pub mod c07;
pub mod c08;
pub mod c09;
pub mod c10;
pub mod c11;
pub mod c12;
pub mod c13;
pub mod c14;
pub mod c15;
pub mod c16;
pub mod c17;
pub mod c18;
pub mod c21;
pub mod c22;
pub mod c24;
pub mod c25;
pub mod c26;
pub mod cluster;
pub mod ublk;

use std::error::Error;
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use bitacora::Volume;
use bitacora::format::BLOCK_SIZE;

use sha2::{Digest, Sha256};

use crate::model::Model;
use crate::rng::block_data;

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

/// Registered runners, in the order `all` runs them: c01–c22, c24, c25
/// and c26 (C23 is `make criteria` itself).
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
        id: "c04",
        default_seed: 1,
        run: c04::run,
    },
    Runner {
        id: "c05",
        default_seed: 1,
        run: c05::run,
    },
    Runner {
        id: "c06",
        default_seed: 1,
        run: c06::run,
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
    Runner {
        id: "c09",
        default_seed: 1,
        run: c09::run,
    },
    Runner {
        id: "c10",
        default_seed: 1,
        run: c10::run,
    },
    Runner {
        id: "c11",
        default_seed: 1,
        run: c11::run,
    },
    Runner {
        id: "c12",
        default_seed: 1,
        run: c12::run,
    },
    Runner {
        id: "c13",
        default_seed: 1,
        run: c13::run,
    },
    Runner {
        id: "c14",
        default_seed: 1,
        run: c14::run,
    },
    Runner {
        id: "c15",
        default_seed: 1,
        run: c15::run,
    },
    Runner {
        id: "c16",
        default_seed: 1,
        run: c16::run,
    },
    Runner {
        id: "c17",
        default_seed: 1,
        run: c17::run,
    },
    Runner {
        id: "c18",
        default_seed: 1,
        run: c18::run,
    },
    Runner {
        id: "c19",
        default_seed: 1,
        run: ublk::c19,
    },
    Runner {
        id: "c20",
        default_seed: 1,
        run: ublk::c20,
    },
    Runner {
        id: "c21",
        default_seed: 1,
        run: c21::run,
    },
    Runner {
        id: "c22",
        default_seed: 1,
        run: c22::run,
    },
    Runner {
        id: "c24",
        default_seed: 1,
        run: c24::run,
    },
    Runner {
        id: "c25",
        default_seed: 1,
        run: c25::run,
    },
    Runner {
        id: "c26",
        default_seed: 1,
        run: c26::run,
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
                for (i, block) in buf.as_chunks::<BLOCK_SIZE>().0.iter().enumerate() {
                    check(&mut scan, lba + i as u64, Some(block.as_slice()));
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

/// Whether `data` is version `min..=max` of `lba` under `seed`; with
/// `min == 0`, zeros also match.
pub fn holds_version(seed: u64, lba: u64, data: &[u8], min: u64, max: u64) -> bool {
    if data.len() != BLOCK_SIZE {
        return false;
    }
    if min == 0 && data.iter().all(|&b| b == 0) {
        return true;
    }
    (min.max(1)..=max).any(|v| data == block_data(seed, lba, v))
}

/// Lowercase hex of a SHA-256 digest.
fn hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SHA-256 of the raw image the model describes (`blocks × 4096` bytes).
pub fn model_sha256(model: &Model) -> String {
    let mut h = Sha256::new();
    for lba in 0..model.blocks() {
        h.update(model.expected(lba));
    }
    hex(h)
}

/// SHA-256 of a file.
pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(h));
        }
        h.update(&buf[..n]);
    }
}

/// SHA-256 of a version of a volume read block range by block range.
pub fn version_sha256(vol: &Volume, version: bitacora::Version) -> bitacora::Result<String> {
    let mut h = Sha256::new();
    let blocks = vol.blocks();
    let mut lba = 0;
    while lba < blocks {
        let n = SCAN_CHUNK.min(blocks - lba);
        h.update(vol.read_at_version(version, lba, n)?);
        lba += n;
    }
    Ok(hex(h))
}

/// Root of the repository, where `thresholds.json` and `results/` live.
pub fn repo_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// The `value` of the entry reached through `keys` in `thresholds.json`.
/// A minimal scan instead of a JSON parser: the file is frozen (contract
/// section 14) and every threshold is `"key": {"value": <number>, ...}`.
pub fn threshold(keys: &[&str]) -> Result<f64, Box<dyn Error>> {
    let text = fs::read_to_string(repo_root().join("thresholds.json"))?;
    let mut pos = 0;
    for key in keys.iter().copied().chain(std::iter::once("value")) {
        let pattern = format!("\"{key}\"");
        let at = text[pos..]
            .find(&pattern)
            .ok_or_else(|| format!("thresholds.json has no {key}"))?;
        pos += at + pattern.len();
    }
    let rest = text[pos..]
        .trim_start()
        .strip_prefix(':')
        .ok_or("thresholds.json: expected ':' after \"value\"")?
        .trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(rest.len());
    Ok(rest[..end].parse()?)
}
