// SPDX-License-Identifier: MIT
//! End-to-end test for the sub-phase 3A trace format: write events
//! through `TraceWriter`, close, re-open through `TraceReader`,
//! verify the events come back identical and zero-copy access works.

use std::fs;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{TraceReader, TraceWriter};

fn sample_manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "6.6.42-test".to_owned(),
        cpu_features: vec!["sse2".into(), "sse4_2".into()],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![("PATH".into(), "/usr/bin".into())],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec!["--probe".into()],
        recorded_at: None,
    }
}

fn temp_trace_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-test-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn roundtrip_one_segment_one_hundred_events() {
    let dir = temp_trace_dir("one-segment");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for i in 0..100u32 {
        writer
            .write_event(Event::Marker { tag: i, data: u64::from(i) * 7 + 1 })
            .unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.manifest().format_version, FormatVersion::V1);
    assert_eq!(reader.manifest().build_id, manifest.build_id);
    assert_eq!(reader.segment_indices(), &[1]);

    let segment = reader.open_segment(1).unwrap();
    let owned = segment.events_owned().unwrap();
    assert_eq!(owned.len(), 100);
    for (i, ev) in owned.iter().enumerate() {
        match ev {
            Event::Marker { tag, data } => {
                assert_eq!(*tag, i as u32);
                assert_eq!(*data, (i as u64) * 7 + 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn roundtrip_multiple_segments_via_explicit_rotate() {
    let dir = temp_trace_dir("multi-segment");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for i in 0..10u32 {
        writer.write_event(Event::Marker { tag: i, data: 1 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 10..25u32 {
        writer.write_event(Event::Marker { tag: i, data: 2 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 25..27u32 {
        writer.write_event(Event::Marker { tag: i, data: 3 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[1, 2, 3]);

    let lengths_and_data: Vec<(usize, u64)> = reader
        .segment_indices()
        .iter()
        .map(|&idx| {
            let seg = reader.open_segment(idx).unwrap();
            let evs = seg.events_owned().unwrap();
            let data = match evs[0] {
                Event::Marker { data, .. } => data,
                ref other => panic!("unexpected variant: {other:?}"),
            };
            (evs.len(), data)
        })
        .collect();
    assert_eq!(lengths_and_data, vec![(10, 1), (15, 2), (2, 3)]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_archived_view_is_zero_copy() {
    use rkyv::vec::ArchivedVec;
    let dir = temp_trace_dir("zero-copy");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 11 }).unwrap();
    writer.write_event(Event::Marker { tag: 2, data: 22 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived: &ArchivedVec<_> = segment.events().unwrap();
    assert_eq!(archived.len(), 2);
    // Walk the archived view directly — the rkyv enum exposes its
    // discriminant + fields without ever materialising owned `Event`
    // values. This is the access pattern that motivates the rkyv
    // choice over speedy/bincode/borsh.
    use bs_replay_engine::format::event::ArchivedEvent;
    match &archived[0] {
        ArchivedEvent::Marker { tag, data } => {
            assert_eq!(tag.to_native(), 1);
            assert_eq!(data.to_native(), 11);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    match &archived[1] {
        ArchivedEvent::Marker { tag, data } => {
            assert_eq!(tag.to_native(), 2);
            assert_eq!(data.to_native(), 22);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_refuses_existing_directory() {
    let dir = temp_trace_dir("existing");
    fs::create_dir(&dir).unwrap();

    let manifest = sample_manifest();
    let err = TraceWriter::create(&dir, &manifest).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("I/O") || s.contains("exists"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_rotates_when_estimated_size_exceeds_threshold() {
    // One Marker is conservatively sized at 28 bytes by
    // approx_archive_size. Setting the threshold to 56 bytes means
    // every two events triggers a rotation: 5 events → 3 segments
    // (2, 2, 1).
    let dir = temp_trace_dir("auto-rotate");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest)
        .unwrap()
        .with_segment_size(56);
    for i in 0..5u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[1, 2, 3]);
    let counts: Vec<usize> = reader
        .segment_indices()
        .iter()
        .map(|&i| reader.open_segment(i).unwrap().events_owned().unwrap().len())
        .collect();
    assert_eq!(counts, vec![2, 2, 1]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_header_index_mismatch_is_caught() {
    // Write a single segment, then rename event-000001.lz4 to
    // event-000099.lz4. The header still says index=1, the filename
    // now claims 99 — open_segment(99) must surface HeaderMismatch.
    let dir = temp_trace_dir("hdr-mismatch");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    writer.finish().unwrap();
    fs::rename(dir.join("event-000001.lz4"), dir.join("event-000099.lz4"))
        .unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[99]);
    let err = reader.open_segment(99).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("file/header disagree"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_reader_exposes_header() {
    let dir = temp_trace_dir("hdr-expose");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for _ in 0..3 {
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let seg = reader.open_segment(1).unwrap();
    let hdr = seg.header().unwrap();
    assert_eq!(hdr.index.to_native(), 1);
    assert_eq!(hdr.event_count.to_native(), 3);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn syscall_event_roundtrip_preserves_args_result_and_output() {
    let dir = temp_trace_dir("syscall-roundtrip");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer
        .write_event(Event::Syscall {
            // Linux x86-64 __NR_read = 0; pretend a read returned 13
            // bytes with the literal payload below.
            nr: 0,
            args: [3, 0xdead_beef, 13, 0, 0, 0],
            result: 13,
            output: b"hello, world!".to_vec(),
        })
        .unwrap();
    writer
        .write_event(Event::Syscall {
            // __NR_close = 3; no output buffer.
            nr: 3,
            args: [3, 0, 0, 0, 0, 0],
            result: 0,
            output: Vec::new(),
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let evs = segment.events_owned().unwrap();
    assert_eq!(evs.len(), 2);
    match &evs[0] {
        Event::Syscall { nr, args, result, output } => {
            assert_eq!(*nr, 0);
            assert_eq!(args[0], 3);
            assert_eq!(args[1], 0xdead_beef);
            assert_eq!(args[2], 13);
            assert_eq!(*result, 13);
            assert_eq!(output, b"hello, world!");
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    match &evs[1] {
        Event::Syscall { nr, result, output, .. } => {
            assert_eq!(*nr, 3);
            assert_eq!(*result, 0);
            assert!(output.is_empty());
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn syscall_archived_view_uses_endian_aware_accessors() {
    use bs_replay_engine::format::event::ArchivedEvent;
    let dir = temp_trace_dir("syscall-archived");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer
        .write_event(Event::Syscall {
            nr: 42,
            args: [1, 2, 3, 4, 5, 6],
            result: -2, // -ENOENT pretend
            output: vec![0xab, 0xcd],
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived = segment.events().unwrap();
    match &archived[0] {
        ArchivedEvent::Syscall { nr, args, result, output } => {
            assert_eq!(nr.to_native(), 42);
            // Each archived arg is a little-endian u64; .to_native()
            // does the host-order conversion.
            for (i, a) in args.iter().enumerate() {
                assert_eq!(a.to_native(), (i + 1) as u64);
            }
            assert_eq!(result.to_native(), -2);
            assert_eq!(output.as_slice(), &[0xab, 0xcd]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn marker_only_traces_still_read_after_syscall_added() {
    // Forward-compat acceptance: a trace that wrote only Marker
    // (variant 0) before Syscall existed still parses identically
    // now that Syscall exists at variant 1. rkyv's enum encoding
    // is "variant index byte + body" so leaving Marker at index 0
    // is the load-bearing decision.
    let dir = temp_trace_dir("variant-zero-stable");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for i in 0..5u32 {
        writer.write_event(Event::Marker { tag: i, data: u64::from(i) }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert_eq!(evs.len(), 5);
    for (i, ev) in evs.iter().enumerate() {
        match ev {
            Event::Marker { tag, data } => {
                assert_eq!(*tag, i as u32);
                assert_eq!(*data, i as u64);
            }
            other => panic!(
                "Marker-only trace decoded as wrong variant: {other:?} \
                 — adding Syscall at the *end* of the enum was supposed \
                 to be additive; if this fires the variant ordering \
                 broke and the on-disk format is no longer v1-compatible",
            ),
        }
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn signal_event_roundtrip_preserves_siginfo_bytes() {
    let dir = temp_trace_dir("signal-roundtrip");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    let siginfo = vec![0xab; 128]; // Linux x86-64 siginfo_t is 128 bytes
    writer
        .write_event(Event::Signal {
            sig_no: 11, // SIGSEGV
            pc: 0x4000_1234,
            siginfo: siginfo.clone(),
        })
        .unwrap();
    writer
        .write_event(Event::Signal {
            sig_no: 17, // SIGCHLD with empty siginfo (recorder might skip)
            pc: 0x4000_5678,
            siginfo: Vec::new(),
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert_eq!(evs.len(), 2);
    match &evs[0] {
        Event::Signal { sig_no, pc, siginfo: bytes } => {
            assert_eq!(*sig_no, 11);
            assert_eq!(*pc, 0x4000_1234);
            assert_eq!(bytes, &siginfo);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    match &evs[1] {
        Event::Signal { sig_no, siginfo: bytes, .. } => {
            assert_eq!(*sig_no, 17);
            assert!(bytes.is_empty());
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn signal_archived_view_uses_endian_aware_accessors() {
    use bs_replay_engine::format::event::ArchivedEvent;
    let dir = temp_trace_dir("signal-archived");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer
        .write_event(Event::Signal {
            sig_no: 9, // SIGKILL
            pc: 0xff00_aa55,
            siginfo: vec![1, 2, 3, 4],
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived = segment.events().unwrap();
    match &archived[0] {
        ArchivedEvent::Signal { sig_no, pc, siginfo } => {
            assert_eq!(sig_no.to_native(), 9);
            assert_eq!(pc.to_native(), 0xff00_aa55);
            assert_eq!(siginfo.as_slice(), &[1u8, 2, 3, 4]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn instruction_trap_event_roundtrip_per_kind() {
    use bs_replay_engine::format::event::InstructionTrapKind;

    let dir = temp_trace_dir("itrap-kinds");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    // RDTSC: one u64 result.
    writer
        .write_event(Event::InstructionTrap {
            pc: 0x4000_1000,
            kind: InstructionTrapKind::Rdtsc,
            result: vec![0xdead_beef_cafe_babe],
        })
        .unwrap();
    // RDRAND: value + success flag.
    writer
        .write_event(Event::InstructionTrap {
            pc: 0x4000_1010,
            kind: InstructionTrapKind::Rdrand,
            result: vec![0x123456_789a, 1],
        })
        .unwrap();
    // CPUID: eax/ebx/ecx/edx.
    writer
        .write_event(Event::InstructionTrap {
            pc: 0x4000_1020,
            kind: InstructionTrapKind::Cpuid,
            result: vec![1, 2, 3, 4],
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert_eq!(evs.len(), 3);
    match &evs[0] {
        Event::InstructionTrap { pc, kind, result } => {
            assert_eq!(*pc, 0x4000_1000);
            assert_eq!(*kind, InstructionTrapKind::Rdtsc);
            assert_eq!(result, &vec![0xdead_beef_cafe_babe]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    match &evs[1] {
        Event::InstructionTrap { kind, result, .. } => {
            assert_eq!(*kind, InstructionTrapKind::Rdrand);
            assert_eq!(result.len(), 2);
            assert_eq!(result[1], 1);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    match &evs[2] {
        Event::InstructionTrap { kind, result, .. } => {
            assert_eq!(*kind, InstructionTrapKind::Cpuid);
            assert_eq!(result, &vec![1u64, 2, 3, 4]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn instruction_trap_archived_view_uses_endian_aware_accessors() {
    use bs_replay_engine::format::event::{ArchivedEvent, InstructionTrapKind};

    let dir = temp_trace_dir("itrap-archived");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer
        .write_event(Event::InstructionTrap {
            pc: 0xff00_aa55,
            kind: InstructionTrapKind::Rdtscp,
            result: vec![42, 1234],
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived = segment.events().unwrap();
    match &archived[0] {
        ArchivedEvent::InstructionTrap { pc, kind: _, result } => {
            assert_eq!(pc.to_native(), 0xff00_aa55);
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].to_native(), 42);
            assert_eq!(result[1].to_native(), 1234);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn pc_marker_event_roundtrip() {
    let dir = temp_trace_dir("pc-marker");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::PcMarker { pc: 0x4000_0000 }).unwrap();
    writer.write_event(Event::PcMarker { pc: 0xff00_aa55 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert_eq!(evs.len(), 2);
    match &evs[0] {
        Event::PcMarker { pc } => assert_eq!(*pc, 0x4000_0000),
        other => panic!("unexpected variant: {other:?}"),
    }
    match &evs[1] {
        Event::PcMarker { pc } => assert_eq!(*pc, 0xff00_aa55),
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn pc_marker_archived_view_uses_endian_aware_accessor() {
    use bs_replay_engine::format::event::ArchivedEvent;
    let dir = temp_trace_dir("pc-marker-archived");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::PcMarker { pc: 0xdead_beef_cafe_babe }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived = segment.events().unwrap();
    match &archived[0] {
        ArchivedEvent::PcMarker { pc } => {
            assert_eq!(pc.to_native(), 0xdead_beef_cafe_babe);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn pc_at_or_before_returns_latest_pcmarker_in_prefix() {
    let dir = temp_trace_dir("pc-at-or-before");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    // event 0: PcMarker(0xaa)
    // event 1: Marker
    // event 2: PcMarker(0xbb)
    // event 3: Marker
    // event 4: Marker
    writer.write_event(Event::PcMarker { pc: 0xaa }).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 0 }).unwrap();
    writer.write_event(Event::PcMarker { pc: 0xbb }).unwrap();
    writer.write_event(Event::Marker { tag: 2, data: 0 }).unwrap();
    writer.write_event(Event::Marker { tag: 3, data: 0 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.pc_at_or_before(0).unwrap(), Some(0xaa));
    assert_eq!(reader.pc_at_or_before(1).unwrap(), Some(0xaa));
    assert_eq!(reader.pc_at_or_before(2).unwrap(), Some(0xbb));
    assert_eq!(reader.pc_at_or_before(3).unwrap(), Some(0xbb));
    assert_eq!(reader.pc_at_or_before(4).unwrap(), Some(0xbb));
    // Past end of trace — last PcMarker still wins.
    assert_eq!(reader.pc_at_or_before(99).unwrap(), Some(0xbb));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn pc_at_or_before_returns_none_when_no_pcmarker_in_prefix() {
    let dir = temp_trace_dir("pc-no-marker");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    // No PcMarker before event 2.
    writer.write_event(Event::Marker { tag: 1, data: 0 }).unwrap();
    writer.write_event(Event::Marker { tag: 2, data: 0 }).unwrap();
    writer.write_event(Event::PcMarker { pc: 0xcc }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.pc_at_or_before(0).unwrap(), None);
    assert_eq!(reader.pc_at_or_before(1).unwrap(), None);
    assert_eq!(reader.pc_at_or_before(2).unwrap(), Some(0xcc));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn marker_through_instructiontrap_traces_still_read_after_pcmarker_added() {
    // Forward-compat *fourth* extension: a trace mixing all four
    // pre-existing variants (Marker / Syscall / Signal /
    // InstructionTrap, indices 0–3) parses identically now that
    // PcMarker exists at index 4. Catches any future PR that
    // breaks the additive-only rule.
    use bs_replay_engine::format::event::InstructionTrapKind;

    let dir = temp_trace_dir("4-stable");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 100 }).unwrap();
    writer.write_event(Event::Syscall {
        nr: 0,
        args: [3, 0, 8, 0, 0, 0],
        result: 8,
        output: vec![0xaa; 8],
    }).unwrap();
    writer.write_event(Event::Signal {
        sig_no: 11,
        pc: 0x4000_5678,
        siginfo: vec![0xbb; 16],
    }).unwrap();
    writer.write_event(Event::InstructionTrap {
        pc: 0x4000_9abc,
        kind: InstructionTrapKind::Rdtsc,
        result: vec![0xdead_beef_cafe_babe],
    }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert!(matches!(evs[0], Event::Marker { tag: 1, data: 100 }));
    assert!(matches!(evs[1], Event::Syscall { nr: 0, .. }));
    assert!(matches!(evs[2], Event::Signal { sig_no: 11, .. }));
    assert!(matches!(evs[3], Event::InstructionTrap { kind: InstructionTrapKind::Rdtsc, .. }));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn marker_syscall_signal_only_traces_still_read_after_instructiontrap_added() {
    // Forward-compat *third* extension: a trace that wrote
    // Marker + Syscall + Signal (variants 0–2) before
    // InstructionTrap existed at variant 3 still parses
    // identically. Each new variant since v1 has been an additive
    // append; this test would catch any future PR that broke the
    // variant ordering even after three rounds of growth.
    let dir = temp_trace_dir("3-stable");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 100 }).unwrap();
    writer
        .write_event(Event::Syscall {
            nr: 0,
            args: [3, 0, 8, 0, 0, 0],
            result: 8,
            output: vec![0xaa; 8],
        })
        .unwrap();
    writer
        .write_event(Event::Signal {
            sig_no: 11,
            pc: 0x4000_5678,
            siginfo: vec![0xbb; 16],
        })
        .unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert!(matches!(evs[0], Event::Marker { tag: 1, data: 100 }));
    assert!(matches!(evs[1], Event::Syscall { nr: 0, .. }));
    assert!(matches!(evs[2], Event::Signal { sig_no: 11, .. }));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn marker_and_syscall_only_traces_still_read_after_signal_added() {
    // Forward-compat acceptance: a trace that wrote Marker + Syscall
    // (variants 0 and 1) before Signal existed still parses
    // identically now that Signal exists at variant 2. This is the
    // *second* extension of the Event enum since v1, so it
    // exercises the "additive forever" promise harder than the
    // original step-5 test.
    let dir = temp_trace_dir("marker-syscall-stable");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 0, data: 100 }).unwrap();
    writer.write_event(Event::Syscall {
        nr: 0,
        args: [3, 0, 16, 0, 0, 0],
        result: 16,
        output: vec![0xaa; 16],
    }).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 200 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert_eq!(evs.len(), 3);
    match &evs[0] {
        Event::Marker { tag: 0, data: 100 } => {}
        other => panic!(
            "Marker at index 0 decoded as wrong variant: {other:?} \
             — adding Signal at variant 2 was supposed to be additive; \
             if this fires the variant ordering broke",
        ),
    }
    match &evs[1] {
        Event::Syscall { nr: 0, result: 16, .. } => {}
        other => panic!("Syscall at index 1 decoded as: {other:?}"),
    }
    match &evs[2] {
        Event::Marker { tag: 1, data: 200 } => {}
        other => panic!("Marker at index 2 decoded as: {other:?}"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn mixed_marker_and_syscall_traces_in_one_segment() {
    let dir = temp_trace_dir("mixed");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 1 }).unwrap();
    writer.write_event(Event::Syscall {
        nr: 0,
        args: [0; 6],
        result: 0,
        output: vec![1, 2, 3],
    }).unwrap();
    writer.write_event(Event::Marker { tag: 2, data: 2 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let evs = reader.open_segment(1).unwrap().events_owned().unwrap();
    assert!(matches!(evs[0], Event::Marker { tag: 1, .. }));
    assert!(matches!(evs[1], Event::Syscall { nr: 0, .. }));
    assert!(matches!(evs[2], Event::Marker { tag: 2, .. }));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_with_missing_manifest_fails_clearly() {
    let dir = temp_trace_dir("no-manifest");
    fs::create_dir(&dir).unwrap();

    let err = TraceReader::open(&dir).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("manifest"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}
