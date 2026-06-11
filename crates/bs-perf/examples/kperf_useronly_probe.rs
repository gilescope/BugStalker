// SPDX-License-Identifier: MIT
//! CONFIGURABLE-counter + KPEP feasibility probe (`debug-step-costs.md` #3, the
//! decisive gate for the precise PMU tier). Root-only.
//!
//! Probe 2 showed the FIXED counters track instructions cleanly but count
//! user+kernel, so they can't dodge the ~35k/trap kernel floor. This probe
//! drives the CONFIGURABLE path: load the per-chip KPEP event database, program
//! an instruction-retired event onto a configurable counter via `kpc_set_config`,
//! and re-run the same workloads. Two questions:
//!
//!   1. Does the full KPEP→kpc_set_config→read pipeline work (foundation for any
//!      precise implementation)?
//!   2. Does the configured counter still count kernel (syscall loop inflates),
//!      or can we get user-only? We dump the generated config words so the
//!      EL0/EL1 control bits are visible for the (riskier) user-only follow-up.
//!
//!   cargo build -p bs-perf --example kperf_useronly_probe
//!   sudo ./target/debug/examples/kperf_useronly_probe
fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    run();
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    println!("kperf user-only probe is macOS/aarch64-only");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run() {
    use bs_perf::darwin::symbols::library;
    use libloading::{Library, Symbol};
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::hint::black_box;
    use std::ptr;

    const KPC_MAX: usize = 32;
    // Candidate retired-instruction event names across Apple Silicon KPEP dbs.
    const INST_EVENTS: &[&str] = &[
        "INST_ALL",
        "FIXED_INSTRUCTIONS",
        "INST_RETIRED",
        "instructions",
    ];
    const KPERFDATA: &str = "/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata";

    // Opaque KPEP handles (kpep_db*, kpep_config*, kpep_event*).
    type Db = *mut c_void;
    type Cfg = *mut c_void;
    type Ev = *mut c_void;

    let euid = unsafe { libc::geteuid() };
    println!("euid={euid} ({})", if euid == 0 { "root" } else { "user" });

    let kpc = match library() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kperf unavailable: {e}");
            return;
        }
    };

    // Load kperfdata (the KPEP database lives here, not in kperf).
    let kpd = match unsafe { Library::new(KPERFDATA) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot load kperfdata: {e}");
            return;
        }
    };
    // Resolve the KPEP symbols we need. SAFETY: each asserts the C ABI type
    // from the reverse-engineered <kperfdata> headers (kpc_demo.c lineage).
    macro_rules! sym {
        ($n:literal, $t:ty) => {
            match unsafe { kpd.get::<$t>(concat!($n, "\0").as_bytes()) } {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("missing kperfdata symbol {}: {e}", $n);
                    return;
                }
            }
        };
    }
    let kpep_db_create: Symbol<unsafe extern "C" fn(*const c_char, *mut Db) -> c_int> = sym!(
        "kpep_db_create",
        unsafe extern "C" fn(*const c_char, *mut Db) -> c_int
    );
    let kpep_db_free: Symbol<unsafe extern "C" fn(Db)> =
        sym!("kpep_db_free", unsafe extern "C" fn(Db));
    let kpep_db_event: Symbol<unsafe extern "C" fn(Db, *const c_char, *mut Ev) -> c_int> = sym!(
        "kpep_db_event",
        unsafe extern "C" fn(Db, *const c_char, *mut Ev) -> c_int
    );
    let kpep_config_create: Symbol<unsafe extern "C" fn(Db, *mut Cfg) -> c_int> = sym!(
        "kpep_config_create",
        unsafe extern "C" fn(Db, *mut Cfg) -> c_int
    );
    let kpep_config_free: Symbol<unsafe extern "C" fn(Cfg)> =
        sym!("kpep_config_free", unsafe extern "C" fn(Cfg));
    let kpep_config_force_counters: Symbol<unsafe extern "C" fn(Cfg) -> c_int> = sym!(
        "kpep_config_force_counters",
        unsafe extern "C" fn(Cfg) -> c_int
    );
    let kpep_config_add_event: Symbol<unsafe extern "C" fn(Cfg, *mut Ev, u32, *mut u32) -> c_int> = sym!(
        "kpep_config_add_event",
        unsafe extern "C" fn(Cfg, *mut Ev, u32, *mut u32) -> c_int
    );
    let kpep_config_kpc_count: Symbol<unsafe extern "C" fn(Cfg, *mut usize) -> c_int> = sym!(
        "kpep_config_kpc_count",
        unsafe extern "C" fn(Cfg, *mut usize) -> c_int
    );
    let kpep_config_kpc_classes: Symbol<unsafe extern "C" fn(Cfg, *mut u32) -> c_int> = sym!(
        "kpep_config_kpc_classes",
        unsafe extern "C" fn(Cfg, *mut u32) -> c_int
    );
    let kpep_config_kpc_map: Symbol<unsafe extern "C" fn(Cfg, *mut usize, usize) -> c_int> = sym!(
        "kpep_config_kpc_map",
        unsafe extern "C" fn(Cfg, *mut usize, usize) -> c_int
    );
    let kpep_config_kpc: Symbol<unsafe extern "C" fn(Cfg, *mut u64, usize) -> c_int> = sym!(
        "kpep_config_kpc",
        unsafe extern "C" fn(Cfg, *mut u64, usize) -> c_int
    );

    // --- build the KPEP config for an instruction event ---
    // SAFETY: out-params point at locals that outlive each call; we check every
    // return code and bail before using a handle that failed to initialise.
    let mut db: Db = ptr::null_mut();
    if unsafe { kpep_db_create(ptr::null(), &mut db) } != 0 || db.is_null() {
        eprintln!("kpep_db_create failed");
        return;
    }
    let mut cfg: Cfg = ptr::null_mut();
    if unsafe { kpep_config_create(db, &mut cfg) } != 0 || cfg.is_null() {
        eprintln!("kpep_config_create failed");
        unsafe { kpep_db_free(db) };
        return;
    }
    unsafe { kpep_config_force_counters(cfg) };

    let mut ev: Ev = ptr::null_mut();
    let mut chosen = "";
    for name in INST_EVENTS {
        let c = CString::new(*name).unwrap();
        if unsafe { kpep_db_event(db, c.as_ptr(), &mut ev) } == 0 && !ev.is_null() {
            chosen = name;
            break;
        }
    }
    if ev.is_null() {
        eprintln!("no instruction event found in KPEP db (tried {INST_EVENTS:?})");
        unsafe {
            kpep_config_free(cfg);
            kpep_db_free(db);
        }
        return;
    }
    println!("KPEP instruction event: {chosen:?}");

    let mut err = 0u32;
    if unsafe { kpep_config_add_event(cfg, &mut ev, 0, &mut err) } != 0 {
        eprintln!("kpep_config_add_event failed (err={err})");
        unsafe {
            kpep_config_free(cfg);
            kpep_db_free(db);
        }
        return;
    }

    let mut count = 0usize;
    let mut classes = 0u32;
    let mut config = [0u64; KPC_MAX];
    let mut map = [0usize; KPC_MAX];
    // SAFETY: all buffers are KPC_MAX-sized; we pass the byte sizes the API expects.
    unsafe {
        kpep_config_kpc_count(cfg, &mut count);
        kpep_config_kpc_classes(cfg, &mut classes);
        kpep_config_kpc_map(
            cfg,
            map.as_mut_ptr(),
            KPC_MAX * std::mem::size_of::<usize>(),
        );
        kpep_config_kpc(
            cfg,
            config.as_mut_ptr(),
            KPC_MAX * std::mem::size_of::<u64>(),
        );
    }
    println!(
        "kpc classes=0x{classes:x}  reg count={count}  event counter index={}",
        map[0]
    );
    print!("config words:");
    for w in &config[..count.max(1)] {
        print!(" 0x{w:016x}");
    }
    println!("\n  (^ inspect for EL0/EL1 enable bits — the user-only follow-up flips these)");

    // --- program the PMU and read ---
    // SAFETY: resolved kpc fn pointers; config buffer matches the class layout.
    if unsafe { (kpc.kpc_force_all_ctrs_set)(1) } != 0 {
        eprintln!(
            "kpc_force_all_ctrs_set failed (errno {}). Run under sudo.",
            std::io::Error::last_os_error()
        );
        unsafe {
            kpep_config_free(cfg);
            kpep_db_free(db);
        }
        return;
    }
    let rc = unsafe { (kpc.kpc_set_config)(classes, config.as_mut_ptr()) };
    if rc != 0 {
        eprintln!("kpc_set_config failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    unsafe {
        let _ = (kpc.kpc_set_counting)(classes);
        let _ = (kpc.kpc_set_thread_counting)(classes);
    }

    let total = {
        // SAFETY: plain int call.
        let t = unsafe { (kpc.kpc_get_counter_count)(classes) };
        usize::try_from(t.max(0)).unwrap().min(KPC_MAX)
    };
    let idx = map[0].min(KPC_MAX - 1);
    let read = || -> u64 {
        let mut buf = [0u64; KPC_MAX];
        // SAFETY: buf is KPC_MAX; we pass `total` (≤ KPC_MAX) as the count.
        let rc = unsafe {
            (kpc.kpc_get_thread_counters)(0, u32::try_from(total).unwrap(), buf.as_mut_ptr())
        };
        assert_eq!(rc, 0, "kpc_get_thread_counters failed");
        buf[idx]
    };

    println!("\n--- configured-event Δ/iter (counter idx {idx}) ---");
    let user_iters = 1_000_000u64;
    let a = read();
    black_box(user_workload(black_box(user_iters)));
    let b = read();
    let user_per = b.saturating_sub(a) as f64 / user_iters as f64;
    println!(
        "pure-user loop   : {:>11} Δ  ({user_per:.2}/it)",
        b.saturating_sub(a)
    );

    let sys_iters = 1_000_000u64;
    let a = read();
    syscall_workload(sys_iters);
    let b = read();
    let sys_per = b.saturating_sub(a) as f64 / sys_iters as f64;
    println!(
        "syscall loop     : {:>11} Δ  ({sys_per:.2}/it)",
        b.saturating_sub(a)
    );
    println!(
        "\nVERDICT: syscall/it = {sys_per:.0} vs user/it = {user_per:.0} → {}",
        if sys_per > user_per * 4.0 {
            "still counts KERNEL (need user-only config / EL0 bits in the words above)"
        } else {
            "USER-ONLY! kernel excluded → PMU can beat the floor (true ~8 per step)"
        }
    );

    // --- teardown ---
    // SAFETY: resolved fn pointers; freeing handles we created.
    unsafe {
        let _ = (kpc.kpc_set_counting)(0);
        let _ = (kpc.kpc_force_all_ctrs_set)(0);
        kpep_config_free(cfg);
        kpep_db_free(db);
    }
    println!("\nPMU released.");

    fn user_workload(n: u64) -> u64 {
        let mut acc = 0u64;
        let mut k = 0u64;
        while k < n {
            acc = acc.wrapping_add(black_box(k));
            k = k.wrapping_add(1);
        }
        acc
    }
    fn syscall_workload(n: u64) {
        for _ in 0..n {
            // SAFETY: FFI to close(2); -1 is invalid → EBADF, no side effect.
            unsafe {
                libc::close(black_box(-1));
            }
        }
    }
}
