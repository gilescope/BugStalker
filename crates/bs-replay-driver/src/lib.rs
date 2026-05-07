// SPDX-License-Identifier: MIT
//! BugStalker time-travel — sub-phase 3I integration seam.
//!
//! See `doc/plans/phase-5-time-travel.md` § "3I. BugStalker driver
//! integration".
//!
//! The plan's promise: "BugStalker doesn't know whether it's
//! attached to a live process or a replay; same breakpoints, same
//! watchpoints, same step semantics." The eventual `bs-replay-driver`
//! exposes the same `ptrace`-event surface the existing tracee
//! plumbing consumes; the debugger's command dispatcher sees a
//! uniform interface either way.
//!
//! That's a multi-week landing — `crates/.../tracee.rs` is heavily
//! Linux-ptrace-coupled. This scaffold lays the integration crate
//! down with a small [`TraceReplayer`] API the future fake-tracee
//! drives through: open a trace, walk events, jump by checkpoint,
//! report position. Each method composes a fresh
//! [`bs_replay_engine::format::EventCursor`] under the hood, which
//! is cheap because the cursor's expensive state (decompressed
//! segments) lives in caches inside [`bs_replay_engine::format::TraceReader`].

#![warn(missing_docs)]

pub mod capture;
pub mod dap;
pub mod host;
#[cfg(target_os = "linux")]
pub mod record;
#[cfg(target_os = "linux")]
pub mod replay;
pub mod replayer;
pub mod reverse;

pub use capture::capture_host_manifest;
#[cfg(target_os = "linux")]
pub use dap::{record as dap_record, DapRecordError};
pub use dap::{
    ReplayRecordExitKind, ReplayRecordOptions, ReplayRecordRequest, ReplayRecordResponse,
};
#[cfg(target_os = "linux")]
pub use record::{
    record_program, ExitStatus as RecorderExitStatus, RecordOptions, RecordProgramError,
    RecordReport,
};
#[cfg(target_os = "linux")]
pub use replay::{
    replay_program, ReplayExit, ReplayOptions, ReplayProgramError, ReplayReport,
    ShimRefusedReason,
};
pub use host::{host_features, HostDetectError};
pub use replayer::{
    BuildIdMismatch, HostMismatchError, ReplayError, ReplayabilityError, TraceReplayer,
};
pub use reverse::ReverseDebugger;

/// Re-export the engine for downstream consumers — most callers
/// want both the driver and the engine's types.
pub use bs_replay_engine as engine;

/// Convenience re-exports of the sub-phase 3B recorder
/// primitives. Consumers driving the recorder loop want
/// `record_one_syscall` + the `MemoryReader` trait without
/// reaching into the engine's module tree.
pub mod record_primitives {
    #[doc(inline)]
    pub use bs_replay_engine::record::syscall_capture::{
        capture_post_syscall, capture_pre_syscall, classify, looks_like_user_pointer,
        BUFFER_CAP, CallFrame, CapturedKind, CapturedRegion, CapturedSyscall, CaptureTier,
        DecodeError as CapturedSyscallDecodeError, MemoryReader, Tier, CATCH_ALL_WINDOW,
        CSTR_CAP,
    };

    #[cfg(target_os = "linux")]
    #[doc(inline)]
    pub use bs_replay_engine::record::linux::{
        exit_stop::{
            classify_wstatus, merge_pre_post, ptrace_cont, ptrace_syscall,
            wait_for_next_stop, ExitStopError, StopKind, UserRegsX86_64,
        },
        ptrace_driver::{
            capture_from_notif, event_for_capture, frame_from_notif, recv_notif,
            record_one_syscall, respond_continue, respond_intercept, ProcMemReader,
            RecorderError, SeccompData, SeccompNotif, SeccompNotifResp,
            RESULT_NOT_CAPTURED_YET, SECCOMP_USER_NOTIF_FLAG_CONTINUE,
        },
        record_child::{spawn as spawn_record_child, RecordChild},
        record_session::{
            ptrace_getsiginfo, record_to_completion, spawn_recorded_child,
            step_until_event, RecordSessionError, RecordSummary, RecordedChild,
            RecordedEventKind, SpawnError, Terminal,
        },
        seccomp::install_trap_all_listener,
        signals::{
            event_for_signal, signal_from_event, validate_replay_plan, SignalCapture,
            SignalDecodeError, SignalLengthError, SignalReplayError, SignalReplayPlan,
            SIGINFO_T_LEN_X86_64,
        },
        thread_sched::{
            current_affinity_for_self, pin_to_single_cpu_for_self, AffinityError,
            AffinityMask, SingleCpuPin,
        },
        vdso_patch::{
            find_vdso_range, find_vdso_range_for_self,
            patch_bytes as patch_remote_bytes, peekdata, pokedata,
            read_proc_maps, read_proc_maps_for_self, read_remote_vdso_bytes,
            scan_remote_vdso, scan_vdso_exports, syscall_nr_for_vdso,
            ProcMapping, ScanRemoteError, VdsoPatch, VdsoPatchError, VdsoScanError,
            VdsoSymbol, VDSO_TARGET_SYMBOLS,
        },
    };

    // x86_64-only re-exports (not available on aarch64 Linux):
    // get_regs / set_regs (use PTRACE_GETREGS request), legacy
    // record_syscall_with_exit (NOTIF + GETREGS path), the
    // iced-x86 instruction classifier, vDSO trampoline applier
    // (uses x86 trampoline payload), and the x86 result-register
    // accessor.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[doc(inline)]
    pub use bs_replay_engine::record::linux::{
        exit_stop::{get_regs, record_syscall_with_exit, result_register_x86_64, set_regs},
        instrs::{
            classify_at_pc, event_for_instruction_trap, set_tsc_trap_for_self, InstrKind,
        },
        record_session::call_frame_from_regs,
        vdso_patch::apply_vdso_trampolines,
    };
}

/// Convenience re-exports of the 3C replay-shim primitives.
#[cfg(target_os = "linux")]
pub mod replay_primitives {
    #[doc(inline)]
    pub use bs_replay_engine::replay::linux::shim::{
        apply_recorded_event, replay_one_syscall, MemoryWriter, ProcMemWriter,
        ReplayError as ReplayShimError, ReplayLoopError, ReplayResponse, SyscallMismatch,
    };
}
