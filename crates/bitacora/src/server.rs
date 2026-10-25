//! Unix socket server (contract section 8): one thread per connection, and
//! the requests of a connection answered in order, one at a time.
//!
//! What a request does is up to a `Handler`: a standalone volume
//! (`Server::bind`, or `Server::bind_migrate_target` for the destination of
//! a live migration, `migrate.rs`) or a replica (`replica.rs`, through
//! `Server::with_handler`).
//!
//! Shutdown is orderly: `ServerHandle::shutdown` stops accepting, closes the
//! read side of every connection so each thread finishes the request in
//! flight, answers it and exits, and `Server::run` returns once every thread
//! is joined. Closing the volume is left to the caller.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::error::{Error, Result};
use crate::format::BLOCK_SIZE;
use crate::migrate::{self, Gate};
use crate::snapshot::Version;
use crate::status::StatusReport;
use crate::volume::Volume;
use crate::wire::{self, Op, Request, Response, Status};

/// Answers the requests of a server. Called from one thread per
/// connection, so it must be shareable.
pub trait Handler: Send + Sync {
    fn handle(&self, req: &Request) -> Response;
}

/// A volume served without replicas.
struct Standalone {
    volume: Arc<Volume>,
    /// Admission of requests during a live migration (section 10).
    gate: Gate,
}

/// State shared by the accept loop, the connections and the handles.
struct Shared {
    handler: Arc<dyn Handler>,
    /// Set when the handler is a standalone volume.
    standalone: Option<Arc<Standalone>>,
    path: PathBuf,
    stop: AtomicBool,
    /// A clone of every open connection, to close its read side on shutdown.
    conns: Mutex<HashMap<u64, UnixStream>>,
    next_conn: AtomicU64,
}

pub struct Server {
    listener: UnixListener,
    shared: Arc<Shared>,
}

/// Controls a running server from other threads.
#[derive(Clone)]
pub struct ServerHandle {
    shared: Arc<Shared>,
}

impl Server {
    /// Serves `volume` without replicas on `socket`, replacing a stale
    /// socket file left by a previous process. Any other existing file is an
    /// error.
    pub fn bind(volume: Arc<Volume>, socket: &Path) -> Result<Server> {
        Server::bind_gated(volume, Gate::serving(), socket)
    }

    /// Serves `volume` as the destination of a live migration: data
    /// requests are answered `NOT_PRIMARY` until a source commits its
    /// migration here. `volume` should be new and of the source's size.
    pub fn bind_migrate_target(volume: Arc<Volume>, socket: &Path) -> Result<Server> {
        Server::bind_gated(volume, Gate::target(), socket)
    }

    fn bind_gated(volume: Arc<Volume>, gate: Gate, socket: &Path) -> Result<Server> {
        let standalone = Arc::new(Standalone { volume, gate });
        Server::open(standalone.clone(), Some(standalone), socket)
    }

    /// Serves `handler` on `socket`, with the same socket rules as `bind`.
    pub fn with_handler(handler: Arc<dyn Handler>, socket: &Path) -> Result<Server> {
        Server::open(handler, None, socket)
    }

    fn open(
        handler: Arc<dyn Handler>,
        standalone: Option<Arc<Standalone>>,
        socket: &Path,
    ) -> Result<Server> {
        if let Ok(meta) = fs::symlink_metadata(socket) {
            if !meta.file_type().is_socket() {
                return Err(Error::InvalidArgument(format!(
                    "{} exists and is not a socket",
                    socket.display()
                )));
            }
            fs::remove_file(socket)?;
        }
        let listener = crate::socket::bind(socket)?;
        Ok(Server {
            listener,
            shared: Arc::new(Shared {
                handler,
                standalone,
                path: socket.to_path_buf(),
                stop: AtomicBool::new(false),
                conns: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(0),
            }),
        })
    }

    pub fn handle(&self) -> ServerHandle {
        ServerHandle {
            shared: self.shared.clone(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.shared.path
    }

    /// Accepts connections until `shutdown`, then joins every connection
    /// thread and removes the socket file.
    pub fn run(self) -> Result<()> {
        let mut threads: Vec<JoinHandle<()>> = Vec::new();
        let result = loop {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(Error::Io(e)),
            };
            if self.shared.stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            threads.retain(|t| !t.is_finished());
            let id = self.shared.next_conn.fetch_add(1, Ordering::Relaxed);
            match stream.try_clone() {
                Ok(clone) => lock(&self.shared.conns).insert(id, clone),
                Err(_) => continue,
            };
            // A shutdown between the check above and the insert would miss
            // this connection; check again now that it is registered.
            if self.shared.stop.load(Ordering::SeqCst) {
                let _ = stream.shutdown(Shutdown::Read);
            }
            let shared = self.shared.clone();
            let spawned = thread::Builder::new()
                .name(format!("bitacora-conn-{id}"))
                .spawn(move || {
                    serve_connection(&shared, stream);
                    lock(&shared.conns).remove(&id);
                });
            match spawned {
                Ok(t) => threads.push(t),
                Err(_) => {
                    lock(&self.shared.conns).remove(&id);
                }
            }
        };
        self.handle().shutdown();
        for t in threads {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.shared.path);
        result
    }
}

impl ServerHandle {
    /// Stops accepting and closes the read side of every connection. Safe
    /// to call more than once and from any thread.
    pub fn shutdown(&self) {
        if !self.shared.stop.swap(true, Ordering::SeqCst) {
            // Wakes the accept loop, which then sees `stop`.
            let _ = crate::socket::connect(&self.shared.path);
        }
        for conn in lock(&self.shared.conns).values() {
            let _ = conn.shutdown(Shutdown::Read);
        }
    }

    /// From now on every request is answered `MOVED` with `destination`;
    /// a request answered `MOVED` is not executed. Only a standalone volume
    /// moves; on any other server it does nothing.
    pub fn set_moved(&self, destination: &str) {
        if let Some(standalone) = &self.shared.standalone {
            standalone.gate.set_moved(destination);
        }
    }

    /// The destination socket once the standalone volume moved.
    pub fn moved_to(&self) -> Option<String> {
        self.shared.standalone.as_ref()?.gate.moved_to()
    }

    /// The volume of a standalone server.
    pub fn volume(&self) -> Option<&Arc<Volume>> {
        self.shared.standalone.as_ref().map(|s| &s.volume)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Reads, executes and answers requests one after another until the peer
/// closes, the read side is shut down or a frame cannot be decoded.
fn serve_connection(shared: &Shared, stream: UnixStream) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(write_half);
    loop {
        let req = match wire::read_request(&mut reader) {
            Ok(Some(req)) => req,
            Ok(None) | Err(_) => return,
        };
        let resp = shared.handler.handle(&req);
        if wire::write_response(&mut writer, &resp).is_err() {
            return;
        }
    }
}

/// Response status of an engine error.
pub fn error_status(e: &Error) -> Status {
    match e {
        Error::Io(_) | Error::InvalidArgument(_) => Status::IoError,
        Error::Integrity { .. } | Error::Corrupt(_) => Status::IntegrityError,
        Error::NoSpace => Status::NoSpace,
        Error::Failed | Error::Closed => Status::Failed,
    }
}

impl Handler for Standalone {
    /// Answers one request. Migration frames go to `migrate.rs`; any other
    /// request passes the gate, which answers `MOVED` once the volume moved,
    /// holds it while a migration is frozen and refuses it on a destination
    /// before the commit (except `STATUS`). Then `FENCED` for an epoch older
    /// than the volume's, otherwise the result of the engine call.
    fn handle(&self, req: &Request) -> Response {
        let vol = &self.volume;
        match req.op {
            Op::Migrate => {
                let result = std::str::from_utf8(&req.payload)
                    .map_err(|_| Error::InvalidArgument("destination is not UTF-8".into()))
                    .and_then(|dest| migrate::run_source(vol, &self.gate, dest))
                    .map(|summary| summary.to_text().into_bytes());
                return respond(req, result);
            }
            Op::MigrateData | Op::MigrateFreeze | Op::MigrateCommit => {
                return respond(req, migrate::handle_target(vol, &self.gate, req));
            }
            _ => {}
        }
        let ticket = match self.gate.enter() {
            Ok(ticket) => Some(ticket),
            Err(migrate::Refusal::NotPrimary) if req.op == Op::Status => None,
            Err(refusal) => return migrate::refusal(req, refusal),
        };
        if req.epoch != 0 && req.epoch < vol.epoch() {
            return Response::error(
                req.req_id,
                Status::Fenced,
                &format!("epoch {} is older than {}", req.epoch, vol.epoch()),
            );
        }
        let result = match req.op {
            Op::Status => StatusReport::standalone(vol, None).map(|r| r.to_json().into_bytes()),
            _ => execute(vol, req),
        };
        if let Some(ticket) = &ticket {
            migrate::throttle(ticket, req.op);
        }
        let resp = respond(req, result);
        drop(ticket);
        resp
    }
}

/// The response to `req` for the result of its engine call.
pub fn respond(req: &Request, result: Result<Vec<u8>>) -> Response {
    match result {
        Ok(payload) => Response::ok(req.req_id, payload),
        Err(e) => Response::error(req.req_id, error_status(&e), &e.to_string()),
    }
}

/// Executes a data-plane op on `vol` and returns the payload of its answer.
/// `STATUS` and the ops of replication and migration are left to the
/// handler and answered `IO_ERROR` here.
pub fn execute(vol: &Volume, req: &Request) -> Result<Vec<u8>> {
    let nblocks = u64::from(req.nblocks);
    match req.op {
        Op::Read => {
            if nblocks as usize * BLOCK_SIZE > wire::MAX_BODY - wire::RESPONSE_HEADER {
                return Err(Error::InvalidArgument(format!(
                    "read of {nblocks} blocks exceeds the frame limit"
                )));
            }
            vol.read(req.lba, nblocks)
        }
        Op::Write | Op::WriteFua => {
            if req.payload.len() as u64 != nblocks * BLOCK_SIZE as u64 {
                return Err(Error::InvalidArgument(format!(
                    "{} payload bytes for {nblocks} blocks",
                    req.payload.len()
                )));
            }
            if req.op == Op::Write {
                vol.write(req.lba, &req.payload)?;
            } else {
                vol.write_fua(req.lba, &req.payload)?;
            }
            Ok(Vec::new())
        }
        Op::Flush => {
            vol.flush()?;
            Ok(Vec::new())
        }
        Op::SnapshotCreate => Ok(vol.snapshot_create()?.to_le_bytes().to_vec()),
        Op::SnapshotDelete => {
            vol.snapshot_delete(payload_u64(&req.payload)?)?;
            Ok(Vec::new())
        }
        Op::Export => {
            let id = payload_u64(&req.payload)?;
            let path = std::str::from_utf8(&req.payload[8..])
                .map_err(|_| Error::InvalidArgument("export path is not UTF-8".into()))?;
            if path.is_empty() {
                return Err(Error::InvalidArgument("export needs a path".into()));
            }
            let version = match id {
                0 => Version::Live,
                id => Version::Snapshot(id),
            };
            vol.export(version, Path::new(path))?;
            Ok(Vec::new())
        }
        op => Err(Error::InvalidArgument(format!(
            "op {op:?} is not served by this server"
        ))),
    }
}

fn payload_u64(payload: &[u8]) -> Result<u64> {
    payload
        .first_chunk::<8>()
        .map(|b| u64::from_le_bytes(*b))
        .ok_or_else(|| Error::InvalidArgument("payload needs a u64".into()))
}

/// Blocks `SIGTERM` and `SIGINT` in the calling thread and in every thread
/// it creates afterwards, so that only `shutdown_on_signal` receives them.
/// Call it before opening the volume (its GC thread inherits the mask).
pub fn block_signals() -> io::Result<()> {
    let set = signal_set();
    // SAFETY: `set` is an initialised signal set; a null old set is allowed.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Spawns a thread that waits for `SIGTERM` or `SIGINT` (blocked by
/// `block_signals`) and shuts the server down.
pub fn shutdown_on_signal(handle: ServerHandle) -> io::Result<JoinHandle<()>> {
    on_signal(move || handle.shutdown())
}

/// Spawns a thread that waits for `SIGTERM` or `SIGINT` (blocked by
/// `block_signals`) and then runs `stop`.
pub fn on_signal(stop: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("bitacora-signal".into())
        .spawn(move || {
            let set = signal_set();
            let mut sig: libc::c_int = 0;
            // SAFETY: `set` is initialised and `sig` is a valid out pointer.
            unsafe { libc::sigwait(&set, &mut sig) };
            stop();
        })
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the set before `sigaddset` uses it.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        set
    }
}
