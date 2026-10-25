//! C3: a new process reopens c01's `volume.img`, rebuilds the model from the
//! seed and rereads every block. Runs c01 first if its image is missing or
//! was written with another seed.

use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::sync::Arc;

use bitacora::Volume;
use bitacora::medium::FileMedium;

use super::{Ctx, Outcome, Verdict, c01, scan};

pub fn run(ctx: &mut Ctx) -> Outcome {
    let c01_dir = ctx.dir.with_file_name("c01");
    let image = c01_dir.join("volume.img");
    let recorded = fs::read_to_string(c01_dir.join("seed"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    if !image.exists() || recorded != Some(ctx.seed) {
        let status = Command::new(env::current_exe()?)
            .args(["c01", "--seed", &ctx.seed.to_string()])
            .stdout(Stdio::null())
            .status()?;
        if !image.exists() {
            return Err(format!("c01 left no volume.img (exit {status})").into());
        }
    }

    let p = &c01::FULL;
    let model = c01::model(ctx.seed, p);
    let vol = Volume::open(Arc::new(FileMedium::open(&image, false)?), p.config())?;
    let result = scan(&vol, &model);
    vol.close()?;

    ctx.line(format!("bloques: {}", p.blocks()));
    ctx.line(format!("diferencias: {}", result.diffs));
    Ok(Verdict::from_pass(result.diffs == 0))
}
