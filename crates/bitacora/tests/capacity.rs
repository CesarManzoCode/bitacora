use bitacora::capacity::{
    self, History, PREFIX_ALLOCATED_BYTES, Projection, physical_cap, segment_payload_blocks,
};
use bitacora::format::{BLOCK_SIZE_U64, DEFAULT_SEGMENT_SIZE};

const MIB: u64 = 1 << 20;

fn history(volume_size: u64, writes: u64, overwrite: f64, snapshots: u64, keep: u64) -> History {
    History {
        volume_size,
        segment_size: DEFAULT_SEGMENT_SIZE,
        write_blocks_per_day: writes,
        overwrite_fraction: overwrite,
        snapshots_per_day: snapshots,
        snapshot_retention_days: keep,
    }
}

fn projected(h: &History, days: u64) -> u64 {
    capacity::project(h, days).unwrap().projected_physical_bytes
}

#[test]
fn without_overwrites_or_snapshots_the_projection_is_the_data_plus_open_segments() {
    // One open segment: header, one summary per batch of up to 128 blocks
    // and the data; the rest of the segment is never written.
    for (writes, blocks) in [(1, 3), (128, 130), (129, 132), (1000, 1009)] {
        let h = history(256 * MIB, writes, 0.0, 0, 0);
        assert_eq!(
            projected(&h, 1),
            PREFIX_ALLOCATED_BYTES + blocks * BLOCK_SIZE_U64,
            "{writes} blocks"
        );
    }

    let payload = segment_payload_blocks(DEFAULT_SEGMENT_SIZE);
    for (volume, writes, days) in [
        (256 * MIB, 1000, 30),
        (256 * MIB, 128 * 50, 10),
        (256 * MIB, 4097, 7),
        (64 * MIB, 500, 30),
    ] {
        let h = history(volume, writes, 0.0, 0, 0);
        let data = writes * days;
        assert!(data <= h.blocks());
        let p = projected(&h, days);
        let last = *capacity::simulate(&h, days).unwrap().last().unwrap();
        assert!(!last.gc_active && !last.no_space);
        assert_eq!(last.written_blocks, data);
        assert_eq!(p, PREFIX_ALLOCATED_BYTES + last.hot_blocks * BLOCK_SIZE_U64);
        // Every data block is there, inside no more than the sealed
        // segments the data fills plus the open one.
        assert!(p >= PREFIX_ALLOCATED_BYTES + data * BLOCK_SIZE_U64);
        assert!(
            p <= PREFIX_ALLOCATED_BYTES + (data.div_ceil(payload) + 1) * DEFAULT_SEGMENT_SIZE,
            "{writes} blocks a day for {days} days: {p}"
        );
    }
}

#[test]
fn more_overwrites_never_project_less_at_the_same_write_rate() {
    // Loads whose referenced versions stay within the level the GC keeps.
    let mut cases = Vec::new();
    for volume in [64 * MIB, 256 * MIB] {
        for writes in [1000, 4000, 12000, 100_000] {
            cases.push((volume, writes, 0, 0));
        }
    }
    for writes in [1000, 4000, 12000] {
        cases.push((256 * MIB, writes, 1, 1));
    }
    for (volume, writes, snapshots, keep) in cases {
        for days in [1, 10, 30] {
            let mut previous = 0;
            for tenth in 0..=10 {
                let f = f64::from(tenth) / 10.0;
                let p = projected(&history(volume, writes, f, snapshots, keep), days);
                assert!(
                    p >= previous,
                    "volume {volume}, {writes} blocks a day, {days} days: \
                     overwrite fraction {f} projects {p} < {previous}"
                );
                previous = p;
            }
        }
    }
}

#[test]
fn the_projection_never_exceeds_max_segments() {
    for (volume, segment) in [
        (4 * MIB, 256 << 10),
        (64 * MIB, DEFAULT_SEGMENT_SIZE),
        (256 * MIB, DEFAULT_SEGMENT_SIZE),
    ] {
        let cap = physical_cap(volume, segment);
        for writes in [1, 1000, 100_000, 1_000_000] {
            for f in [0.0, 0.5, 1.0] {
                for (snapshots, keep) in [(0, 0), (1, 1), (4, 10), (8, 7)] {
                    let h = History {
                        segment_size: segment,
                        ..history(volume, writes, f, snapshots, keep)
                    };
                    for day in capacity::simulate(&h, 120).unwrap() {
                        assert!(
                            day.physical_bytes <= cap,
                            "{h:?} day {}: {} > {cap}",
                            day.day,
                            day.physical_bytes
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn history_and_projection_use_the_contract_keys() {
    let text = r#"{"volume_size": 268435456, "segment_size": 8388608,
        "write_blocks_per_day": 5000, "overwrite_fraction": 0.7,
        "snapshots_per_day": 1, "snapshot_retention_days": 2}"#;
    let h = History::from_json(text).unwrap();
    assert_eq!(h, history(256 * MIB, 5000, 0.7, 1, 2));
    assert_eq!(History::from_json(&h.to_json().unwrap()).unwrap(), h);

    let p = capacity::project(&h, 30).unwrap();
    let json = p.to_json().unwrap();
    assert_eq!(
        json,
        format!(
            "{{\"projected_physical_bytes\":{}}}",
            p.projected_physical_bytes
        )
    );
    assert_eq!(Projection::from_json(&json).unwrap(), p);
}

#[test]
fn invalid_histories_are_rejected() {
    assert!(History::from_json("{}").is_err());
    for h in [
        history(256 * MIB, 10, 1.5, 0, 0),
        history(256 * MIB, 10, -0.1, 0, 0),
        history(4095, 10, 0.0, 0, 0),
        // 8 snapshots a day kept 8 days: 72 would be retained, over 64.
        history(256 * MIB, 10, 0.0, 8, 8),
        History {
            segment_size: 3 << 20,
            ..history(256 * MIB, 10, 0.0, 0, 0)
        },
    ] {
        assert!(capacity::project(&h, 30).is_err(), "{h:?}");
    }
}
