use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use bitacora::format::BLOCK_SIZE;
use bitacora::medium::FileMedium;
use bitacora::status::{ReplicaState, Role};
use bitacora::wire::{self, Op, Request, Response, Status, WireError};
use bitacora::{Client, GcMode, Server, StatusReport, Version, Volume, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
const VOLUME: u64 = 4 << 20;

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("server-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn block(tag: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK_SIZE];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&(tag.wrapping_mul(31) ^ i as u64).to_le_bytes());
    }
    b
}

fn sample_requests() -> Vec<Request> {
    Op::ALL
        .into_iter()
        .enumerate()
        .map(|(i, op)| Request {
            op,
            req_id: 1000 + i as u64,
            epoch: i as u64,
            lba: 7 * i as u64,
            nblocks: i as u32,
            payload: vec![i as u8; i * 3],
        })
        .collect()
}

#[test]
fn every_request_round_trips() {
    for req in sample_requests() {
        let frame = req.encode();
        assert_eq!(frame.len(), 4 + wire::REQUEST_HEADER + req.payload.len());
        assert_eq!(Request::decode_frame(&frame).unwrap(), req);
        let mut reader = frame.as_slice();
        assert_eq!(wire::read_request(&mut reader).unwrap(), Some(req));
        assert!(wire::read_request(&mut reader).unwrap().is_none());
    }
}

#[test]
fn every_response_round_trips() {
    for (i, status) in Status::ALL.into_iter().enumerate() {
        let resp = Response {
            req_id: 42 + i as u64,
            status,
            payload: vec![0xa5; i * 5],
        };
        let frame = resp.encode();
        assert_eq!(Response::decode_frame(&frame).unwrap(), resp);
        let mut reader = frame.as_slice();
        assert_eq!(wire::read_response(&mut reader).unwrap(), Some(resp));
    }
}

#[test]
fn op_codes_are_distinct_and_reserved_ones_known() {
    let codes: BTreeSet<u8> = Op::ALL.iter().map(|op| *op as u8).collect();
    assert_eq!(codes.len(), Op::ALL.len());
    for op in Op::ALL {
        assert_eq!(Op::from_u8(op as u8), Some(op));
    }
    assert!(Op::Append.is_reserved());
    assert!(Op::MigrateData.is_reserved());
    assert!(!Op::Export.is_reserved());
}

#[test]
fn truncated_frames_are_rejected() {
    let req = Request {
        op: Op::Write,
        req_id: 9,
        epoch: 1,
        lba: 3,
        nblocks: 1,
        payload: block(1),
    };
    let frame = req.encode();
    for cut in 0..frame.len() {
        let err = Request::decode_frame(&frame[..cut]).unwrap_err();
        assert!(
            matches!(err, WireError::Truncated { .. }),
            "cut {cut}: {err:?}"
        );
    }
    for cut in 1..frame.len() {
        let mut reader = &frame[..cut];
        let err = wire::read_request(&mut reader).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "cut {cut}");
    }
    let body = &frame[4..];
    for cut in 0..wire::REQUEST_HEADER {
        assert!(matches!(
            Request::decode(&body[..cut]),
            Err(WireError::Truncated { .. })
        ));
    }
    let resp = Response::ok(5, vec![1, 2, 3]).encode();
    for cut in 0..resp.len() {
        assert!(Response::decode_frame(&resp[..cut]).is_err());
    }
}

#[test]
fn unknown_or_oversized_frames_are_rejected() {
    let mut frame = Request::new(Op::Flush, 1).encode();
    frame[4] = 200;
    assert_eq!(
        Request::decode_frame(&frame),
        Err(WireError::UnknownOp(200))
    );
    frame[4] = 0;
    assert_eq!(Request::decode_frame(&frame), Err(WireError::UnknownOp(0)));

    let mut resp = Response::ok(1, Vec::new()).encode();
    resp[12] = 99;
    assert_eq!(
        Response::decode_frame(&resp),
        Err(WireError::UnknownStatus(99))
    );

    let mut long = Request::new(Op::Flush, 1).encode();
    long.push(0);
    assert!(matches!(
        Request::decode_frame(&long),
        Err(WireError::Trailing { .. })
    ));

    let huge = ((wire::MAX_BODY + 1) as u32).to_le_bytes();
    assert!(matches!(
        Request::decode_frame(&huge),
        Err(WireError::TooLarge(_))
    ));
    let mut reader = huge.as_slice();
    let err = wire::read_request(&mut reader).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// A server on its own thread over a fresh volume.
struct Running {
    socket: PathBuf,
    volume: Arc<Volume>,
    handle: bitacora::ServerHandle,
    thread: thread::JoinHandle<bitacora::Result<()>>,
}

fn start(dir: &Path) -> Running {
    start_at(dir, &dir.join("s.sock"))
}

fn start_at(dir: &Path, socket: &Path) -> Running {
    let medium = Arc::new(FileMedium::create(&dir.join("volume.img"), false).unwrap());
    let config = VolumeConfig::new(VOLUME)
        .with_segment_size(SEGMENT)
        .with_gc_mode(GcMode::Auto);
    let volume = Arc::new(Volume::create(medium, config).unwrap());
    let socket = socket.to_path_buf();
    let server = Server::bind(volume.clone(), &socket).unwrap();
    let handle = server.handle();
    let thread = thread::spawn(move || server.run());
    Running {
        socket,
        volume,
        handle,
        thread,
    }
}

impl Running {
    fn stop(self) {
        self.handle.shutdown();
        self.thread.join().unwrap().unwrap();
        assert!(!self.socket.exists());
        self.volume.close().unwrap();
    }
}

#[test]
fn socket_path_longer_than_sun_path_serves() {
    let dir = temp_dir("long-path").join("d".repeat(120));
    fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    assert!(socket.as_os_str().len() > 108);
    let srv = start_at(&dir, &socket);
    assert!(socket.exists());

    let mut client = Client::connect(&srv.socket).unwrap();
    client.write_fua(3, &block(3)).unwrap();
    assert_eq!(client.read(3, 1).unwrap(), block(3));
    drop(client);
    srv.stop();
}

#[test]
fn two_clients_write_flush_and_read() {
    let dir = temp_dir("two-clients");
    let srv = start(&dir);

    let writers: Vec<_> = (0..2u64)
        .map(|c| {
            let socket = srv.socket.clone();
            thread::spawn(move || {
                let mut client = Client::connect(&socket).unwrap();
                for i in 0..64u64 {
                    let lba = c * 100 + i;
                    if i % 2 == 0 {
                        client.write(lba, &block(lba)).unwrap();
                    } else {
                        client.write_fua(lba, &block(lba)).unwrap();
                    }
                }
                client.flush().unwrap();
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }

    let mut a = Client::connect(&srv.socket).unwrap();
    let mut b = Client::connect(&srv.socket).unwrap();
    for c in 0..2u64 {
        for i in 0..64u64 {
            let lba = c * 100 + i;
            let reader = if lba % 2 == 0 { &mut a } else { &mut b };
            assert_eq!(reader.read(lba, 1).unwrap(), block(lba), "lba {lba}");
        }
    }
    let run = a.read(100, 4).unwrap();
    let expected: Vec<u8> = (100..104).flat_map(block).collect();
    assert_eq!(run, expected);
    assert_eq!(b.read(80, 1).unwrap(), vec![0u8; BLOCK_SIZE]);

    let st = a.status().unwrap();
    assert!(st.volume.durable_lsn >= 128);
    drop(a);
    drop(b);
    srv.stop();
}

#[test]
fn errors_map_to_statuses() {
    let dir = temp_dir("errors");
    let srv = start(&dir);
    let mut client = Client::connect(&srv.socket).unwrap();
    let blocks = VOLUME / BLOCK_SIZE as u64;
    let err = client.read(blocks, 1).unwrap_err();
    assert_eq!(err.status(), Some(Status::IoError));
    let err = client.snapshot_delete(77).unwrap_err();
    assert!(err.status().is_some());
    // The connection still answers after an error.
    client.flush().unwrap();
    drop(client);
    srv.stop();
}

#[test]
fn snapshots_and_export_over_the_socket() {
    let dir = temp_dir("snapshots");
    let srv = start(&dir);
    let mut client = Client::connect(&srv.socket).unwrap();
    client.write(0, &block(1)).unwrap();
    let id = client.snapshot_create().unwrap();
    client.write(0, &block(2)).unwrap();
    let image = dir.join("snap.img");
    client.export(Version::Snapshot(id), &image).unwrap();
    let bytes = fs::read(&image).unwrap();
    assert_eq!(bytes.len() as u64, VOLUME);
    assert_eq!(&bytes[..BLOCK_SIZE], block(1).as_slice());
    assert_eq!(client.status().unwrap().volume.snapshots, 1);
    client.snapshot_delete(id).unwrap();
    assert_eq!(client.status().unwrap().volume.snapshots, 0);
    drop(client);
    srv.stop();
}

#[test]
fn moved_is_followed_to_the_destination() {
    let src = start(&temp_dir("moved-src"));
    let dst = start(&temp_dir("moved-dst"));
    let mut client = Client::connect(&src.socket).unwrap();
    client.write(5, &block(5)).unwrap();
    src.handle.set_moved(dst.socket.to_str().unwrap());
    client.write(6, &block(6)).unwrap();
    assert_eq!(client.path(), dst.socket.as_path());
    assert_eq!(dst.volume.read(6, 1).unwrap(), block(6));
    // The request answered MOVED was not executed on the source.
    assert_eq!(src.volume.read(6, 1).unwrap(), vec![0u8; BLOCK_SIZE]);
    drop(client);
    src.stop();
    dst.stop();
}

fn schema() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/status.schema.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn required(schema: &serde_json::Value) -> BTreeSet<String> {
    schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn status_json_has_exactly_the_required_keys() {
    let dir = temp_dir("status");
    let srv = start(&dir);
    let mut client = Client::connect(&srv.socket).unwrap();
    client.write(0, &block(0)).unwrap();
    let text = client.status_json().unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let schema = schema();

    assert_eq!(keys(&json), required(&schema));
    let props = &schema["properties"];
    for name in ["volume", "gc", "space"] {
        assert_eq!(keys(&json[name]), required(&props[name]), "{name}");
    }
    let replicas = json["replicas"].as_array().unwrap();
    assert_eq!(replicas.len(), 1);
    assert_eq!(keys(&replicas[0]), required(&props["replicas"]["items"]));

    assert_eq!(json["format"], "bitacora-status/1");
    assert_eq!(json["volume"]["state"], "ok");
    assert_eq!(json["volume"]["volume_size"], VOLUME);
    assert_eq!(json["replicas"][0]["role"], "standalone");
    assert_eq!(json["replicas"][0]["state"], "activa");
    assert_eq!(json["gc"]["mode"], "auto");
    assert_eq!(json["space"]["segment_size"], SEGMENT);

    let report = StatusReport::from_json(text.as_bytes()).unwrap();
    assert_eq!(report.replicas[0].role, Role::Standalone);
    assert_eq!(report.replicas[0].state, ReplicaState::Activa);
    assert_eq!(report.volume.volume_id.len(), 32);
    assert!(report.space.physical_bytes > 0);
    assert!(StatusReport::from_json(br#"{"format":"x","extra":1}"#).is_err());
    drop(client);
    srv.stop();
}
