use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bitacora::format::BLOCK_SIZE;
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
const VOLUME: u64 = 4 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;
/// Blocks touched by the scenarios: half the volume, so that a snapshot and
/// the live volume fit together with room for the GC.
const HALF: u64 = BLOCKS / 2;

fn config(mode: GcMode) -> VolumeConfig {
    VolumeConfig::new(VOLUME)
        .with_segment_size(SEGMENT)
        .with_gc_mode(mode)
}

fn temp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("snapshot-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn create(path: &Path, mode: GcMode) -> Volume {
    let medium = Arc::new(FileMedium::create(path, false).unwrap());
    Volume::create(medium, config(mode)).unwrap()
}

fn open(path: &Path, mode: GcMode) -> Volume {
    let medium = Arc::new(FileMedium::open(path, false).unwrap());
    Volume::open(medium, config(mode)).unwrap()
}

/// Content of version `version` of `lba` (version 0 = never written).
fn pattern(lba: u64, version: u64) -> Vec<u8> {
    if version == 0 {
        return vec![0; BLOCK_SIZE];
    }
    let mut x = lba.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ version.wrapping_mul(0xD1B5_4A32_D192_ED03)
        ^ 0x2545_F491_4F6C_DD1D;
    let mut out = Vec::with_capacity(BLOCK_SIZE);
    for _ in 0..BLOCK_SIZE / 8 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn block(buf: &[u8], i: u64) -> &[u8] {
    &buf[i as usize * BLOCK_SIZE..(i as usize + 1) * BLOCK_SIZE]
}

/// Writes `version` of every lba in `0..n` and records it in `model`.
fn write_all(vol: &Volume, model: &mut [u64], n: u64, version: u64) {
    for lba in 0..n {
        vol.write(lba, &pattern(lba, version)).unwrap();
        model[lba as usize] = version;
    }
}

/// Reads a whole version and compares it with `model` (version per lba).
fn check(vol: &Volume, version: Version, model: &[u64]) {
    let all = vol.read_at_version(version, 0, BLOCKS).unwrap();
    for (lba, &v) in model.iter().enumerate() {
        assert!(
            block(&all, lba as u64) == pattern(lba as u64, v),
            "{version:?}: lba {lba} differs from version {v}"
        );
    }
}

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn snapshot_survives_overwrite_gc_and_crash() {
    let path = temp_path("survives.img");
    let vol = create(&path, GcMode::Auto);
    let mut live = vec![0u64; BLOCKS as usize];
    write_all(&vol, &mut live, HALF, 1);
    let frozen = live.clone();
    let id = vol.snapshot_create().unwrap();
    check(&vol, Version::Snapshot(id), &frozen);

    // Overwrite three times: the dead versions force the GC to relocate
    // the blocks only the snapshot still references.
    for version in 2..=4 {
        write_all(&vol, &mut live, HALF, version);
        vol.flush().unwrap();
        check(&vol, Version::Snapshot(id), &frozen);
    }
    vol.compact().unwrap();
    check(&vol, Version::Snapshot(id), &frozen);
    check(&vol, Version::Live, &live);
    let status = vol.status();
    assert!(status.gc.segments_cleaned > 0, "{status:?}");

    // Crash: no orderly close.
    drop(vol);
    let vol = open(&path, GcMode::Off);
    let snaps = vol.snapshots();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].id, id);
    check(&vol, Version::Snapshot(id), &frozen);
    check(&vol, Version::Live, &live);
    vol.close().unwrap();
}

#[test]
fn deleting_a_middle_snapshot_keeps_the_others() {
    let path = temp_path("middle.img");
    let vol = create(&path, GcMode::Auto);
    let mut live = vec![0u64; BLOCKS as usize];
    let mut taken = Vec::new();
    for version in 1..=3 {
        write_all(&vol, &mut live, HALF / 2, version);
        taken.push((vol.snapshot_create().unwrap(), live.clone()));
    }
    write_all(&vol, &mut live, HALF / 2, 4);
    let (middle, _) = taken.remove(1);
    vol.snapshot_delete(middle).unwrap();
    assert!(vol.snapshot_delete(middle).is_err());
    assert!(vol.read_snapshot(middle, 0, 1).is_err());
    vol.compact().unwrap();

    let ids: Vec<u64> = vol.snapshots().iter().map(|s| s.id).collect();
    assert_eq!(ids, taken.iter().map(|(id, _)| *id).collect::<Vec<_>>());
    for (id, model) in &taken {
        check(&vol, Version::Snapshot(*id), model);
    }
    check(&vol, Version::Live, &live);
    vol.flush().unwrap();
    drop(vol);

    let vol = open(&path, GcMode::Off);
    let ids: Vec<u64> = vol.snapshots().iter().map(|s| s.id).collect();
    assert_eq!(ids, taken.iter().map(|(id, _)| *id).collect::<Vec<_>>());
    for (id, model) in &taken {
        check(&vol, Version::Snapshot(*id), model);
    }
    check(&vol, Version::Live, &live);
    // A new snapshot never reuses the id of a deleted one.
    let next = vol.snapshot_create().unwrap();
    assert!(next > middle && taken.iter().all(|(id, _)| next > *id));
    vol.close().unwrap();
}

#[test]
fn delete_and_compact_lower_the_allocated_space() {
    let path = temp_path("compact.img");
    let vol = create(&path, GcMode::Off);
    let mut live = vec![0u64; BLOCKS as usize];
    write_all(&vol, &mut live, HALF, 1);
    let id = vol.snapshot_create().unwrap();
    write_all(&vol, &mut live, HALF, 2);
    vol.flush().unwrap();
    vol.compact().unwrap();
    let held = allocated(&path);
    let live_bytes = vol.status().live_bytes;

    vol.snapshot_delete(id).unwrap();
    assert!(vol.status().live_bytes < live_bytes);
    vol.compact().unwrap();
    check(&vol, Version::Live, &live);
    vol.close().unwrap();
    drop(vol);
    let freed = allocated(&path);
    assert!(freed < held, "allocated {freed} bytes, {held} before");

    let vol = open(&path, GcMode::Off);
    assert!(vol.snapshots().is_empty());
    check(&vol, Version::Live, &live);
    vol.close().unwrap();
}

#[test]
fn export_matches_block_reads() {
    let path = temp_path("export.img");
    let vol = create(&path, GcMode::Auto);
    let mut live = vec![0u64; BLOCKS as usize];
    // Every third block, so the image has holes.
    for lba in (0..BLOCKS).step_by(3) {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        live[lba as usize] = 1;
    }
    let id = vol.snapshot_create().unwrap();
    for lba in (0..BLOCKS).step_by(5) {
        vol.write(lba, &pattern(lba, 2)).unwrap();
        live[lba as usize] = 2;
    }

    let image = temp_path("export-live.raw");
    vol.export(Version::Live, &image).unwrap();
    let bytes = fs::read(&image).unwrap();
    assert_eq!(bytes.len() as u64, VOLUME);
    for lba in 0..BLOCKS {
        assert!(block(&bytes, lba) == vol.read(lba, 1).unwrap(), "lba {lba}");
        assert!(block(&bytes, lba) == pattern(lba, live[lba as usize]));
    }

    let image = temp_path("export-snapshot.raw");
    vol.export(Version::Snapshot(id), &image).unwrap();
    let bytes = fs::read(&image).unwrap();
    assert_eq!(bytes.len() as u64, VOLUME);
    for lba in 0..BLOCKS {
        assert!(block(&bytes, lba) == vol.read_snapshot(id, lba, 1).unwrap());
    }
    assert!(vol.export(Version::Snapshot(id + 1), &image).is_err());
    vol.close().unwrap();
}
