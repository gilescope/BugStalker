# Changelog

All notable changes to this project will be documented in this file.

# [?.?.?] Unreleased

### Added

- debugger: experimental Linux/aarch64 support. Software breakpoints
  (`BRK #0`), general-purpose register read/write via `PTRACE_GETREGSET` +
  `NT_PRSTATUS`, and DWARF unwinding are functional. Hardware watchpoints,
  inferior function calls, and `libthread_db`-backed TLS inspection are
  stubbed and return clear errors.
- register: `RegisterMap::pc()` / `set_pc()` / `sp()` / `set_sp()`
  architecture-agnostic accessors, and `Register::PC` / `Register::SP`
  aliases.
- build: `Earthfile` with `+check`, `+build`, `+build-rel`, `+clippy`,
  `+fmt-check`, `+test`, `+all` targets. Select the target platform with
  `--BS_PLATFORM=linux/arm64` (default) or `--BS_PLATFORM=linux/amd64`.
- error: new `Error::WatchpointUnsupported` variant (returned on
  architectures where hardware watchpoints are not yet wired up).

### Changed

- `thread_db` is now an x86_64-only dependency; a thin in-tree shim
  (`debugger::thread_db_compat`) provides stubs on other architectures
  so the debugger degrades gracefully rather than failing to build.
- `src/debugger/register.rs` split into `src/debugger/register/{mod,
  x86_64,aarch64,debug}.rs`. Public surface under `debugger::register::*`
  and `debugger::register::debug::*` is preserved.
- doc: new `doc/ROADMAP.md` describing the larger-than-one-PR efforts
  the codebase is moving towards (Linux/aarch64 port progress, planned
  time-travel record-and-replay support, eventual native macOS port).
- debugger: experimental native macOS-arm64 support — pure-Mach
  Tracer (no ptrace), Mach-native `CallHelper`, software-breakpoint
  install via `mach_vm_write` + page-protection round-tripping,
  per-dylib `__TEXT.vmaddr` slide computation, dSYM bundle DWARF
  loader, eh_frame BaseAddresses with Mach-O section names, CU
  disambiguation when dsymutil's range engulfs other CUs, stray-BRK
  swallowing in dyld pages, and `Tracee::location()` fallback for
  unknown PC mappings. The integration suite reaches **58 passed /
  4 failed / 1 ignored / 12 filtered out (75 runnable)** on darwin
  with `--skip multithreaded --skip tokio --skip signal --skip
  test_step_over_for_loop_issue_156 --skip test_read_tls`. The 4
  remaining failures (dyld dlopen rendezvous, Debug::fmt vtable
  dispatch) and the skipped categories (multithreading, signals,
  TLS, the loop-step edge case) are tracked in the roadmap.

### Fixed

- call: align the debuggee's RSP to 16 bytes before the inferior `CALL`
  instruction in `CallHelper::call_fn`, as System V AMD64 requires.
  Previously the trampoline kept whatever RSP the debuggee was stopped
  at; depending on which line the breakpoint landed on, RSP was often
  only 8-aligned, which caused alignment-sensitive callees (anything
  using `movaps`/`movdqa` on stack locals — `Vec::reserve`,
  `String::push_str`, …) to take an intermittent `#GP` partway through
  `Debug::fmt`. Manifested as a flaky
  `tests/debugger/variables.rs::test_debug_trait_repr_vars` regardless
  of architecture.
### Deprecated
### Breaking changes

# [0.4.5] Apr 18 2026

### Added

- debugger: added support for rustc 1.95

### Changed

- lock gimli version to 0.33.0
- dap: rename Zed extension id

### Fixed

- debugger: stepover may skip some lines in for loops

---
# [0.4.4] March 28 2026

### Added

- dap: new DAP server with remote debugging support
- dap: `Zed` extension

### Fixed

- dap: now output events send immediately

---

# [0.4.3] March 6 2026

### Added

- debugger: added support rustc 1.94
- debugger: help for subcommands (#81)

### Changed

- debugger: set MSRV to 1.89.0 (#139)
- debugger: update gimli to 0.33.0

### Fixed

- debugger: panic when capacity of VecDeq equals to 0 (#144)

---

# [0.4.2] Jan 23 2026

### Added

- debugger: added support rustc 1.93

### Fixed

- console: fixed `watch +w` command
- fix: now `BsUnit::find_exact_place_by_pc` deterministically return always first suitable place

---

# [0.4.1] Jan 19 2026

### Fixed

- debugger: unwinder no longer stops if there is no debug information in some frame of a call stack

---

# [0.4.0] Jan 5 2026

### Added
- dap: introduce DAP extension for VS Code
- dap: introduce DAP server

### Changed
- build: remove libunwind-specific test target

---

# [0.3.6] Dec 13 2025

### Added

- debugger: added support rustc 1.92

---

# [0.3.5] Nov 3 2025

### Added

- debugger: add `GlobalContext`
- debugger: added support rustc 1.91

### Changed

- debugger: use string interner
- debugger: use ecx/ccx/pcx/etc naming for different contexts
- debugger: parse DIEs on demand rather than upfront to reduce initial memory load
- debugger: reduce memory consumption for debug information representation
- debugger: reduce memory consumption for symbol tables

### Fixed

- debugger: panic when vecdeque have infinite capacity (bug in debug info)

### Deprecated
### Breaking changes

---

# [0.3.4] Sep 19 2025

### Added

- debugger: added support for rustc 1.90

### Fixed

- build: fail early at compile rather than runtime

### Deprecated

- debugger: deprecate `libunwind` support

---

# [0.3.3] Aug 9 2025

### Added

- ui: new output for `backtrace` command (with source file and line)
- debugger: add `--save-history` option
- debugger: added support for rustc 1.89

### Changed

- update `tui-realm` and `tui-realm-treeview` components
- add `PopIf::pop_if_single_el`
- update `chumsky` to a stable version `0.10.1`
- debugger: now backtrace frames contains a source file and line

### Fixed

- tui: fix panic when there is a thread with unknown first frame function in backtrace
- debugger: fix panic when when parse zero-length arrays

---

# [0.3.2] Jun 30 2025

### Added
- debugger: added support for rustc 1.88
- debugger: new `DataCast` DQE op

---

# [0.3.1] May 18 2025

### Added
- debugger: added support for rustc 1.87

### Fixed
- debugger: enable LTO and codegen-units = 1 for release build

---

# [0.3.0] Apr 26 2025

### Added

- debugger: support for `SystemTime` and `Instant` std types
- debugger: support for constant initialized TLS variables
- debugger: new `async backtrace` command (#27)
- debugger: new `async backtrace all` command (#27)
- debugger: new `async task` command (#27)
- debugger: new `async stepover` command
- debugger: new `async stepout` command
- debugger: new `trigger` command (#39)
- debugger: new `call` command
- debugger: new `vard` and `argd` commands (#47)
- docs: introduce website and update README

### Changed

- debugger: refactor `select` module
- debugger: rename watch_point -> spy_point
- debugger: refactor `TypeIdentity`
- debugger: refactor variables specialized representation
- debugger: `variable` module refactoring
- debugger: improve rustc versions resolving
- ui: refactor command parser tests
- debugger: use IndexMap instead of HashMap for storing type parameters


### Fixed

- debugger: `stepover` command can no longer step out from the current source file
- debugger: now `restart` command doesn't affect a breakpoint numbers
- console: reduce redundant output for collections (arrays, maps, etc.) (fix #52)
- console: in variables output use spaces instead of tabs
- console: better memory command output
- debugger: fix rustup toolchain command parsing
- console: don't send duplicate SIGINT signal

---

# [0.2.8] Apr 7 2025

### Added
- debugger: added support for rustc 1.86

### Fixed
- fix broken nix flake
- fix CI libunwind installation script

---

# [0.2.7] Feb 23 2025

### Added
- debugger: added support for rustc 1.85

### Changed
- use rust edition 2024

---

# [0.2.6] Jan 13 2025

### Added
- debugger: added support for rustc 1.84

### Fixed
- update github actions

---

# [0.2.5] Nov 30 2024

### Added
- debugger: added support for rustc 1.83

---

# [0.2.4] Oct 20 2024

### Added

- debugger: added support for rustc 1.82
- debugger: fix flaky ordering in `sharedlib info` command

---

# [0.2.3] Sep 8 2024

### Added

- debugger: added support for rustc 1.81

---

# [0.2.2] Jul 27 2024

### Added

- debugger: added support for rustc 1.80

---

# [0.2.1] Jun 15 2024

### Added

- debugger: added support for rustc 1.79
- chore: added nix flake

### Changed

- debugger: now can find debugee binaries with `which`

---

# [0.2.0] Jun 3 2024

### Added

- tui: added ability to select tab across both windows
- tui: now left and right windows can expand (and the opposite window,
  accordingly, collapsed)
- ui: new argument (`-t` or `--theme`) for theme switching (affects program data
  and source code output)
- ui: warning if debugee compiled with an unsupported rustc version
- debugger: the index operation is now applicable to hashmaps, hashsets,
  btreemaps and others
- debugger: now containers (hashmaps, hashsets, etc.) can be indexed by literal
  objects for advanced searching
- console: improve index operation, now index accepts literal objects
- debugger: added address operator in data query expressions
- debugger: added watchpoints over hardware breakpoints
- debugger: added canonic operator
- tui: added keymap configuration

### Changed

- tui: now current active line (in a source code window and disassemble window)
  glued to the middle of render area instead of the bottom of the screen
- console: now program data (variables and arguments) stylized with syntect
- tui: now variable and thread tabs stylized with syntect

### Fixed

- ui: possible stack overflow when switching between ui types
- debugger: panic, when value of the right bound in a slice operator was greater than the underlying container lenght
- tui: panic, when breakpoint set at memory address
- tui: async error leads to ignoring of a new commands by TUI app
- debugger: check that value of DW_ATE_UTF encoding is valid utf8 char

---

# [0.1.5] May 3 2024

### Added

- debugger: added support for rustc 1.78

### Fixed

- debugger: now tracer doesn't add new tracee to tracee_ctl if first
  tracee.wait() return exited status instead of ptrace event status

---

# [0.1.4] April 3 2024

### Changed

- console: history hints now have better highlighting (grey instead of bolt)

### Fixed

- console: now sub commands (like break remove or break info) don't clash with
  operation + argument
- debugger: updated `unwind` crate to 0.4.2, now it must support rcX releases of
  libunwind
- console: fix expression parser. Now field op, index op and slice op have the
  same priority and can be combined in any order
- console: now command parser considers spaces when finding subcommands
