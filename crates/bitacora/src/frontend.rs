//! Translation of block device requests to engine calls (contract section
//! 11). It does not depend on libublk, so it can be tested without root:
//! `ublk.rs` decodes the kernel's I/O descriptor into a [`Request`] and
//! hands it to [`execute`].

use std::fmt;

use crate::format::BLOCK_SIZE;
use crate::{Error, Volume};

/// Size of a sector in the block layer's addressing.
pub const SECTOR_SIZE: u64 = 512;

/// Sectors per 4096-byte engine block.
pub const SECTORS_PER_BLOCK: u64 = BLOCK_SIZE as u64 / SECTOR_SIZE;

/// Operation codes of `UBLK_IO_OP_*` in `linux/ublk_cmd.h`.
pub const OP_READ: u32 = 0;
pub const OP_WRITE: u32 = 1;
pub const OP_FLUSH: u32 = 2;
pub const OP_DISCARD: u32 = 3;
pub const OP_WRITE_ZEROES: u32 = 5;

/// `UBLK_IO_F_FUA` in `linux/ublk_cmd.h`.
pub const FLAG_FUA: u32 = 1 << 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Flush,
    Discard,
    WriteZeroes,
    Other(u32),
}

impl Op {
    pub fn from_code(code: u32) -> Op {
        match code {
            OP_READ => Op::Read,
            OP_WRITE => Op::Write,
            OP_FLUSH => Op::Flush,
            OP_DISCARD => Op::Discard,
            OP_WRITE_ZEROES => Op::WriteZeroes,
            other => Op::Other(other),
        }
    }
}

/// A block request as the block layer describes it: 512-byte sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    pub sector: u64,
    pub sectors: u32,
    pub fua: bool,
}

impl Request {
    /// Decodes the `op_flags`, `start_sector` and `nr_sectors` of a
    /// `ublksrv_io_desc`: the low 8 bits of `op_flags` are the operation.
    pub fn from_ublk(op_flags: u32, start_sector: u64, nr_sectors: u32) -> Request {
        Request {
            op: Op::from_code(op_flags & 0xff),
            sector: start_sector,
            sectors: nr_sectors,
            fua: op_flags & FLAG_FUA != 0,
        }
    }
}

/// The engine call a request becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Read { lba: u64, nblocks: u64 },
    Write { lba: u64, nblocks: u64, fua: bool },
    Flush,
}

impl Command {
    /// Bytes the command moves through the request buffer.
    pub fn bytes(&self) -> usize {
        match *self {
            Command::Read { nblocks, .. } | Command::Write { nblocks, .. } => {
                nblocks as usize * BLOCK_SIZE
            }
            Command::Flush => 0,
        }
    }
}

#[derive(Debug)]
pub enum FrontendError {
    /// Start or length is not a multiple of 4096 bytes, or the length is 0.
    Unaligned { sector: u64, sectors: u32 },
    /// The request ends past the end of the volume.
    OutOfRange { sector: u64, sectors: u32 },
    /// The operation is not advertised (`DISCARD`, `WRITE_ZEROES`, ...).
    Unsupported(Op),
    /// The request buffer is smaller than the request.
    BufferTooSmall { needed: usize, len: usize },
    /// The engine failed the call.
    Engine(Error),
}

impl FrontendError {
    /// Positive errno for the completion of the request.
    pub fn errno(&self) -> i32 {
        match self {
            FrontendError::Unaligned { .. }
            | FrontendError::OutOfRange { .. }
            | FrontendError::BufferTooSmall { .. } => libc::EINVAL,
            FrontendError::Unsupported(_) => libc::EOPNOTSUPP,
            FrontendError::Engine(Error::NoSpace) => libc::ENOSPC,
            FrontendError::Engine(_) => libc::EIO,
        }
    }
}

impl fmt::Display for FrontendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrontendError::Unaligned { sector, sectors } => {
                write!(f, "unaligned request: sector {sector}, {sectors} sectors")
            }
            FrontendError::OutOfRange { sector, sectors } => {
                write!(
                    f,
                    "request out of range: sector {sector}, {sectors} sectors"
                )
            }
            FrontendError::Unsupported(op) => write!(f, "unsupported operation: {op:?}"),
            FrontendError::BufferTooSmall { needed, len } => {
                write!(f, "request buffer of {len} bytes, {needed} needed")
            }
            FrontendError::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl From<Error> for FrontendError {
    fn from(e: Error) -> Self {
        FrontendError::Engine(e)
    }
}

/// Translates `req` for a volume of `volume_blocks` blocks.
pub fn translate(req: &Request, volume_blocks: u64) -> Result<Command, FrontendError> {
    let (sector, sectors) = (req.sector, req.sectors);
    let write = match req.op {
        Op::Flush => return Ok(Command::Flush),
        Op::Read => false,
        Op::Write => true,
        op => return Err(FrontendError::Unsupported(op)),
    };
    let count = u64::from(sectors);
    if count == 0 || sector % SECTORS_PER_BLOCK != 0 || count % SECTORS_PER_BLOCK != 0 {
        return Err(FrontendError::Unaligned { sector, sectors });
    }
    let lba = sector / SECTORS_PER_BLOCK;
    let nblocks = count / SECTORS_PER_BLOCK;
    if lba
        .checked_add(nblocks)
        .is_none_or(|end| end > volume_blocks)
    {
        return Err(FrontendError::OutOfRange { sector, sectors });
    }
    Ok(if write {
        Command::Write {
            lba,
            nblocks,
            fua: req.fua,
        }
    } else {
        Command::Read { lba, nblocks }
    })
}

/// Runs `req` against `vol`. A read fills the start of `buf`; a write takes
/// its data from there. Returns the bytes transferred (0 for a flush).
pub fn execute(vol: &Volume, req: &Request, buf: &mut [u8]) -> Result<usize, FrontendError> {
    let cmd = translate(req, vol.blocks())?;
    let bytes = cmd.bytes();
    if buf.len() < bytes {
        return Err(FrontendError::BufferTooSmall {
            needed: bytes,
            len: buf.len(),
        });
    }
    match cmd {
        Command::Read { lba, nblocks } => {
            let data = vol.read(lba, nblocks)?;
            buf[..bytes].copy_from_slice(&data);
        }
        Command::Write {
            lba, fua: false, ..
        } => vol.write(lba, &buf[..bytes])?,
        Command::Write { lba, fua: true, .. } => vol.write_fua(lba, &buf[..bytes])?,
        Command::Flush => vol.flush()?,
    }
    Ok(bytes)
}
