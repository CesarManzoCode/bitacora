use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use bitacora::backup::{self, BackupHeader, ENTRY_LEN, FOOTER_LEN, HEADER_LEN};
use bitacora::format::BLOCK_SIZE;
use bitacora::medium::FileMedium;
use bitacora::{GcMode, Version, Volume, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
const VOLUME: u64 = 4 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;

fn config() -> VolumeConfig {
    VolumeConfig::new(VOLUME)
        .with_segment_size(SEGMENT)
        .with_gc_mode(GcMode::Auto)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("backup-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn create(path: &Path) -> Volume {
    let medium = Arc::new(FileMedium::create(path, false).unwrap());
    Volume::create(medium, config()).unwrap()
}

fn restore<P: AsRef<Path>>(path: &Path, backups: &[P]) -> bitacora::Result<Volume> {
    let _ = fs::remove_file(path);
    let medium = Arc::new(FileMedium::create(path, false).unwrap());
    backup::restore(backups, medium, config())
}

/// Content of version `version` of `lba`.
fn pattern(lba: u64, version: u64) -> Vec<u8> {
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

/// Writes version `version` of every LBA in `lbas` and flushes.
fn write_all(vol: &Volume, lbas: impl Iterator<Item = u64>, version: u64) {
    for lba in lbas {
        vol.write(lba, &pattern(lba, version)).unwrap();
    }
    vol.flush().unwrap();
}

fn image(vol: &Volume, version: Version) -> Vec<u8> {
    vol.read_at_version(version, 0, BLOCKS).unwrap()
}

fn header(path: &Path) -> BackupHeader {
    backup::verify(path).unwrap()
}

/// A volume with a full backup and two incrementals, each of its own
/// snapshot.
struct Chain {
    vol: Volume,
    full: PathBuf,
    first: PathBuf,
    second: PathBuf,
    last_snapshot: u64,
}

fn chain(dir: &Path) -> Chain {
    let vol = create(&dir.join("volume.img"));
    write_all(&vol, 0..BLOCKS / 2, 1);
    let s1 = vol.snapshot_create().unwrap();
    let full = dir.join("full.bkp");
    vol.backup_full(s1, &full).unwrap();

    write_all(&vol, (0..BLOCKS).step_by(7), 2);
    let s2 = vol.snapshot_create().unwrap();
    let first = dir.join("first.bkp");
    vol.backup_incremental(s1, s2, &first).unwrap();

    write_all(&vol, (3..BLOCKS).step_by(11), 3);
    let s3 = vol.snapshot_create().unwrap();
    let second = dir.join("second.bkp");
    vol.backup_incremental(s2, s3, &second).unwrap();
    Chain {
        vol,
        full,
        first,
        second,
        last_snapshot: s3,
    }
}

#[test]
fn full_and_two_incrementals_restore_the_exact_image() {
    let dir = temp_dir("chain");
    let c = chain(&dir);
    let restored = restore(&dir.join("restored.img"), &[&c.full, &c.first, &c.second]).unwrap();
    assert!(image(&restored, Version::Live) == image(&c.vol, Version::Snapshot(c.last_snapshot)));

    // The raw exports match byte for byte too.
    let a = dir.join("snapshot.raw");
    let b = dir.join("restored.raw");
    c.vol
        .export(Version::Snapshot(c.last_snapshot), &a)
        .unwrap();
    restored.export(Version::Live, &b).unwrap();
    assert!(fs::read(&a).unwrap() == fs::read(&b).unwrap());

    // The chain is linked by LSN.
    let (f, i1, i2) = (header(&c.full), header(&c.first), header(&c.second));
    assert_eq!(f.base_lsn, 0);
    assert_eq!(f.count, BLOCKS / 2);
    assert_eq!(i1.base_lsn, f.target_lsn);
    assert_eq!(i2.base_lsn, i1.target_lsn);
    assert_eq!(i1.count, BLOCKS.div_ceil(7));
    assert_eq!(
        fs::metadata(&c.first).unwrap().len(),
        (HEADER_LEN + FOOTER_LEN) as u64 + i1.count * ENTRY_LEN as u64
    );
    restored.close().unwrap();
    c.vol.close().unwrap();
}

#[test]
fn out_of_order_or_missing_incrementals_are_rejected() {
    let dir = temp_dir("order");
    let c = chain(&dir);
    let target = dir.join("restored.img");
    assert!(restore(&target, &[&c.full, &c.second, &c.first]).is_err());
    assert!(restore(&target, &[&c.full, &c.second]).is_err());
    assert!(restore(&target, &[&c.first, &c.second]).is_err());
    assert!(restore(&target, &[&c.full, &c.full]).is_err());
    assert!(restore::<&Path>(&target, &[]).is_err());
    c.vol.close().unwrap();
}

#[test]
fn a_flipped_byte_is_rejected() {
    let dir = temp_dir("flip");
    let c = chain(&dir);
    let original = fs::read(&c.first).unwrap();
    // A data byte of the first entry, a header byte and the footer.
    for at in [HEADER_LEN + 16 + 100, 40, original.len() - 1] {
        let mut bad = original.clone();
        bad[at] ^= 0x01;
        let path = dir.join(format!("flipped-{at}.bkp"));
        fs::write(&path, &bad).unwrap();
        assert!(backup::verify(&path).is_err(), "byte {at}");
        assert!(
            restore(&dir.join("restored.img"), &[&c.full, &path]).is_err(),
            "byte {at}"
        );
    }
    c.vol.close().unwrap();
}

#[test]
fn an_incremental_without_changes_has_only_header_and_footer() {
    let dir = temp_dir("empty");
    let c = chain(&dir);
    let again = c.vol.snapshot_create().unwrap();
    let empty = dir.join("empty.bkp");
    let h = c
        .vol
        .backup_incremental(c.last_snapshot, again, &empty)
        .unwrap();
    assert_eq!(h.count, 0);
    assert_eq!(
        fs::metadata(&empty).unwrap().len(),
        (HEADER_LEN + FOOTER_LEN) as u64
    );
    let restored = restore(
        &dir.join("restored.img"),
        &[&c.full, &c.first, &c.second, &empty],
    )
    .unwrap();
    assert!(image(&restored, Version::Live) == image(&c.vol, Version::Snapshot(again)));
    restored.close().unwrap();
    c.vol.close().unwrap();
}

#[test]
fn a_backup_runs_while_another_thread_writes() {
    let dir = temp_dir("online");
    let vol = create(&dir.join("volume.img"));
    write_all(&vol, 0..BLOCKS / 2, 1);
    let snap = vol.snapshot_create().unwrap();
    let expected = image(&vol, Version::Snapshot(snap));
    let full = dir.join("full.bkp");
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let writer = s.spawn(|| {
            let mut version = 2;
            while !stop.load(Ordering::Acquire) {
                write_all(&vol, (0..BLOCKS / 2).step_by(5), version);
                version += 1;
            }
        });
        vol.backup_full(snap, &full).unwrap();
        stop.store(true, Ordering::Release);
        writer.join().unwrap();
    });
    let restored = restore(&dir.join("restored.img"), &[&full]).unwrap();
    assert!(image(&restored, Version::Live) == expected);
    restored.close().unwrap();
    vol.close().unwrap();
}
