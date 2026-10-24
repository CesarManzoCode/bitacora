//! Replication (contract section 9): three replicas on threads of this
//! process, with their volumes and sockets under `target/`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bitacora::format::{BLOCK_SIZE, EpochEntry};
use bitacora::replica::{self, Claim, ResyncMode};
use bitacora::status::{ReplicaState, Role};
use bitacora::wire::{Op, Request, Status};
use bitacora::{Client, GcMode, Replica, ReplicaConfig, Version, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
const VOLUME: u64 = 4 << 20;
const IDS: [u64; 3] = [1, 2, 3];

fn temp_dir(name: &str) -> PathBuf {
    // Short names: a Unix socket path has at most 107 bytes.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("repl")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn block(tag: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK_SIZE];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&(tag.wrapping_mul(0x9e37_79b9) ^ i as u64).to_le_bytes());
    }
    b
}

/// Polls `cond` every 10 ms for up to 20 s.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

struct Cluster {
    dir: PathBuf,
    replicas: HashMap<u64, Replica>,
}

impl Cluster {
    fn start(name: &str) -> Cluster {
        let dir = temp_dir(name);
        let mut cluster = Cluster {
            dir,
            replicas: HashMap::new(),
        };
        for id in IDS {
            cluster.start_one(id);
        }
        cluster
    }

    fn socket(&self, id: u64) -> PathBuf {
        self.dir.join(format!("r{id}.sock"))
    }

    fn sockets(&self) -> Vec<PathBuf> {
        IDS.iter().map(|id| self.socket(*id)).collect()
    }

    fn start_one(&mut self, id: u64) {
        let config = ReplicaConfig {
            id,
            volume: self.dir.join(format!("r{id}.img")),
            socket: self.socket(id),
            peers: IDS.iter().map(|p| (*p, self.socket(*p))).collect(),
            create: Some(VolumeConfig::new(VOLUME).with_segment_size(SEGMENT)),
            gc_mode: GcMode::Auto,
        };
        self.replicas.insert(id, Replica::start(config).unwrap());
    }

    fn stop_one(&mut self, id: u64) {
        self.replicas.remove(&id).unwrap().stop().unwrap();
    }

    fn elect(&self) -> bitacora::Elected {
        let elected = replica::elect(&self.sockets()).unwrap();
        assert!(self.replicas[&elected.id].is_primary());
        elected
    }

    fn get(&self, id: u64) -> &Replica {
        &self.replicas[&id]
    }

    /// Waits until every running follower is attached and holds the
    /// primary's last LSN.
    fn converged(&self, primary: u64) {
        eventually("followers to converge", || {
            let last = self.get(primary).volume().status().lsn;
            self.replicas
                .iter()
                .filter(|(id, _)| **id != primary)
                .all(|(_, r)| {
                    let claim = r.claim();
                    claim.attached && claim.last_lsn == last
                })
        });
    }

    fn stop(self) {
        for (_, r) in self.replicas {
            r.stop().unwrap();
        }
    }
}

#[test]
fn acknowledged_writes_are_on_two_volumes() {
    let cluster = Cluster::start("acked");
    let elected = cluster.elect();
    assert_eq!(elected.epoch, 1);

    let sockets = cluster.sockets();
    let writers: Vec<_> = (0..4u64)
        .map(|w| {
            let sockets = sockets.clone();
            thread::spawn(move || {
                let mut client = Client::connect_replicas(&sockets).unwrap();
                for i in 0..64u64 {
                    let lba = w * 100 + i;
                    client.write(lba, &block(lba)).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }

    // Every write was acknowledged, so it is on the primary and on at least
    // one follower already, without waiting.
    for w in 0..4u64 {
        for i in 0..64u64 {
            let lba = w * 100 + i;
            let holders = IDS
                .iter()
                .filter(|id| cluster.get(**id).volume().read(lba, 1).unwrap() == block(lba))
                .count();
            assert!(holders >= 2, "lba {lba} is on {holders} volumes");
        }
    }

    // A follower answers NOT_PRIMARY to clients; FLUSH on the primary is
    // immediate.
    let follower = IDS.iter().find(|id| **id != elected.id).unwrap();
    let mut direct = Client::connect(cluster.socket(*follower)).unwrap();
    let err = direct.write(0, &block(0)).unwrap_err();
    assert_eq!(err.status(), Some(Status::NotPrimary));
    let mut client = Client::connect_replicas(&cluster.sockets()).unwrap();
    client.flush().unwrap();

    // The primary's status lists itself and both followers.
    cluster.converged(elected.id);
    let report = Client::connect(&elected.socket).unwrap().status().unwrap();
    assert_eq!(report.replicas.len(), 3);
    assert_eq!(report.replicas[0].role, Role::Primary);
    assert_eq!(report.volume.epoch, 1);
    for r in &report.replicas[1..] {
        assert_eq!(r.role, Role::Follower);
        assert_ne!(r.state, ReplicaState::Caida);
    }
    cluster.stop();
}

#[test]
fn a_new_primary_has_every_acknowledged_write() {
    let mut cluster = Cluster::start("failover");
    let first = cluster.elect();

    let stop = Arc::new(AtomicBool::new(false));
    let sockets = cluster.sockets();
    let writer = {
        let stop = stop.clone();
        thread::spawn(move || {
            let mut client = Client::connect_replicas(&sockets).unwrap();
            client.set_failover_timeout(Duration::from_secs(20));
            let mut acked = Vec::new();
            let mut lba = 0u64;
            while !stop.load(Ordering::SeqCst) && lba < 1000 {
                // A write without `OK` is not acknowledged and not checked.
                if client.write(lba, &block(lba + 7)).is_ok() {
                    acked.push(lba);
                }
                lba += 1;
            }
            acked
        })
    };

    eventually("writes before the failover", || {
        cluster.get(first.id).volume().status().lsn >= 100
    });
    cluster.stop_one(first.id);
    let second = replica::elect(&cluster.sockets()).unwrap();
    assert_ne!(second.id, first.id);
    assert!(second.epoch > first.epoch);
    let primary = cluster.get(second.id);
    eventually("writes after the failover", || {
        primary.volume().status().lsn >= 300
    });
    stop.store(true, Ordering::SeqCst);
    let acked = writer.join().unwrap();
    assert!(acked.len() >= 300);

    let vol = primary.volume();
    for lba in &acked {
        assert_eq!(
            vol.read(*lba, 1).unwrap(),
            block(lba + 7),
            "acknowledged write at lba {lba} lost"
        );
    }
    // The history of the new primary starts its epoch after what it held.
    let history = vol.epoch_history();
    assert_eq!(history.last().unwrap().epoch, second.epoch);
    assert_eq!(
        history.last().unwrap().start_lsn,
        second.claim.last_lsn + 1
    );
    cluster.stop();
}

#[test]
fn an_append_of_an_old_epoch_is_fenced() {
    let cluster = Cluster::start("fenced");
    let first = cluster.elect();
    let mut client = Client::connect_replicas(&cluster.sockets()).unwrap();
    for lba in 0..32u64 {
        client.write(lba, &block(lba)).unwrap();
    }
    cluster.converged(first.id);

    // A second election: every replica accepts the new epoch, the old
    // primary included.
    let second = cluster.elect();
    assert_eq!(second.epoch, first.epoch + 1);
    cluster.converged(second.id);

    for id in IDS {
        let vol = cluster.get(id).volume();
        let before = vol.status().lsn;
        let old = vol.read(5, 1).unwrap();
        let mut payload = 5u64.to_le_bytes().to_vec();
        payload.extend_from_slice(&block(999));
        let req = Request {
            op: Op::Append,
            req_id: 0,
            epoch: first.epoch,
            lba: before,
            nblocks: 1,
            payload,
        };
        let mut conn = replica::PeerConn::open(&cluster.socket(id), Duration::from_secs(5)).unwrap();
        let resp = conn.call(req).unwrap();
        assert_eq!(resp.status, Status::Fenced, "replica {id}");
        let mut beat = Request::new(Op::Heartbeat, 0);
        beat.epoch = first.epoch;
        assert_eq!(conn.call(beat).unwrap().status, Status::Fenced);
        assert_eq!(vol.status().lsn, before, "replica {id} changed");
        assert_eq!(vol.read(5, 1).unwrap(), old, "replica {id} changed");
        assert_eq!(vol.epoch(), second.epoch);
    }

    // A PREPARE that is not newer is refused too.
    let mut prepare = Request::new(Op::Prepare, 0);
    prepare.epoch = second.epoch;
    let mut conn = replica::PeerConn::open(&second.socket, Duration::from_secs(5)).unwrap();
    assert_eq!(conn.call(prepare).unwrap().status, Status::Fenced);
    assert!(cluster.get(second.id).is_primary());
    cluster.stop();
}

#[test]
fn a_restarted_replica_converges_to_the_same_contents() {
    let mut cluster = Cluster::start("rejoin");
    let elected = cluster.elect();
    let mut client = Client::connect_replicas(&cluster.sockets()).unwrap();
    for lba in 0..200u64 {
        client.write(lba, &block(lba)).unwrap();
    }
    cluster.converged(elected.id);

    let follower = *IDS.iter().find(|id| **id != elected.id).unwrap();
    cluster.stop_one(follower);
    // The other follower keeps the majority; some blocks are overwritten.
    for lba in 100..400u64 {
        client.write(lba, &block(lba * 3)).unwrap();
    }
    let report = Client::connect(&elected.socket).unwrap().status().unwrap();
    let stopped = report.replicas.iter().find(|r| r.id == follower).unwrap();
    assert_ne!(stopped.state, ReplicaState::Activa);

    cluster.start_one(follower);
    cluster.converged(elected.id);
    for lba in 400..450u64 {
        client.write(lba, &block(lba)).unwrap();
    }
    cluster.converged(elected.id);

    let images: Vec<Vec<u8>> = IDS
        .iter()
        .map(|id| {
            let path = cluster.dir.join(format!("r{id}.export"));
            cluster.get(*id).volume().export(Version::Live, &path).unwrap();
            fs::read(&path).unwrap()
        })
        .collect();
    assert_eq!(images[0].len() as u64, VOLUME);
    assert!(images[0] == images[1] && images[1] == images[2]);
    let vol = cluster.get(follower).volume();
    assert_eq!(vol.read(150, 1).unwrap(), block(450));
    assert_eq!(vol.read(50, 1).unwrap(), block(50));
    cluster.stop();
}

fn claim(last_epoch: u64, last_lsn: u64) -> Claim {
    Claim {
        id: 2,
        last_epoch,
        last_lsn,
        current_epoch: last_epoch,
        volume_size: VOLUME,
        segment_size: SEGMENT,
        attached: false,
        needs_full: false,
    }
}

fn history(entries: &[(u64, u64)]) -> Vec<EpochEntry> {
    entries
        .iter()
        .map(|&(epoch, start_lsn)| EpochEntry { epoch, start_lsn })
        .collect()
}

#[test]
fn the_rejoin_rule_picks_delta_only_for_a_prefix() {
    // Epoch 1 covered LSNs 1..=10, epoch 3 starts at 11; the primary is at 20.
    let h = history(&[(1, 1), (3, 11)]);
    let geo = (VOLUME, SEGMENT);
    let plan = |c: Claim| replica::plan_resync(&h, 20, geo, &c);
    assert_eq!(plan(claim(0, 0)), ResyncMode::Delta);
    assert_eq!(plan(claim(1, 7)), ResyncMode::Delta);
    assert_eq!(plan(claim(1, 10)), ResyncMode::Delta);
    // LSNs 11.. of epoch 1 never reached the primary: divergent suffix.
    assert_eq!(plan(claim(1, 12)), ResyncMode::Full);
    assert_eq!(plan(claim(3, 15)), ResyncMode::Delta);
    assert_eq!(plan(claim(3, 21)), ResyncMode::Full);
    // An epoch the primary never saw.
    assert_eq!(plan(claim(2, 5)), ResyncMode::Full);
    let mut marked = claim(3, 15);
    marked.needs_full = true;
    assert_eq!(plan(marked), ResyncMode::Full);
    let mut other = claim(3, 15);
    other.volume_size = VOLUME * 2;
    assert_eq!(plan(other), ResyncMode::Full);

    assert_eq!(replica::epoch_at(&h, 0), 0);
    assert_eq!(replica::epoch_at(&h, 10), 1);
    assert_eq!(replica::epoch_at(&h, 11), 3);

    // Promoted at LSN 10 in epoch 4: the entry of epoch 3 covered nothing.
    let promoted = replica::promoted_history(h.clone(), 4, 10);
    assert_eq!(promoted, history(&[(1, 1), (4, 11)]));
    let long: Vec<EpochEntry> = (1..=64).map(|e| EpochEntry { epoch: e, start_lsn: e }).collect();
    let capped = replica::promoted_history(long, 65, 100);
    assert_eq!(capped.len(), 64);
    assert_eq!(capped.last().unwrap().epoch, 65);

    let c = claim(3, 15);
    assert_eq!(Claim::decode(&c.encode()), Some(c));
    assert_eq!(Claim::decode(&[0u8; 3]), None);
}

#[test]
fn a_replica_refuses_promote_without_prepare() {
    let cluster = Cluster::start("promote");
    let mut promote = Request::new(Op::Promote, 0);
    promote.epoch = 1;
    let path: &Path = &cluster.socket(1);
    let mut conn = replica::PeerConn::open(path, Duration::from_secs(5)).unwrap();
    assert_eq!(conn.call(promote).unwrap().status, Status::IoError);
    assert!(!cluster.get(1).is_primary());
    let mut client = Client::connect(cluster.socket(1)).unwrap();
    let err = client.read(0, 1).unwrap_err();
    assert_eq!(err.status(), Some(Status::NotPrimary));
    cluster.stop();
}
