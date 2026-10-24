use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use bitacora::format::{BLOCK_SIZE, DATA_OFFSET, segment_offset};
use bitacora::medium::{FileMedium, Medium};
use bitacora::{Error, GcMode, Result, Volume, VolumeConfig, VolumeState};

const SEGMENT: u64 = 256 << 10;
const SEG_BLOCKS: u64 = SEGMENT / BLOCK_SIZE as u64;
const VOLUME: u64 = 4 << 20;
const BLOCKS: u64 = VOLUME / BLOCK_SIZE as u64;

fn config() -> VolumeConfig {
    VolumeConfig::new(VOLUME)
        .with_segment_size(SEGMENT)
        .with_gc_mode(GcMode::Off)
}

fn temp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("volume-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn create_with(path: &Path, config: VolumeConfig) -> Volume {
    let medium = Arc::new(FileMedium::create(path, false).unwrap());
    Volume::create(medium, config).unwrap()
}

fn create(path: &Path) -> Volume {
    create_with(path, config())
}

fn open(path: &Path) -> Result<Volume> {
    let medium = Arc::new(FileMedium::open(path, false)?);
    Volume::open(medium, config())
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

fn segment_of(offset: u64) -> u64 {
    (offset - DATA_OFFSET) / SEGMENT
}

#[test]
fn write_and_read_blocks_and_ranges() {
    let vol = create(&temp_path("rw.img"));
    let mut model = vec![0u64; BLOCKS as usize];

    let singles = [0, 1, 7, 513, BLOCKS - 1];
    for lba in singles {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        model[lba as usize] = 1;
    }
    // 300 blocks in one request cross batches and segments.
    let mut range = Vec::new();
    for lba in 100..400 {
        range.extend(pattern(lba, 1));
        model[lba as usize] = 1;
    }
    vol.write(100, &range).unwrap();
    assert_eq!(vol.read(100, 300).unwrap(), range);
    for lba in singles {
        assert_eq!(vol.read(lba, 1).unwrap(), pattern(lba, 1));
    }
    assert!(segment_of(vol.locate(100).unwrap()) < segment_of(vol.locate(399).unwrap()));

    vol.flush().unwrap();
    // These overwrites stay in the open batch; their neighbours come from
    // the medium.
    for lba in (150..170).step_by(3) {
        vol.write(lba, &pattern(lba, 2)).unwrap();
        model[lba as usize] = 2;
    }
    assert_eq!(vol.read(153, 1).unwrap(), pattern(153, 2));
    assert_eq!(vol.read(154, 1).unwrap(), pattern(154, 1));
    check(&vol, &model);
    vol.flush().unwrap();
    check(&vol, &model);

    // A range read that mixes written and never-written blocks.
    let mixed = vol.read(395, 10).unwrap();
    for i in 0..10 {
        let lba = 395 + i;
        assert_eq!(block(&mixed, i), pattern(lba, model[lba as usize]));
    }

    let status = vol.status();
    assert_eq!(status.state, VolumeState::Open);
    assert_eq!(status.lsn, 5 + 300 + 7);
    assert_eq!(status.durable_lsn, status.lsn);
    assert_eq!(status.live_bytes, 305 * BLOCK_SIZE as u64);
    assert_eq!(status.max_segments, config().max_segments());
    assert_eq!(
        status.segments_used + status.segments_free,
        status.max_segments
    );

    assert!(matches!(
        vol.write(0, &[0u8; 100]),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        vol.write(BLOCKS, &pattern(0, 1)),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        vol.read(BLOCKS - 1, 2),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn write_fua_is_durable_on_return() {
    let vol = create(&temp_path("fua.img"));
    vol.write(3, &pattern(3, 1)).unwrap();
    vol.write_fua(4, &pattern(4, 1)).unwrap();
    let status = vol.status();
    assert_eq!(status.lsn, 2);
    assert_eq!(status.durable_lsn, 2);
}

#[test]
fn never_written_blocks_read_as_zeros() {
    let vol = create(&temp_path("zeros.img"));
    let all = vol.read(0, BLOCKS).unwrap();
    assert!(all.iter().all(|&b| b == 0));
    assert_eq!(vol.locate(10), None);

    let mut model = vec![0u64; BLOCKS as usize];
    for lba in [10, 11, 600] {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        model[lba as usize] = 1;
    }
    check(&vol, &model);
    vol.flush().unwrap();
    check(&vol, &model);
}

#[test]
fn clean_close_and_reopen_keeps_content() {
    let path = temp_path("reopen.img");
    let vol = create(&path);
    let mut model = vec![0u64; BLOCKS as usize];
    for i in 0..800u64 {
        let lba = (i * 37 + i / 7) % BLOCKS;
        model[lba as usize] += 1;
        vol.write(lba, &pattern(lba, model[lba as usize])).unwrap();
        if i % 50 == 49 {
            vol.flush().unwrap();
        }
    }
    let before = vol.status();
    vol.close().unwrap();
    assert!(matches!(vol.read(0, 1), Err(Error::Closed)));
    assert!(matches!(vol.write(0, &pattern(0, 9)), Err(Error::Closed)));
    assert_eq!(vol.status().state, VolumeState::Closed);
    drop(vol);

    let vol = open(&path).unwrap();
    check(&vol, &model);
    let after = vol.status();
    assert_eq!(after.lsn, 800);
    assert_eq!(after.durable_lsn, 800);
    assert_eq!(after.live_bytes, before.live_bytes);
    assert_eq!(after.segments_used, before.segments_used);

    // The first write after opening goes to a new hot segment: the free one
    // with the lowest number, right after the recovered ones.
    model[5] += 1;
    vol.write(5, &pattern(5, model[5])).unwrap();
    assert_eq!(segment_of(vol.locate(5).unwrap()), before.segments_used);
    vol.close().unwrap();
    drop(vol);

    let vol = open(&path).unwrap();
    check(&vol, &model);
    assert_eq!(vol.status().lsn, 801);
}

#[test]
fn reopen_without_close_keeps_flushed_writes() {
    let path = temp_path("crash.img");
    let vol = create(&path);
    for lba in 0..200 {
        vol.write(lba, &pattern(lba, 1)).unwrap();
    }
    vol.flush().unwrap();
    for lba in 100..300 {
        vol.write(lba, &pattern(lba, 2)).unwrap();
    }
    // No close: the open batch is lost, as on a crash.
    drop(vol);

    let vol = open(&path).unwrap();
    let all = vol.read(0, 300).unwrap();
    for lba in 0..100 {
        assert_eq!(block(&all, lba), pattern(lba, 1), "flushed lba {lba}");
    }
    // The unflushed writes survive as a prefix of their issue order.
    let k = (100..300)
        .find(|&lba| block(&all, lba) != pattern(lba, 2))
        .unwrap_or(300);
    for lba in k..300 {
        let old = if lba < 200 { 1 } else { 0 };
        assert_eq!(
            block(&all, lba),
            pattern(lba, old),
            "lba {lba} after cut {k}"
        );
    }
    assert_eq!(vol.status().lsn, 200 + (k - 100));
}

#[test]
fn truncated_last_segment_reopens_with_a_prefix() {
    let src = temp_path("truncate-src.img");
    const N: u64 = 300;
    let vol = create(&src);
    for lba in 0..N {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        if lba % 7 == 6 {
            vol.flush().unwrap();
        }
    }
    vol.flush().unwrap();
    let offsets: Vec<u64> = (0..N).map(|lba| vol.locate(lba).unwrap()).collect();
    drop(vol);

    let len = fs::metadata(&src).unwrap().len();
    let last = segment_of(len - 1);
    let seg_start = segment_offset(last, SEGMENT);
    // Blocks in earlier segments were synced before the last one began.
    let synced = offsets.iter().filter(|&&o| segment_of(o) < last).count() as u64;
    assert!(synced > 0 && synced < N);

    let mut points = Vec::new();
    for k in 0..=SEG_BLOCKS {
        let p = seg_start + k * BLOCK_SIZE as u64 + if k % 2 == 1 { 2000 } else { 0 };
        if p < len {
            points.push(p);
        }
    }
    points.push(len);

    let copy = temp_path("truncate-copy.img");
    let mut prev_k = 0;
    for &point in &points {
        fs::copy(&src, &copy).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_len(point)
            .unwrap();

        let vol = open(&copy).unwrap_or_else(|e| panic!("open truncated at {point}: {e}"));
        let all = vol.read(0, N + 1).unwrap();
        let k = (0..N)
            .find(|&lba| block(&all, lba) != pattern(lba, 1))
            .unwrap_or(N);
        for lba in k..=N {
            assert_eq!(
                block(&all, lba),
                pattern(lba, 0),
                "truncated at {point}: lba {lba} after cut {k}"
            );
        }
        assert!(k >= synced, "truncated at {point}: lost synced lba {k}");
        assert!(
            k >= prev_k,
            "truncated at {point}: shorter prefix than before"
        );
        if point == len {
            assert_eq!(k, N);
        }
        prev_k = k;

        // The recovered volume keeps working and recovers again.
        vol.write(N, &pattern(N, 1)).unwrap();
        vol.close().unwrap();
        drop(vol);
        let vol = open(&copy).unwrap();
        let all = vol.read(0, N + 1).unwrap();
        for lba in 0..N {
            let expected = if lba < k { 1 } else { 0 };
            assert_eq!(block(&all, lba), pattern(lba, expected));
        }
        assert_eq!(block(&all, N), pattern(N, 1));
    }
}

fn flip_byte(path: &Path, offset: u64) {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut b = [0u8; 1];
    f.read_exact_at(&mut b, offset).unwrap();
    b[0] ^= 0xff;
    f.write_all_at(&b, offset).unwrap();
}

#[test]
fn flipped_data_byte_reads_as_integrity_error() {
    let path = temp_path("flip.img");
    let vol = create(&path);
    for lba in 0..200 {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        if lba % 10 == 9 {
            vol.flush().unwrap();
        }
    }
    let first = vol.locate(0).unwrap();
    let last = vol.locate(199).unwrap();
    assert!(segment_of(first) < segment_of(last));
    vol.close().unwrap();
    drop(vol);

    // One block in an old segment and one in the last batch of the last
    // segment, which was durable at close: corruption, not a torn write.
    flip_byte(&path, first + 123);
    flip_byte(&path, last + 4000);
    let vol = open(&path).unwrap();
    assert!(matches!(vol.read(0, 1), Err(Error::Integrity { lba: 0 })));
    assert!(matches!(
        vol.read(199, 1),
        Err(Error::Integrity { lba: 199 })
    ));
    assert!(matches!(vol.read(0, 200), Err(Error::Integrity { lba: 0 })));
    assert!(matches!(
        vol.read(190, 10),
        Err(Error::Integrity { lba: 199 })
    ));
    assert_eq!(vol.read(1, 1).unwrap(), pattern(1, 1));
    assert_eq!(vol.read(198, 1).unwrap(), pattern(198, 1));
    assert_eq!(vol.status().lsn, 200);

    flip_byte(&path, first + 123);
    flip_byte(&path, last + 4000);
    assert_eq!(vol.read(0, 1).unwrap(), pattern(0, 1));
    assert_eq!(vol.read(199, 1).unwrap(), pattern(199, 1));
}

#[test]
fn lost_synced_batch_fails_open_with_corrupt() {
    let path = temp_path("lost.img");
    let vol = create(&path);
    let mut offsets = Vec::new();
    // Batches of 5 blocks, each flushed, until the current segment holds at
    // least four batches.
    let mut lba = 0;
    let target = loop {
        vol.write(lba, &pattern(lba, 1)).unwrap();
        offsets.push(vol.locate(lba).unwrap());
        lba += 1;
        if lba % 5 == 0 {
            vol.flush().unwrap();
            let seg = segment_of(offsets[lba as usize - 1]);
            let starts: Vec<u64> = (0..lba as usize)
                .filter(|&i| segment_of(offsets[i]) == seg)
                .filter(|&i| i == 0 || offsets[i - 1] + BLOCK_SIZE as u64 != offsets[i])
                .map(|i| offsets[i])
                .collect();
            if starts.len() >= 4 {
                break starts[1];
            }
        }
    };
    vol.close().unwrap();
    drop(vol);

    // Break the summary of an early batch of the last segment: later batches
    // report it as synced, so the loss is detected.
    let summary = target - BLOCK_SIZE as u64;
    flip_byte(&path, summary + 100);
    assert!(matches!(open(&path), Err(Error::Corrupt(_))));

    flip_byte(&path, summary + 100);
    let vol = open(&path).unwrap();
    for l in 0..lba {
        assert_eq!(vol.read(l, 1).unwrap(), pattern(l, 1));
    }
}

/// A `FileMedium` whose writes fail once `fail` is set.
struct FlakyMedium {
    inner: FileMedium,
    fail: Arc<AtomicBool>,
}

impl Medium for FlakyMedium {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.inner.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Io(io::Error::other("injected write failure")));
        }
        self.inner.write_at(buf, offset)
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    fn len(&self) -> Result<u64> {
        self.inner.len()
    }

    fn punch_hole(&self, offset: u64, len: u64) -> Result<()> {
        self.inner.punch_hole(offset, len)
    }
}

#[test]
fn medium_write_error_fails_stop() {
    let fail = Arc::new(AtomicBool::new(false));
    let medium = Arc::new(FlakyMedium {
        inner: FileMedium::create(&temp_path("flaky.img"), false).unwrap(),
        fail: fail.clone(),
    });
    let vol = Volume::create(medium, config()).unwrap();
    for lba in 0..10 {
        vol.write(lba, &pattern(lba, 1)).unwrap();
    }
    vol.flush().unwrap();

    fail.store(true, Ordering::SeqCst);
    vol.write(10, &pattern(10, 1)).unwrap();
    assert!(matches!(vol.flush(), Err(Error::Io(_))));
    assert_eq!(vol.status().state, VolumeState::Failed);

    // Nothing is acknowledged any more, even once the medium recovers.
    fail.store(false, Ordering::SeqCst);
    assert!(matches!(vol.write(11, &pattern(11, 1)), Err(Error::Failed)));
    assert!(matches!(
        vol.write_fua(11, &pattern(11, 1)),
        Err(Error::Failed)
    ));
    assert!(matches!(vol.read(0, 1), Err(Error::Failed)));
    assert!(matches!(vol.flush(), Err(Error::Failed)));
    assert!(matches!(vol.close(), Err(Error::Failed)));
    assert_eq!(vol.status().state, VolumeState::Failed);
}

#[test]
fn filling_max_segments_returns_no_space() {
    let small = VolumeConfig::new(SEGMENT)
        .with_segment_size(SEGMENT)
        .with_gc_mode(GcMode::Off);
    let max = small.max_segments();
    let vol = create_with(&temp_path("full.img"), small);
    let blocks = vol.blocks();

    let mut written = 0u64;
    let err = loop {
        let lba = written % blocks;
        match vol.write(lba, &pattern(lba, written / blocks + 1)) {
            Ok(()) => written += 1,
            Err(e) => break e,
        }
    };
    assert!(matches!(err, Error::NoSpace), "{err}");
    // Each full segment holds a header, one summary and the rest data.
    assert_eq!(written, max * (SEG_BLOCKS - 2));

    let status = vol.status();
    assert_eq!(status.state, VolumeState::Open);
    assert_eq!(status.segments_free, 0);
    assert_eq!(status.segments_used, max);
    vol.flush().unwrap();
    for lba in 0..blocks {
        let last = (0..written).rev().find(|w| w % blocks == lba).unwrap();
        assert_eq!(vol.read(lba, 1).unwrap(), pattern(lba, last / blocks + 1));
    }
    assert!(matches!(vol.write(0, &pattern(0, 99)), Err(Error::NoSpace)));
}

#[test]
fn volume_is_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Volume>();

    let path = temp_path("threads.img");
    let vol = Arc::new(create(&path));
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let vol = Arc::clone(&vol);
            thread::spawn(move || {
                for i in 0..200 {
                    let lba = t * 200 + i;
                    vol.write(lba, &pattern(lba, 1)).unwrap();
                    if i % 25 == 24 {
                        vol.flush().unwrap();
                    }
                    assert_eq!(vol.read(lba, 1).unwrap(), pattern(lba, 1));
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }

    let mut model = vec![0u64; BLOCKS as usize];
    model[..800].fill(1);
    check(&vol, &model);
    assert_eq!(vol.status().lsn, 800);
    vol.close().unwrap();
    drop(vol);
    check(&open(&path).unwrap(), &model);
}

#[test]
fn create_and_open_reject_bad_arguments() {
    let path = temp_path("args.img");
    let vol = create(&path);
    vol.close().unwrap();
    drop(vol);

    let medium = Arc::new(FileMedium::open(&path, false).unwrap());
    assert!(matches!(
        Volume::create(medium, config()),
        Err(Error::InvalidArgument(_))
    ));
    let medium = Arc::new(FileMedium::open(&path, false).unwrap());
    assert!(matches!(
        Volume::open(medium, VolumeConfig::new(VOLUME)),
        Err(Error::InvalidArgument(_))
    ));
    let medium = Arc::new(FileMedium::create(&temp_path("bad-seg.img"), false).unwrap());
    assert!(matches!(
        Volume::create(medium, VolumeConfig::new(VOLUME).with_segment_size(3 << 20)),
        Err(Error::InvalidArgument(_))
    ));

    // A volume never written opens empty.
    let vol = open(&path).unwrap();
    assert_eq!(vol.status().lsn, 0);
    assert!(vol.read(0, 8).unwrap().iter().all(|&b| b == 0));
}
