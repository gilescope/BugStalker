fn main() {
    let linux_gnu = cfg!(target_os = "linux") && cfg!(target_env = "gnu");
    let supported_arch = cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64");
    if !(linux_gnu && supported_arch) {
        panic!(
            "{} only works on linux-gnu (x86_64 or aarch64)",
            env!("CARGO_PKG_NAME")
        );
    }

    println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
    println!("cargo:rustc-link-tests=-Wl,--export-dynamic");
}
