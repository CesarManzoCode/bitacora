//! Full and incremental online backups and restore (`bitacora-backup/1`,
//! contract section 7).
//!
//! File layout, every integer little-endian:
//!
//! | Offset | Field |
//! | --- | --- |
//! | 0 | `magic = "BITACBK1"` |
//! | 8 | `version` u32 (= 1) |
//! | 12 | `volume_id` (16 bytes) |
//! | 28 | `volume_size` u64 |
//! | 36 | `base_lsn` u64 (0 = full) |
//! | 44 | `target_lsn` u64 |
//! | 52 | `count` u64 |
//! | 60 | `count` entries in strictly increasing LBA order: `lba` u64, `crc32c` u32, `pad` u32 (= 0), 4096 data bytes |
//! | 60 + 4112 × `count` | footer: `crc32c` u32 of the header and every entry |
//!
//! The entry `crc32c` covers `lba ‖ data`, so a block moved to another LBA
//! also fails. A full backup holds every written block of the target
//! snapshot; an incremental one, the LBAs whose `birth` differs between the
//! base and the target snapshot, found by comparing index pages: a page the
//! two versions share by pointer is skipped without reading it.
//!
//! Online: a backup only reads its snapshots, so the volume keeps taking
//! writes from other threads while it runs. The lock is held just to clone
//! the frozen indexes (a vector of shared pages); the blocks are read with
//! `read_snapshot`, which goes to the medium outside the lock.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{BLOCK_SIZE, BLOCK_SIZE_U64, VolumeId};
use crate::index::{Index, PAGE_ENTRIES};
use crate::medium::Medium;
use crate::volume::{Volume, VolumeConfig};

pub const BACKUP_MAGIC: [u8; 8] = *b"BITACBK1";
pub const BACKUP_VERSION: u32 = 1;
/// Bytes of the header.
pub const HEADER_LEN: usize = 60;
/// Bytes of one entry: `lba`, `crc32c`, `pad` and the block.
pub const ENTRY_LEN: usize = 16 + BLOCK_SIZE;
/// Bytes of the footer.
pub const FOOTER_LEN: usize = 4;

/// Most blocks read from a snapshot in one call.
const READ_RUN: u64 = 256;

/// Header of a backup file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupHeader {
    pub volume_id: VolumeId,
    pub volume_size: u64,
    /// LSN of the base version; 0 for a full backup.
    pub base_lsn: u64,
    /// LSN of the target version.
    pub target_lsn: u64,
    /// Number of entries.
    pub count: u64,
}

impl BackupHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..8].copy_from_slice(&BACKUP_MAGIC);
        buf[8..12].copy_from_slice(&BACKUP_VERSION.to_le_bytes());
        buf[12..28].copy_from_slice(&self.volume_id);
        buf[28..36].copy_from_slice(&self.volume_size.to_le_bytes());
        buf[36..44].copy_from_slice(&self.base_lsn.to_le_bytes());
        buf[44..52].copy_from_slice(&self.target_lsn.to_le_bytes());
        buf[52..60].copy_from_slice(&self.count.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; HEADER_LEN]) -> Result<Self> {
        if buf[0..8] != BACKUP_MAGIC {
            return Err(Error::Corrupt("backup: bad magic".into()));
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != BACKUP_VERSION {
            return Err(Error::Corrupt(format!("backup: unknown version {version}")));
        }
        let u64_at = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        Ok(BackupHeader {
            volume_id: buf[12..28].try_into().unwrap(),
            volume_size: u64_at(28),
            base_lsn: u64_at(36),
            target_lsn: u64_at(44),
            count: u64_at(52),
        })
    }

    /// Whether this is a full backup.
    pub fn is_full(&self) -> bool {
        self.base_lsn == 0
    }

    /// Size in bytes of the whole file this header describes.
    pub fn file_len(&self) -> Option<u64> {
        self.count
            .checked_mul(ENTRY_LEN as u64)?
            .checked_add((HEADER_LEN + FOOTER_LEN) as u64)
    }
}

/// `crc32c(lba ‖ data)` of one entry.
pub fn entry_crc(lba: u64, data: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&lba.to_le_bytes()), data)
}

/// LBAs whose `birth` differs between `base` and `target`, in order. Pages
/// shared by pointer, or never written in either, are skipped.
pub fn changed_lbas(base: &Index, target: &Index) -> Vec<u64> {
    let mut out = Vec::new();
    for (pi, (b, t)) in base.pages().iter().zip(target.pages()).enumerate() {
        let first = (pi * PAGE_ENTRIES) as u64;
        match (b, t) {
            (None, None) => {}
            (Some(b), Some(t)) if Arc::ptr_eq(b, t) => {}
            (Some(b), Some(t)) => out.extend(
                (0..PAGE_ENTRIES)
                    .filter(|&s| b.get(s) != t.get(s))
                    .map(|s| first + s as u64),
            ),
            (Some(p), None) | (None, Some(p)) => out.extend(
                (0..PAGE_ENTRIES)
                    .filter(|&s| p.get(s) != 0)
                    .map(|s| first + s as u64),
            ),
        }
    }
    out
}

impl Volume {
    /// Writes a full backup of snapshot `snapshot` to a new file at `out`.
    pub fn backup_full(&self, snapshot: u64, out: &Path) -> Result<BackupHeader> {
        let (lsn, index) = self.frozen(snapshot)?;
        let lbas: Vec<u64> = index.written().map(|(lba, _)| lba).collect();
        self.write_backup(snapshot, 0, lsn, &lbas, out)
    }

    /// Writes to `out` an incremental backup from snapshot `base` to snapshot
    /// `target`: the LBAs whose `birth` differs between them.
    pub fn backup_incremental(&self, base: u64, target: u64, out: &Path) -> Result<BackupHeader> {
        let (base_lsn, base_index) = self.frozen(base)?;
        let (target_lsn, target_index) = self.frozen(target)?;
        if base_lsn > target_lsn {
            return Err(Error::InvalidArgument(format!(
                "base snapshot {base} (lsn {base_lsn}) is newer than target {target} (lsn {target_lsn})"
            )));
        }
        // A base at LSN 0 (an empty volume) would read as a full backup.
        if base_lsn == 0 {
            return Err(Error::InvalidArgument(format!(
                "base snapshot {base} is empty; take a full backup instead"
            )));
        }
        let lbas = changed_lbas(&base_index, &target_index);
        self.write_backup(target, base_lsn, target_lsn, &lbas, out)
    }

    /// LSN and a clone of the index of snapshot `id`; the clone shares every
    /// page, so the lock is held only for the copy of the page vector.
    fn frozen(&self, id: u64) -> Result<(u64, Index)> {
        let inner = self.core.lock()?;
        inner.usable()?;
        let f = inner
            .snapshots
            .get(&id)
            .ok_or_else(|| Error::InvalidArgument(format!("no snapshot {id}")))?;
        Ok((f.lsn, f.index.clone()))
    }

    fn write_backup(
        &self,
        snapshot: u64,
        base_lsn: u64,
        target_lsn: u64,
        lbas: &[u64],
        out: &Path,
    ) -> Result<BackupHeader> {
        let header = BackupHeader {
            volume_id: self.volume_id(),
            volume_size: self.volume_size(),
            base_lsn,
            target_lsn,
            count: lbas.len() as u64,
        };
        let file = File::create(out)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let head = header.encode();
        w.write_all(&head)?;
        let mut crc = crc32c::crc32c(&head);
        let mut start = 0;
        while start < lbas.len() {
            // A run of consecutive LBAs is read with one call.
            let mut end = start + 1;
            while end < lbas.len()
                && lbas[end] == lbas[end - 1] + 1
                && ((end - start) as u64) < READ_RUN
            {
                end += 1;
            }
            let data = self.read_snapshot(snapshot, lbas[start], (end - start) as u64)?;
            for (k, block) in data.chunks_exact(BLOCK_SIZE).enumerate() {
                let lba = lbas[start + k];
                let mut meta = [0u8; 16];
                meta[0..8].copy_from_slice(&lba.to_le_bytes());
                meta[8..12].copy_from_slice(&entry_crc(lba, block).to_le_bytes());
                w.write_all(&meta)?;
                w.write_all(block)?;
                crc = crc32c::crc32c_append(crc32c::crc32c_append(crc, &meta), block);
            }
            start = end;
        }
        w.write_all(&crc.to_le_bytes())?;
        let file = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        Ok(header)
    }
}

/// Reads and checks a whole backup file: length, magic, version, LBA order
/// and range, every entry CRC and the footer. Returns its header.
pub fn verify(path: &Path) -> Result<BackupHeader> {
    for_each_entry(path, |_, _| Ok(()))
}

/// Calls `f` for every entry of the backup at `path` after checking it, then
/// checks the footer. A caller that writes in `f` must have run `verify`
/// first, since a bad footer is found only at the end.
fn for_each_entry(
    path: &Path,
    mut f: impl FnMut(u64, &[u8]) -> Result<()>,
) -> Result<BackupHeader> {
    let bad = |msg: String| Error::Corrupt(format!("backup {}: {msg}", path.display()));
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut head = [0u8; HEADER_LEN];
    r.read_exact(&mut head).map_err(|_| bad("truncated header".into()))?;
    let header = BackupHeader::decode(&head)?;
    if header.volume_size == 0 || !header.volume_size.is_multiple_of(BLOCK_SIZE_U64) {
        return Err(bad(format!("invalid volume_size {}", header.volume_size)));
    }
    if header.target_lsn < header.base_lsn {
        return Err(bad("target_lsn is before base_lsn".into()));
    }
    if header.file_len() != Some(len) {
        return Err(bad(format!("length {len} does not match {} entries", header.count)));
    }
    let blocks = header.volume_size / BLOCK_SIZE_U64;
    let mut crc = crc32c::crc32c(&head);
    let mut meta = [0u8; 16];
    let mut data = vec![0u8; BLOCK_SIZE];
    let mut prev = None;
    for i in 0..header.count {
        r.read_exact(&mut meta)?;
        r.read_exact(&mut data)?;
        crc = crc32c::crc32c_append(crc32c::crc32c_append(crc, &meta), &data);
        let lba = u64::from_le_bytes(meta[0..8].try_into().unwrap());
        let entry = u32::from_le_bytes(meta[8..12].try_into().unwrap());
        if lba >= blocks || prev.is_some_and(|p| lba <= p) {
            return Err(bad(format!("entry {i}: lba {lba} out of order or range")));
        }
        if meta[12..16] != [0; 4] || entry_crc(lba, &data) != entry {
            return Err(bad(format!("entry {i} (lba {lba}) fails its crc32c")));
        }
        prev = Some(lba);
        f(lba, &data)?;
    }
    let mut foot = [0u8; FOOTER_LEN];
    r.read_exact(&mut foot)?;
    if u32::from_le_bytes(foot) != crc {
        return Err(bad("footer crc32c does not match".into()));
    }
    Ok(header)
}

/// Restores `backups` (a full one followed by incrementals, in order) into a
/// new volume created on `medium` with `config`, whose `volume_size` must
/// match. Every file is checked before anything is written: CRCs, the same
/// `volume_id` and `volume_size`, and the chain (each incremental's
/// `base_lsn` is the previous `target_lsn`). The volume is flushed before it
/// is returned.
pub fn restore<P: AsRef<Path>>(
    backups: &[P],
    medium: Arc<dyn Medium>,
    config: VolumeConfig,
) -> Result<Volume> {
    let Some(first) = backups.first() else {
        return Err(Error::InvalidArgument("restore needs at least one backup".into()));
    };
    let head = verify(first.as_ref())?;
    if !head.is_full() {
        return Err(Error::InvalidArgument(format!(
            "{} is not a full backup",
            first.as_ref().display()
        )));
    }
    let mut prev = head;
    for path in &backups[1..] {
        let path = path.as_ref();
        let h = verify(path)?;
        if h.volume_id != head.volume_id || h.volume_size != head.volume_size {
            return Err(Error::InvalidArgument(format!(
                "{} belongs to another volume",
                path.display()
            )));
        }
        if h.is_full() || h.base_lsn != prev.target_lsn {
            return Err(Error::InvalidArgument(format!(
                "{} starts at lsn {}, expected an incremental from lsn {}",
                path.display(),
                h.base_lsn,
                prev.target_lsn
            )));
        }
        prev = h;
    }
    if config.volume_size != head.volume_size {
        return Err(Error::InvalidArgument(format!(
            "the backups are of a {} byte volume, the config says {}",
            head.volume_size, config.volume_size
        )));
    }

    let vol = Volume::create(medium, config)?;
    for path in backups {
        for_each_entry(path.as_ref(), |lba, data| vol.write(lba, data))?;
    }
    vol.flush()?;
    Ok(vol)
}
