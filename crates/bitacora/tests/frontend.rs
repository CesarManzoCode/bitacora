use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bitacora::format::BLOCK_SIZE;
use bitacora::frontend::{
    self, Command, FLAG_FUA, FrontendError, OP_DISCARD, OP_FLUSH, OP_READ, OP_WRITE,
    OP_WRITE_ZEROES, Op, Request,
};
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Volume, VolumeConfig};

const VOLUME: u64 = 4 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;
/// 512-byte sectors per 4096-byte block.
const SPB: u64 = 8;

fn temp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("frontend-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn create(path: &Path) -> Volume {
    let medium = Arc::new(FileMedium::create(path, false).unwrap());
    let config = VolumeConfig::new(VOLUME)
        .with_segment_size(256 << 10)
        .with_gc_mode(GcMode::Off);
    Volume::create(medium, config).unwrap()
}

fn req(op: u32, sector: u64, sectors: u32) -> Request {
    Request::from_ublk(op, sector, sectors)
}

fn pattern(lba: u64, seed: u8) -> Vec<u8> {
    (0..BLOCK_SIZE)
        .map(|i| (lba as u8).wrapping_mul(31) ^ seed ^ (i as u8))
        .collect()
}

#[test]
fn decodes_ublk_descriptor() {
    let r = req(OP_WRITE | FLAG_FUA, 16, 8);
    assert_eq!(
        r,
        Request {
            op: Op::Write,
            sector: 16,
            sectors: 8,
            fua: true
        }
    );
    assert_eq!(req(OP_READ, 0, 8).op, Op::Read);
    assert_eq!(req(OP_FLUSH, 0, 0).op, Op::Flush);
    assert_eq!(req(OP_DISCARD, 0, 8).op, Op::Discard);
    assert_eq!(req(OP_WRITE_ZEROES, 0, 8).op, Op::WriteZeroes);
    assert!(!req(OP_WRITE, 0, 8).fua);
}

#[test]
fn sectors_become_blocks() {
    assert_eq!(
        frontend::translate(&req(OP_READ, 3 * SPB, 2 * SPB as u32), BLOCKS).unwrap(),
        Command::Read { lba: 3, nblocks: 2 }
    );
    assert_eq!(
        frontend::translate(&req(OP_WRITE, 0, SPB as u32), BLOCKS).unwrap(),
        Command::Write {
            lba: 0,
            nblocks: 1,
            fua: false
        }
    );
    assert_eq!(
        frontend::translate(&req(OP_WRITE | FLAG_FUA, 10 * SPB, 4 * SPB as u32), BLOCKS).unwrap(),
        Command::Write {
            lba: 10,
            nblocks: 4,
            fua: true
        }
    );
    assert_eq!(
        frontend::translate(&req(OP_FLUSH, 0, 0), BLOCKS).unwrap(),
        Command::Flush
    );
    // The last block of the volume is in range.
    assert_eq!(
        frontend::translate(&req(OP_READ, (BLOCKS - 1) * SPB, SPB as u32), BLOCKS).unwrap(),
        Command::Read {
            lba: BLOCKS - 1,
            nblocks: 1
        }
    );
}

#[test]
fn rejects_unaligned_requests() {
    for (sector, sectors) in [(1, 8), (4, 8), (8, 1), (8, 12), (0, 0), (7, 9)] {
        let err = frontend::translate(&req(OP_WRITE, sector, sectors), BLOCKS).unwrap_err();
        assert!(
            matches!(err, FrontendError::Unaligned { .. }),
            "sector {sector}, {sectors} sectors: {err}"
        );
        assert_eq!(err.errno(), libc::EINVAL);
    }
}

#[test]
fn rejects_requests_out_of_range() {
    for (sector, sectors) in [
        (BLOCKS * SPB, 8),
        ((BLOCKS - 1) * SPB, 16),
        (u64::MAX - 7, 8),
    ] {
        let err = frontend::translate(&req(OP_READ, sector, sectors), BLOCKS).unwrap_err();
        assert!(
            matches!(err, FrontendError::OutOfRange { .. }),
            "sector {sector}, {sectors} sectors: {err}"
        );
        assert_eq!(err.errno(), libc::EINVAL);
    }
}

#[test]
fn rejects_discard_and_write_zeroes() {
    for (op, expected) in [(OP_DISCARD, Op::Discard), (OP_WRITE_ZEROES, Op::WriteZeroes)] {
        let err = frontend::translate(&req(op, 0, 8), BLOCKS).unwrap_err();
        assert!(matches!(err, FrontendError::Unsupported(o) if o == expected));
        assert_eq!(err.errno(), libc::EOPNOTSUPP);
    }
}

#[test]
fn executes_against_a_volume() {
    let path = temp_path("execute.img");
    let vol = create(&path);

    // Plain write: acknowledged, not yet durable.
    let mut buf = [pattern(5, 1), pattern(6, 1)].concat();
    let n = frontend::execute(&vol, &req(OP_WRITE, 5 * SPB, 2 * SPB as u32), &mut buf).unwrap();
    assert_eq!(n, 2 * BLOCK_SIZE);
    let st = vol.status();
    assert!(st.durable_lsn < st.lsn);

    // FLUSH makes it durable.
    let n = frontend::execute(&vol, &req(OP_FLUSH, 0, 0), &mut []).unwrap();
    assert_eq!(n, 0);
    let st = vol.status();
    assert_eq!(st.durable_lsn, st.lsn);

    // FUA write: durable on return (write_fua).
    let mut buf = pattern(9, 2);
    frontend::execute(&vol, &req(OP_WRITE | FLAG_FUA, 9 * SPB, SPB as u32), &mut buf).unwrap();
    let st = vol.status();
    assert_eq!(st.durable_lsn, st.lsn);

    // Reads go through the same translation.
    let mut out = vec![0u8; 5 * BLOCK_SIZE];
    let n = frontend::execute(&vol, &req(OP_READ, 5 * SPB, 5 * SPB as u32), &mut out).unwrap();
    assert_eq!(n, 5 * BLOCK_SIZE);
    let expected = [
        pattern(5, 1),
        pattern(6, 1),
        vec![0; BLOCK_SIZE],
        vec![0; BLOCK_SIZE],
        pattern(9, 2),
    ]
    .concat();
    assert_eq!(out, expected);
    assert_eq!(vol.read(9, 1).unwrap(), pattern(9, 2));

    // Rejected requests leave the volume untouched.
    let before = vol.status().lsn;
    let mut buf = vec![0xffu8; 2 * BLOCK_SIZE];
    for r in [
        req(OP_WRITE, 5 * SPB + 1, SPB as u32),
        req(OP_WRITE, BLOCKS * SPB, SPB as u32),
        req(OP_DISCARD, 5 * SPB, SPB as u32),
        req(OP_WRITE_ZEROES, 5 * SPB, SPB as u32),
    ] {
        assert!(frontend::execute(&vol, &r, &mut buf).is_err());
    }
    // A buffer shorter than the request is an error, not a short write.
    let err = frontend::execute(&vol, &req(OP_WRITE, 0, 4 * SPB as u32), &mut buf).unwrap_err();
    assert!(matches!(err, FrontendError::BufferTooSmall { .. }));
    assert_eq!(vol.status().lsn, before);
    assert_eq!(vol.read(5, 1).unwrap(), pattern(5, 1));

    vol.close().unwrap();
}
