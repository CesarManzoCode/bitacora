//! bitacora: a log-structured block storage engine.
//!
//! Modules follow section 2 of `vault/CONTRACT.md`; the remaining ones arrive
//! in their own sprints.

pub mod backup;
pub mod capacity;
pub mod client;
pub mod error;
pub mod format;
pub mod frontend;
pub mod gc;
pub mod index;
pub mod medium;
pub mod migrate;
pub mod replica;
pub mod server;
pub mod snapshot;
pub mod socket;
pub mod status;
#[cfg(feature = "ublk")]
pub mod ublk;
pub mod volume;
pub mod wire;

pub use backup::BackupHeader;
pub use capacity::{History, Projection};
pub use client::{Client, ClientError};
pub use error::{Error, Result};
pub use gc::GcStatus;
pub use replica::{Claim, Elected, Replica, ReplicaConfig};
pub use server::{Server, ServerHandle};
pub use snapshot::{SnapshotInfo, Version};
pub use status::StatusReport;
pub use volume::{GcMode, Pin, SegmentUsage, Volume, VolumeConfig, VolumeState, VolumeStatus};
