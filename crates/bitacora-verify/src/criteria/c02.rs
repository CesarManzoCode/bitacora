//! C2: a new volume reads as zeros everywhere; after 1,000 random writes,
//! every block never written still reads as zeros.

use std::sync::Arc;

use bitacora::Volume;
use bitacora::medium::FileMedium;

use super::{Ctx, Outcome, Verdict, c01, scan};
use crate::model::Model;
use crate::rng::Rng;

const WRITES: u64 = 1_000;

pub fn run(ctx: &mut Ctx) -> Outcome {
    let p = &c01::FULL;
    let medium = Arc::new(FileMedium::create(&ctx.dir.join("volume.img"), false)?);
    let vol = Volume::create(medium, p.config())?;
    let mut model = Model::new(ctx.seed, p.blocks());
    let before = scan(&vol, &model);
    let mut rng = Rng::new(ctx.seed);
    for _ in 0..WRITES {
        let lba = rng.below(p.blocks());
        vol.write(lba, &model.write(lba))?;
    }
    let after = scan(&vol, &model);
    vol.close()?;

    let unwritten_nonzero = before.unwritten_nonzero + after.unwritten_nonzero;
    let diffs = before.diffs + after.diffs;
    ctx.line(format!("bloques: {}", p.blocks()));
    ctx.line(format!("no_escritos_no_cero: {unwritten_nonzero}"));
    ctx.line(format!("diferencias: {diffs}"));
    Ok(Verdict::from_pass(diffs == 0))
}
