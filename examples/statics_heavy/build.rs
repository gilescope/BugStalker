//! Generate a debuggee with many file-scope statics spread across
//! nested modules, mirroring the real-world pathology a debugger hits
//! on a dependency-rich binary (e.g. `tracing`/`hyper_util` litter the
//! address space with `__CALLSITE` statics). 40 modules × 50 entries ×
//! {read-only, interior-mutable} = 4000 statics.
//!
//! Two flavours per slot so the variables-view work can tell them
//! apart:
//!   - `RO_n: u64`            → read-only segment (`.rodata`), value
//!                              fixed at load — never needs re-reading.
//!   - `RW_n: AtomicU64`      → writable segment (`.data`), value can
//!                              change at runtime — must be re-read.
use std::io::Write;
use std::path::Path;

fn main() {
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("statics.rs");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&out).unwrap());
    for m in 0..40 {
        writeln!(f, "pub mod m{m} {{").unwrap();
        for i in 0..50 {
            writeln!(f, "    #[used] pub static RO_{i}: u64 = {i};").unwrap();
            writeln!(
                f,
                "    #[used] pub static RW_{i}: core::sync::atomic::AtomicU64 = \
                 core::sync::atomic::AtomicU64::new({i});"
            )
            .unwrap();
        }
        writeln!(f, "}}").unwrap();
    }
    println!("cargo:rerun-if-changed=build.rs");
}
