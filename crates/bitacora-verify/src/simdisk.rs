//! Simulated disk (contract section 15, «Disco simulado y generador por
//! semilla»).
//!
//! `SimDisk` keeps a durable image and the list of writes not yet covered by
//! a `sync`. Reads see every completed write, like a page cache. `crash`
//! builds the disk that a power cut would leave: each unsynced write is kept
//! or dropped independently, and one of them may be cut to a multiple of 512
//! bytes. A failed `sync` may drop unsynced writes for good, as in fsyncgate:
//! reads still see them, but they never become durable.

use std::io;
use std::sync::{Mutex, MutexGuard};

use bitacora::Error;
use bitacora::medium::Medium;

use crate::rng::Rng;

/// Granularity of a torn write.
pub const SECTOR: usize = 512;

pub struct SimDisk {
    state: Mutex<State>,
}

struct State {
    /// What survives a crash for sure.
    durable: Vec<u8>,
    /// What reads see: `durable` plus every pending write.
    image: Vec<u8>,
    /// Writes and punched holes since the last successful `sync`, in order.
    pending: Vec<Pending>,
    writes: u64,
    syncs: u64,
    fail_write_at: Option<u64>,
    fail_sync_at: Option<u64>,
    /// Decides which pending writes a failed `sync` drops.
    rng: Rng,
}

struct Pending {
    offset: u64,
    data: Vec<u8>,
}

fn apply(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = offset as usize;
    let end = start + data.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[start..end].copy_from_slice(data);
}

fn injected(what: &str) -> Error {
    Error::Io(io::Error::other(format!("simulated {what} failure")))
}

impl SimDisk {
    /// An empty disk; `seed` drives what a failed `sync` drops.
    pub fn new(seed: u64) -> Self {
        SimDisk::from_image(Vec::new(), seed)
    }

    /// A disk whose durable content is `image`.
    pub fn from_image(image: Vec<u8>, seed: u64) -> Self {
        SimDisk {
            state: Mutex::new(State {
                durable: image.clone(),
                image,
                pending: Vec::new(),
                writes: 0,
                syncs: 0,
                fail_write_at: None,
                fail_sync_at: None,
                rng: Rng::new(seed),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `n`-th call to `write_at` (from 1) fails and writes nothing.
    pub fn fail_write_at(&self, n: u64) {
        self.lock().fail_write_at = Some(n);
    }

    /// The `n`-th call to `sync` (from 1) fails.
    pub fn fail_sync_at(&self, n: u64) {
        self.lock().fail_sync_at = Some(n);
    }

    /// Calls to `write_at` so far, failed ones included.
    pub fn writes(&self) -> u64 {
        self.lock().writes
    }

    /// Calls to `sync` so far, failed ones included.
    pub fn syncs(&self) -> u64 {
        self.lock().syncs
    }

    /// Writes not yet covered by a successful `sync`.
    pub fn pending_writes(&self) -> usize {
        self.lock().pending.len()
    }

    /// What reads see now.
    pub fn image(&self) -> Vec<u8> {
        self.lock().image.clone()
    }

    /// What survives any crash.
    pub fn durable_image(&self) -> Vec<u8> {
        self.lock().durable.clone()
    }

    /// The disk left by a power cut now. Under `seed`, each pending write is
    /// kept or dropped independently, and one kept write may be torn to a
    /// prefix that is a multiple of `SECTOR` bytes. The result has no pending
    /// writes and no injected failures.
    pub fn crash(&self, seed: u64) -> SimDisk {
        let st = self.lock();
        let mut rng = Rng::new(seed);
        let torn = if !st.pending.is_empty() && rng.chance(1, 2) {
            Some(rng.below(st.pending.len() as u64) as usize)
        } else {
            None
        };
        let mut image = st.durable.clone();
        for (i, w) in st.pending.iter().enumerate() {
            if !rng.chance(1, 2) {
                continue;
            }
            let len = if torn == Some(i) {
                let sectors = (w.data.len() / SECTOR) as u64;
                rng.below(sectors + 1) as usize * SECTOR
            } else {
                w.data.len()
            };
            if len > 0 {
                apply(&mut image, w.offset, &w.data[..len]);
            }
        }
        SimDisk::from_image(image, seed)
    }
}

impl Medium for SimDisk {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> bitacora::Result<usize> {
        let st = self.lock();
        let len = st.image.len() as u64;
        if offset >= len {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(st.image.len() - start);
        buf[..n].copy_from_slice(&st.image[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> bitacora::Result<()> {
        let mut st = self.lock();
        st.writes += 1;
        if st.fail_write_at == Some(st.writes) {
            return Err(injected("write"));
        }
        apply(&mut st.image, offset, buf);
        st.pending.push(Pending {
            offset,
            data: buf.to_vec(),
        });
        Ok(())
    }

    fn sync(&self) -> bitacora::Result<()> {
        let mut guard = self.lock();
        let st = &mut *guard;
        st.syncs += 1;
        if st.fail_sync_at == Some(st.syncs) {
            let rng = &mut st.rng;
            st.pending.retain(|_| rng.chance(1, 2));
            return Err(injected("sync"));
        }
        for w in st.pending.drain(..) {
            apply(&mut st.durable, w.offset, &w.data);
        }
        Ok(())
    }

    fn len(&self) -> bitacora::Result<u64> {
        Ok(self.lock().image.len() as u64)
    }

    fn punch_hole(&self, offset: u64, len: u64) -> bitacora::Result<()> {
        let mut st = self.lock();
        let size = st.image.len() as u64;
        let end = offset.saturating_add(len).min(size);
        if offset >= end {
            return Ok(());
        }
        let zeros = vec![0u8; (end - offset) as usize];
        apply(&mut st.image, offset, &zeros);
        st.pending.push(Pending {
            offset,
            data: zeros,
        });
        Ok(())
    }
}
