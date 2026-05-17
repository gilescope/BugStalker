// SPDX-License-Identifier: MIT
use super::call::CallError;
use super::call::fmt::FmtCallError;
use crate::debugger::address::GlobalAddress;
use crate::debugger::r#async::AsyncError;
use crate::debugger::debugee::RendezvousError;
use crate::debugger::debugee::dwarf::unit::DieAddr;
use crate::debugger::variable::value::ParsingError;
use gimli::UnitOffset;
use nix::unistd::Pid;
use std::str::Utf8Error;
use std::string::FromUtf8Error;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // --------------------------------- generic errors --------------------------------------------
    #[error("debugee already run")]
    AlreadyRun,
    #[error(transparent)]
    IO(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] Utf8Error),
    #[error(transparent)]
    FromUtf8(#[from] FromUtf8Error),
    #[error(transparent)]
    RegEx(#[from] regex::Error),

    // --------------------------------- debugger entity not found----------------------------------
    #[error("no debug information for {0}")]
    NoDebugInformation(&'static str),
    #[error("unknown register {0:?}")]
    RegisterNotFound(gimli::Register),
    #[error("unknown register {0:?}")]
    RegisterNameNotFound(String),
    #[error("source place not found at address {0}")]
    PlaceNotFound(GlobalAddress),
    #[error("there are no suitable places for this request")]
    NoSuitablePlace,
    #[error("unit not found at address {0}")]
    UnitNotFound(GlobalAddress),
    #[error("function not found at address {0}")]
    FunctionNotFound(GlobalAddress),
    #[error("type not found")]
    TypeNotFound,
    #[error("frame number {0} not found")]
    FrameNotFound(u32),
    #[error("tracee number {0} not found")]
    TraceeNotFound(u32),
    #[error("debug information entry (die) not found, reference: {0:?}")]
    DieNotFound(DieAddr),
    #[error("section \"{0}\" not found")]
    SectionNotFound(&'static str),

    // --------------------------------- remote memory errors --------------------------------------
    #[error("invalid binary representation of type `{0}`: {1:?}")]
    TypeBinaryRepr(&'static str, Box<[u8]>),
    #[error("unknown address")]
    UnknownAddress,
    #[error("memory region offset not found ({0})")]
    MappingOffsetNotFound(&'static str),
    #[error("memory region not found for a file: {0}")]
    MappingNotFound(String),

    // --------------------------------- syscall errors --------------------------------------------
    #[error("waitpid syscall error: {0}")]
    Waitpid(nix::Error),
    #[error("ptrace syscall error: {0}")]
    Ptrace(nix::Error),
    #[error(
        "macOS denied debugger access ({mach}). \n  \
         help: re-sign the bs/bugstalker binary with the \
         `com.apple.security.cs.debugger` entitlement. The \
         entitlements XML has been written to `{entitlements}` for you. \
         Run:\n    codesign -s - --entitlements {entitlements} --force {binary}\n  \
         note: bs normally auto-signs and re-execs itself on first run; if \
         you saw this message it means auto-sign failed (set \
         BS_NO_AUTO_SIGN to disable, or check the [bs] auto-sign log line \
         above for the underlying reason)."
    )]
    DarwinDebuggerEntitlementMissing {
        mach: String,
        binary: String,
        entitlements: String,
    },
    /// Generic Mach failure that isn't task_for_pid's
    /// missing-entitlement signature. Preserves the kr code +
    /// description verbatim so the user (and grep) can match it
    /// against `<mach/kern_return.h>` instead of being told a
    /// confidently-wrong remediation.
    ///
    /// `backtrace` carries a frame chain captured at the
    /// `From<MachError>` conversion — that's the cheapest way to
    /// pinpoint which Mach call (`thread_set_state`, `task_resume`,
    /// `vm_write`, …) actually failed without instrumenting every
    /// call site by hand. It's appended to the user-facing error
    /// message so the failure report carries its own diagnostics.
    #[error("Mach failure: {mach}\n{backtrace}")]
    DarwinMach { mach: String, backtrace: String },
    /// Write attempted to a region whose `max_protection` does not
    /// include `VM_PROT_WRITE`, so `mach_vm_protect` cannot widen the
    /// page even temporarily. Typical hits: the LC_CODE_SIGNATURE
    /// blob, the dyld shared cache, and pages explicitly sealed by
    /// the loader (`__DATA_CONST` post-init).
    ///
    /// This is a callable signal — `apply-patch` skips entries that
    /// hit it, since for the EnC use case the only writable target
    /// that matters is `__TEXT` (function bodies). The codesign blob
    /// changes wild emits when re-linking are disk-only artefacts;
    /// the running process's signature check has already happened.
    #[error(
        "darwin: target region at 0x{addr:x} is read-only \
         (max_prot=0x{max_prot:x}); skipping"
    )]
    DarwinReadOnlyRegion { addr: usize, max_prot: u32 },
    #[error("{0} syscall error: {1}")]
    Syscall(&'static str, nix::Error),
    #[error("multiple syscall errors {0:?}")]
    MultipleErrors(Vec<Self>),

    // --------------------------------- watchpoint errors -----------------------------------------
    #[error("variable or argument to watch not found")]
    WatchSubjectNotFound,
    #[error("there is more than one watchpoint candidate, try to specify the expression")]
    WatchpointCollision,
    #[error("there is no memory address for watchpoint")]
    WatchpointNoAddress,
    #[error("size of watch object is undefined")]
    WatchpointUndefinedSize,
    #[error(
        "the size of the watch object does not fit into one of the size class (1, 2, 4, 8 bytes), try to specify a field to observe"
    )]
    WatchpointWrongSize,
    #[error("watchpoint limit is reached (maximum 4 watchpoints), try to remove unused")]
    WatchpointLimitReached,
    #[error("hardware watchpoints are not supported on this architecture")]
    WatchpointUnsupported,
    #[error("memory location observed by another watchpoint")]
    AddressAlreadyObserved,
    #[error("unknown expression scope")]
    UnknownScope,
    #[error("variable frame is unavailable")]
    VarFrameNotFound,

    // --------------------------------- parsing errors --------------------------------------------
    #[error("dwarf file parsing error: {0}")]
    DwarfParsing(#[from] gimli::Error),
    #[error("invalid debug-id note format")]
    DebugIDFormat,
    #[error("object file parsing error: {0}")]
    ObjParsing(#[from] object::Error),
    #[error(transparent)]
    VariableParsing(#[from] ParsingError),
    #[error("function specification ({0:?}) reference to unseen declaration")]
    InvalidSpecification(UnitOffset),

    // --------------------------------- unwind errors ---------------------------------------------
    #[error("unwind: no unwind context")]
    UnwindNoContext,
    #[error("unwind: too deep frame number")]
    UnwindTooDeepFrame,

    // --------------------------------- dwarf errors ----------------------------------------------
    #[error("dwarf expression evaluation: eval option `{0}` required")]
    EvalOptionRequired(&'static str),
    #[error("dwarf expression evaluation: unsupported evaluation require ({0})")]
    EvalUnsupportedRequire(&'static str),
    #[error("no frame base address")]
    NoFBA,
    #[error("frame base address attribute not an expression")]
    FBANotAnExpression,
    #[error("range information for function `{0:?}` not exists")]
    NoFunctionRanges(Option<String>),
    #[error("die type not exists")]
    NoDieType,
    #[error("fail to read/evaluate implicit pointer address")]
    ImplicitPointer,

    // --------------------------------- libthread_db errors ---------------------------------------
    #[error("libthread_db not enabled")]
    NoThreadDB,
    #[error("libthread_db: {0}")]
    ThreadDB(#[from] crate::debugger::thread_db_compat::ThreadDbError),

    // --------------------------------- linker errors ---------------------------------------------
    #[error(transparent)]
    Rendezvous(#[from] RendezvousError),

    // --------------------------------- debugee process errors ------------------------------------
    #[error("debugee process exit with code {0}")]
    ProcessExit(i32),
    #[error("program is not being started")]
    ProcessNotStarted,

    // --------------------------------- rust toolchain errors -------------------------------------
    #[error("default toolchain not found")]
    DefaultToolchainNotFound,
    #[error("unrecognized rustup output")]
    UnrecognizedRustupOut,

    // --------------------------------- disasm ----------------------------------------------------
    #[error("install disassembler: {0}")]
    DisAsmInit(capstone::Error),
    #[error("instructions disassembly error: {0}")]
    DisAsm(capstone::Error),
    #[error("error to determine current function start/end place")]
    FunctionRangeNotFound,

    // --------------------------------- third party errors ----------------------------------------
    #[error("hook: {0}")]
    Hook(anyhow::Error),

    // --------------------------------- attach debugee errors -------------------------------------
    #[error("process pid {0} not found")]
    AttachedProcessNotFound(Pid),
    #[error("attach a running process: {0}")]
    Attach(nix::Error),

    // --------------------------------- async exploration errors ----------------------------------
    #[error("{0}. Maybe your async runtime version is unsupported.")]
    Async(#[from] AsyncError),

    // --------------------------------- function call errors -------------------------------------
    #[error(transparent)]
    Call(#[from] CallError),
    #[error(transparent)]
    FmtCall(#[from] FmtCallError),

    // --------------------------------- EnC restart safety ----------------------------------------
    /// Refused to auto-restart a function whose body makes outbound calls.
    /// The DWARF-only restart path can't reconstruct the state held in
    /// caller-saved registers and unnamed stack slots that those inner
    /// calls depend on (iterator `Iter::ptr/end`, trait-object vtables,
    /// XMM-passed floats). Restarting anyway produces plausible-looking
    /// garbage. Override with [`crate::debugger::DebuggerBuilder::with_force_restart`].
    #[error(
        "refusing to auto-restart `{function}` from entry: the function body contains \
         {inner_calls} outbound CALL{plural} ({first_call_offset}). State held in \
         caller-saved registers and unnamed stack slots cannot be reconstructed from \
         DWARF alone, so a restart would re-execute the body against stale memory and \
         produce plausible-looking garbage. To force the restart anyway, build the \
         debugger with `DebuggerBuilder::with_force_restart(true)` (Tier-2 \
         writable-state restoration is the planned fix; see \
         doc/plans/phase-5-time-travel.md)."
    )]
    RestartRefusedInnerCalls {
        /// Human-readable function name (linkage or short).
        function: String,
        /// Count of CALL/BL instructions found in the body.
        inner_calls: usize,
        /// `"s"` when `inner_calls != 1`, empty otherwise — keeps the
        /// message grammatical without separate format strings.
        plural: &'static str,
        /// Address of the first outbound call, for diagnostics.
        first_call_offset: String,
    },
}

impl Error {
    /// Return a hint to an interface - continue debugging after error or stop whole process.
    pub fn is_fatal(&self) -> bool {
        match self {
            Error::AlreadyRun => false,
            Error::IO(_) => false,
            Error::Utf8(_) => false,
            Error::FromUtf8(_) => false,
            Error::RegEx(_) => false,
            Error::NoDebugInformation(_) => false,
            Error::RegisterNotFound(_) => false,
            Error::RegisterNameNotFound(_) => false,
            Error::PlaceNotFound(_) => false,
            Error::NoSuitablePlace => false,
            Error::UnitNotFound(_) => false,
            Error::FunctionNotFound(_) => false,
            Error::TypeNotFound => false,
            Error::FrameNotFound(_) => false,
            Error::TraceeNotFound(_) => false,
            Error::DieNotFound(_) => false,
            Error::TypeBinaryRepr(_, _) => false,
            Error::UnknownAddress => false,
            Error::MappingOffsetNotFound(_) => false,
            Error::MappingNotFound(_) => false,
            Error::Waitpid(_) => false,
            Error::Ptrace(_) => false,
            // Missing debugger entitlement on darwin is fatal —
            // every subsequent ptrace/task_for_pid call will fail
            // for the same reason. Surface it once and stop.
            Error::DarwinDebuggerEntitlementMissing { .. } => true,
            // Generic Mach failures aren't always fatal — a single
            // failed `thread_set_state` during step-over shouldn't
            // tear down the whole session — but we don't have
            // per-call recovery yet, so treat them like ptrace
            // errors and let the user continue/inspect the session.
            Error::DarwinMach { .. } => false,
            // Read-only region writes are recoverable: callers (like
            // `apply-patch`) skip the offending entry and keep going.
            Error::DarwinReadOnlyRegion { .. } => false,
            Error::MultipleErrors(_) => false,
            Error::DebugIDFormat => false,
            Error::VariableParsing(_) => false,
            Error::UnwindNoContext => false,
            Error::UnwindTooDeepFrame => false,
            Error::EvalOptionRequired(_) => false,
            Error::EvalUnsupportedRequire(_) => false,
            Error::NoFBA => false,
            Error::FBANotAnExpression => false,
            Error::NoFunctionRanges(_) => false,
            Error::NoDieType => false,
            Error::ImplicitPointer => false,
            Error::ThreadDB(_) => false,
            Error::Rendezvous(_) => false,
            Error::ProcessExit(_) => false,
            Error::ProcessNotStarted => false,
            Error::DefaultToolchainNotFound => false,
            Error::UnrecognizedRustupOut => false,
            Error::Hook(_) => false,
            Error::SectionNotFound(_) => false,
            Error::DisAsm(_) => false,
            Error::InvalidSpecification(_) => false,
            Error::FunctionRangeNotFound => false,
            Error::WatchpointCollision => false,
            Error::WatchpointNoAddress => false,
            Error::WatchpointUndefinedSize => false,
            Error::WatchpointWrongSize => false,
            Error::WatchpointLimitReached => false,
            Error::WatchpointUnsupported => false,
            Error::WatchSubjectNotFound => false,
            Error::AddressAlreadyObserved => false,
            Error::UnknownScope => false,
            Error::VarFrameNotFound => false,
            Error::Async(_) => false,
            Error::Call(_) => false,
            Error::FmtCall(_) => false,
            // EnC restart refusal is a routing decision, not a
            // process-fatal failure — the user can edit again, try
            // a different patch, or `--force-restart` on opt-in.
            Error::RestartRefusedInnerCalls { .. } => false,

            // currently fatal errors
            Error::DwarfParsing(_) => true,
            Error::ObjParsing(_) => true,
            Error::Syscall(_, _) => true,
            Error::NoThreadDB => true,
            Error::DisAsmInit(_) => true,
            Error::AttachedProcessNotFound(_) => true,
            Error::Attach(_) => true,
        }
    }
}

#[macro_export]
macro_rules! _error {
    ($log_fn: path, $res: expr) => {
        match $res {
            Ok(value) => Some(value),
            Err(e) => {
                $log_fn!(target: "debugger", "{:#}", e);
                None
            }
        }
    };
    ($log_fn: path, $res: expr, $msg: tt) => {
        match $res {
            Ok(value) => Some(value),
            Err(e) => {
                $log_fn!(target: "debugger", concat!($msg, " {:#}"), e);
                None
            }
        }
    };
}

/// Transforms `Result` into `Option` and logs an error if it occurs.
#[macro_export]
macro_rules! weak_error {
    ($res: expr) => {
        $crate::_error!(log::warn, $res)
    };
    ($res: expr, $msg: tt) => {
        $crate::_error!(log::warn, $res, $msg)
    };
}

/// Transforms `Result` into `Option` and put error into debug logs if it occurs.
#[macro_export]
macro_rules! muted_error {
    ($res: expr) => {
        $crate::_error!(log::debug, $res)
    };
    ($res: expr, $msg: tt) => {
        $crate::_error!(log::debug, $res, $msg)
    };
}

/// Macro for handle an error lists as warnings.
#[macro_export]
macro_rules! print_warns {
    ($errors:expr) => {
        $errors.iter().for_each(|e| {
            log::warn!(target: "debugger", "{:#}", e);
        })
    };
}
