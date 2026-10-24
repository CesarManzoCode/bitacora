//! bitacora: a log-structured block storage engine.
//!
//! Modules follow section 2 of `vault/CONTRACT.md`; the remaining ones arrive
//! in their own sprints.

pub mod error;
pub mod format;
pub mod index;
pub mod medium;
pub mod volume;

pub use error::{Error, Result};
pub use volume::{GcMode, Volume, VolumeConfig, VolumeState, VolumeStatus};
