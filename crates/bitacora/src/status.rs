//! `bitacora-status/1`: the status JSON of contract section 8, validated by
//! `schemas/status.schema.json`. The structs carry exactly the keys the
//! schema requires and reject any other on decoding.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::volume::{GcMode, Volume, VolumeState};

pub const FORMAT: &str = "bitacora-status/1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub format: String,
    pub volume: VolumeReport,
    pub replicas: Vec<ReplicaReport>,
    pub gc: GcReport,
    pub space: SpaceReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeReport {
    /// The 16-byte `volume_id` in lowercase hexadecimal.
    pub volume_id: String,
    pub volume_size: u64,
    pub state: VolumeHealth,
    pub epoch: u64,
    pub last_lsn: u64,
    pub durable_lsn: u64,
    /// Snapshots not deleted.
    pub snapshots: u64,
}

/// Serving state of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeHealth {
    Ok,
    /// Fail-stop, or closed.
    Failed,
    /// A migration froze the source (section 10).
    Frozen,
    /// The volume now lives behind another socket.
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaReport {
    pub id: u64,
    pub role: Role,
    pub state: ReplicaState,
    pub epoch: u64,
    pub last_lsn: u64,
    /// Milliseconds since the last heartbeat answer; 0 for the reporting
    /// process itself.
    pub last_heartbeat_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Primary,
    Follower,
    Standalone,
}

/// Replica states of section 9; the names are the contract's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicaState {
    Activa,
    Atrasada,
    Caida,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcReport {
    pub mode: GcModeName,
    pub running: bool,
    pub segments_cleaned: u64,
    pub bytes_relocated: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GcModeName {
    Auto,
    Continuous,
    Off,
}

impl From<GcMode> for GcModeName {
    fn from(mode: GcMode) -> Self {
        match mode {
            GcMode::Auto => GcModeName::Auto,
            GcMode::Continuous => GcModeName::Continuous,
            GcMode::Off => GcModeName::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceReport {
    pub segment_size: u64,
    pub max_segments: u64,
    pub used_segments: u64,
    pub free_segments: u64,
    pub live_bytes: u64,
    /// Bytes the volume file occupies (`st_blocks × 512` for a file).
    pub physical_bytes: u64,
}

impl StatusReport {
    /// The status of a volume with the given replica list. `health`
    /// overrides the engine state when the server knows more (frozen,
    /// moved); `None` derives it from the volume.
    pub fn build(
        vol: &Volume,
        health: Option<VolumeHealth>,
        replicas: Vec<ReplicaReport>,
    ) -> Result<StatusReport> {
        let st = vol.status();
        let health = health.unwrap_or(match st.state {
            VolumeState::Open => VolumeHealth::Ok,
            VolumeState::Failed | VolumeState::Closed => VolumeHealth::Failed,
        });
        Ok(StatusReport {
            format: FORMAT.to_string(),
            volume: VolumeReport {
                volume_id: hex(&vol.volume_id()),
                volume_size: vol.volume_size(),
                state: health,
                epoch: vol.epoch(),
                last_lsn: st.lsn,
                durable_lsn: st.durable_lsn,
                snapshots: vol.snapshots().len() as u64,
            },
            replicas,
            gc: GcReport {
                mode: st.gc.mode.into(),
                running: st.gc.running,
                segments_cleaned: st.gc.segments_cleaned,
                bytes_relocated: st.gc.bytes_relocated,
            },
            space: SpaceReport {
                segment_size: vol.segment_size(),
                max_segments: st.max_segments,
                used_segments: st.segments_used,
                free_segments: st.segments_free,
                live_bytes: st.live_bytes,
                physical_bytes: vol.physical_bytes()?,
            },
        })
    }

    /// A volume without replicas: one `standalone` entry, `activa`.
    pub fn standalone(vol: &Volume, health: Option<VolumeHealth>) -> Result<StatusReport> {
        let st = vol.status();
        let me = ReplicaReport {
            id: 0,
            role: Role::Standalone,
            state: ReplicaState::Activa,
            epoch: vol.epoch(),
            last_lsn: st.lsn,
            last_heartbeat_ms: 0,
        };
        StatusReport::build(vol, health, vec![me])
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("status serialises")
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("status serialises")
    }

    pub fn from_json(text: &[u8]) -> serde_json::Result<StatusReport> {
        serde_json::from_slice(text)
    }
}

impl Volume {
    /// `current_epoch` of the superblock.
    pub fn epoch(&self) -> u64 {
        match self.core.inner.lock() {
            Ok(inner) => inner.sb.current_epoch,
            Err(poisoned) => poisoned.into_inner().sb.current_epoch,
        }
    }

    /// Bytes the medium occupies on its backing store.
    pub fn physical_bytes(&self) -> Result<u64> {
        self.core.medium.allocated_bytes()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
