//! Three real `bitacora replica` processes for C16, C17, C18 and C25.
//!
//! The binary is the `bitacora` next to this executable (`target/release/`
//! for the runners, `target/<profile>/` one level up for the tests). Each
//! replica has its volume `r<id>.img`, its socket `r<id>.sock` and its log
//! `r<id>.log` in the runner's directory. Dropping a `Cluster` kills and
//! reaps every process it started, also when the runner fails half-way.

use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bitacora::status::{ReplicaState, Role};
use bitacora::{Client, StatusReport, Version};

use super::file_sha256;

pub const IDS: [u64; 3] = [1, 2, 3];

/// How long a started replica may take to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long followers may take to catch up with the primary.
pub const CONVERGE_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);

type Res<T> = Result<T, Box<dyn Error>>;

/// The `bitacora` binary next to the running executable or one directory
/// up (test binaries live in `target/<profile>/deps/`).
pub fn bitacora_bin() -> io::Result<PathBuf> {
    let exe = env::current_exe()?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    [Some(dir), dir.parent()]
        .into_iter()
        .flatten()
        .map(|d| d.join("bitacora"))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no bitacora binary next to {}", exe.display()),
            )
        })
}

pub struct Cluster {
    dir: PathBuf,
    bin: PathBuf,
    volume_size: u64,
    segment_size: u64,
    /// Process of replica `id` at `id - 1`; `None` once killed.
    children: Vec<Option<Child>>,
}

impl Cluster {
    /// Removes what a previous run left in `dir` and starts the three
    /// replicas as followers with new volumes.
    pub fn start(dir: &Path, volume_size: u64, segment_size: u64) -> Res<Cluster> {
        fs::create_dir_all(dir)?;
        let dir = fs::canonicalize(dir)?;
        for id in IDS {
            for ext in ["img", "img.resync", "img.full", "sock", "log"] {
                let _ = fs::remove_file(dir.join(format!("r{id}.{ext}")));
            }
        }
        let mut cluster = Cluster {
            dir,
            bin: bitacora_bin()?,
            volume_size,
            segment_size,
            children: IDS.iter().map(|_| None).collect(),
        };
        for id in IDS {
            cluster.spawn(id)?;
        }
        for id in IDS {
            cluster.wait_ready(id)?;
        }
        Ok(cluster)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub fn socket(&self, id: u64) -> PathBuf {
        self.dir.join(format!("r{id}.sock"))
    }

    pub fn sockets(&self) -> Vec<PathBuf> {
        IDS.iter().map(|id| self.socket(*id)).collect()
    }

    fn spawn(&mut self, id: u64) -> io::Result<()> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(format!("r{id}.log")))?;
        let peers: Vec<String> = IDS
            .iter()
            .map(|p| format!("{p}={}", self.socket(*p).display()))
            .collect();
        let child = Command::new(&self.bin)
            .arg("replica")
            .args(["--id", &id.to_string()])
            .arg("--volume")
            .arg(self.dir.join(format!("r{id}.img")))
            .arg("--socket")
            .arg(self.socket(id))
            .args(["--peers", &peers.join(",")])
            .args(["--create", &self.volume_size.to_string()])
            .args(["--segment-size", &self.segment_size.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        self.children[slot(id)] = Some(child);
        Ok(())
    }

    /// Waits until replica `id` accepts connections on its socket.
    fn wait_ready(&mut self, id: u64) -> Res<()> {
        let socket = self.socket(id);
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if bitacora::socket::connect(&socket).is_ok() {
                return Ok(());
            }
            if let Some(child) = self.children[slot(id)].as_mut()
                && let Some(status) = child.try_wait()?
            {
                return Err(format!("replica {id} exited at start: {status}").into());
            }
            if Instant::now() >= deadline {
                return Err(format!("replica {id} did not open {}", socket.display()).into());
            }
            thread::sleep(POLL);
        }
    }

    /// Starts replica `id` again on its existing volume.
    pub fn restart(&mut self, id: u64) -> Res<()> {
        self.kill(id)?;
        self.spawn(id)?;
        self.wait_ready(id)
    }

    /// `SIGKILL` to replica `id`, then reaps it.
    pub fn kill(&mut self, id: u64) -> io::Result<()> {
        if let Some(mut child) = self.children[slot(id)].take() {
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }

    /// Sends `signal` (`STOP`, `CONT`) to replica `id` with `kill(1)`.
    pub fn signal(&self, id: u64, signal: &str) -> Res<()> {
        let child = self.children[slot(id)]
            .as_ref()
            .ok_or_else(|| format!("replica {id} is not running"))?;
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(child.id().to_string())
            .status()?;
        if !status.success() {
            return Err(format!("kill -{signal} of replica {id}: {status}").into());
        }
        Ok(())
    }

    /// `bitacora elect --replicas <every socket>`; returns the elected id
    /// and its epoch.
    pub fn elect(&self) -> Res<(u64, u64)> {
        let sockets: Vec<String> = self
            .sockets()
            .iter()
            .map(|s| s.display().to_string())
            .collect();
        let out = Command::new(&self.bin)
            .arg("elect")
            .args(["--replicas", &sockets.join(",")])
            .stdin(Stdio::null())
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Err(format!(
                "elect failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        // `primary <id> epoch <e> socket <s>`
        let words: Vec<&str> = text.split_whitespace().collect();
        match words.as_slice() {
            ["primary", id, "epoch", epoch, "socket", ..] => Ok((id.parse()?, epoch.parse()?)),
            _ => Err(format!("unexpected elect output: {}", text.trim()).into()),
        }
    }

    /// `bitacora status --socket <r<id>.sock> --json`, as printed.
    pub fn status_json(&self, id: u64) -> Res<String> {
        let out = Command::new(&self.bin)
            .arg("status")
            .arg("--socket")
            .arg(self.socket(id))
            .arg("--json")
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(format!(
                "status of replica {id} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    /// Waits until the primary `primary` reports every replica in
    /// `followers` `activa` at its own last LSN. False on timeout.
    pub fn converge(&self, primary: u64, followers: &[u64], timeout: Duration) -> Res<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let report = Client::connect(self.socket(primary))?.status()?;
            if converged(&report, followers) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL);
        }
    }

    /// Replica `id` exports its live volume to `name` in the directory;
    /// returns the SHA-256 of the image.
    pub fn export_sha256(&self, id: u64, name: &str) -> Res<String> {
        let path = self.dir.join(name);
        let _ = fs::remove_file(&path);
        Client::connect(self.socket(id))?.export(Version::Live, &path)?;
        Ok(file_sha256(&path)?)
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for mut child in self.children.iter_mut().filter_map(Option::take) {
            // SIGKILL also ends a stopped process.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn slot(id: u64) -> usize {
    (id - 1) as usize
}

/// Whether `report`, from a primary, shows every replica of `followers`
/// `activa` at the primary's last LSN.
pub fn converged(report: &StatusReport, followers: &[u64]) -> bool {
    let Some(primary) = report.replicas.iter().find(|r| r.role == Role::Primary) else {
        return false;
    };
    followers.iter().all(|f| {
        report.replicas.iter().any(|r| {
            r.id == *f && r.state == ReplicaState::Activa && r.last_lsn == primary.last_lsn
        })
    })
}

/// The replicas other than `id`.
pub fn others(id: u64) -> Vec<u64> {
    IDS.into_iter().filter(|r| *r != id).collect()
}
