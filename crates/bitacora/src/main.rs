//! `bitacora` command line. Subcommands not listed in `run` arrive in later
//! sprints.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bitacora::backup;
use bitacora::medium::FileMedium;
use bitacora::server;
use bitacora::{Client, GcMode, Server, Version, Volume, VolumeConfig};

const USAGE: &str = "\
usage: bitacora <command> [options]

commands:
  snapshot   create|delete ID|list --volume F
  export     --volume F [--snapshot ID] --out IMG
  backup     full --volume F [--snapshot ID] --out BKP
             incremental --volume F --base ID [--snapshot ID] --out BKP
  restore    --out F BKP...
  serve      --volume F --socket S [--create SIZE]
  status     --socket S [--json]
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
        "backup" => backup(rest),
        "restore" => restore(rest),
        "serve" => serve(rest),
        "status" => status(rest),
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

/// `backup full|incremental`. Without `--snapshot`, a snapshot is taken at
/// the start and its id printed, so the next incremental can use it as base.
fn backup(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["volume", "snapshot", "base", "out"])?;
    let [kind] = opts.positional.as_slice() else {
        return Err(CliError::Usage("backup needs full or incremental".into()));
    };
    let base = match (kind.as_str(), opts.get("base")) {
        ("full", None) => None,
        ("incremental", Some(id)) => Some(parse_id(id)?),
        ("full", Some(_)) => return Err(CliError::Usage("backup full takes no --base".into())),
        ("incremental", None) => return Err(CliError::Usage("missing --base".into())),
        (other, _) => return Err(CliError::Usage(format!("unknown backup kind: {other}"))),
    };
    let path = opts.require("volume")?;
    let out = PathBuf::from(opts.require("out")?);
    let snapshot = opts.get("snapshot").map(parse_id).transpose()?;
    let vol = open_volume(path)?;
    let target = match snapshot {
        Some(id) => id,
        None => {
            let id = vol.snapshot_create()?;
            println!("{id}");
            id
        }
    };
    match base {
        None => vol.backup_full(target, &out)?,
        Some(base) => vol.backup_incremental(base, target, &out)?,
    };
    vol.close()?;
    Ok(())
}

/// `restore --out F BKP...`: a new volume at F from a full backup and its
/// incrementals, in order.
fn restore(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["out"])?;
    if opts.positional.is_empty() {
        return Err(CliError::Usage("restore needs at least one backup".into()));
    }
    let out = PathBuf::from(opts.require("out")?);
    if out.exists() {
        return Err(CliError::Failed(format!("{} already exists", out.display())));
    }
    let head = backup::verify(&PathBuf::from(&opts.positional[0]))?;
    // The GC stays on: the incrementals overwrite blocks of the full backup.
    let config = VolumeConfig::new(head.volume_size);
    let medium = Arc::new(FileMedium::create(&out, false)?);
    let vol = backup::restore(&opts.positional, medium, config)?;
    vol.close()?;
    Ok(())
}

/// `SIZE` in bytes, with an optional binary suffix K, M, G or T.
fn parse_size(text: &str) -> Result<u64, CliError> {
    let (digits, shift) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 10),
        Some('M' | 'm') => (&text[..text.len() - 1], 20),
        Some('G' | 'g') => (&text[..text.len() - 1], 30),
        Some('T' | 't') => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .ok_or_else(|| CliError::Usage(format!("invalid size: {text}")))
}

/// `serve --volume F --socket S [--create SIZE]`: serves the volume until
/// `SIGTERM` or `SIGINT`, then closes it. With `--create`, a missing or
/// empty file becomes a new volume of SIZE bytes; an existing volume is
/// opened and must have that size.
fn serve(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["volume", "socket", "create"])?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("serve: unexpected arguments".into()));
    }
    let path = PathBuf::from(opts.require("volume")?);
    let socket = PathBuf::from(opts.require("socket")?);
    let create = opts.get("create").map(parse_size).transpose()?;
    // Before any thread exists, so that only the signal thread gets them.
    server::block_signals().map_err(|e| CliError::Failed(e.to_string()))?;
    let empty = std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
    let vol = match create {
        Some(size) if empty => {
            let medium = Arc::new(FileMedium::create(&path, false)?);
            Volume::create(medium, VolumeConfig::new(size))?
        }
        Some(size) => {
            let medium = Arc::new(FileMedium::open(&path, false)?);
            let config = VolumeConfig::from_medium(&*medium)?;
            if config.volume_size != size {
                return Err(CliError::Failed(format!(
                    "{} has size {}, not {size}",
                    path.display(),
                    config.volume_size
                )));
            }
            Volume::open(medium, config)?
        }
        None => {
            let medium = Arc::new(FileMedium::open(&path, false)?);
            let config = VolumeConfig::from_medium(&*medium)?;
            Volume::open(medium, config)?
        }
    };
    let vol = Arc::new(vol);
    let srv = Server::bind(vol.clone(), &socket)?;
    server::shutdown_on_signal(srv.handle()).map_err(|e| CliError::Failed(e.to_string()))?;
    let served = srv.run();
    let closed = vol.close();
    served?;
    closed?;
    Ok(())
}

/// `status --socket S [--json]`: the `bitacora-status/1` JSON of a served
/// volume, or a short summary without `--json`.
fn status(args: &[String]) -> Result<(), CliError> {
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    let opts = Options::parse(&rest, &["socket"])?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("status: unexpected arguments".into()));
    }
    let socket = opts.require("socket")?;
    let failed = |e: bitacora::ClientError| CliError::Failed(e.to_string());
    let mut client = Client::connect(socket).map_err(failed)?;
    let report = client.status().map_err(failed)?;
    if json {
        println!("{}", report.to_json_pretty());
    } else {
        let v = &report.volume;
        println!("volume {} state {:?}", v.volume_id, v.state);
        println!("size {} epoch {} snapshots {}", v.volume_size, v.epoch, v.snapshots);
        println!("last_lsn {} durable_lsn {}", v.last_lsn, v.durable_lsn);
        let s = &report.space;
        println!(
            "segments used {} free {} of {}",
            s.used_segments, s.free_segments, s.max_segments
        );
    }
    Ok(())
}
