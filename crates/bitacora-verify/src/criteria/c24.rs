//! C24: the engineering requirements document exists with its six level-2
//! headings (contract section 12). Whether its content is right is a human
//! judgement; this runner only checks the headings.

use std::fs;

use super::{Ctx, Outcome, Verdict, repo_root};

pub const DOCUMENT: &str = "docs/engineering-requirements.md";

/// The exact headings, in the order of the contract.
pub const HEADINGS: [&str; 6] = [
    "## Requisitos",
    "## Garantías prometidas",
    "## Garantías no prometidas",
    "## Modos de falla conocidos",
    "## Plan de capacidad",
    "## Con 100 veces más tamaño",
];

/// How many of `HEADINGS` appear in `text` as a whole line (trailing
/// spaces ignored).
pub fn headings_found(text: &str) -> usize {
    HEADINGS
        .iter()
        .filter(|h| text.lines().any(|l| l.trim_end() == **h))
        .count()
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let text = match fs::read_to_string(repo_root().join(DOCUMENT)) {
        Ok(text) => text,
        Err(e) => {
            ctx.line(format!("{DOCUMENT}: {e}"));
            String::new()
        }
    };
    let found = headings_found(&text);
    for h in HEADINGS {
        if !text.lines().any(|l| l.trim_end() == h) {
            ctx.line(format!("falta: {h}"));
        }
    }
    ctx.line(format!("encabezados: {found} de {}", HEADINGS.len()));
    Ok(Verdict::from_pass(found == HEADINGS.len()))
}
