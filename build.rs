// SPDX-License-Identifier: MIT
fn main() {
    emit_build_stamp();
    let linux_gnu = cfg!(target_os = "linux")
        && cfg!(target_env = "gnu")
        && (cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64"));
    let darwin_aarch64 = cfg!(target_os = "macos") && cfg!(target_arch = "aarch64");

    if !(linux_gnu || darwin_aarch64) {
        panic!(
            "{} supports linux-gnu (x86_64 or aarch64) and macos (aarch64; \
             native Darwin port is in progress). Other targets are not built.",
            env!("CARGO_PKG_NAME")
        );
    }

    // `--export-dynamic` is GNU ld; ld64 (the macOS linker) doesn't have
    // it. The flag is what lets `libthread_db.so.1` resolve our
    // `ps_*` proc_service symbols at dlopen time on Linux; on macOS
    // there's no libthread_db (and no equivalent), so the flag would
    // be a no-op anyway.
    if linux_gnu {
        println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
        println!("cargo:rustc-link-tests=-Wl,--export-dynamic");
    }
}

/// Emit `BS_BUILD_STAMP` so `--version` carries enough information to
/// tell a stale `~/.cargo/bin/bs` from a fresh one without manual
/// mtime detective work. Format: `<git-sha-12>[+dirty] <unix-ts>`.
/// If git isn't available (tarball install) we fall back to the
/// build timestamp alone — better than the previous nothing.
fn emit_build_stamp() {
    use std::process::Command;
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = match sha {
        Some(s) => format!("{s}{} {ts}", if dirty { "+dirty" } else { "" }),
        None => format!("nogit {ts}"),
    };
    println!("cargo:rustc-env=BS_BUILD_STAMP={stamp}");
    // Re-run when HEAD or the working tree changes so the stamp stays
    // accurate during interactive development.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
