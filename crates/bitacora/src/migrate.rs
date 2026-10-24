//! Live migration between two standalone servers (contract section 10).
//!
//! The source S runs the migration on the connection thread of the
//! `MIGRATE` request sent by `bitacora migrate --from S --to D`, while its
//! other connections keep serving:
//!
//! 1. A pin (an internal snapshot kept only in memory, `volume/resync.rs`)
//!    freezes the live index and every block it holds goes to D in
//!    `MIGRATE_DATA` frames, each block with its `birth`.
//! 2. Rounds: a new pin, and the blocks whose `birth` is newer than the
//!    previous pin's last LSN go to D. The rounds stop once a difference
//!    has fewer than `ROUND_TARGET` blocks or after `MAX_ROUNDS` rounds.
//!    From round `THROTTLE_ROUND` on, S delays the answer to every write by
//!    `THROTTLE` so the difference converges.
//! 3. Freeze: S stops starting requests (new ones wait at the `Gate`),
//!    waits for the ones in flight, sends the final difference and
//!    `MIGRATE_FREEZE` with its last LSN, on which D syncs every block
//!    (`fdatasync`). S then raises the volume epoch and D adopts it with
//!    `MIGRATE_COMMIT`, after which D serves.
//! 4. S answers `MOVED(D)` to every waiting request and to every new one.
//!
//! Pins are dropped when the migration ends, successful or not, so they
//! leave no reference behind and nothing in the superblock. A migration
//! that fails before the commit thaws S, which keeps serving.
//!
//! On D, `bitacora serve --migrate-target` starts with the gate in
//! `Awaiting`: data requests are answered `NOT_PRIMARY` until the commit.
//! The blocks from S go to the resync stream with their original `birth`
//! (`Volume::apply_resync`), and `MIGRATE_FREEZE` moves D's next LSN past
//! S's last one (`Volume::finish_resync`), so D can later be the source of
//! another migration.
//!
//! Frame use: `MIGRATE` carries the destination socket path. `MIGRATE_DATA`
//! carries `nblocks` entries `(lba u64, birth u64, 4096 bytes)` and the
//! source `volume_size` in `lba`, which D checks against its own.
//! `MIGRATE_FREEZE` carries S's last LSN in `lba`; `MIGRATE_COMMIT` the new
//! epoch in `epoch`.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::format::BLOCK_SIZE;
use crate::volume::{Pin, Volume};
use crate::wire::{self, Op, Request, Response, Status};

/// A round difference below this many blocks ends the rounds.
pub const ROUND_TARGET: usize = 1024;
/// Rounds after the initial copy, at most.
pub const MAX_ROUNDS: u32 = 8;
/// First round in which every write answer of the source is delayed.
pub const THROTTLE_ROUND: u32 = 5;
/// Delay of every write answer from `THROTTLE_ROUND` on.
pub const THROTTLE: Duration = Duration::from_millis(1);
/// Blocks per `MIGRATE_DATA` frame: about 1 MiB.
const DATA_CHUNK: usize = 256;
/// Bytes of one block entry of `MIGRATE_DATA`.
const ENTRY: usize = 16 + BLOCK_SIZE;
/// Socket timeout of the connection from source to destination.
const PEER_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a server stands in a migration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Serving requests.
    Serving,
    /// Source between freeze and commit: requests wait.
    Frozen,
    /// Source after the commit: every request is answered `MOVED`.
    Moved(String),
    /// Destination before the commit: data requests are refused.
    Awaiting,
}

struct GateState {
    phase: Phase,
    /// Requests started and not yet answered.
    in_flight: u64,
    /// Whether write answers are delayed by `THROTTLE`.
    throttle: bool,
    /// Whether this server is running a migration as source.
    migrating: bool,
}

/// Admission of client requests during a migration. Every request that
/// executes on the volume holds a `Ticket` from `enter` until its answer is
/// written, so a freeze knows when nothing is in flight.
pub struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

/// Why a request may not execute.
pub enum Refusal {
    Moved(String),
    NotPrimary,
}

/// A request admitted by the gate; dropping it ends the request.
pub struct Ticket<'a> {
    gate: &'a Gate,
    throttle: bool,
}

impl Ticket<'_> {
    /// Whether the answer to a write must be delayed by `THROTTLE`.
    pub fn throttled(&self) -> bool {
        self.throttle
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut st = self.gate.lock();
        st.in_flight -= 1;
        if st.in_flight == 0 {
            self.gate.changed.notify_all();
        }
    }
}

impl Gate {
    /// A gate that serves from the start.
    pub fn serving() -> Gate {
        Gate::with_phase(Phase::Serving)
    }

    /// A gate of a destination: nothing is served until the commit.
    pub fn target() -> Gate {
        Gate::with_phase(Phase::Awaiting)
    }

    fn with_phase(phase: Phase) -> Gate {
        Gate {
            state: Mutex::new(GateState {
                phase,
                in_flight: 0,
                throttle: false,
                migrating: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Admits a request, waiting while the source is frozen.
    pub fn enter(&self) -> std::result::Result<Ticket<'_>, Refusal> {
        let mut st = self.lock();
        loop {
            match st.phase.clone() {
                Phase::Serving => {
                    st.in_flight += 1;
                    return Ok(Ticket {
                        gate: self,
                        throttle: st.throttle,
                    });
                }
                Phase::Frozen => {
                    st = match self.changed.wait(st) {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
                Phase::Moved(dest) => return Err(Refusal::Moved(dest)),
                Phase::Awaiting => return Err(Refusal::NotPrimary),
            }
        }
    }

    /// The destination once the volume moved.
    pub fn moved_to(&self) -> Option<String> {
        match &self.lock().phase {
            Phase::Moved(dest) => Some(dest.clone()),
            _ => None,
        }
    }

    /// Every request from now on, and every one waiting, is answered
    /// `MOVED(destination)`.
    pub fn set_moved(&self, destination: &str) {
        let mut st = self.lock();
        st.phase = Phase::Moved(destination.to_string());
        st.throttle = false;
        self.changed.notify_all();
    }

    fn set_throttle(&self, on: bool) {
        self.lock().throttle = on;
    }

    /// Stops admitting requests and waits for the ones in flight.
    fn freeze(&self) {
        let mut st = self.lock();
        st.phase = Phase::Frozen;
        while st.in_flight > 0 {
            st = match self.changed.wait(st) {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    /// Back to serving after a failed migration.
    fn thaw(&self) {
        let mut st = self.lock();
        if st.phase == Phase::Frozen {
            st.phase = Phase::Serving;
        }
        st.throttle = false;
        st.migrating = false;
        self.changed.notify_all();
    }

    /// Marks the start of a migration as source; only one at a time, and
    /// only while serving.
    fn begin(&self) -> Result<()> {
        let mut st = self.lock();
        if st.migrating || st.phase != Phase::Serving {
            return Err(Error::InvalidArgument(
                "the volume is not serving or is already migrating".into(),
            ));
        }
        st.migrating = true;
        Ok(())
    }

    fn end(&self) {
        self.lock().migrating = false;
    }
}

/// What a migration did, for the `MIGRATE` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Rounds after the initial copy.
    pub rounds: u32,
    /// Blocks sent in all, final difference included.
    pub blocks: u64,
    /// Blocks of the final difference, sent while frozen.
    pub final_blocks: u64,
    /// Epoch the destination adopted.
    pub epoch: u64,
}

impl Summary {
    pub fn to_text(&self) -> String {
        format!(
            "rondas {} bloques {} diferencia_final {} epoca {}",
            self.rounds, self.blocks, self.final_blocks, self.epoch
        )
    }

    pub fn from_text(text: &str) -> Option<Summary> {
        let words: Vec<&str> = text.split_whitespace().collect();
        match words.as_slice() {
            ["rondas", r, "bloques", b, "diferencia_final", f, "epoca", e] => Some(Summary {
                rounds: r.parse().ok()?,
                blocks: b.parse().ok()?,
                final_blocks: f.parse().ok()?,
                epoch: e.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// Connection from the source to the destination.
struct Peer {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    next_req: u64,
}

impl Peer {
    fn connect(path: &Path) -> Result<Peer> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(PEER_TIMEOUT))?;
        stream.set_write_timeout(Some(PEER_TIMEOUT))?;
        let write_half = stream.try_clone()?;
        Ok(Peer {
            reader: BufReader::new(stream),
            writer: BufWriter::new(write_half),
            next_req: 1,
        })
    }

    /// Sends `req` and fails unless the answer is `OK`.
    fn call(&mut self, mut req: Request) -> Result<Vec<u8>> {
        req.req_id = self.next_req;
        self.next_req += 1;
        wire::write_request(&mut self.writer, &req)?;
        let resp = wire::read_response(&mut self.reader)?.ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the destination closed the connection",
            ))
        })?;
        if resp.req_id != req.req_id {
            return Err(Error::InvalidArgument(format!(
                "destination answered request {} while waiting for {}",
                resp.req_id, req.req_id
            )));
        }
        match resp.status {
            Status::Ok => Ok(resp.payload),
            status => Err(Error::InvalidArgument(format!(
                "destination answered {}: {}",
                status.name(),
                resp.text()
            ))),
        }
    }

    /// Sends the blocks `(lba, birth)` of `pin` in `MIGRATE_DATA` frames.
    fn send_blocks(&mut self, vol: &Volume, pin: &Pin, blocks: &[(u64, u64)]) -> Result<()> {
        for chunk in blocks.chunks(DATA_CHUNK) {
            let mut payload = Vec::with_capacity(chunk.len() * ENTRY);
            for (lba, birth) in chunk {
                payload.extend_from_slice(&lba.to_le_bytes());
                payload.extend_from_slice(&birth.to_le_bytes());
                payload.extend_from_slice(&vol.read_pinned(pin, *lba, 1)?);
            }
            let mut req = Request::new(Op::MigrateData, 0);
            req.lba = vol.volume_size();
            req.nblocks = chunk.len() as u32;
            req.payload = payload;
            self.call(req)?;
        }
        Ok(())
    }
}

/// Runs a migration of `vol` to the server at `destination` (the four
/// phases of the module documentation). On success the gate answers
/// `MOVED(destination)` from then on.
pub fn run_source(vol: &Volume, gate: &Gate, destination: &str) -> Result<Summary> {
    gate.begin()?;
    let result = migrate(vol, gate, destination);
    match &result {
        Ok(_) => gate.end(),
        Err(_) => gate.thaw(),
    }
    result
}

fn migrate(vol: &Volume, gate: &Gate, destination: &str) -> Result<Summary> {
    let mut peer = Peer::connect(Path::new(destination))?;
    let mut sent = 0u64;

    // 1. Initial copy.
    let mut prev = vol.pin()?;
    let blocks = vol.pinned_births_after(&prev, 0)?;
    peer.send_blocks(vol, &prev, &blocks)?;
    sent += blocks.len() as u64;

    // 2. Rounds by difference of `birth`.
    let mut rounds = 0;
    while rounds < MAX_ROUNDS {
        rounds += 1;
        if rounds >= THROTTLE_ROUND {
            gate.set_throttle(true);
        }
        let pin = vol.pin()?;
        let diff = vol.pinned_births_after(&pin, prev.lsn())?;
        peer.send_blocks(vol, &pin, &diff)?;
        sent += diff.len() as u64;
        // The previous pin goes: its references are released here.
        prev = pin;
        if diff.len() < ROUND_TARGET {
            break;
        }
    }

    // 3. Freeze, drain and final difference. Nothing is acknowledged by
    // the source after `freeze` returns, so the last pin holds every
    // acknowledged write.
    gate.freeze();
    let last = vol.pin()?;
    let diff = vol.pinned_births_after(&last, prev.lsn())?;
    drop(prev);
    peer.send_blocks(vol, &last, &diff)?;
    sent += diff.len() as u64;
    let mut freeze = Request::new(Op::MigrateFreeze, 0);
    freeze.lba = last.lsn();
    peer.call(freeze)?;
    drop(last);

    let epoch = vol.epoch() + 1;
    vol.set_epoch(epoch, None)?;
    let mut commit = Request::new(Op::MigrateCommit, 0);
    commit.epoch = epoch;
    // If the commit fails the epoch already moved; the source thaws and
    // keeps serving the volume with it.
    peer.call(commit)?;

    // 4. Everything waiting and everything new is answered `MOVED`.
    gate.set_moved(destination);
    Ok(Summary {
        rounds,
        blocks: sent,
        final_blocks: diff.len() as u64,
        epoch,
    })
}

/// Answers a migration frame from a source on a destination volume.
pub fn handle_target(vol: &Volume, gate: &Gate, req: &Request) -> Result<Vec<u8>> {
    match req.op {
        Op::MigrateData => {
            if gate.lock().phase != Phase::Awaiting {
                return Err(Error::InvalidArgument(
                    "this server is not waiting for a migration".into(),
                ));
            }
            if req.lba != vol.volume_size() {
                return Err(Error::InvalidArgument(format!(
                    "source volume has {} bytes, destination {}",
                    req.lba,
                    vol.volume_size()
                )));
            }
            let n = req.nblocks as usize;
            if req.payload.len() != n * ENTRY {
                return Err(Error::InvalidArgument(format!(
                    "{} payload bytes for {n} migrated blocks",
                    req.payload.len()
                )));
            }
            let items: Vec<(u64, u64, &[u8])> = req
                .payload
                .as_chunks::<ENTRY>()
                .0
                .iter()
                .map(|e| (get_u64(e, 0), get_u64(e, 8), &e[16..]))
                .collect();
            vol.apply_resync(&items)?;
            // Synced frame by frame, so the `fdatasync` of the freeze only
            // covers the final difference and the pause stays short.
            vol.flush()?;
            Ok(Vec::new())
        }
        Op::MigrateFreeze => {
            if gate.lock().phase != Phase::Awaiting {
                return Err(Error::InvalidArgument(
                    "this server is not waiting for a migration".into(),
                ));
            }
            // Syncs every migrated block (`fdatasync`) and moves the next
            // LSN past the source's last one.
            vol.finish_resync(req.lba)?;
            Ok(Vec::new())
        }
        Op::MigrateCommit => {
            if gate.lock().phase != Phase::Awaiting {
                return Err(Error::InvalidArgument(
                    "this server is not waiting for a migration".into(),
                ));
            }
            if req.epoch <= vol.epoch() {
                return Err(Error::InvalidArgument(format!(
                    "commit epoch {} is not newer than {}",
                    req.epoch,
                    vol.epoch()
                )));
            }
            vol.set_epoch(req.epoch, None)?;
            let mut st = gate.lock();
            st.phase = Phase::Serving;
            gate.changed.notify_all();
            Ok(Vec::new())
        }
        op => Err(Error::InvalidArgument(format!(
            "op {op:?} is not a migration frame"
        ))),
    }
}

/// Delays the answer to a write admitted while the source throttles.
pub fn throttle(ticket: &Ticket<'_>, op: Op) {
    if ticket.throttled() && matches!(op, Op::Write | Op::WriteFua) {
        thread::sleep(THROTTLE);
    }
}

/// `bitacora migrate --from S --to D`: asks the server at `from` to migrate
/// its volume to the server at `to` and returns what it did.
pub fn request(from: &Path, to: &Path) -> Result<Summary> {
    let to = to
        .to_str()
        .ok_or_else(|| Error::InvalidArgument("destination path is not UTF-8".into()))?;
    let mut peer = Peer::connect(from)?;
    // The source may take long: no timeout on this connection.
    peer.reader.get_ref().set_read_timeout(None)?;
    let mut req = Request::new(Op::Migrate, 0);
    req.payload = to.as_bytes().to_vec();
    let text = String::from_utf8_lossy(&peer.call(req)?).into_owned();
    Summary::from_text(&text)
        .ok_or_else(|| Error::InvalidArgument(format!("unexpected migrate answer: {text}")))
}

/// The response to a request refused by the gate.
pub fn refusal(req: &Request, refusal: Refusal) -> Response {
    match refusal {
        Refusal::Moved(dest) => Response::error(req.req_id, Status::Moved, &dest),
        Refusal::NotPrimary => Response::error(
            req.req_id,
            Status::NotPrimary,
            "migration target: nothing is served before the commit",
        ),
    }
}

fn get_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
