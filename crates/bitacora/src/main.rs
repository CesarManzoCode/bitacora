//! `bitacora` command line. Subcommands not listed in `run` arrive in later
//! sprints.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bitacora::backup;
use bitacora::medium::FileMedium;
use bitacora::{migrate, replica, server};
use bitacora::{Client, GcMode, Replica, ReplicaConfig, Server, Version, Volume, VolumeConfig};

const USAGE: &str = "\
usage: bitacora <command> [options]

commands:
  snapshot   create|delete ID|list --volume F
  export     --volume F [--snapshot ID] --out IMG
  backup     full --volume F [--snapshot ID] --out BKP
             incremental --volume F --base ID [--snapshot ID] --out BKP
  restore    --out F BKP...
  serve      --volume F --socket S [--create SIZE] [--migrate-target]
  status     --socket S [--json]
  replica    --id N --volume F --socket S --peers ID=S,... [--create SIZE]
             [--segment-size SIZE]
  elect      --replicas S1,S2,S3
  migrate    --from S --to D
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
        "replica" => replica_cmd(rest),
        "elect" => elect(rest),
        "migrate" => migrate_cmd(rest),
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

/// `serve --volume F --socket S [--create SIZE] [--migrate-target]`:
/// serves the volume until `SIGTERM` or `SIGINT`, then closes it. With
/// `--create`, a missing or empty file becomes a new volume of SIZE bytes;
/// an existing volume is opened and must have that size. With
/// `--migrate-target`, the server waits for a live migration and serves
/// only once a source commits it (contract section 10).
fn serve(args: &[String]) -> Result<(), CliError> {
    let target = args.iter().any(|a| a == "--migrate-target");
    let rest: Vec<String> = args
        .iter()
        .filter(|a| *a != "--migrate-target")
        .cloned()
        .collect();
    let opts = Options::parse(&rest, &["volume", "socket", "create"])?;
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
    let srv = if target {
        Server::bind_migrate_target(vol.clone(), &socket)?
    } else {
        Server::bind(vol.clone(), &socket)?
    };
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

/// `ID=SOCKET,...` of `--peers`.
fn parse_peers(text: &str) -> Result<Vec<(u64, PathBuf)>, CliError> {
    text.split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (id, socket) = p
                .split_once('=')
                .ok_or_else(|| CliError::Usage(format!("peer {p} is not ID=SOCKET")))?;
            let id = id
                .parse()
                .map_err(|_| CliError::Usage(format!("invalid peer id: {id}")))?;
            Ok((id, PathBuf::from(socket)))
        })
        .collect()
}

/// `replica --id N --volume F --socket S --peers ID=S,... [--create SIZE]
/// [--segment-size SIZE]`: runs a replica (contract section 9) until
/// `SIGTERM` or `SIGINT`. It starts as a follower; `elect` makes a primary.
/// With `--create`, a missing or empty file becomes a new volume of SIZE
/// bytes.
fn replica_cmd(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(
        args,
        &["id", "volume", "socket", "peers", "create", "segment-size"],
    )?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("replica: unexpected arguments".into()));
    }
    let id_text = opts.require("id")?;
    let id = id_text
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid replica id: {id_text}")))?;
    let peers = parse_peers(opts.require("peers")?)?;
    let segment = opts.get("segment-size").map(parse_size).transpose()?;
    let create = opts
        .get("create")
        .map(parse_size)
        .transpose()?
        .map(|size| match segment {
            Some(seg) => VolumeConfig::new(size).with_segment_size(seg),
            None => VolumeConfig::new(size),
        });
    // Before any thread exists, so that only the signal thread gets them.
    server::block_signals().map_err(|e| CliError::Failed(e.to_string()))?;
    let running = Replica::start(ReplicaConfig {
        id,
        volume: PathBuf::from(opts.require("volume")?),
        socket: PathBuf::from(opts.require("socket")?),
        peers,
        create,
        gc_mode: GcMode::Auto,
    })?;
    let stopper = running.stopper();
    server::on_signal(move || stopper.stop()).map_err(|e| CliError::Failed(e.to_string()))?;
    running.wait()?;
    Ok(())
}

/// `elect --replicas S1,S2,S3`: two-phase election; prints the elected
/// replica, its epoch and its socket.
fn elect(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["replicas"])?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("elect: unexpected arguments".into()));
    }
    let sockets: Vec<PathBuf> = opts
        .require("replicas")?
        .split(',')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    if sockets.is_empty() {
        return Err(CliError::Usage("elect needs at least one replica".into()));
    }
    let elected = replica::elect(&sockets)?;
    println!(
        "primary {} epoch {} socket {}",
        elected.id,
        elected.epoch,
        elected.socket.display()
    );
    Ok(())
}

/// `migrate --from S --to D`: the server at S migrates its volume live to
/// the server at D, started with `serve --migrate-target`, and from then on
/// answers `MOVED(D)`. Prints the rounds and blocks sent.
fn migrate_cmd(args: &[String]) -> Result<(), CliError> {
    let opts = Options::parse(args, &["from", "to"])?;
    if !opts.positional.is_empty() {
        return Err(CliError::Usage("migrate: unexpected arguments".into()));
    }
    let from = PathBuf::from(opts.require("from")?);
    // The source connects to D from its own working directory.
    let to = std::fs::canonicalize(opts.require("to")?)
        .map_err(|e| CliError::Failed(format!("destination socket: {e}")))?;
    let summary = migrate::request(&from, &to)?;
    println!("{}", summary.to_text());
    Ok(())
}
