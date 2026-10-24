//! Engine error type.

use std::fmt;
use std::io;

/// Every fallible engine operation returns this error.
#[derive(Debug)]
pub enum Error {
    /// The medium reported an I/O error.
    Io(io::Error),
    /// A data block failed its `data_crc` check; no data is returned.
    Integrity { lba: u64 },
    /// On-disk metadata is invalid or a loss was detected.
    Corrupt(String),
    /// The volume is in the fail-stop state.
    Failed,
    /// No free segment is left and the GC cannot free one.
    NoSpace,
    /// The caller passed an invalid argument.
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Integrity { lba } => write!(f, "integrity error at lba {lba}"),
            Error::Corrupt(msg) => write!(f, "corrupt volume: {msg}"),
            Error::Failed => write!(f, "volume failed"),
            Error::NoSpace => write!(f, "no space left on volume"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
