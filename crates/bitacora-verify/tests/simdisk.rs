use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use bitacora::medium::{FileMedium, Medium};
use bitacora::{GcMode, Volume, VolumeConfig};
use bitacora_verify::criteria::{Ctx, c01, c08, scan};
use bitacora_verify::model::Model;
use bitacora_verify::rng::block_data;
use bitacora_verify::simdisk::SimDisk;

fn test_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("verify-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn read(disk: &dyn Medium, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = disk.read_at(&mut buf, offset).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn synced_writes_survive_every_crash() {
    let disk = SimDisk::new(1);
    disk.write_at(&[0xAA; 8192], 0).unwrap();
    disk.sync().unwrap();
    disk.write_at(&[0xBB; 4096], 8192).unwrap();
    disk.write_at(&[0xCC; 4096], 16384).unwrap();
    assert_eq!(disk.pending_writes(), 2);
    for seed in 0..256 {
        let crashed = disk.crash(seed);
        assert_eq!(read(&crashed, 0, 8192), vec![0xAA; 8192], "seed {seed}");
        assert_eq!(crashed.pending_writes(), 0);
    }
}

#[test]
fn crash_is_a_function_of_the_seed() {
    let disk = SimDisk::new(1);
    disk.write_at(&[1; 4096], 0).unwrap();
    disk.sync().unwrap();
    for i in 0..16u8 {
        disk.write_at(&[i + 2; 4096], u64::from(i) * 4096).unwrap();
    }
    let first = disk.crash(1).image();
    assert_eq!(disk.crash(1).image(), first);
    assert!((2..64).any(|seed| disk.crash(seed).image() != first));
}

#[test]
fn crash_tears_at_sector_boundaries() {
    let disk = SimDisk::new(1);
    disk.write_at(&[0; 8192], 0).unwrap();
    disk.sync().unwrap();
    disk.write_at(&[7; 8192], 0).unwrap();
    for seed in 0..256 {
        let image = disk.crash(seed).image();
        let written = image.iter().take_while(|&&b| b == 7).count();
        assert_eq!(written % 512, 0, "seed {seed}");
        assert!(image[written..].iter().all(|&b| b == 0), "seed {seed}");
    }
}

#[test]
fn fail_write_at_fails_exactly_that_write() {
    let disk = SimDisk::new(1);
    disk.fail_write_at(3);
    let results: Vec<bool> = (0..5u8)
        .map(|i| disk.write_at(&[i + 1; 512], u64::from(i) * 512).is_ok())
        .collect();
    assert_eq!(results, [true, true, false, true, true]);
    assert_eq!(disk.writes(), 5);
    assert_eq!(read(&disk, 1024, 512), vec![0; 512]);
    assert_eq!(read(&disk, 1536, 512), vec![4; 512]);
}

#[test]
fn failed_sync_keeps_earlier_durable_data() {
    let disk = SimDisk::new(3);
    disk.fail_sync_at(2);
    disk.write_at(&[1; 4096], 0).unwrap();
    assert!(disk.sync().is_ok());
    for i in 1..9u64 {
        disk.write_at(&[2; 4096], i * 4096).unwrap();
    }
    assert!(disk.sync().is_err());
    assert!(disk.pending_writes() <= 8);
    assert!(disk.sync().is_ok());
    assert_eq!(disk.syncs(), 3);
    assert_eq!(read(&disk.crash(1), 0, 4096), vec![1; 4096]);
}

#[test]
fn block_data_is_deterministic() {
    assert_eq!(block_data(1, 7, 1), block_data(1, 7, 1));
    assert_ne!(block_data(1, 7, 1), block_data(1, 7, 2));
    assert_ne!(block_data(1, 7, 1), block_data(1, 8, 1));
    assert_ne!(block_data(1, 7, 1), block_data(2, 7, 1));
}

#[test]
fn flushed_writes_survive_a_simulated_crash() {
    let config = VolumeConfig::new(4 << 20)
        .with_segment_size(256 << 10)
        .with_gc_mode(GcMode::Off);
    let disk = Arc::new(SimDisk::new(1));
    let vol = Volume::create(disk.clone(), config).unwrap();
    let mut model = Model::new(1, vol.blocks());
    for lba in 0..300 {
        vol.write(lba, &model.write(lba)).unwrap();
    }
    vol.flush().unwrap();
    let mut extra = Model::new(1, vol.blocks());
    for lba in 0..300 {
        extra.bump(lba);
        vol.write(lba, &extra.write(lba)).unwrap();
    }
    for seed in 0..8 {
        let crashed = Arc::new(disk.crash(seed));
        let reopened = Volume::open(crashed, config).unwrap();
        assert!(reopened.status().lsn >= 300, "seed {seed}");
        let k = reopened.status().lsn;
        let mut prefix = Model::new(1, reopened.blocks());
        for lba in 0..300 {
            prefix.bump(lba);
        }
        for lba in 0..k - 300 {
            prefix.bump(lba);
        }
        assert_eq!(scan(&reopened, &prefix).diffs, 0, "seed {seed}");
    }
    drop(vol);
}

#[test]
fn reduced_c01_and_c03_find_no_differences() {
    let dir = test_dir("c01");
    let path = dir.join("volume.img");
    let p = c01::Params {
        volume_size: 4 << 20,
        segment_size: 256 << 10,
        ops: 2_000,
    };
    assert_eq!(c01::exercise(&path, 1, &p).unwrap(), 0);
    let vol = Volume::open(
        Arc::new(FileMedium::open(&path, false).unwrap()),
        p.config(),
    )
    .unwrap();
    assert_eq!(scan(&vol, &c01::model(1, &p)).diffs, 0);
    vol.close().unwrap();
}

#[test]
fn reduced_c08_gives_valid_prefixes() {
    let mut ctx = Ctx::quiet(test_dir("c08"), 1);
    let p = c08::Params {
        volume_size: 4 << 20,
        segment_size: 1 << 20,
        writes: 400,
        trials: 30,
    };
    assert_eq!(c08::exercise(&mut ctx, &p).unwrap(), p.trials);
}
