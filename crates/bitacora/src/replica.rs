//! Replication with a durable majority, epochs, election and fencing
//! (contract section 9).
//!
//! Three replica processes, each with its own volume file, serve the same
//! volume. One is the primary and the others follow it. Every replica starts
//! as a follower; `elect` makes one of them primary.
//!
//! **Write.** The primary assigns the LSN, appends locally and queues the
//! blocks. One pump thread per follower sends `APPEND(epoch, prev_lsn,
//! entries)` with everything queued since its previous `APPEND`, and a
//! flusher thread syncs the local volume with everything appended since its
//! previous sync: concurrent writes share both (group commit). A follower
//! applies the entries with plain writes, which receive the primary's LSNs
//! because its log ends at `prev_lsn`, syncs and answers. The commit point
//! is the smaller of the primary's synced LSN and the greatest LSN a
//! follower reported synced: a write is answered `OK` only when the commit
//! point covers it, that is, when it is durable on the primary and on at
//! least one follower. `FLUSH` is then immediate.
//!
//! **Epochs and fencing.** `current_epoch` lives in the superblock. Every
//! replication frame carries the sender's epoch; a replica answers `FENCED`
//! to one older than its own, and adopts (persisting it first) a newer one,
//! leaving the primary role if it had it. A primary that receives `FENCED`
//! stops acknowledging and becomes a follower; its writes waiting for the
//! majority are answered `NOT_PRIMARY`, never `OK`. Client ops on a follower
//! are answered `NOT_PRIMARY`.
//!
//! **Claim.** What a replica holds is summarised by `(last_epoch,
//! last_lsn)`: its contents equal the contents of the primary of
//! `last_epoch` at `last_lsn`. `last_lsn` is the last LSN of the volume and
//! `last_epoch` the epoch of the last `epoch_history` entry that starts at or
//! before it.
//!
//! **Election** (`elect`). `PREPARE(e)` goes to every replica with `e` one
//! more than the greatest epoch any of them reports. A replica accepts if
//! `e > current_epoch`: it persists `e`, from then on rejects frames of
//! older epochs, and answers its claim, which can no longer change. With a
//! majority of acceptances, the acceptor with the greatest claim receives
//! `PROMOTE(e)`, appends `(e, last_lsn + 1)` to its `epoch_history`, persists
//! it and starts serving. Two majorities of three intersect, so each epoch
//! has at most one primary, and every acknowledged write is in the claim of
//! some acceptor, hence in the elected one.
//!
//! **Rejoining.** A follower is accepted for `APPEND` only after the primary
//! of the current epoch resynchronised it (it then is *attached*). The
//! primary reads the follower's claim `(e_f, l_f)`: if `l_f` is at most the
//! end of epoch `e_f` in its own history (the start of the next epoch minus
//! one, or its last LSN), the follower's log is a prefix of the primary's
//! and a delta resync sends the blocks of a pinned version with `birth >
//! l_f` (stream 2); otherwise the follower has a divergent suffix and a full
//! resync builds a new volume. Both end with the follower copying the
//! primary's `epoch_history` and moving its next LSN past the pinned one;
//! then `APPEND` continues from there with the blocks written meanwhile,
//! which the primary kept queued.
//!
//! **Interrupted resync.** Delta blocks arrive in LBA order, so a follower
//! that dies in the middle holds a mix that is not the primary's state at
//! any LSN. Before the first block it writes and syncs a marker file
//! `<volume>.resync` with `l_f`, removed only after the resync is synced.
//! While the marker exists the claim stays `(e_f, l_f)` (the contents hold
//! at least that state, so acknowledged writes are still covered), the next
//! resync is full, and if the replica is elected its new epoch starts at
//! `l_f + 1`, so that every other replica past `l_f` gets a full resync.
//! A full resync is built in `<volume>.full` and renamed over the volume
//! when complete; a leftover `.full` file is deleted at start.
//!
//! **Liveness.** Each pump sends `HEARTBEAT` every 100 ms. In the primary's
//! status a follower is `caida` after 500 ms without an answer, `atrasada`
//! while it is not attached or its `last_lsn` is behind the primary's, and
//! `activa` otherwise.
//!
//! Frames (request fields not listed are 0):
//!
//! | op | request | `OK` payload |
//! | --- | --- | --- |
//! | `HEARTBEAT` | `epoch`, `lba` = primary id | claim |
//! | `APPEND` | `epoch`, `lba` = `prev_lsn`, `nblocks` = n, n × (`lba` u64, 4096 bytes) | `applied` u8, claim |
//! | `PREPARE` | `epoch` = e | claim |
//! | `PROMOTE` | `epoch` = e | empty |
//! | `RESYNC` begin | `epoch`, `lba` = 1, mode u8 (1 delta, 2 full), `base` u64, `target` u64, `volume_size` u64, `segment_size` u64, history count u32, count × (`epoch` u64, `start_lsn` u64) | empty |
//! | `RESYNC` data | `epoch`, `lba` = 2, `nblocks` = n, n × (`lba` u64, `birth` u64, 4096 bytes) | empty |
//! | `RESYNC` end | `epoch`, `lba` = 3, `target` u64 | claim |
//!
//! Claim: `id`, `last_epoch`, `last_lsn`, `current_epoch`, `volume_size`,
//! `segment_size` (u64 each), `attached` u8, `needs_full` u8.

use std::cmp::Reverse;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE, EpochEntry, MAX_EPOCH_HISTORY};
use crate::medium::FileMedium;
use crate::server::{self, Handler, Server, ServerHandle};
use crate::status::{ReplicaReport, ReplicaState, Role, StatusReport};
use crate::volume::{GcMode, Pin, Volume, VolumeConfig, VolumeState};
use crate::wire::{self, Op, Request, Response, Status};

/// Interval between heartbeats of the primary to each follower.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
/// Without an answer for this long, a follower is `caida`.
pub const DOWN_AFTER: Duration = Duration::from_millis(500);
/// Read and write timeout of the primary's connections to its followers.
pub const PEER_IO_TIMEOUT: Duration = Duration::from_secs(2);
/// A write not durable on a majority within this time is answered
/// `IO_ERROR` (it may still become durable later).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout of each request `elect` sends.
pub const ELECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest number of blocks in one `APPEND`.
pub const MAX_APPEND_BLOCKS: usize = 1024;
/// Largest number of blocks in one `RESYNC` data frame.
pub const RESYNC_CHUNK_BLOCKS: usize = 256;
/// Blocks the primary keeps queued for a lagging follower; past it, the
/// follower loses its place and is resynchronised.
pub const QUEUE_LIMIT_BLOCKS: u64 = 16384;

/// `lba` field of the `RESYNC` frames.
pub const RESYNC_BEGIN: u64 = 1;
pub const RESYNC_DATA: u64 = 2;
pub const RESYNC_END: u64 = 3;

const APPEND_ENTRY: usize = 8 + BLOCK_SIZE;
const RESYNC_ENTRY: usize = 16 + BLOCK_SIZE;

/// How a follower is resynchronised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResyncMode {
    /// Blocks with `birth > l_f` on top of the follower's volume.
    Delta = 1,
    /// Every block, into a new volume.
    Full = 2,
}

/// What a replica holds and where it stands (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    pub id: u64,
    pub last_epoch: u64,
    pub last_lsn: u64,
    pub current_epoch: u64,
    pub volume_size: u64,
    pub segment_size: u64,
    /// Attached by the primary of `current_epoch`: it accepts `APPEND`.
    pub attached: bool,
    /// An interrupted resync left a marker: only a full resync fixes it.
    pub needs_full: bool,
}

pub const CLAIM_LEN: usize = 6 * 8 + 2;

impl Claim {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(CLAIM_LEN);
        for v in [
            self.id,
            self.last_epoch,
            self.last_lsn,
            self.current_epoch,
            self.volume_size,
            self.segment_size,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(u8::from(self.attached));
        b.push(u8::from(self.needs_full));
        b
    }

    pub fn decode(b: &[u8]) -> Option<Claim> {
        if b.len() != CLAIM_LEN {
            return None;
        }
        Some(Claim {
            id: get_u64(b, 0),
            last_epoch: get_u64(b, 8),
            last_lsn: get_u64(b, 16),
            current_epoch: get_u64(b, 24),
            volume_size: get_u64(b, 32),
            segment_size: get_u64(b, 40),
            attached: b[48] != 0,
            needs_full: b[49] != 0,
        })
    }

    /// Order of the election: the greatest `(last_epoch, last_lsn)` wins,
    /// the lowest id on a tie.
    fn rank(&self) -> (u64, u64, Reverse<u64>) {
        (self.last_epoch, self.last_lsn, Reverse(self.id))
    }
}

/// Epoch of the log entry at `lsn` according to `history`: the last entry
/// that starts at or before it; 0 for an empty log or an LSN before every
/// entry.
pub fn epoch_at(history: &[EpochEntry], lsn: u64) -> u64 {
    if lsn == 0 {
        return 0;
    }
    history
        .iter()
        .rev()
        .find(|e| e.start_lsn <= lsn)
        .map_or(0, |e| e.epoch)
}

/// The rejoin rule of contract section 9, from the primary's `history`
/// and last LSN, its geometry and the follower's claim.
pub fn plan_resync(
    history: &[EpochEntry],
    last_lsn: u64,
    geometry: (u64, u64),
    claim: &Claim,
) -> ResyncMode {
    if claim.needs_full || (claim.volume_size, claim.segment_size) != geometry {
        return ResyncMode::Full;
    }
    if claim.last_lsn == 0 {
        // An empty log is a prefix of every log.
        return ResyncMode::Delta;
    }
    if claim.last_lsn > last_lsn {
        return ResyncMode::Full;
    }
    let Some(i) = history.iter().position(|e| e.epoch == claim.last_epoch) else {
        return ResyncMode::Full;
    };
    if claim.last_lsn < history[i].start_lsn {
        return ResyncMode::Full;
    }
    let end = history
        .get(i + 1)
        .map_or(last_lsn, |next| next.start_lsn.saturating_sub(1));
    if claim.last_lsn <= end {
        ResyncMode::Delta
    } else {
        ResyncMode::Full
    }
}

/// History of a primary promoted in `epoch` whose consistent log ends at
/// `last_lsn`: entries that would start after it cover nothing and go,
/// the new one is appended, and the oldest ones are dropped past 64 (an
/// epoch missing from the history only forces a full resync).
pub fn promoted_history(
    mut history: Vec<EpochEntry>,
    epoch: u64,
    last_lsn: u64,
) -> Vec<EpochEntry> {
    let start_lsn = last_lsn + 1;
    history.retain(|e| e.start_lsn < start_lsn && e.epoch < epoch);
    history.push(EpochEntry { epoch, start_lsn });
    let excess = history.len().saturating_sub(MAX_EPOCH_HISTORY);
    history.drain(..excess);
    history
}

/// Parameters of a replica process.
#[derive(Debug, Clone)]
pub struct ReplicaConfig {
    pub id: u64,
    pub volume: PathBuf,
    pub socket: PathBuf,
    /// `(id, socket)` of the other replicas; an entry with this replica's
    /// id is ignored.
    pub peers: Vec<(u64, PathBuf)>,
    /// Geometry of the volume created when the file is missing or empty;
    /// an existing volume must have its size.
    pub create: Option<VolumeConfig>,
    pub gc_mode: GcMode,
}

/// A running replica: its socket server and, while primary, its pump and
/// flusher threads.
pub struct Replica {
    node: Arc<Node>,
    handle: ServerHandle,
    server: Option<JoinHandle<Result<()>>>,
}

/// Stops a replica from another thread (a signal handler).
#[derive(Clone)]
pub struct Stopper {
    node: Arc<Node>,
    handle: ServerHandle,
}

impl Stopper {
    pub fn stop(&self) {
        self.node.halt();
        self.handle.shutdown();
    }
}

impl Replica {
    /// Opens (or creates) the volume and serves it as a follower.
    pub fn start(config: ReplicaConfig) -> Result<Replica> {
        let full = sidecar(&config.volume, "full");
        if full.exists() {
            fs::remove_file(&full)?;
            sync_dir(&full)?;
        }
        let empty = fs::metadata(&config.volume)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        let volume = if empty {
            let Some(create) = config.create else {
                return Err(Error::InvalidArgument(format!(
                    "{} is missing and no size was given",
                    config.volume.display()
                )));
            };
            let medium = Arc::new(FileMedium::create(&config.volume, false)?);
            Volume::create(medium, create.with_gc_mode(config.gc_mode))?
        } else {
            let medium = Arc::new(FileMedium::open(&config.volume, false)?);
            let geometry = VolumeConfig::from_medium(&*medium)?.with_gc_mode(config.gc_mode);
            if let Some(create) = config.create
                && create.volume_size != geometry.volume_size
            {
                return Err(Error::InvalidArgument(format!(
                    "{} has size {}, not {}",
                    config.volume.display(),
                    geometry.volume_size,
                    create.volume_size
                )));
            }
            Volume::open(medium, geometry)?
        };
        let marker = read_marker(&sidecar(&config.volume, "resync"), volume.status().lsn)?;
        let peers = config
            .peers
            .iter()
            .filter(|(id, _)| *id != config.id)
            .map(|(id, socket)| Peer {
                id: *id,
                socket: socket.clone(),
            })
            .collect();
        let node = Arc::new_cyclic(|me| Node {
            me: me.clone(),
            id: config.id,
            path: config.volume.clone(),
            peers,
            gc_mode: config.gc_mode,
            state: Mutex::new(State {
                volume: Arc::new(volume),
                primary: None,
                promised: None,
                attached: None,
                marker,
                incoming: None,
                leader: None,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        });
        let server = match Server::with_handler(node.clone(), &config.socket) {
            Ok(s) => s,
            Err(e) => {
                let _ = node.shut_volume();
                return Err(e);
            }
        };
        let handle = server.handle();
        let thread = thread::Builder::new()
            .name(format!("bitacora-replica-{}", config.id))
            .spawn(move || server.run())?;
        Ok(Replica {
            node,
            handle,
            server: Some(thread),
        })
    }

    pub fn id(&self) -> u64 {
        self.node.id
    }

    pub fn stopper(&self) -> Stopper {
        Stopper {
            node: self.node.clone(),
            handle: self.handle.clone(),
        }
    }

    /// The volume this replica serves now (a full resync replaces it).
    pub fn volume(&self) -> Arc<Volume> {
        self.node.lock().volume.clone()
    }

    pub fn is_primary(&self) -> bool {
        self.node.lock().primary.is_some()
    }

    /// The current claim of this replica.
    pub fn claim(&self) -> Claim {
        self.node.lock().claim(self.node.id)
    }

    /// Waits until the server stops (`Stopper::stop`), then stops the
    /// replication threads and closes the volume.
    pub fn wait(mut self) -> Result<()> {
        let served = match self.server.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => Err(Error::Failed),
            None => Ok(()),
        };
        self.node.halt();
        let threads = std::mem::take(&mut *lock(&self.node.threads));
        for t in threads {
            let _ = t.join();
        }
        let closed = self.node.shut_volume();
        served?;
        closed
    }

    /// Orderly stop: `Stopper::stop` and `wait`.
    pub fn stop(self) -> Result<()> {
        self.stopper().stop();
        self.wait()
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        if self.server.is_some() {
            self.stopper().stop();
            if let Some(t) = self.server.take() {
                let _ = t.join();
            }
            self.node.halt();
        }
    }
}

#[derive(Debug, Clone)]
struct Peer {
    id: u64,
    socket: PathBuf,
}

struct Node {
    /// To hand an `Arc` of itself to the threads it spawns.
    me: Weak<Node>,
    id: u64,
    path: PathBuf,
    peers: Vec<Peer>,
    gc_mode: GcMode,
    state: Mutex<State>,
    /// Commit point, queue, role or stop changed.
    changed: Condvar,
    stop: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct State {
    volume: Arc<Volume>,
    /// Set while this replica is the primary.
    primary: Option<Primary>,
    /// Epoch accepted by `PREPARE` and not yet used by a `PROMOTE`.
    promised: Option<u64>,
    /// Epoch whose primary resynchronised this follower.
    attached: Option<u64>,
    /// `l_f` of an interrupted delta resync (its marker file exists).
    marker: Option<u64>,
    incoming: Option<Incoming>,
    /// Id of the last primary that reached this follower.
    leader: Option<u64>,
}

/// A resync this follower is receiving.
struct Incoming {
    epoch: u64,
    target: u64,
    history: Vec<EpochEntry>,
    /// Full resync: the new volume and its temporary path.
    full: Option<(Arc<Volume>, PathBuf)>,
}

struct Primary {
    epoch: u64,
    since: Instant,
    /// Blocks after `base`, in LSN order: entry i has LSN `base + 1 + i`.
    queue: VecDeque<Entry>,
    base: u64,
    /// Last LSN appended locally.
    appended: u64,
    /// Last LSN covered by a local `fdatasync`.
    local_durable: u64,
    /// Writes up to here are durable on a majority.
    commit: u64,
    links: Vec<Link>,
}

struct Entry {
    lba: u64,
    data: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Needs a handshake and a resync before any `APPEND`.
    Detached,
    Resyncing,
    Attached,
}

/// The primary's view of one follower.
struct Link {
    id: u64,
    socket: PathBuf,
    phase: Phase,
    /// Last LSN sent (or pinned for a resync); the queue keeps what follows.
    sent: u64,
    /// Last LSN the follower reported synced in this epoch.
    matched: u64,
    last_answer: Option<Instant>,
    reported_epoch: u64,
    reported_lsn: u64,
}

impl Primary {
    fn new(epoch: u64, last: u64, peers: &[Peer]) -> Primary {
        Primary {
            epoch,
            since: Instant::now(),
            queue: VecDeque::new(),
            base: last,
            appended: last,
            local_durable: last,
            commit: last,
            links: peers
                .iter()
                .map(|p| Link {
                    id: p.id,
                    socket: p.socket.clone(),
                    phase: Phase::Detached,
                    sent: 0,
                    matched: 0,
                    last_answer: None,
                    reported_epoch: 0,
                    reported_lsn: 0,
                })
                .collect(),
        }
    }

    /// Moves the commit point to min(local synced, best follower synced).
    fn advance(&mut self) {
        let best = self.links.iter().map(|l| l.matched).max().unwrap_or(0);
        let point = self.local_durable.min(best).min(self.appended);
        if point > self.commit {
            self.commit = point;
        }
    }

    /// Drops the queued blocks no follower in a resync or attached needs.
    /// A follower lagging more than `QUEUE_LIMIT_BLOCKS` is detached.
    fn trim(&mut self) {
        let appended = self.appended;
        for l in &mut self.links {
            if l.phase != Phase::Detached && appended - l.sent.min(appended) > QUEUE_LIMIT_BLOCKS {
                l.phase = Phase::Detached;
            }
        }
        let keep_after = self
            .links
            .iter()
            .filter(|l| l.phase != Phase::Detached)
            .map(|l| l.sent)
            .min()
            .unwrap_or(appended)
            .min(appended);
        while self.base < keep_after && self.queue.pop_front().is_some() {
            self.base += 1;
        }
    }

    /// Up to `max` queued blocks after LSN `after`, as `(lba, data)`.
    fn entries_after(&self, after: u64, max: usize) -> Option<Vec<(u64, Arc<[u8]>)>> {
        if after < self.base {
            return None;
        }
        let start = (after - self.base) as usize;
        Some(
            self.queue
                .iter()
                .skip(start)
                .take(max)
                .map(|e| (e.lba, e.data.clone()))
                .collect(),
        )
    }
}

impl State {
    fn claim(&self, id: u64) -> Claim {
        let vol = &self.volume;
        let last_lsn = self.marker.unwrap_or_else(|| vol.status().lsn);
        let current_epoch = vol.epoch();
        Claim {
            id,
            last_epoch: epoch_at(&vol.epoch_history(), last_lsn),
            last_lsn,
            current_epoch,
            volume_size: vol.volume_size(),
            segment_size: vol.segment_size(),
            attached: self.attached == Some(current_epoch) && self.primary.is_none(),
            needs_full: self.marker.is_some(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Node {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Asks every thread of the node to finish.
    fn halt(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _guard = self.lock();
        self.changed.notify_all();
    }

    /// Leaves the primary role and closes the volume.
    fn shut_volume(&self) -> Result<()> {
        let vol = {
            let mut st = self.lock();
            self.step_down(&mut st);
            self.abort_incoming(&mut st);
            st.volume.clone()
        };
        match vol.close() {
            Err(Error::Closed) => Ok(()),
            r => r,
        }
    }

    /// Leaves the primary role. Writes waiting for the majority see it and
    /// are answered `NOT_PRIMARY`.
    fn step_down(&self, st: &mut State) {
        if st.primary.take().is_some() {
            let _ = st.volume.flush();
            self.changed.notify_all();
        }
    }

    /// Drops a resync in progress. A delta keeps its marker (the claim
    /// stays `l_f`); a full one deletes its temporary volume.
    fn abort_incoming(&self, st: &mut State) {
        if let Some(inc) = st.incoming.take()
            && let Some((vol, tmp)) = inc.full
        {
            drop(vol);
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Adopts a newer epoch seen in a frame: persisted before anything else.
    fn adopt(&self, st: &mut State, epoch: u64) -> Result<()> {
        self.step_down(st);
        self.abort_incoming(st);
        st.volume.set_epoch(epoch, None)?;
        st.promised = None;
        st.attached = None;
        Ok(())
    }

    /// Admits a frame sent by a primary (`HEARTBEAT`, `APPEND`, `RESYNC`):
    /// `FENCED` if its epoch is older, adoption if newer.
    fn admit(&self, st: &mut State, req: &Request) -> std::result::Result<(), Response> {
        let current = st.volume.epoch();
        if req.epoch == 0 {
            return Err(Response::error(
                req.req_id,
                Status::IoError,
                "replication frame without an epoch",
            ));
        }
        if req.epoch < current {
            return Err(fenced(req, current));
        }
        if req.epoch > current {
            self.adopt(st, req.epoch)
                .map_err(|e| server::respond(req, Err(e)))?;
        } else if st.primary.is_some() {
            return Err(Response::error(
                req.req_id,
                Status::IoError,
                &format!("this replica is the primary of epoch {current}"),
            ));
        }
        // Another replica is the primary of this epoch: no PROMOTE for it
        // may be honoured here.
        st.promised = None;
        Ok(())
    }

    /// Client ops: `FENCED` for an older non-zero epoch, `NOT_PRIMARY` on
    /// a follower.
    fn check_client(&self, st: &State, req: &Request) -> Option<Response> {
        let current = st.volume.epoch();
        if req.epoch != 0 && req.epoch < current {
            return Some(fenced(req, current));
        }
        if st.primary.is_none() {
            let hint = match st.leader {
                Some(id) => format!("replica {} is not the primary; replica {id} was", self.id),
                None => format!("replica {} is not the primary", self.id),
            };
            return Some(Response::error(req.req_id, Status::NotPrimary, &hint));
        }
        None
    }

    fn on_heartbeat(&self, req: &Request) -> Response {
        let mut st = self.lock();
        if let Err(r) = self.admit(&mut st, req) {
            return r;
        }
        st.leader = Some(req.lba);
        Response::ok(req.req_id, st.claim(self.id).encode())
    }

    fn on_append(&self, req: &Request) -> Response {
        let mut st = self.lock();
        if let Err(r) = self.admit(&mut st, req) {
            return r;
        }
        let claim = st.claim(self.id);
        let applies = claim.attached
            && !claim.needs_full
            && st.incoming.is_none()
            && st.attached == Some(req.epoch)
            && req.lba == claim.last_lsn;
        if !applies {
            let mut payload = vec![0u8];
            payload.extend_from_slice(&claim.encode());
            return Response::ok(req.req_id, payload);
        }
        let n = req.nblocks as usize;
        if req.payload.len() != n * APPEND_ENTRY {
            return Response::error(
                req.req_id,
                Status::IoError,
                &format!("{} payload bytes for {n} entries", req.payload.len()),
            );
        }
        let vol = st.volume.clone();
        if let Err(e) = apply_entries(&vol, &req.payload) {
            st.attached = None;
            return server::respond(req, Err(e));
        }
        if vol.status().lsn != req.lba + n as u64 {
            // The LSNs no longer match the primary's: start over.
            st.attached = None;
            return Response::error(req.req_id, Status::IoError, "APPEND left a different LSN");
        }
        let mut payload = vec![1u8];
        payload.extend_from_slice(&st.claim(self.id).encode());
        Response::ok(req.req_id, payload)
    }

    fn on_prepare(&self, req: &Request) -> Response {
        let mut st = self.lock();
        let current = st.volume.epoch();
        if req.epoch <= current {
            return Response::error(
                req.req_id,
                Status::Fenced,
                &format!("epoch {} is not newer than {current}", req.epoch),
            );
        }
        self.step_down(&mut st);
        self.abort_incoming(&mut st);
        if let Err(e) = st.volume.set_epoch(req.epoch, None) {
            return server::respond(req, Err(e));
        }
        st.promised = Some(req.epoch);
        st.attached = None;
        Response::ok(req.req_id, st.claim(self.id).encode())
    }

    fn on_promote(&self, req: &Request) -> Response {
        let mut st = self.lock();
        let current = st.volume.epoch();
        if req.epoch < current {
            return fenced(req, current);
        }
        if st.primary.as_ref().is_some_and(|p| p.epoch == req.epoch) {
            return Response::ok(req.req_id, Vec::new());
        }
        if st.promised != Some(req.epoch) || req.epoch != current {
            return Response::error(
                req.req_id,
                Status::IoError,
                &format!(
                    "epoch {} was not prepared on replica {}",
                    req.epoch, self.id
                ),
            );
        }
        self.abort_incoming(&mut st);
        let claim = st.claim(self.id);
        let history = promoted_history(st.volume.epoch_history(), req.epoch, claim.last_lsn);
        if let Err(e) = st.volume.set_epoch(req.epoch, Some(&history)) {
            return server::respond(req, Err(e));
        }
        if st.marker.is_some() {
            if let Err(e) = remove_marker(&sidecar(&self.path, "resync")) {
                return server::respond(req, Err(e));
            }
            st.marker = None;
        }
        let last = st.volume.status().lsn;
        st.primary = Some(Primary::new(req.epoch, last, &self.peers));
        st.promised = None;
        st.attached = None;
        st.leader = Some(self.id);
        drop(st);
        self.spawn_primary_threads(req.epoch);
        Response::ok(req.req_id, Vec::new())
    }

    fn spawn_primary_threads(&self, epoch: u64) {
        let Some(me) = self.me.upgrade() else {
            return;
        };
        let mut threads = lock(&self.threads);
        threads.retain(|t| !t.is_finished());
        let node = me.clone();
        if let Ok(t) = thread::Builder::new()
            .name(format!("bitacora-flush-{}", self.id))
            .spawn(move || flusher(&node, epoch))
        {
            threads.push(t);
        }
        for slot in 0..self.peers.len() {
            let node = me.clone();
            if let Ok(t) = thread::Builder::new()
                .name(format!("bitacora-pump-{}-{}", self.id, self.peers[slot].id))
                .spawn(move || pump(&node, slot, epoch))
            {
                threads.push(t);
            }
        }
    }

    fn on_resync(&self, req: &Request) -> Response {
        let mut st = self.lock();
        if let Err(r) = self.admit(&mut st, req) {
            return r;
        }
        let result = match req.lba {
            RESYNC_BEGIN => self.resync_begin(&mut st, req),
            RESYNC_DATA => self.resync_data(&mut st, req),
            RESYNC_END => self.resync_end(&mut st, req),
            phase => Err(Error::InvalidArgument(format!(
                "unknown resync phase {phase}"
            ))),
        };
        if result.is_err() {
            self.abort_incoming(&mut st);
        }
        server::respond(req, result)
    }

    fn resync_begin(&self, st: &mut State, req: &Request) -> Result<Vec<u8>> {
        let p = &req.payload;
        if p.len() < 37 {
            return Err(Error::InvalidArgument("short resync begin".into()));
        }
        let mode = p[0];
        let base = get_u64(p, 1);
        let target = get_u64(p, 9);
        let volume_size = get_u64(p, 17);
        let segment_size = get_u64(p, 25);
        let count = get_u32(p, 33) as usize;
        if count > MAX_EPOCH_HISTORY || p.len() != 37 + 16 * count {
            return Err(Error::InvalidArgument("bad resync history".into()));
        }
        let history = (0..count)
            .map(|i| EpochEntry {
                epoch: get_u64(p, 37 + 16 * i),
                start_lsn: get_u64(p, 45 + 16 * i),
            })
            .collect();
        self.abort_incoming(st);
        st.attached = None;
        let full = if mode == ResyncMode::Delta as u8 {
            let claim = st.claim(self.id);
            if claim.needs_full || claim.last_lsn != base || target < base {
                return Err(Error::InvalidArgument(format!(
                    "delta from {base} to {target}, but this replica claims {}",
                    claim.last_lsn
                )));
            }
            write_marker(&sidecar(&self.path, "resync"), base)?;
            st.marker = Some(base);
            None
        } else if mode == ResyncMode::Full as u8 {
            let tmp = sidecar(&self.path, "full");
            if tmp.exists() {
                fs::remove_file(&tmp)?;
            }
            let config = VolumeConfig::new(volume_size)
                .with_segment_size(segment_size)
                .with_gc_mode(self.gc_mode);
            let medium = Arc::new(FileMedium::create(&tmp, false)?);
            Some((Arc::new(Volume::create(medium, config)?), tmp))
        } else {
            return Err(Error::InvalidArgument(format!(
                "unknown resync mode {mode}"
            )));
        };
        st.incoming = Some(Incoming {
            epoch: req.epoch,
            target,
            history,
            full,
        });
        Ok(Vec::new())
    }

    /// The volume a resync of `epoch` writes to.
    fn incoming_volume(st: &State, epoch: u64) -> Result<Arc<Volume>> {
        match &st.incoming {
            Some(inc) if inc.epoch == epoch => Ok(inc
                .full
                .as_ref()
                .map_or_else(|| st.volume.clone(), |(v, _)| v.clone())),
            _ => Err(Error::InvalidArgument("no resync in progress".into())),
        }
    }

    fn resync_data(&self, st: &mut State, req: &Request) -> Result<Vec<u8>> {
        let vol = Node::incoming_volume(st, req.epoch)?;
        let n = req.nblocks as usize;
        if req.payload.len() != n * RESYNC_ENTRY {
            return Err(Error::InvalidArgument(format!(
                "{} payload bytes for {n} resync blocks",
                req.payload.len()
            )));
        }
        let items: Vec<(u64, u64, &[u8])> = req
            .payload
            .as_chunks::<RESYNC_ENTRY>()
            .0
            .iter()
            .map(|c| (get_u64(c, 0), get_u64(c, 8), &c[16..]))
            .collect();
        vol.apply_resync(&items)?;
        Ok(Vec::new())
    }

    fn resync_end(&self, st: &mut State, req: &Request) -> Result<Vec<u8>> {
        let vol = Node::incoming_volume(st, req.epoch)?;
        let inc = st.incoming.take().expect("incoming_volume found it");
        if req
            .payload
            .first_chunk::<8>()
            .map(|b| u64::from_le_bytes(*b))
            != Some(inc.target)
        {
            st.incoming = Some(inc);
            return Err(Error::InvalidArgument(
                "resync end with another target".into(),
            ));
        }
        let finished = vol
            .finish_resync(inc.target)
            .and_then(|()| vol.set_epoch(req.epoch, Some(&inc.history)));
        if let Err(e) = finished {
            st.incoming = Some(inc);
            return Err(e);
        }
        if let Some((new_vol, tmp)) = inc.full {
            fs::rename(&tmp, &self.path)?;
            sync_dir(&self.path)?;
            st.volume = new_vol;
        }
        if st.marker.is_some() {
            remove_marker(&sidecar(&self.path, "resync"))?;
            st.marker = None;
        }
        st.attached = Some(req.epoch);
        Ok(st.claim(self.id).encode())
    }

    fn on_write(&self, req: &Request) -> Response {
        let n = req.nblocks as usize;
        if n == 0 || req.payload.len() != n * BLOCK_SIZE {
            return Response::error(
                req.req_id,
                Status::IoError,
                &format!("{} payload bytes for {n} blocks", req.payload.len()),
            );
        }
        let mut st = self.lock();
        if let Some(r) = self.check_client(&st, req) {
            return r;
        }
        let vol = st.volume.clone();
        let Some(before) = st.primary.as_ref().map(|p| p.appended) else {
            return Response::error(req.req_id, Status::NotPrimary, "not the primary");
        };
        let result = vol.write_lsn(req.lba, &req.payload);
        let after = match &result {
            Ok(last) => *last,
            Err(_) => vol.status().lsn,
        };
        let p = st.primary.as_mut().expect("the lock is held");
        if after < before || after - before > n as u64 {
            // Someone else wrote to the volume: the LSNs no longer match
            // what the followers receive. Stop being the primary.
            self.step_down(&mut st);
            return Response::error(req.req_id, Status::Failed, "LSN out of order");
        }
        // Every LSN the volume assigned is forwarded, even after an error.
        for i in 0..(after - before) as usize {
            p.queue.push_back(Entry {
                lba: req.lba + i as u64,
                data: Arc::from(&req.payload[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE]),
            });
        }
        p.appended = after;
        p.trim();
        let epoch = p.epoch;
        self.changed.notify_all();
        if let Err(e) = result {
            if vol.status().state == VolumeState::Failed {
                self.step_down(&mut st);
            }
            return server::respond(req, Err(e));
        }
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            match &st.primary {
                Some(p) if p.epoch == epoch => {
                    if p.commit >= after {
                        return Response::ok(req.req_id, Vec::new());
                    }
                }
                _ => {
                    return Response::error(
                        req.req_id,
                        Status::NotPrimary,
                        "stopped being the primary before the write was durable on a majority",
                    );
                }
            }
            if self.stopping() {
                return Response::error(req.req_id, Status::NotPrimary, "the replica is stopping");
            }
            let now = Instant::now();
            if now >= deadline {
                return Response::error(
                    req.req_id,
                    Status::IoError,
                    "the write is not durable on a majority",
                );
            }
            st = match self.changed.wait_timeout(st, deadline - now) {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn on_read(&self, req: &Request) -> Response {
        let vol = {
            let st = self.lock();
            if let Some(r) = self.check_client(&st, req) {
                return r;
            }
            st.volume.clone()
        };
        server::respond(req, server::execute(&vol, req))
    }

    fn on_flush(&self, req: &Request) -> Response {
        let st = self.lock();
        if let Some(r) = self.check_client(&st, req) {
            return r;
        }
        // Every acknowledged write is already durable on a majority.
        Response::ok(req.req_id, Vec::new())
    }

    /// Snapshots and export are local to each replica.
    fn on_local(&self, req: &Request) -> Response {
        let vol = self.lock().volume.clone();
        server::respond(req, server::execute(&vol, req))
    }

    fn on_status(&self, req: &Request) -> Response {
        let (vol, replicas) = {
            let st = self.lock();
            let vol = st.volume.clone();
            let epoch = vol.epoch();
            let mut replicas = Vec::new();
            match &st.primary {
                Some(p) => {
                    replicas.push(ReplicaReport {
                        id: self.id,
                        role: Role::Primary,
                        state: ReplicaState::Activa,
                        epoch,
                        last_lsn: p.appended,
                        last_heartbeat_ms: 0,
                    });
                    let now = Instant::now();
                    for l in &p.links {
                        let since = now.duration_since(l.last_answer.unwrap_or(p.since));
                        let last_lsn = l.matched.max(l.reported_lsn);
                        let state = if since > DOWN_AFTER {
                            ReplicaState::Caida
                        } else if l.phase != Phase::Attached || last_lsn < p.appended {
                            ReplicaState::Atrasada
                        } else {
                            ReplicaState::Activa
                        };
                        replicas.push(ReplicaReport {
                            id: l.id,
                            role: Role::Follower,
                            state,
                            epoch: l.reported_epoch,
                            last_lsn,
                            last_heartbeat_ms: since.as_millis() as u64,
                        });
                    }
                }
                None => replicas.push(ReplicaReport {
                    id: self.id,
                    role: Role::Follower,
                    state: ReplicaState::Activa,
                    epoch,
                    last_lsn: st.claim(self.id).last_lsn,
                    last_heartbeat_ms: 0,
                }),
            }
            (vol, replicas)
        };
        server::respond(
            req,
            StatusReport::build(&vol, None, replicas).map(|r| r.to_json().into_bytes()),
        )
    }
}

impl Handler for Node {
    fn handle(&self, req: &Request) -> Response {
        match req.op {
            Op::Heartbeat => self.on_heartbeat(req),
            Op::Append => self.on_append(req),
            Op::Prepare => self.on_prepare(req),
            Op::Promote => self.on_promote(req),
            Op::Resync => self.on_resync(req),
            Op::Write | Op::WriteFua => self.on_write(req),
            Op::Read => self.on_read(req),
            Op::Flush => self.on_flush(req),
            Op::Status => self.on_status(req),
            Op::SnapshotCreate | Op::SnapshotDelete | Op::Export => self.on_local(req),
            op => Response::error(
                req.req_id,
                Status::IoError,
                &format!("op {op:?} is not served by a replica"),
            ),
        }
    }
}

/// Writes the entries of an `APPEND` in order and syncs them.
fn apply_entries(vol: &Volume, payload: &[u8]) -> Result<()> {
    for entry in payload.as_chunks::<APPEND_ENTRY>().0 {
        vol.write(get_u64(entry, 0), &entry[8..])?;
    }
    vol.flush()
}

fn fenced(req: &Request, current: u64) -> Response {
    Response::error(
        req.req_id,
        Status::Fenced,
        &format!("epoch {} is older than {current}", req.epoch),
    )
}

/// Syncs the local volume with everything appended since the last sync,
/// for as long as the node is the primary of `epoch`.
fn flusher(node: &Node, epoch: u64) {
    loop {
        let (vol, target) = {
            let mut st = node.lock();
            loop {
                if node.stopping() {
                    return;
                }
                let Some(p) = st.primary.as_ref().filter(|p| p.epoch == epoch) else {
                    return;
                };
                if p.appended > p.local_durable {
                    let target = p.appended;
                    break (st.volume.clone(), target);
                }
                st = match node.changed.wait_timeout(st, HEARTBEAT_INTERVAL) {
                    Ok((g, _)) => g,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
        };
        let synced = vol.flush();
        let mut st = node.lock();
        if !st.primary.as_ref().is_some_and(|p| p.epoch == epoch) {
            return;
        }
        if synced.is_err() {
            // Fail-stop volume: this replica can no longer be the primary.
            node.step_down(&mut st);
            return;
        }
        let p = st.primary.as_mut().expect("checked above");
        p.local_durable = p.local_durable.max(target);
        p.advance();
        node.changed.notify_all();
    }
}

/// A connection to another replica, one request at a time.
pub struct PeerConn {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    next_req: u64,
}

impl PeerConn {
    pub fn open(socket: &Path, timeout: Duration) -> io::Result<PeerConn> {
        let stream = crate::socket::connect(socket)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let write_half = stream.try_clone()?;
        Ok(PeerConn {
            reader: BufReader::new(stream),
            writer: BufWriter::new(write_half),
            next_req: 1,
        })
    }

    /// Sends `req` with a fresh `req_id` and returns its answer.
    pub fn call(&mut self, mut req: Request) -> io::Result<Response> {
        req.req_id = self.next_req;
        self.next_req += 1;
        wire::write_request(&mut self.writer, &req)?;
        let resp = wire::read_response(&mut self.reader)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"))?;
        if resp.req_id != req.req_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("answer {} for request {}", resp.req_id, req.req_id),
            ));
        }
        Ok(resp)
    }
}

/// One request to `socket` on a new connection.
fn call_once(socket: &Path, req: Request, timeout: Duration) -> io::Result<Response> {
    PeerConn::open(socket, timeout)?.call(req)
}

/// What a pump does next.
enum Action {
    Connect(PathBuf),
    Handshake,
    Heartbeat,
    Append(u64, Vec<(u64, Arc<[u8]>)>),
    Wait(Duration),
}

/// Why a pump dropped its connection.
enum Broken {
    /// The follower answered `FENCED`: this replica is no longer primary.
    Fenced,
    /// Socket error or unexpected answer: reconnect and resync.
    Link,
    /// The node is no longer the primary of the pump's epoch.
    Gone,
}

/// Replicates to follower `slot` for as long as the node is the primary of
/// `epoch`: handshake and resync, then `APPEND` and `HEARTBEAT`.
fn pump(node: &Node, slot: usize, epoch: u64) {
    let mut conn: Option<PeerConn> = None;
    let mut last_beat: Option<Instant> = None;
    loop {
        if node.stopping() {
            return;
        }
        let action = {
            let mut st = node.lock();
            let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
                return;
            };
            let beat_due = last_beat.is_none_or(|t| t.elapsed() >= HEARTBEAT_INTERVAL);
            let link = &mut p.links[slot];
            if conn.is_none() {
                link.phase = Phase::Detached;
                Action::Connect(link.socket.clone())
            } else if link.phase == Phase::Detached {
                Action::Handshake
            } else if beat_due {
                Action::Heartbeat
            } else if p.links[slot].sent < p.appended {
                let sent = p.links[slot].sent;
                match p.entries_after(sent, MAX_APPEND_BLOCKS) {
                    Some(entries) if !entries.is_empty() => {
                        p.links[slot].sent = sent + entries.len() as u64;
                        Action::Append(sent, entries)
                    }
                    _ => {
                        p.links[slot].phase = Phase::Detached;
                        Action::Handshake
                    }
                }
            } else {
                let elapsed = last_beat.map_or(HEARTBEAT_INTERVAL, |t| t.elapsed());
                Action::Wait(HEARTBEAT_INTERVAL.saturating_sub(elapsed))
            }
        };
        let outcome = match action {
            Action::Connect(socket) => match PeerConn::open(&socket, PEER_IO_TIMEOUT) {
                Ok(c) => {
                    conn = Some(c);
                    Ok(())
                }
                Err(_) => {
                    wait(node, HEARTBEAT_INTERVAL);
                    Ok(())
                }
            },
            Action::Wait(d) => {
                wait(node, d);
                Ok(())
            }
            Action::Heartbeat => {
                last_beat = Some(Instant::now());
                let c = conn.as_mut().expect("connected");
                heartbeat(node, c, slot, epoch).map(drop)
            }
            Action::Append(prev, entries) => {
                let c = conn.as_mut().expect("connected");
                append(node, c, slot, epoch, prev, &entries)
            }
            Action::Handshake => {
                last_beat = Some(Instant::now());
                let c = conn.as_mut().expect("connected");
                handshake(node, c, slot, epoch)
            }
        };
        match outcome {
            Ok(()) => {}
            Err(Broken::Link) => {
                conn = None;
                let mut st = node.lock();
                if let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) {
                    p.links[slot].phase = Phase::Detached;
                    p.trim();
                }
                drop(st);
                wait(node, HEARTBEAT_INTERVAL);
            }
            Err(Broken::Fenced) => {
                let mut st = node.lock();
                if st.primary.as_ref().is_some_and(|p| p.epoch == epoch) {
                    node.step_down(&mut st);
                }
                return;
            }
            Err(Broken::Gone) => return,
        }
    }
}

/// Waits on the node's condition for at most `d` (or until it changes).
fn wait(node: &Node, d: Duration) {
    if d.is_zero() {
        return;
    }
    let st = node.lock();
    if node.stopping() {
        return;
    }
    let _ = node.changed.wait_timeout(st, d);
}

/// Sends one frame to a follower and classifies its answer: `OK` payload,
/// or why the link broke. Every answer refreshes the follower's liveness.
fn exchange(
    node: &Node,
    conn: &mut PeerConn,
    slot: usize,
    epoch: u64,
    req: Request,
) -> std::result::Result<Vec<u8>, Broken> {
    let resp = conn.call(req).map_err(|_| Broken::Link)?;
    let mut st = node.lock();
    let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
        return Err(Broken::Gone);
    };
    p.links[slot].last_answer = Some(Instant::now());
    match resp.status {
        Status::Ok => Ok(resp.payload),
        Status::Fenced => Err(Broken::Fenced),
        _ => Err(Broken::Link),
    }
}

/// Records the claim a follower answered.
fn note_claim(
    node: &Node,
    slot: usize,
    epoch: u64,
    claim: &Claim,
) -> std::result::Result<(), Broken> {
    let mut st = node.lock();
    let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
        return Err(Broken::Gone);
    };
    let link = &mut p.links[slot];
    link.reported_epoch = claim.current_epoch;
    link.reported_lsn = claim.last_lsn;
    if !claim.attached && link.phase == Phase::Attached {
        link.phase = Phase::Detached;
    }
    Ok(())
}

fn heartbeat(
    node: &Node,
    conn: &mut PeerConn,
    slot: usize,
    epoch: u64,
) -> std::result::Result<Claim, Broken> {
    let mut req = Request::new(Op::Heartbeat, 0);
    req.epoch = epoch;
    req.lba = node.id;
    let payload = exchange(node, conn, slot, epoch, req)?;
    let claim = Claim::decode(&payload).ok_or(Broken::Link)?;
    note_claim(node, slot, epoch, &claim)?;
    Ok(claim)
}

fn append(
    node: &Node,
    conn: &mut PeerConn,
    slot: usize,
    epoch: u64,
    prev: u64,
    entries: &[(u64, Arc<[u8]>)],
) -> std::result::Result<(), Broken> {
    let mut payload = Vec::with_capacity(entries.len() * APPEND_ENTRY);
    for (lba, data) in entries {
        payload.extend_from_slice(&lba.to_le_bytes());
        payload.extend_from_slice(data);
    }
    let req = Request {
        op: Op::Append,
        req_id: 0,
        epoch,
        lba: prev,
        nblocks: entries.len() as u32,
        payload,
    };
    let answer = exchange(node, conn, slot, epoch, req)?;
    let (applied, claim) = match answer.split_first() {
        Some((applied, rest)) => (*applied == 1, Claim::decode(rest).ok_or(Broken::Link)?),
        None => return Err(Broken::Link),
    };
    note_claim(node, slot, epoch, &claim)?;
    let mut st = node.lock();
    let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
        return Err(Broken::Gone);
    };
    let link = &mut p.links[slot];
    if !applied {
        link.phase = Phase::Detached;
        p.trim();
        return Ok(());
    }
    link.matched = link.matched.max(prev + entries.len() as u64);
    p.advance();
    p.trim();
    node.changed.notify_all();
    Ok(())
}

/// Reads the follower's claim, picks delta or full by the rejoin rule and
/// sends a pinned version of the volume; the follower is attached at the
/// pinned LSN and the queue keeps everything written since.
fn handshake(
    node: &Node,
    conn: &mut PeerConn,
    slot: usize,
    epoch: u64,
) -> std::result::Result<(), Broken> {
    let claim = heartbeat(node, conn, slot, epoch)?;
    let (vol, pin, mode, history) = {
        let mut st = node.lock();
        let vol = st.volume.clone();
        let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
            return Err(Broken::Gone);
        };
        let history = vol.epoch_history();
        let geometry = (vol.volume_size(), vol.segment_size());
        let mode = plan_resync(&history, p.appended, geometry, &claim);
        // Writes take the node lock, so the pin is exactly at `appended`.
        let pin = vol.pin().map_err(|_| Broken::Gone)?;
        let link = &mut p.links[slot];
        link.phase = Phase::Resyncing;
        link.sent = pin.lsn();
        p.trim();
        (vol, pin, mode, history)
    };
    let base = match mode {
        ResyncMode::Delta => claim.last_lsn,
        ResyncMode::Full => 0,
    };
    send_resync(node, conn, slot, epoch, &vol, &pin, mode, base, &history)?;
    let target = pin.lsn();
    drop(pin);
    let mut st = node.lock();
    let Some(p) = st.primary.as_mut().filter(|p| p.epoch == epoch) else {
        return Err(Broken::Gone);
    };
    let link = &mut p.links[slot];
    if link.phase == Phase::Resyncing {
        link.phase = Phase::Attached;
    }
    link.matched = link.matched.max(target);
    p.advance();
    p.trim();
    node.changed.notify_all();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_resync(
    node: &Node,
    conn: &mut PeerConn,
    slot: usize,
    epoch: u64,
    vol: &Volume,
    pin: &Pin,
    mode: ResyncMode,
    base: u64,
    history: &[EpochEntry],
) -> std::result::Result<(), Broken> {
    let target = pin.lsn();
    let mut begin = vec![mode as u8];
    for v in [base, target, vol.volume_size(), vol.segment_size()] {
        begin.extend_from_slice(&v.to_le_bytes());
    }
    begin.extend_from_slice(&(history.len() as u32).to_le_bytes());
    for e in history {
        begin.extend_from_slice(&e.epoch.to_le_bytes());
        begin.extend_from_slice(&e.start_lsn.to_le_bytes());
    }
    let frame = |lba: u64, nblocks: u32, payload: Vec<u8>| Request {
        op: Op::Resync,
        req_id: 0,
        epoch,
        lba,
        nblocks,
        payload,
    };
    exchange(node, conn, slot, epoch, frame(RESYNC_BEGIN, 0, begin))?;

    let births = vol
        .pinned_births_after(pin, base)
        .map_err(|_| Broken::Gone)?;
    for chunk in births.chunks(RESYNC_CHUNK_BLOCKS) {
        let mut payload = Vec::with_capacity(chunk.len() * RESYNC_ENTRY);
        // One read per run of consecutive LBAs.
        let mut i = 0;
        while i < chunk.len() {
            let mut j = i + 1;
            while j < chunk.len() && chunk[j].0 == chunk[j - 1].0 + 1 {
                j += 1;
            }
            let data = vol
                .read_pinned(pin, chunk[i].0, (j - i) as u64)
                .map_err(|_| Broken::Gone)?;
            for (k, (lba, birth)) in chunk[i..j].iter().enumerate() {
                payload.extend_from_slice(&lba.to_le_bytes());
                payload.extend_from_slice(&birth.to_le_bytes());
                payload.extend_from_slice(&data[k * BLOCK_SIZE..(k + 1) * BLOCK_SIZE]);
            }
            i = j;
        }
        exchange(
            node,
            conn,
            slot,
            epoch,
            frame(RESYNC_DATA, chunk.len() as u32, payload),
        )?;
    }

    let answer = exchange(
        node,
        conn,
        slot,
        epoch,
        frame(RESYNC_END, 0, target.to_le_bytes().to_vec()),
    )?;
    let claim = Claim::decode(&answer).ok_or(Broken::Link)?;
    note_claim(node, slot, epoch, &claim)?;
    if !claim.attached || claim.last_lsn != target {
        return Err(Broken::Link);
    }
    Ok(())
}

/// Result of an election.
#[derive(Debug, Clone)]
pub struct Elected {
    pub id: u64,
    pub epoch: u64,
    pub socket: PathBuf,
    /// The claim the elected replica answered to `PREPARE`.
    pub claim: Claim,
}

/// Two-phase election over `replicas` (see the module documentation).
/// Unreachable or stopped replicas are skipped after `ELECT_TIMEOUT`; it
/// fails without a majority of acceptances. A race with another election
/// is retried with a higher epoch, up to three times.
pub fn elect(replicas: &[PathBuf]) -> Result<Elected> {
    if replicas.is_empty() {
        return Err(Error::InvalidArgument("no replicas to elect from".into()));
    }
    let majority = replicas.len() / 2 + 1;
    let mut known = 0;
    let mut last_error = String::new();
    for _ in 0..3 {
        let epochs = on_each(replicas, |_| Request::new(Op::Status, 0));
        for resp in epochs.into_iter().flatten() {
            if resp.status == Status::Ok
                && let Ok(report) = StatusReport::from_json(&resp.payload)
            {
                known = known.max(report.volume.epoch);
            }
        }
        let epoch = known + 1;
        let answers = on_each(replicas, |_| {
            let mut req = Request::new(Op::Prepare, 0);
            req.epoch = epoch;
            req
        });
        let accepted: Vec<(PathBuf, Claim)> = replicas
            .iter()
            .zip(answers)
            .filter_map(|(socket, resp)| match resp {
                Some(r) if r.status == Status::Ok => {
                    Claim::decode(&r.payload).map(|c| (socket.clone(), c))
                }
                _ => None,
            })
            .collect();
        known = epoch;
        if accepted.len() < majority {
            last_error = format!(
                "{} of {} replicas accepted epoch {epoch}, {majority} needed",
                accepted.len(),
                replicas.len()
            );
            continue;
        }
        let (socket, claim) = accepted
            .into_iter()
            .max_by_key(|(_, c)| c.rank())
            .expect("a majority is not empty");
        let mut req = Request::new(Op::Promote, 0);
        req.epoch = epoch;
        match call_once(&socket, req, ELECT_TIMEOUT) {
            Ok(r) if r.status == Status::Ok => {
                return Ok(Elected {
                    id: claim.id,
                    epoch,
                    socket,
                    claim,
                });
            }
            Ok(r) => last_error = format!("PROMOTE({epoch}): {}: {}", r.status.name(), r.text()),
            Err(e) => last_error = format!("PROMOTE({epoch}): {e}"),
        }
    }
    Err(Error::Io(io::Error::other(format!(
        "election failed: {last_error}"
    ))))
}

/// Sends the request built for each replica in parallel and returns the
/// answers in order; `None` where the replica did not answer in time.
fn on_each(replicas: &[PathBuf], build: impl Fn(&Path) -> Request + Sync) -> Vec<Option<Response>> {
    thread::scope(|s| {
        let handles: Vec<_> = replicas
            .iter()
            .map(|socket| {
                let req = build(socket.as_path());
                s.spawn(move || call_once(socket, req, ELECT_TIMEOUT).ok())
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    })
}

/// `<path>.<ext>` next to the volume file.
fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// Syncs the directory holding `path`, so that a create, rename or remove
/// in it is durable.
fn sync_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn write_marker(path: &Path, lsn: u64) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(format!("{lsn}\n").as_bytes())?;
    f.sync_all()?;
    sync_dir(path)
}

fn remove_marker(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// `l_f` of an interrupted delta resync. A marker that cannot be parsed
/// was being written when the process died, before any resync block:
/// the volume's own last LSN is then still consistent.
fn read_marker(path: &Path, last_lsn: u64) -> Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            text.trim().parse::<u64>().unwrap_or(last_lsn).min(last_lsn),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

fn get_u32(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(a)
}

fn get_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
