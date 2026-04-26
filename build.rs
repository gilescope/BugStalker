fn main() {
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
