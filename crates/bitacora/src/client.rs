//! Client of the Unix socket protocol (contract section 8).
//!
//! Requests go one at a time over a single connection. A `MOVED` answer
//! means the request was not executed: the client reconnects to the socket
//! in its payload and sends the same request again.

use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use crate::format::BLOCK_SIZE;
use crate::snapshot::Version;
use crate::status::StatusReport;
use crate::wire::{self, Op, Request, Response, Status};

/// `MOVED` answers followed for one request before giving up.
pub const MAX_REDIRECTS: usize = 8;

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

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    /// Socket currently connected; changes after a `MOVED`.
    path: PathBuf,
    next_req: u64,
    /// Epoch sent with every request; 0 means none.
    epoch: u64,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> ClientResult<Client> {
        let path = path.as_ref().to_path_buf();
        let (reader, writer) = open(&path)?;
        Ok(Client {
            reader,
            writer,
            path,
            next_req: 1,
            epoch: 0,
        })
    }

    /// Socket currently connected.
    pub fn path(&self) -> &Path {
        &self.path
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
    /// following `MOVED`.
    fn call(&mut self, op: Op, lba: u64, nblocks: u32, payload: Vec<u8>) -> ClientResult<Vec<u8>> {
        let mut req = Request {
            op,
            req_id: 0,
            epoch: self.epoch,
            lba,
            nblocks,
            payload,
        };
        for _ in 0..=MAX_REDIRECTS {
            req.req_id = self.next_req;
            self.next_req += 1;
            let resp = self.round_trip(&req)?;
            match resp.status {
                Status::Ok => return Ok(resp.payload),
                Status::Moved => self.reconnect(PathBuf::from(resp.text()))?,
                status => {
                    return Err(ClientError::Remote {
                        status,
                        message: resp.text(),
                    });
                }
            }
        }
        Err(ClientError::TooManyRedirects)
    }

    fn round_trip(&mut self, req: &Request) -> ClientResult<Response> {
        wire::write_request(&mut self.writer, req)?;
        let resp = wire::read_response(&mut self.reader)?.ok_or_else(|| {
            ClientError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed the connection",
            ))
        })?;
        if resp.req_id != req.req_id {
            return Err(ClientError::Protocol(format!(
                "answer for request {} while waiting for {}",
                resp.req_id, req.req_id
            )));
        }
        Ok(resp)
    }

    fn reconnect(&mut self, path: PathBuf) -> ClientResult<()> {
        let (reader, writer) = open(&path)?;
        self.reader = reader;
        self.writer = writer;
        self.path = path;
        Ok(())
    }
}

fn open(path: &Path) -> io::Result<(BufReader<UnixStream>, BufWriter<UnixStream>)> {
    let stream = UnixStream::connect(path)?;
    let write_half = stream.try_clone()?;
    Ok((BufReader::new(stream), BufWriter::new(write_half)))
}
