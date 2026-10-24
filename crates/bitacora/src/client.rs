//! Client of the Unix socket protocol (contract section 8).
//!
//! Requests go one at a time over a single connection. A `MOVED` answer
//! means the request was not executed: the client reconnects to the socket
//! in its payload and sends the same request again.
//!
//! With a list of replica sockets (`connect_replicas`, contract section 9)
//! the client looks for the primary: on `NOT_PRIMARY`, `FENCED` or a broken
//! connection it moves to the next replica and sends the request again,
//! until one answers or `failover_timeout` passes. A write is acknowledged
//! only by an `OK` answer; one sent again after a broken connection may have
//! been applied before, which for a block write is the same content.

use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::format::BLOCK_SIZE;
use crate::snapshot::Version;
use crate::status::StatusReport;
use crate::wire::{self, Op, Request, Response, Status};

/// `MOVED` answers followed for one request before giving up.
pub const MAX_REDIRECTS: usize = 8;
/// How long a client with several replicas keeps looking for the primary.
pub const DEFAULT_FAILOVER_TIMEOUT: Duration = Duration::from_secs(30);
/// Socket timeout of a client with several replicas, so that a stopped
/// primary does not hold it forever.
pub const REPLICA_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause after every replica in the list refused or failed.
const FAILOVER_PAUSE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum ClientError {
    /// The socket failed or the server sent an undecodable frame.
    Io(io::Error),
    /// The server answered with a status other than `OK`.
    Remote { status: Status, message: String },
    /// The answer does not match the request.
    Protocol(String),
    /// More than `MAX_REDIRECTS` `MOVED` answers in a row.
    TooManyRedirects,
    /// No replica answered as primary within the failover timeout; holds
    /// the last error seen.
    NoPrimary(Box<ClientError>),
}

impl ClientError {
    /// Status of a `Remote` error.
    pub fn status(&self) -> Option<Status> {
        match self {
            ClientError::Remote { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "socket error: {e}"),
            ClientError::Remote { status, message } => write!(f, "{}: {message}", status.name()),
            ClientError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            ClientError::TooManyRedirects => write!(f, "more than {MAX_REDIRECTS} redirects"),
            ClientError::NoPrimary(last) => write!(f, "no primary found: {last}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub type ClientResult<T> = std::result::Result<T, ClientError>;

type Conn = (BufReader<UnixStream>, BufWriter<UnixStream>);

pub struct Client {
    /// `None` after a broken connection, until the next request reconnects.
    conn: Option<Conn>,
    /// Socket currently connected; changes after a `MOVED` or a failover.
    path: PathBuf,
    /// Replica sockets to look for the primary in; empty for a single
    /// server.
    replicas: Vec<PathBuf>,
    /// Position in `replicas` of the socket in use.
    current: usize,
    failover_timeout: Duration,
    io_timeout: Option<Duration>,
    next_req: u64,
    /// Epoch sent with every request; 0 means none.
    epoch: u64,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> ClientResult<Client> {
        let path = path.as_ref().to_path_buf();
        let conn = open(&path, None)?;
        Ok(Client {
            conn: Some(conn),
            path,
            replicas: Vec::new(),
            current: 0,
            failover_timeout: DEFAULT_FAILOVER_TIMEOUT,
            io_timeout: None,
            next_req: 1,
            epoch: 0,
        })
    }

    /// A client of a replicated volume. It connects to the first replica
    /// that accepts a connection; the first request finds the primary.
    pub fn connect_replicas<P: AsRef<Path>>(replicas: &[P]) -> ClientResult<Client> {
        let replicas: Vec<PathBuf> = replicas.iter().map(|p| p.as_ref().to_path_buf()).collect();
        let Some(first) = replicas.first().cloned() else {
            return Err(ClientError::Protocol("no replica sockets".into()));
        };
        let mut client = Client {
            conn: None,
            path: first,
            replicas,
            current: 0,
            failover_timeout: DEFAULT_FAILOVER_TIMEOUT,
            io_timeout: Some(REPLICA_IO_TIMEOUT),
            next_req: 1,
            epoch: 0,
        };
        let mut last = None;
        for i in 0..client.replicas.len() {
            client.current = i;
            client.path = client.replicas[i].clone();
            match open(&client.path, client.io_timeout) {
                Ok(conn) => {
                    client.conn = Some(conn);
                    return Ok(client);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(ClientError::Io(last.expect("at least one replica was tried")))
    }

    /// Socket currently connected.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How long a client with several replicas looks for the primary
    /// before a request fails.
    pub fn set_failover_timeout(&mut self, timeout: Duration) {
        self.failover_timeout = timeout;
    }

    pub fn set_epoch(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    pub fn read(&mut self, lba: u64, nblocks: u32) -> ClientResult<Vec<u8>> {
        let data = self.call(Op::Read, lba, nblocks, Vec::new())?;
        if data.len() != nblocks as usize * BLOCK_SIZE {
            return Err(ClientError::Protocol(format!(
                "read of {nblocks} blocks returned {} bytes",
                data.len()
            )));
        }
        Ok(data)
    }

    /// Writes `data` (a multiple of 4096 bytes) at `lba`; durable after
    /// `flush`.
    pub fn write(&mut self, lba: u64, data: &[u8]) -> ClientResult<()> {
        self.write_op(Op::Write, lba, data)
    }

    /// Writes `data` at `lba`, durable when it returns.
    pub fn write_fua(&mut self, lba: u64, data: &[u8]) -> ClientResult<()> {
        self.write_op(Op::WriteFua, lba, data)
    }

    pub fn flush(&mut self) -> ClientResult<()> {
        self.call(Op::Flush, 0, 0, Vec::new()).map(drop)
    }

    pub fn status(&mut self) -> ClientResult<StatusReport> {
        let json = self.status_json()?;
        StatusReport::from_json(json.as_bytes())
            .map_err(|e| ClientError::Protocol(format!("status JSON: {e}")))
    }

    /// The status exactly as the server sent it.
    pub fn status_json(&mut self) -> ClientResult<String> {
        let payload = self.call(Op::Status, 0, 0, Vec::new())?;
        String::from_utf8(payload)
            .map_err(|_| ClientError::Protocol("status is not UTF-8".into()))
    }

    pub fn snapshot_create(&mut self) -> ClientResult<u64> {
        let payload = self.call(Op::SnapshotCreate, 0, 0, Vec::new())?;
        payload
            .as_slice()
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| ClientError::Protocol("snapshot id is not a u64".into()))
    }

    pub fn snapshot_delete(&mut self, id: u64) -> ClientResult<()> {
        self.call(Op::SnapshotDelete, 0, 0, id.to_le_bytes().to_vec())
            .map(drop)
    }

    /// The server writes the raw image of `version` at `path`, on its own
    /// file system.
    pub fn export(&mut self, version: Version, path: &Path) -> ClientResult<()> {
        let id = match version {
            Version::Live => 0,
            Version::Snapshot(id) => id,
        };
        let path = path
            .to_str()
            .ok_or_else(|| ClientError::Protocol("export path is not UTF-8".into()))?;
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend_from_slice(path.as_bytes());
        self.call(Op::Export, 0, 0, payload).map(drop)
    }

    fn write_op(&mut self, op: Op, lba: u64, data: &[u8]) -> ClientResult<()> {
        if !data.len().is_multiple_of(BLOCK_SIZE) {
            return Err(ClientError::Protocol(format!(
                "{} bytes is not a multiple of {BLOCK_SIZE}",
                data.len()
            )));
        }
        let nblocks = u32::try_from(data.len() / BLOCK_SIZE)
            .map_err(|_| ClientError::Protocol("write too large".into()))?;
        self.call(op, lba, nblocks, data.to_vec()).map(drop)
    }

    /// Sends one request and returns the payload of its `OK` answer,
    /// following `MOVED` and, with several replicas, looking for the
    /// primary.
    fn call(&mut self, op: Op, lba: u64, nblocks: u32, payload: Vec<u8>) -> ClientResult<Vec<u8>> {
        let mut req = Request {
            op,
            req_id: 0,
            epoch: self.epoch,
            lba,
            nblocks,
            payload,
        };
        let deadline = Instant::now() + self.failover_timeout;
        let mut redirects = 0;
        // Replicas refused or failed since the last pause.
        let mut misses = 0;
        loop {
            let resp = match self.round_trip(&mut req) {
                Ok(resp) => resp,
                Err(e) if self.replicas.len() > 1 => {
                    self.fail_over(e, deadline, &mut misses)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            match resp.status {
                Status::Ok => return Ok(resp.payload),
                Status::Moved => {
                    redirects += 1;
                    if redirects > MAX_REDIRECTS {
                        return Err(ClientError::TooManyRedirects);
                    }
                    self.reconnect(PathBuf::from(resp.text()))?;
                }
                Status::NotPrimary | Status::Fenced if self.replicas.len() > 1 => {
                    let e = ClientError::Remote {
                        status: resp.status,
                        message: resp.text(),
                    };
                    self.fail_over(e, deadline, &mut misses)?;
                }
                status => {
                    return Err(ClientError::Remote {
                        status,
                        message: resp.text(),
                    });
                }
            }
        }
    }

    /// Drops the connection and moves to the next replica; after a whole
    /// round of misses, pauses. Past the deadline, fails with `error`.
    fn fail_over(
        &mut self,
        error: ClientError,
        deadline: Instant,
        misses: &mut usize,
    ) -> ClientResult<()> {
        self.conn = None;
        if Instant::now() >= deadline {
            return Err(ClientError::NoPrimary(Box::new(error)));
        }
        *misses += 1;
        if *misses >= self.replicas.len() {
            *misses = 0;
            thread::sleep(FAILOVER_PAUSE);
        }
        self.current = (self.current + 1) % self.replicas.len();
        self.path = self.replicas[self.current].clone();
        Ok(())
    }

    /// Sends `req` with a fresh `req_id` (connecting first if needed) and
    /// returns its answer. A socket error drops the connection.
    fn round_trip(&mut self, req: &mut Request) -> ClientResult<Response> {
        if self.conn.is_none() {
            self.conn = Some(open(&self.path, self.io_timeout)?);
        }
        req.req_id = self.next_req;
        self.next_req += 1;
        let (reader, writer) = self.conn.as_mut().expect("connected above");
        let result = wire::write_request(writer, req).and_then(|()| wire::read_response(reader));
        let resp = match result {
            Ok(Some(resp)) => resp,
            Ok(None) => {
                self.conn = None;
                return Err(ClientError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed the connection",
                )));
            }
            Err(e) => {
                self.conn = None;
                return Err(ClientError::Io(e));
            }
        };
        if resp.req_id != req.req_id {
            self.conn = None;
            return Err(ClientError::Protocol(format!(
                "answer for request {} while waiting for {}",
                resp.req_id, req.req_id
            )));
        }
        Ok(resp)
    }

    fn reconnect(&mut self, path: PathBuf) -> ClientResult<()> {
        self.conn = None;
        self.conn = Some(open(&path, self.io_timeout)?);
        self.path = path;
        Ok(())
    }
}

fn open(path: &Path, timeout: Option<Duration>) -> io::Result<Conn> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    let write_half = stream.try_clone()?;
    Ok((BufReader::new(stream), BufWriter::new(write_half)))
}
