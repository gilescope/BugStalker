// SPDX-License-Identifier: MIT
//! Property tests for the format layer.
//!
//! Phase 8 § "Property testing" lists this as the Phase 5 property:
//! "record→replay never produces a different PC trace from the
//! recorded one (the property is determinism itself)". At the
//! format layer the equivalent shape is: a sequence of Events
//! written through TraceWriter and read back through EventCursor
//! must be byte-identical.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{TraceReader, TraceWriter};

/// Per-case counter so each proptest iteration writes into its own
/// directory. proptest runs cases sequentially by default but a
/// crash mid-case must not contaminate the next.
static CASE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(label: &str) -> PathBuf {
    let n = CASE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-prop-{label}-{}-{n}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn fixed_manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "ab".repeat(32),
        kernel_release: "test".to_owned(),
        cpu_features: vec!["sse2".into()],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec![],
    }
}

fn arb_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        (any::<u32>(), any::<u64>()).prop_map(|(tag, data)| Event::Marker { tag, data }),
        (
            any::<u32>(),
            any::<[u64; 6]>(),
            any::<i64>(),
            // Cap output length so a single case isn't multi-MB.
            prop::collection::vec(any::<u8>(), 0..256),
        )
            .prop_map(|(nr, args, result, output)| Event::Syscall { nr, args, result, output }),
    ]
}

/// A strategy that produces a manifest with random text-bearing
/// fields. Skips embedded `\n` in the simple fields to keep the
/// generator small (the dedicated roundtrip test for embedded
/// newlines covers that path). Env keys exclude `=` per the
/// format's POSIX-aligned contract documented on `Manifest`.
fn arb_manifest() -> impl Strategy<Value = Manifest> {
    let safe_string = "[a-zA-Z0-9 _.\\-/:=]{0,40}";
    let env_key = "[a-zA-Z0-9 _.\\-/:]{0,40}"; // no `=`
    (
        safe_string,
        safe_string,
        prop::collection::vec(safe_string.prop_map(|s: String| s), 0..6),
        safe_string,
        prop::collection::vec(
            (env_key, safe_string).prop_map(|(k, v): (String, String)| (k, v)),
            0..6,
        ),
        safe_string,
        prop::collection::vec(safe_string.prop_map(|s: String| s), 0..6),
    )
        .prop_map(
            |(
                build_id,
                kernel_release,
                cpu_features,
                engine_version,
                initial_env,
                initial_cwd,
                initial_args,
            ): (String, String, Vec<String>, String, Vec<(String, String)>, String, Vec<String>)| {
                Manifest {
                    format_version: FormatVersion::V1,
                    build_id,
                    kernel_release,
                    cpu_features,
                    engine_version,
                    initial_env,
                    initial_cwd,
                    initial_args,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Phase 8 property at the format layer: a sequence of events
    /// goes in via TraceWriter, comes back identical via EventCursor.
    #[test]
    fn record_replay_event_sequence_is_identical(
        events in prop::collection::vec(arb_event(), 0..40)
    ) {
        let dir = fresh_dir("seq");
        {
            let mut writer = TraceWriter::create(&dir, &fixed_manifest()).unwrap();
            for ev in &events {
                writer.write_event(ev.clone()).unwrap();
            }
            writer.finish().unwrap();
        }
        let reader = TraceReader::open(&dir).unwrap();
        let mut cursor = reader.cursor();
        let mut got = Vec::new();
        while let Some(ev) = cursor.next().unwrap() {
            got.push(ev);
        }
        prop_assert_eq!(events, got);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same as above but with explicit rotations forcing the events
    /// to span multiple segments. Invariant must hold across the
    /// cross-segment boundaries the cursor walks.
    #[test]
    fn record_replay_survives_arbitrary_rotations(
        events in prop::collection::vec(arb_event(), 1..40),
        rotation_points in prop::collection::vec(any::<u8>(), 0..6),
    ) {
        let dir = fresh_dir("rot");
        let n = events.len();
        // Convert rotation points into sorted, in-bounds indices.
        let mut pivots: Vec<usize> = rotation_points
            .into_iter()
            .map(|b| (b as usize) % n.max(1))
            .collect();
        pivots.sort();
        pivots.dedup();
        let mut next_pivot = 0;
        {
            let mut writer = TraceWriter::create(&dir, &fixed_manifest()).unwrap();
            for (i, ev) in events.iter().enumerate() {
                writer.write_event(ev.clone()).unwrap();
                while next_pivot < pivots.len() && pivots[next_pivot] == i {
                    writer.rotate().unwrap();
                    next_pivot += 1;
                }
            }
            writer.finish().unwrap();
        }
        let reader = TraceReader::open(&dir).unwrap();
        let got: Vec<Event> = {
            let mut cur = reader.cursor();
            let mut acc = Vec::new();
            while let Some(ev) = cur.next().unwrap() {
                acc.push(ev);
            }
            acc
        };
        prop_assert_eq!(events, got);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Manifest text format roundtrip — write to text, parse back,
    /// assert equality across arbitrary safe-string fields.
    #[test]
    fn manifest_text_roundtrip(m in arb_manifest()) {
        let s = m.to_text();
        let back = Manifest::from_text(&s).unwrap();
        prop_assert_eq!(m, back);
    }

    /// `find_checkpoint_at_or_before(target)` always returns a
    /// header with `event_index <= target` (or None). Phase 5
    /// invariant on the seek API.
    #[test]
    fn checkpoint_seek_is_at_or_before(
        layout in prop::collection::vec(any::<u8>(), 1..16),
        target in any::<u64>(),
    ) {
        let dir = fresh_dir("cp-seek");
        {
            let mut writer = TraceWriter::create(&dir, &fixed_manifest()).unwrap();
            for &gap in &layout {
                // gap events then a checkpoint.
                for i in 0..(gap as u32 % 8) {
                    writer
                        .write_event(Event::Marker { tag: i, data: 0 })
                        .unwrap();
                }
                writer.take_checkpoint(Vec::new()).unwrap();
            }
            writer.finish().unwrap();
        }
        let reader = TraceReader::open(&dir).unwrap();
        let found = reader.find_checkpoint_at_or_before(target).unwrap();
        if let Some(h) = found {
            prop_assert!(
                h.event_index <= target,
                "found checkpoint at event_index {} but target was {}",
                h.event_index, target,
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
