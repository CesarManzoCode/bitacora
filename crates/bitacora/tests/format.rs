use std::fs;
use std::path::PathBuf;

use bitacora::Error;
use bitacora::format::{
    BLOCK_SIZE, BatchEntry, BatchSummary, EpochEntry, FORMAT_VERSION, MAX_BATCH_BLOCKS,
    MAX_EPOCH_HISTORY, MAX_SNAPSHOTS, SNAPSHOT_FLAG_DELETED, SegmentHeader, SnapshotEntry, Stream,
    Superblock, SuperblockSlot, VolumeId, data_crc, select_superblock,
};
use bitacora::medium::{FileMedium, Medium};

const VID: VolumeId = [7, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const OTHER_VID: VolumeId = [9; 16];

fn superblock(sb_seq: u64) -> Superblock {
    Superblock {
        sb_seq,
        volume_id: VID,
        volume_size: 1 << 30,
        segment_size: 8 << 20,
        max_segments: 164,
        durable_lsn: 12345,
        clean_shutdown: true,
        current_epoch: 3,
        epoch_history: (0..MAX_EPOCH_HISTORY as u64)
            .map(|i| EpochEntry {
                epoch: i + 1,
                start_lsn: i * 100 + 1,
            })
            .collect(),
        snapshots: (0..MAX_SNAPSHOTS as u64)
            .map(|i| SnapshotEntry {
                snapshot_id: i + 1,
                lsn: i * 10,
                flags: if i % 2 == 0 { SNAPSHOT_FLAG_DELETED } else { 0 },
            })
            .collect(),
    }
}

fn header() -> SegmentHeader {
    SegmentHeader {
        volume_id: VID,
        seg_gen: 42,
        stream: Stream::Cold,
        first_lsn: 9000,
    }
}

fn summary() -> BatchSummary {
    BatchSummary {
        volume_id: VID,
        seg_gen: 42,
        stream: Stream::Hot,
        epoch: 2,
        first_lsn: 500,
        synced_through_lsn: 499,
        entries: (0..MAX_BATCH_BLOCKS as u64)
            .map(|i| BatchEntry {
                lba: i * 3,
                birth: 500 + i,
                data_crc: 0xdead_0000 + i as u32,
                flags: 0,
            })
            .collect(),
    }
}

fn reseal(b: &mut [u8]) {
    let crc = crc32c::crc32c(&b[..BLOCK_SIZE - 4]);
    b[BLOCK_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn superblock_round_trip() {
    let sb = superblock(7);
    let enc = sb.encode().unwrap();
    assert_eq!(enc.len(), BLOCK_SIZE);
    assert_eq!(&enc[0..8], b"BITACSB1");
    assert_eq!(Superblock::decode(&enc, Some(&VID)).unwrap(), sb);
    assert_eq!(Superblock::decode(&enc, None).unwrap(), sb);

    let mut empty = superblock(1);
    empty.epoch_history.clear();
    empty.snapshots.clear();
    empty.clean_shutdown = false;
    let enc = empty.encode().unwrap();
    assert_eq!(Superblock::decode(&enc, Some(&VID)).unwrap(), empty);
}

#[test]
fn segment_header_round_trip() {
    for stream in [Stream::Hot, Stream::Cold, Stream::Resync] {
        let h = SegmentHeader { stream, ..header() };
        let enc = h.encode();
        assert_eq!(enc.len(), BLOCK_SIZE);
        assert_eq!(&enc[0..8], b"BITACSG1");
        assert_eq!(SegmentHeader::decode(&enc, &VID).unwrap(), h);
    }
}

#[test]
fn batch_summary_round_trip() {
    let s = summary();
    let enc = s.encode().unwrap();
    assert_eq!(enc.len(), BLOCK_SIZE);
    assert_eq!(&enc[0..8], b"BITACBT1");
    assert_eq!(BatchSummary::decode(&enc, &VID).unwrap(), s);

    let mut one = summary();
    one.entries.truncate(1);
    let enc = one.encode().unwrap();
    assert_eq!(BatchSummary::decode(&enc, &VID).unwrap(), one);
}

#[test]
fn batch_summary_rejects_bad_count_on_encode() {
    let mut s = summary();
    s.entries.clear();
    assert!(matches!(s.encode(), Err(Error::InvalidArgument(_))));
    let mut s = summary();
    s.entries.push(s.entries[0]);
    assert!(matches!(s.encode(), Err(Error::InvalidArgument(_))));
}

#[test]
fn flipped_byte_fails_superblock_decode() {
    let enc = superblock(7).encode().unwrap();
    for pos in 0..BLOCK_SIZE {
        let mut b = enc.clone();
        b[pos] ^= 0x01;
        assert!(
            Superblock::decode(&b, None).is_err(),
            "flip at byte {pos} accepted"
        );
    }
}

#[test]
fn flipped_byte_fails_segment_header_decode() {
    let enc = header().encode();
    for pos in 0..BLOCK_SIZE {
        let mut b = enc.clone();
        b[pos] ^= 0x01;
        assert!(
            SegmentHeader::decode(&b, &VID).is_err(),
            "flip at byte {pos} accepted"
        );
    }
}

#[test]
fn flipped_byte_fails_batch_summary_decode() {
    let enc = summary().encode().unwrap();
    for pos in 0..BLOCK_SIZE {
        let mut b = enc.clone();
        b[pos] ^= 0x01;
        assert!(
            BatchSummary::decode(&b, &VID).is_err(),
            "flip at byte {pos} accepted"
        );
    }
}

#[test]
fn decode_rejects_other_volume_id() {
    let sb = superblock(1).encode().unwrap();
    assert!(matches!(
        Superblock::decode(&sb, Some(&OTHER_VID)),
        Err(Error::Corrupt(_))
    ));
    let h = header().encode();
    assert!(matches!(
        SegmentHeader::decode(&h, &OTHER_VID),
        Err(Error::Corrupt(_))
    ));
    let s = summary().encode().unwrap();
    assert!(matches!(
        BatchSummary::decode(&s, &OTHER_VID),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn decode_rejects_bad_magic_with_valid_crc() {
    for enc in [
        superblock(1).encode().unwrap(),
        header().encode(),
        summary().encode().unwrap(),
    ] {
        let mut b = enc.clone();
        b[7] = b'X';
        reseal(&mut b);
        assert!(Superblock::decode(&b, None).is_err());
        assert!(SegmentHeader::decode(&b, &VID).is_err());
        assert!(BatchSummary::decode(&b, &VID).is_err());
    }
}

#[test]
fn superblock_decode_rejects_other_version() {
    let mut b = superblock(1).encode().unwrap();
    b[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    reseal(&mut b);
    assert!(matches!(
        Superblock::decode(&b, None),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn decode_never_panics_on_garbage() {
    let zeros = vec![0u8; BLOCK_SIZE];
    let short = vec![0u8; 100];
    let ones = vec![0xffu8; BLOCK_SIZE];
    for b in [&zeros, &short, &ones] {
        assert!(Superblock::decode(b, None).is_err());
        assert!(SegmentHeader::decode(b, &VID).is_err());
        assert!(BatchSummary::decode(b, &VID).is_err());
    }
    // Valid CRC but impossible counts.
    let mut b = summary().encode().unwrap();
    b[56..60].copy_from_slice(&1000u32.to_le_bytes());
    reseal(&mut b);
    assert!(BatchSummary::decode(&b, &VID).is_err());
    let mut b = superblock(1).encode().unwrap();
    b[92..96].copy_from_slice(&65u32.to_le_bytes());
    reseal(&mut b);
    assert!(Superblock::decode(&b, None).is_err());
}

#[test]
fn select_superblock_prefers_greater_sb_seq() {
    let a = superblock(5).encode().unwrap();
    let b = superblock(6).encode().unwrap();
    let (slot, sb) = select_superblock(&a, &b).unwrap();
    assert_eq!(slot, SuperblockSlot::B);
    assert_eq!(sb.sb_seq, 6);

    let (slot, sb) = select_superblock(&b, &a).unwrap();
    assert_eq!(slot, SuperblockSlot::A);
    assert_eq!(sb.sb_seq, 6);
}

#[test]
fn select_superblock_falls_back_when_newer_is_corrupt() {
    let a = superblock(5).encode().unwrap();
    let mut b = superblock(6).encode().unwrap();
    b[100] ^= 0xff;
    let (slot, sb) = select_superblock(&a, &b).unwrap();
    assert_eq!(slot, SuperblockSlot::A);
    assert_eq!(sb.sb_seq, 5);

    let mut a2 = superblock(9).encode().unwrap();
    a2[BLOCK_SIZE - 1] ^= 0x01;
    let b2 = superblock(8).encode().unwrap();
    let (slot, sb) = select_superblock(&a2, &b2).unwrap();
    assert_eq!(slot, SuperblockSlot::B);
    assert_eq!(sb.sb_seq, 8);

    assert!(matches!(select_superblock(&a2, &b), Err(Error::Corrupt(_))));
}

#[test]
fn superblock_slots() {
    assert_eq!(SuperblockSlot::A.offset(), 0);
    assert_eq!(SuperblockSlot::B.offset(), 4096);
    assert_eq!(SuperblockSlot::A.other(), SuperblockSlot::B);
    assert_eq!(SuperblockSlot::B.other(), SuperblockSlot::A);
}

#[test]
fn data_crc_binds_lba_and_birth() {
    let data = vec![0xabu8; BLOCK_SIZE];
    let base = data_crc(10, 20, &data);
    assert_eq!(base, data_crc(10, 20, &data));
    assert_ne!(base, data_crc(11, 20, &data));
    assert_ne!(base, data_crc(10, 21, &data));
    let mut other = data.clone();
    other[4095] ^= 1;
    assert_ne!(base, data_crc(10, 20, &other));
}

fn temp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("format-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn exercise_medium(m: &FileMedium) {
    assert_eq!(m.len().unwrap(), 0);
    assert!(m.is_empty().unwrap());

    let mut data = vec![0u8; 3 * BLOCK_SIZE];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8 + 1;
    }
    m.write_at(&data, 0).unwrap();
    m.sync().unwrap();
    assert_eq!(m.len().unwrap(), 3 * BLOCK_SIZE as u64);

    let mut back = vec![0u8; 3 * BLOCK_SIZE];
    assert_eq!(m.read_at(&mut back, 0).unwrap(), 3 * BLOCK_SIZE);
    assert_eq!(back, data);

    // Reading past the end returns a short count.
    let mut tail = vec![0u8; 2 * BLOCK_SIZE];
    assert_eq!(
        m.read_at(&mut tail, 2 * BLOCK_SIZE as u64).unwrap(),
        BLOCK_SIZE
    );
    assert_eq!(&tail[..BLOCK_SIZE], &data[2 * BLOCK_SIZE..]);

    m.punch_hole(BLOCK_SIZE as u64, BLOCK_SIZE as u64).unwrap();
    m.sync().unwrap();
    assert_eq!(m.len().unwrap(), 3 * BLOCK_SIZE as u64);
    let mut back = vec![0xeeu8; 3 * BLOCK_SIZE];
    assert_eq!(m.read_at(&mut back, 0).unwrap(), 3 * BLOCK_SIZE);
    assert_eq!(&back[..BLOCK_SIZE], &data[..BLOCK_SIZE]);
    assert!(back[BLOCK_SIZE..2 * BLOCK_SIZE].iter().all(|&b| b == 0));
    assert_eq!(&back[2 * BLOCK_SIZE..], &data[2 * BLOCK_SIZE..]);
}

#[test]
fn file_medium_buffered() {
    let path = temp_path("buffered.img");
    let m = FileMedium::create(&path, false).unwrap();
    exercise_medium(&m);
    drop(m);

    let m = FileMedium::open(&path, false).unwrap();
    assert_eq!(m.len().unwrap(), 3 * BLOCK_SIZE as u64);
    let mut b = vec![0xeeu8; 10];
    assert_eq!(m.read_at(&mut b, BLOCK_SIZE as u64 + 5).unwrap(), 10);
    assert!(b.iter().all(|&x| x == 0));
}

#[test]
fn file_medium_direct() {
    let path = temp_path("direct.img");
    let m = FileMedium::create(&path, true).unwrap();
    assert!(m.is_direct());
    exercise_medium(&m);

    // Unaligned offset or length is rejected, not silently mangled.
    let mut b = vec![0u8; BLOCK_SIZE];
    assert!(matches!(
        m.read_at(&mut b, 1),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        m.write_at(&b[..100], 0),
        Err(Error::InvalidArgument(_))
    ));

    // A deliberately misaligned buffer still works through the bounce buffer.
    let mut big = vec![0u8; 2 * BLOCK_SIZE + 1];
    let unaligned = &mut big[1..BLOCK_SIZE + 1];
    unaligned.fill(0x5a);
    m.write_at(unaligned, 0).unwrap();
    m.sync().unwrap();
    let mut back = vec![0u8; BLOCK_SIZE + 1];
    assert_eq!(m.read_at(&mut back[1..], 0).unwrap(), BLOCK_SIZE);
    assert!(back[1..].iter().all(|&x| x == 0x5a));
}
