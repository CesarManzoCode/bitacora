//! C26: the engine has more lines than its verification (contract
//! section 13).
//!
//! - Engine: the `.rs` files under `crates/bitacora/src/`.
//! - Verification: the `.rs` files under `crates/bitacora/tests/` and
//!   `crates/bitacora-verify/`, and every file under `scripts/`.
//! - A Rust line counts if it is not blank, does not start (after leading
//!   spaces) with `//` and is not inside a block comment. A shell or Python
//!   line counts if it is not blank and does not start with `#`.
//!
//! Passes only if engine > verification and `crates/bitacora/src/` holds no
//! `#[cfg(test)]`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{Ctx, Outcome, Verdict, repo_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    /// Shell and Python: `#` starts a comment line.
    Hash,
}

/// Counted lines of `text` under the rule of section 13.
///
/// A block comment is recognised when a line starts with its opening
/// delimiter, so a delimiter inside a string literal is not taken for one;
/// nested block comments are followed to their last closing delimiter. Code
/// after the closing delimiter on the same line counts.
pub fn count_lines(text: &str, lang: Lang) -> u64 {
    let mut count = 0;
    let mut depth = 0usize;
    for line in text.lines() {
        let mut rest = line.trim();
        if lang == Lang::Hash {
            if !rest.is_empty() && !rest.starts_with('#') {
                count += 1;
            }
            continue;
        }
        loop {
            if depth > 0 {
                match close_block(rest, &mut depth) {
                    Some(after) => rest = after.trim(),
                    None => break,
                }
                continue;
            }
            if let Some(inner) = rest.strip_prefix("/*") {
                depth = 1;
                rest = inner;
                continue;
            }
            if !rest.is_empty() && !rest.starts_with("//") {
                count += 1;
            }
            break;
        }
    }
    count
}

/// Scans `text` inside a block comment of nesting `depth`. Returns what
/// follows the delimiter that closes it, or `None` if it stays open past the
/// end of the line.
fn close_block<'a>(text: &'a str, depth: &mut usize) -> Option<&'a str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                *depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                *depth -= 1;
                i += 2;
                if *depth == 0 {
                    return Some(&text[i..]);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Every file under `dir`, recursively, in a stable order. A missing
/// directory has no files.
fn files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(files(&path)?);
        } else {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn is_rust(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "rs")
}

/// Counted lines of the `.rs` files under `dir`.
fn count_rust(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for path in files(dir)?.into_iter().filter(|p| is_rust(p)) {
        total += count_lines(&fs::read_to_string(&path)?, Lang::Rust);
    }
    Ok(total)
}

/// Counted lines of every file under `dir`, by its language.
fn count_any(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for path in files(dir)? {
        let lang = if is_rust(&path) {
            Lang::Rust
        } else {
            Lang::Hash
        };
        total += count_lines(&fs::read_to_string(&path)?, lang);
    }
    Ok(total)
}

/// `.rs` files under `dir` that contain `#[cfg(test)]`.
fn with_cfg_test(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in files(dir)?.into_iter().filter(|p| is_rust(p)) {
        if fs::read_to_string(&path)?.contains("#[cfg(test)]") {
            out.push(path);
        }
    }
    Ok(out)
}

pub fn run(ctx: &mut Ctx) -> Outcome {
    let root = repo_root();
    let engine_dir = root.join("crates/bitacora/src");
    let engine = count_rust(&engine_dir)?;
    let verification = count_rust(&root.join("crates/bitacora/tests"))?
        + count_rust(&root.join("crates/bitacora-verify"))?
        + count_any(&root.join("scripts"))?;
    let cfg_test = with_cfg_test(&engine_dir)?;
    for path in &cfg_test {
        let shown = path.strip_prefix(&root).unwrap_or(path);
        ctx.line(format!("cfg_test_en_motor: {}", shown.display()));
    }
    ctx.line(format!("lineas_motor: {engine}"));
    ctx.line(format!("lineas_verificacion: {verification}"));
    Ok(Verdict::from_pass(
        engine > verification && cfg_test.is_empty(),
    ))
}
