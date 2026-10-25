//! Frames of the Unix socket protocol (contract section 8).
//!
//! Every frame is a little-endian `u32` body length followed by the body.
//!
//! | Request body | Response body |
//! | --- | --- |
//! | `op` u8 (0), `req_id` u64 (1), `epoch` u64 (9), `lba` u64 (17), `nblocks` u32 (25), payload (29) | `req_id` u64 (0), `status` u8 (8), payload (9) |
//!
//! Payloads by op: `READ` returns `nblocks × 4096` bytes; `WRITE` and
//! `WRITE_FUA` carry them; `STATUS` returns `bitacora-status/1` JSON;
//! `SNAPSHOT_CREATE` returns the new id as u64; `SNAPSHOT_DELETE` carries the
//! id as u64; `EXPORT` carries the version as u64 (0 = live, otherwise the
//! snapshot id) followed by the UTF-8 output path. `MIGRATE` and the other
//! frames of live migration are described in `migrate.rs`. A response with
//! a status other than `OK` carries a UTF-8 message, except `MOVED`, whose
//! payload is the path of the destination socket.
//!
//! Decoding never panics: a truncated, oversized or unknown frame is a
//! `WireError`.

use std::fmt;
use std::io::{self, Read, Write};

/// Bytes of a request body before the payload.
pub const REQUEST_HEADER: usize = 29;
/// Bytes of a response body before the payload.
pub const RESPONSE_HEADER: usize = 9;
/// Largest body accepted: a 32 MiB payload plus the request header.
pub const MAX_BODY: usize = (32 << 20) + REQUEST_HEADER;

/// Operation of a request. Codes 16–31 belong to replication (section 9)
/// and 32–47 to live migration (section 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Op {
    Read = 1,
    Write = 2,
    WriteFua = 3,
    Flush = 4,
    Status = 5,
    SnapshotCreate = 6,
    SnapshotDelete = 7,
    Export = 8,
    /// Replication: `APPEND(epoch, prev_lsn, entries)`.
    Append = 16,
    /// Replication: primary heartbeat.
    Heartbeat = 17,
    /// Replication: first phase of an election.
    Prepare = 18,
    /// Replication: second phase of an election.
    Promote = 19,
    /// Replication: delta or full resynchronisation of a follower.
    Resync = 20,
    /// Migration: blocks with their `birth` from source to destination.
    MigrateData = 32,
    /// Migration: the source froze and sent the final difference.
    MigrateFreeze = 33,
    /// Migration: the destination adopts the new epoch and serves.
    MigrateCommit = 34,
    /// Migration: `bitacora migrate` asks the source to migrate to the
    /// socket in the payload.
    Migrate = 35,
}

impl Op {
    pub const ALL: [Op; 17] = [
        Op::Read,
        Op::Write,
        Op::WriteFua,
        Op::Flush,
        Op::Status,
        Op::SnapshotCreate,
        Op::SnapshotDelete,
        Op::Export,
        Op::Append,
        Op::Heartbeat,
        Op::Prepare,
        Op::Promote,
        Op::Resync,
        Op::MigrateData,
        Op::MigrateFreeze,
        Op::MigrateCommit,
        Op::Migrate,
    ];

    pub fn from_u8(code: u8) -> Option<Op> {
        Op::ALL.into_iter().find(|op| *op as u8 == code)
    }

    /// Whether the op belongs to replication or migration.
    pub fn is_reserved(self) -> bool {
        self as u8 >= 16
    }
}

/// Status of a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    IoError = 1,
    IntegrityError = 2,
    NoSpace = 3,
    Failed = 4,
    Fenced = 5,
    NotPrimary = 6,
    Moved = 7,
}

impl Status {
    pub const ALL: [Status; 8] = [
        Status::Ok,
        Status::IoError,
        Status::IntegrityError,
        Status::NoSpace,
        Status::Failed,
        Status::Fenced,
        Status::NotPrimary,
        Status::Moved,
    ];

    pub fn from_u8(code: u8) -> Option<Status> {
        Status::ALL.into_iter().find(|s| *s as u8 == code)
    }

    /// Name used by the contract.
    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::IoError => "IO_ERROR",
            Status::IntegrityError => "INTEGRITY_ERROR",
            Status::NoSpace => "NO_SPACE",
            Status::Failed => "FAILED",
            Status::Fenced => "FENCED",
            Status::NotPrimary => "NOT_PRIMARY",
            Status::Moved => "MOVED",
        }
    }
}

/// A frame that cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Fewer bytes than the header or the declared length.
    Truncated {
        needed: usize,
        got: usize,
    },
    /// More bytes than the declared length.
    Trailing {
        expected: usize,
        got: usize,
    },
    /// Declared body length above `MAX_BODY`.
    TooLarge(usize),
    UnknownOp(u8),
    UnknownStatus(u8),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated { needed, got } => {
                write!(f, "truncated frame: needed {needed} bytes, got {got}")
            }
            WireError::Trailing { expected, got } => {
                write!(f, "frame has {got} bytes, its length says {expected}")
            }
            WireError::TooLarge(n) => write!(f, "frame body of {n} bytes exceeds {MAX_BODY}"),
            WireError::UnknownOp(c) => write!(f, "unknown op {c}"),
            WireError::UnknownStatus(c) => write!(f, "unknown status {c}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<WireError> for io::Error {
    fn from(e: WireError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    pub req_id: u64,
    /// Epoch known to the sender; 0 means none.
    pub epoch: u64,
    pub lba: u64,
    pub nblocks: u32,
    pub payload: Vec<u8>,
}

impl Request {
    pub fn new(op: Op, req_id: u64) -> Request {
        Request {
            op,
            req_id,
            epoch: 0,
            lba: 0,
            nblocks: 0,
            payload: Vec::new(),
        }
    }

    /// The whole frame, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = frame_start(REQUEST_HEADER + self.payload.len());
        b.push(self.op as u8);
        b.extend_from_slice(&self.req_id.to_le_bytes());
        b.extend_from_slice(&self.epoch.to_le_bytes());
        b.extend_from_slice(&self.lba.to_le_bytes());
        b.extend_from_slice(&self.nblocks.to_le_bytes());
        b.extend_from_slice(&self.payload);
        b
    }

    /// Decodes a body (without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Request, WireError> {
        check_body(body, REQUEST_HEADER)?;
        let op = Op::from_u8(body[0]).ok_or(WireError::UnknownOp(body[0]))?;
        Ok(Request {
            op,
            req_id: get_u64(body, 1),
            epoch: get_u64(body, 9),
            lba: get_u64(body, 17),
            nblocks: get_u32(body, 25),
            payload: body[REQUEST_HEADER..].to_vec(),
        })
    }

    /// Decodes a whole frame, which must hold exactly its declared length.
    pub fn decode_frame(frame: &[u8]) -> Result<Request, WireError> {
        Request::decode(split_frame(frame)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub req_id: u64,
    pub status: Status,
    pub payload: Vec<u8>,
}

impl Response {
    pub fn ok(req_id: u64, payload: Vec<u8>) -> Response {
        Response {
            req_id,
            status: Status::Ok,
            payload,
        }
    }

    pub fn error(req_id: u64, status: Status, message: &str) -> Response {
        Response {
            req_id,
            status,
            payload: message.as_bytes().to_vec(),
        }
    }

    /// The whole frame, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = frame_start(RESPONSE_HEADER + self.payload.len());
        b.extend_from_slice(&self.req_id.to_le_bytes());
        b.push(self.status as u8);
        b.extend_from_slice(&self.payload);
        b
    }

    /// Decodes a body (without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Response, WireError> {
        check_body(body, RESPONSE_HEADER)?;
        let status = Status::from_u8(body[8]).ok_or(WireError::UnknownStatus(body[8]))?;
        Ok(Response {
            req_id: get_u64(body, 0),
            status,
            payload: body[RESPONSE_HEADER..].to_vec(),
        })
    }

    /// Decodes a whole frame, which must hold exactly its declared length.
    pub fn decode_frame(frame: &[u8]) -> Result<Response, WireError> {
        Response::decode(split_frame(frame)?)
    }

    /// The payload as text (error message or `MOVED` destination).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }
}

/// Reads one frame body. `Ok(None)` on end of stream before the first byte
/// of the length; a stream that ends inside a frame is `UnexpectedEof`.
pub fn read_body<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_BODY {
        return Err(WireError::TooLarge(len).into());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

pub fn read_request<R: Read>(r: &mut R) -> io::Result<Option<Request>> {
    match read_body(r)? {
        Some(body) => Ok(Some(Request::decode(&body)?)),
        None => Ok(None),
    }
}

pub fn read_response<R: Read>(r: &mut R) -> io::Result<Option<Response>> {
    match read_body(r)? {
        Some(body) => Ok(Some(Response::decode(&body)?)),
        None => Ok(None),
    }
}

pub fn write_request<W: Write>(w: &mut W, req: &Request) -> io::Result<()> {
    w.write_all(&req.encode())?;
    w.flush()
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> io::Result<()> {
    w.write_all(&resp.encode())?;
    w.flush()
}

fn frame_start(body_len: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + body_len);
    b.extend_from_slice(&(body_len as u32).to_le_bytes());
    b
}

fn check_body(body: &[u8], header: usize) -> Result<(), WireError> {
    if body.len() > MAX_BODY {
        return Err(WireError::TooLarge(body.len()));
    }
    if body.len() < header {
        return Err(WireError::Truncated {
            needed: header,
            got: body.len(),
        });
    }
    Ok(())
}

fn split_frame(frame: &[u8]) -> Result<&[u8], WireError> {
    let Some((len, body)) = frame.split_first_chunk::<4>() else {
        return Err(WireError::Truncated {
            needed: 4,
            got: frame.len(),
        });
    };
    let len = u32::from_le_bytes(*len) as usize;
    if len > MAX_BODY {
        return Err(WireError::TooLarge(len));
    }
    if body.len() < len {
        return Err(WireError::Truncated {
            needed: 4 + len,
            got: frame.len(),
        });
    }
    if body.len() > len {
        return Err(WireError::Trailing {
            expected: 4 + len,
            got: frame.len(),
        });
    }
    Ok(body)
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
