//! `bitacora` command line. Subcommands not listed in `run` arrive in later
//! sprints.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

const USAGE: &str = "\
usage: bitacora <command> [options]

commands:
  snapshot   create|delete ID|list --volume F
  export     --volume F [--snapshot ID] --out IMG
  serve      serve a volume over a Unix socket
  status     print the status of a served volume
  replica    run a replica process
  elect      elect a primary among replicas
  migrate    live-migrate a volume between servers
  capacity   project physical capacity from a write history
  ublk       expose a volume as a ublk block device (feature `ublk`)";

/// A command line error: a usage error (exit 2) or a failed operation
/// (exit 1).
enum CliError {
    Usage(String),
    Failed(String),
}

impl From<bitacora::Error> for CliError {
    fn from(e: bitacora::Error) -> Self {
        CliError::Failed(e.to_string())
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            eprintln!("bitacora: {msg}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Failed(msg)) => {
            eprintln!("bitacora: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), CliError> {
    let Some((command, rest)) = args.split_first() else {
        return Err(CliError::Usage("missing command".into()));
    };
    match command.as_str() {
        "snapshot" => snapshot(rest),
        "export" => export(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(CliError::Usage(format!("unknown command: {other}"))),
    }
}

/// Options `--name value` and positional arguments, in order.
struct Options {
    values: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Options {
    fn parse(args: &[String], known: &[&str]) -> Result<Options, CliError> {
        let mut values = Vec::new();
        let mut positional = Vec::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if let Some(name) = arg.strip_prefix("--") {
                if !known.contains(&name) {
                    return Err(CliError::Usage(format!("unknown option: {arg}")));
                }
                let value = it
                    .next()
                    .ok_or_else(|| CliError::Usage(format!("{arg} needs a value")))?;
                values.push((name.to_string(), value.clone()));
            } else {
                positional.push(arg.clone());
            }
        }
        Ok(Options { values, positional })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn require(&self, name: &str) -> Result<&str, CliError> {
        self.get(name)
            .ok_or_else(|| CliError::Usage(format!("missing --{name}")))
    }
}

fn parse_id(text: &str) -> Result<u64, CliError> {
    text.parse()
        .map_err(|_| CliError::Usage(format!("invalid snapshot id: {text}")))
}

/// Opens the volume at `path` with the geometry of its superblock. The GC
/// is off: the command is short and leaves the space to the next server.
fn open_volume(path: &str) -> Result<Volume, CliError> {
    let medium = Arc::new(FileMedium::open(&PathBuf::from(path), false)?);
    let config = VolumeConfig::from_medium(&*medium)?.with_gc_mode(GcMode::Off);
    Ok(Volume::open(medium, config)?)
}

fn snapshot(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["volume"])?;
    let (action, rest) = opts
        .positional
        .split_first()
        .ok_or_else(|| CliError::Usage("snapshot needs create, delete or list".into()))?;
    let expected = usize::from(action == "delete");
    if rest.len() != expected {
        return Err(CliError::Usage(format!("snapshot {action}: unexpected arguments")));
    }
    let path = opts.require("volume")?;
    let vol = match action.as_str() {
        "create" | "delete" | "list" => open_volume(path)?,
        other => return Err(CliError::Usage(format!("unknown snapshot action: {other}"))),
    };
    match action.as_str() {
        "create" => {
            let id = vol.snapshot_create()?;
            println!("{id}");
        }
        "delete" => vol.snapshot_delete(parse_id(&rest[0])?)?,
        _ => {
            for s in vol.snapshots() {
                println!("{} {}", s.id, s.lsn);
            }
        }
    }
    vol.close()?;
    Ok(())
}

fn export(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["volume", "snapshot", "out"])?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("export: unexpected arguments".into()));
    }
    let path = opts.require("volume")?;
    let out = PathBuf::from(opts.require("out")?);
    let version = match opts.get("snapshot") {
        Some(id) => Version::Snapshot(parse_id(id)?),
        None => Version::Live,
    };
    let vol = open_volume(path)?;
    vol.export(version, &out)?;
    vol.close()?;
    Ok(())
}
