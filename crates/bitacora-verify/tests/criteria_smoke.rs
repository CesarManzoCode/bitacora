use std::fs;
use std::path::PathBuf;

use bitacora::medium::{FileMedium, Medium};
use bitacora_verify::criteria::c06::CountingMedium;
use bitacora_verify::criteria::{c05, c09, c10, c13, c17, c18};

fn test_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("criteria-smoke")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn reduced_c05_loses_no_update() {
    let path = test_dir("c05").join("volume.img");
    let p = c05::Params {
        volume_size: 4 << 20,
        segment_size: 256 << 10,
        threads: 4,
        blocks: 256,
        writes_per_thread: 500,
    };
    for seed in 1..=2 {
        assert_eq!(
            c05::lost_updates(&path, seed, &p).unwrap(),
            0,
            "seed {seed}"
        );
    }
}

#[test]
fn reduced_c09_loses_no_acknowledged_write() {
    let p = c09::Params {
        volume_size: 4 << 20,
        segment_size: 256 << 10,
        ops: 1_500,
    };
    for seed in 1..=5 {
        let r = c09::trial(seed, &p).unwrap();
        assert_eq!(r.lost, 0, "seed {seed}");
        assert!(r.prefix, "seed {seed}");
        assert_eq!(
            c09::trial(seed, &p).unwrap(),
            r,
            "seed {seed} not repeatable"
        );
    }
}

#[test]
fn reduced_c10_acknowledges_nothing_after_an_error() {
    let p = c10::Params {
        volume_size: 4 << 20,
        segment_size: 256 << 10,
        writes: 600,
        flush_every: 16,
    };
    for n in 1..=12 {
        for case in [c10::Case::Write, c10::Case::Sync] {
            assert_eq!(
                c10::trial(case, n, 1, &p).unwrap(),
                0,
                "{} {n}",
                case.name()
            );
        }
    }
}

#[test]
fn counting_medium_adds_exactly_the_bytes_written() {
    let path = test_dir("counting").join("medium.img");
    let medium = CountingMedium::new(FileMedium::create(&path, false).unwrap());
    let sizes = [4096usize, 512, 8192, 1, 12288];
    let mut offset = 0u64;
    for (i, &len) in sizes.iter().enumerate() {
        medium.write_at(&vec![i as u8 + 1; len], offset).unwrap();
        offset += len as u64;
    }
    medium.sync().unwrap();
    medium.punch_hole(0, 4096).unwrap();
    let mut buf = vec![0u8; 512];
    medium.read_at(&mut buf, 4096).unwrap();
    let total: u64 = sizes.iter().map(|&s| s as u64).sum();
    assert_eq!(medium.written(), total);
    assert_eq!(fs::metadata(&path).unwrap().len(), total);
    medium.reset();
    assert_eq!(medium.written(), 0);
    medium.write_at(&[9; 4096], 0).unwrap();
    assert_eq!(medium.written(), 4096);
}

#[test]
fn reduced_c13_restores_the_incremental_snapshot() {
    let p = c13::Params {
        volume_size: 4 << 20,
        segment_size: 256 << 10,
        before: 600,
        changed: 200,
        flush_every: 32,
    };
    let r = c13::exercise(&test_dir("c13"), 1, &p).unwrap();
    assert!(r.identical());
    assert!(r.size_ok());
    assert_eq!(r.entries, r.modified);
    assert!(r.concurrent_writes > 0);
}

#[test]
fn reduced_c17_loses_no_acknowledged_write_on_failover() {
    let dir = test_dir("c17");
    let p = c17::Params {
        volume_size: 64 << 20,
        segment_size: 1 << 20,
        min_writes: 1,
        max_writes: 50,
    };
    for seed in 1..=2 {
        let t = c17::trial(&dir, seed, &p).unwrap();
        assert_eq!(t.lost, 0, "seed {seed}");
        assert!(t.live_identical, "seed {seed}");
    }
}

#[test]
fn reduced_c18_accepts_nothing_from_the_old_epoch() {
    let dir = test_dir("c18");
    let p = c18::Params {
        volume_size: 64 << 20,
        segment_size: 1 << 20,
        min_writes: 1,
        max_writes: 50,
        forged_entries: 4,
    };
    for seed in 1..=2 {
        let t = c18::trial(&dir, seed, &p).unwrap();
        assert_eq!(t.accepted(), 0, "seed {seed}");
    }
}
