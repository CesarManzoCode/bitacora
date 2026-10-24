use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use bitacora::format::{BLOCK_SIZE, DATA_OFFSET};
use bitacora::medium::{FileMedium, Medium};
use bitacora::{GcMode, Volume, VolumeConfig};

const SEGMENT: u64 = 256 << 10;
const VOLUME: u64 = 4 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;

fn config(mode: GcMode) -> VolumeConfig {
    VolumeConfig::new(VOLUME)
        .with_segment_size(SEGMENT)
        .with_gc_mode(mode)
}

fn temp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gc-tests");
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

/// Reads the whole volume and compares it with the model (version per lba).
fn check(vol: &Volume, model: &[u64]) {
    let all = vol.read(0, model.len() as u64).unwrap();
    for (lba, &version) in model.iter().enumerate() {
        assert!(
            block(&all, lba as u64) == pattern(lba as u64, version),
            "lba {lba} differs from version {version}"
        );
    }
}

/// Small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn max_file_len() -> u64 {
    DATA_OFFSET + config(GcMode::Auto).max_segments() * SEGMENT
}

#[test]
fn auto_gc_keeps_overwrites_within_max_segments() {
    let path = temp_path("auto.img");
    let vol = create(&path, GcMode::Auto);
    let max = config(GcMode::Auto).max_segments();
    let mut model = vec![0u64; BLOCKS as usize];
    let mut rng = Rng(0x5EED_0001);

    // Fill once, then overwrite the volume ten times over at random.
    for lba in 0..BLOCKS {
        model[lba as usize] = 1;
        vol.write(lba, &pattern(lba, 1)).unwrap();
    }
    for i in 0..10 * BLOCKS {
        let lba = rng.below(BLOCKS);
        model[lba as usize] += 1;
        vol.write(lba, &pattern(lba, model[lba as usize])).unwrap();
        if i % 64 == 0 {
            vol.flush().unwrap();
        }
        let status = vol.status();
        assert!(status.segments_used <= max);
    }
    vol.flush().unwrap();

    let status = vol.status();
    assert!(status.gc.segments_cleaned > 0, "{status:?}");
    assert!(status.gc.bytes_relocated > 0, "{status:?}");
    assert!(status.segments_used <= max);
    assert!(fs::metadata(&path).unwrap().len() <= max_file_len());
    check(&vol, &model);
    vol.close().unwrap();

    let vol = open(&path, GcMode::Off);
    check(&vol, &model);
    vol.close().unwrap();
}

#[test]
fn continuous_gc_with_two_writers_keeps_each_last_write() {
    const SHARED: u64 = 128;
    const ROUNDS: u64 = 200;
    let path = temp_path("continuous.img");
    let vol = Arc::new(create(&path, GcMode::Continuous));

    // Writer t writes version `t + 1 + 2 × round` of every shared block.
    let writers: Vec<_> = (0..2u64)
        .map(|t| {
            let vol = vol.clone();
            thread::spawn(move || {
                let mut rng = Rng(0xC0FF_EE00 + t);
                for round in 0..ROUNDS {
                    let version = t + 1 + 2 * round;
                    let mut order: Vec<u64> = (0..SHARED).collect();
                    for i in (1..order.len()).rev() {
                        order.swap(i, rng.below(i as u64 + 1) as usize);
                    }
                    for lba in order {
                        vol.write(lba, &pattern(lba, version)).unwrap();
                    }
                    if round % 8 == 0 {
                        vol.flush().unwrap();
                    }
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    vol.flush().unwrap();

    // The final value of each block is the last write of one of the two
    // writers, never an older version brought back by a relocation.
    let last = [1 + 2 * (ROUNDS - 1), 2 + 2 * (ROUNDS - 1)];
    let all = vol.read(0, SHARED).unwrap();
    for lba in 0..SHARED {
        let got = block(&all, lba);
        assert!(
            last.iter().any(|&v| got == pattern(lba, v)),
            "lba {lba} is not the last write of either writer"
        );
    }
    assert!(vol.status().gc.segments_cleaned > 0);
    assert!(vol.status().segments_used <= vol.status().max_segments);
    vol.close().unwrap();
}

#[test]
fn compact_leaves_no_dead_data_in_sealed_segments() {
    let path = temp_path("compact.img");
    let vol = create(&path, GcMode::Off);
    let mut model = vec![0u64; BLOCKS as usize];
    let mut rng = Rng(0x5EED_0003);
    for lba in 0..BLOCKS {
        model[lba as usize] = 1;
        vol.write(lba, &pattern(lba, 1)).unwrap();
    }
    for _ in 0..BLOCKS / 4 {
        let lba = rng.below(BLOCKS);
        model[lba as usize] += 1;
        vol.write(lba, &pattern(lba, model[lba as usize])).unwrap();
    }
    vol.flush().unwrap();
    let before = vol.status();

    vol.compact().unwrap();

    let usage = vol.segment_usage();
    for u in usage.iter().filter(|u| u.sealed) {
        assert_eq!(
            u.live_blocks, u.data_blocks,
            "sealed segment {} keeps dead data: {u:?}",
            u.index
        );
    }
    let after = vol.status();
    assert!(after.gc.segments_cleaned > 0);
    assert!(after.segments_used <= before.segments_used);
    assert!(!after.gc.running);
    check(&vol, &model);
    vol.close().unwrap();

    let vol = open(&path, GcMode::Off);
    check(&vol, &model);
    vol.close().unwrap();
}

#[test]
fn reopen_after_gc_without_close_keeps_everything_flushed() {
    let path = temp_path("crash.img");
    let mut model = vec![0u64; BLOCKS as usize];
    // Versions written after the last flush: any of them may survive.
    let mut after_flush: Vec<Vec<u64>> = vec![Vec::new(); BLOCKS as usize];
    {
        let vol = create(&path, GcMode::Continuous);
        let mut rng = Rng(0x5EED_0004);
        for lba in 0..BLOCKS {
            model[lba as usize] = 1;
            vol.write(lba, &pattern(lba, 1)).unwrap();
        }
        for i in 0..4 * BLOCKS {
            let lba = rng.below(BLOCKS);
            model[lba as usize] += 1;
            vol.write(lba, &pattern(lba, model[lba as usize])).unwrap();
            if i % 100 == 0 {
                vol.flush().unwrap();
            }
        }
        vol.flush().unwrap();
        vol.compact().unwrap();
        assert!(vol.status().gc.segments_cleaned > 0);

        let mut version = model.clone();
        for _ in 0..300 {
            let lba = rng.below(BLOCKS);
            version[lba as usize] += 1;
            after_flush[lba as usize].push(version[lba as usize]);
            vol.write(lba, &pattern(lba, version[lba as usize])).unwrap();
        }
        // Dropped without `close`: no orderly shutdown.
    }

    let medium: Arc<dyn Medium> = Arc::new(FileMedium::open(&path, false).unwrap());
    let vol = Volume::open(medium, config(GcMode::Auto)).unwrap();
    let all = vol.read(0, BLOCKS).unwrap();
    for lba in 0..BLOCKS {
        let got = block(&all, lba);
        let flushed = model[lba as usize];
        assert!(
            got == pattern(lba, flushed)
                || after_flush[lba as usize]
                    .iter()
                    .any(|&v| got == pattern(lba, v)),
            "lba {lba} lost its flushed version {flushed}"
        );
    }
    vol.close().unwrap();
}
