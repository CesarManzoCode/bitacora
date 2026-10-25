use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bitacora::format::BLOCK_SIZE;
use bitacora::medium::FileMedium;
use bitacora::migrate;
use bitacora::wire::{self, Op, Request, Status};
use bitacora::{Client, Server, ServerHandle, Volume, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
/// The 64 MiB of C14. While a migration runs, its pin keeps every pinned
/// version, so the source only has the space rule's headroom (a quarter of
/// the volume) for the writes that go on; with 16 MiB an unpaced writer
/// filled it before the copy ended and got `NO_SPACE`, as the contract
/// says it must.
const VOLUME: u64 = 64 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("migrate-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

fn block(tag: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK_SIZE];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&(tag.wrapping_mul(0x9e37_79b9) ^ i as u64).to_le_bytes());
    }
    b
}

/// A server on its own thread of this process.
struct Node {
    volume: Arc<Volume>,
    socket: PathBuf,
    handle: ServerHandle,
    thread: Option<JoinHandle<()>>,
}

impl Node {
    fn start(dir: &Path, name: &str, target: bool) -> Node {
        let path = dir.join(format!("{name}.img"));
        let medium = Arc::new(FileMedium::create(&path, false).unwrap());
        let config = VolumeConfig::new(VOLUME).with_segment_size(SEGMENT);
        let volume = Arc::new(Volume::create(medium, config).unwrap());
        let socket = dir.join(format!("{name}.sock"));
        let srv = if target {
            Server::bind_migrate_target(volume.clone(), &socket).unwrap()
        } else {
            Server::bind(volume.clone(), &socket).unwrap()
        };
        let handle = srv.handle();
        let thread = thread::spawn(move || srv.run().unwrap());
        Node {
            volume,
            socket,
            handle,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        self.volume.close().unwrap();
    }
}

/// Status of a raw `STATUS` request and its payload as text.
fn raw_status(socket: &Path) -> (Status, String) {
    let mut stream = bitacora::socket::connect(socket).unwrap();
    wire::write_request(&mut stream, &Request::new(Op::Status, 1)).unwrap();
    let resp = wire::read_response(&mut stream).unwrap().unwrap();
    (resp.status, resp.text())
}

/// Whether `vol` holds exactly `model` (blocks not in it read as zeros).
fn assert_matches(vol: &Volume, model: &HashMap<u64, u64>) {
    let zeros = vec![0u8; BLOCK_SIZE];
    for lba in 0..BLOCKS {
        let data = vol.read(lba, 1).unwrap();
        match model.get(&lba) {
            Some(tag) => assert_eq!(data, block(*tag), "lba {lba}"),
            None => assert_eq!(data, zeros, "lba {lba} was never written"),
        }
    }
}

/// A writer that sends writes through a `Client` without pause and keeps
/// the model of acknowledged writes. Any error panics: every write
/// attempted must be acknowledged once, by whichever server executed it.
struct Writer {
    acked: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<(HashMap<u64, u64>, PathBuf)>,
}

impl Writer {
    fn start(socket: &Path, first_tag: u64) -> Writer {
        let acked = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let socket = socket.to_path_buf();
        let (a, s) = (acked.clone(), stop.clone());
        let thread = thread::spawn(move || {
            let mut client = Client::connect(&socket).unwrap();
            let mut model = HashMap::new();
            let mut tag = first_tag;
            let mut lba = 0u64;
            while !s.load(Ordering::SeqCst) {
                lba = lba
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407)
                    % BLOCKS;
                client.write(lba, &block(tag)).unwrap();
                model.insert(lba, tag);
                tag += 1;
                let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                if n.is_multiple_of(64) {
                    client.flush().unwrap();
                }
            }
            client.flush().unwrap();
            (model, client.path().to_path_buf())
        });
        Writer {
            acked,
            stop,
            thread,
        }
    }

    fn wait_for(&self, count: u64) {
        while self.acked.load(Ordering::SeqCst) < count {
            assert!(!self.thread.is_finished(), "the writer stopped early");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn finish(self) -> (HashMap<u64, u64>, PathBuf) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

#[test]
fn migration_under_writes_moves_every_acknowledged_write() {
    let dir = temp_dir("under-writes");
    let src = Node::start(&dir, "src", false);
    let dst = Node::start(&dir, "dst", true);

    // The destination serves nothing before the commit.
    assert_eq!(raw_status(&dst.socket).0, Status::Ok);
    let mut early = Client::connect(&dst.socket).unwrap();
    let err = early.write(0, &block(0)).unwrap_err();
    assert_eq!(err.status(), Some(Status::NotPrimary));
    drop(early);

    let writer = Writer::start(&src.socket, 1);
    writer.wait_for(3_000);
    let summary = migrate::request(&src.socket, &dst.socket).unwrap();
    assert!(summary.rounds >= 1 && summary.rounds <= migrate::MAX_ROUNDS);
    assert!(summary.blocks > 0);
    let after = writer.acked.load(Ordering::SeqCst);
    writer.wait_for(after + 500);
    let (model, path) = writer.finish();

    assert_eq!(path, dst.socket, "the writer followed MOVED");
    assert_eq!(
        raw_status(&src.socket),
        (Status::Moved, dst.socket.to_str().unwrap().to_string())
    );
    assert_matches(&dst.volume, &model);
    assert_eq!(dst.volume.epoch(), src.volume.epoch());
    assert!(dst.volume.epoch() > 0);
    // The pins of the migration are gone: no snapshot is left behind.
    assert!(src.volume.snapshots().is_empty());
    assert!(dst.volume.snapshots().is_empty());

    src.stop();
    dst.stop();
}

#[test]
fn a_migrated_destination_migrates_again() {
    let dir = temp_dir("twice");
    let a = Node::start(&dir, "a", false);
    let b = Node::start(&dir, "b", true);
    let c = Node::start(&dir, "c", true);

    let writer = Writer::start(&a.socket, 1);
    writer.wait_for(2_000);
    migrate::request(&a.socket, &b.socket).unwrap();
    let after = writer.acked.load(Ordering::SeqCst);
    writer.wait_for(after + 1_000);
    let (mut model, path) = writer.finish();
    assert_eq!(path, b.socket);

    // A client still pointing at `a` follows two hops once `b` moved too.
    let mut stale = Client::connect(&a.socket).unwrap();
    let writer = Writer::start(&b.socket, 1_000_000);
    writer.wait_for(1_000);
    let summary = migrate::request(&b.socket, &c.socket).unwrap();
    assert_eq!(summary.epoch, b.volume.epoch());
    let after = writer.acked.load(Ordering::SeqCst);
    writer.wait_for(after + 500);
    let (second, path) = writer.finish();
    assert_eq!(path, c.socket);
    model.extend(second);

    stale.write(BLOCKS - 1, &block(7_000_000)).unwrap();
    assert_eq!(stale.path(), c.socket.as_path());
    model.insert(BLOCKS - 1, 7_000_000);
    drop(stale);

    assert_eq!(raw_status(&b.socket).0, Status::Moved);
    assert_matches(&c.volume, &model);
    assert_eq!(c.volume.epoch(), b.volume.epoch());
    assert!(c.volume.epoch() > a.volume.epoch());

    a.stop();
    b.stop();
    c.stop();
}
