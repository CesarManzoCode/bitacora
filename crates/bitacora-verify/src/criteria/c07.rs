//! C7: flip one byte inside a live data block of a closed volume, reopen,
//! read the block and restore the byte; 1,000 times. A read must fail with
//! an integrity error, never return wrong data.

use std::error::Error;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use bitacora::format::BLOCK_SIZE;
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

use super::{Ctx, Outcome, Verdict};
use crate::model::Model;
use crate::rng::Rng;

const VOLUME_SIZE: u64 = 64 << 20;
const POSITIONS: u64 = 1_000;

enum Probe {
    Detected,
    WrongData,
    Unexpected(String),
}

fn probe(
    path: &Path,
    config: VolumeConfig,
    lba: u64,
    model: &Model,
) -> Result<Probe, Box<dyn Error>> {
    let medium = Arc::new(FileMedium::open(path, false)?);
    let vol = match Volume::open(medium, config) {
        Ok(vol) => vol,
        Err(bitacora::Error::Corrupt(_)) => return Ok(Probe::Detected),
        Err(e) => return Ok(Probe::Unexpected(format!("open: {e}"))),
    };
    let probe = match vol.read(lba, 1) {
        Err(bitacora::Error::Integrity { .. }) => Probe::Detected,
        Err(e) => Probe::Unexpected(format!("read: {e}")),
        Ok(data) if model.matches(lba, &data) => {
            Probe::Unexpected("the flipped byte was not visible".into())
        }
        Ok(_) => Probe::WrongData,
    };
    vol.close()?;
    Ok(probe)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let path = ctx.dir.join("volume.img");
    let config = VolumeConfig::new(VOLUME_SIZE).with_gc_mode(GcMode::Off);
    let blocks = VOLUME_SIZE / BLOCK_SIZE as u64;
    let mut model = Model::new(ctx.seed, blocks);
    let vol = Volume::create(Arc::new(FileMedium::create(&path, false)?), config)?;
    for lba in 0..blocks {
        vol.write(lba, &model.write(lba))?;
    }
    vol.close()?;
    let offsets = (0..blocks)
        .map(|lba| {
            vol.locate(lba)
                .ok_or_else(|| format!("lba {lba} has no location"))
        })
        .collect::<Result<Vec<u64>, String>>()?;
    drop(vol);

    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let mut rng = Rng::new(ctx.seed);
    let mut wrong = 0u64;
    let mut unexpected = 0u64;
    for i in 1..=POSITIONS {
        let lba = rng.below(blocks);
        let pos = offsets[lba as usize] + rng.below(BLOCK_SIZE as u64);
        let mut byte = [0u8; 1];
        file.read_exact_at(&mut byte, pos)?;
        file.write_all_at(&[!byte[0]], pos)?;
        let result = probe(&path, config, lba, &model);
        file.write_all_at(&byte, pos)?;
        match result? {
            Probe::Detected => ctx.line(format!("posicion {i}: integridad: detectada")),
            Probe::WrongData => {
                wrong += 1;
                ctx.line(format!("posicion {i}: datos erroneos sin error"));
            }
            Probe::Unexpected(msg) => {
                unexpected += 1;
                ctx.line(format!("posicion {i}: error inesperado: {msg}"));
            }
        }
    }

    let pass = wrong == 0 && unexpected == 0;
    ctx.line(if pass {
        "integridad: detectada"
    } else {
        "integridad: no detectada"
    });
    ctx.line(format!("lecturas_erroneas_sin_error: {wrong}"));
    Ok(Verdict::from_pass(pass))
}
