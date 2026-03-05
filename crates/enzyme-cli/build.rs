use std::env;
use std::process::Command;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        let out_dir = env::var("OUT_DIR").unwrap();
        let obj = format!("{}/glibc_compat.o", out_dir);

        // Compile as a raw object file (not a static library) so that
        // all symbols are unconditionally included in the final binary.
        // GNU ld on aarch64 processes static libraries left-to-right and
        // discards unreferenced symbols, but .o files are always kept.
        let status = Command::new("cc")
            .args(["-c", "-o", &obj, "compat/glibc_compat.c"])
            .status()
            .expect("Failed to compile glibc_compat.c");
        assert!(status.success(), "Failed to compile glibc_compat.c");

        println!("cargo:rustc-link-arg={obj}");
        println!("cargo:rerun-if-changed=compat/glibc_compat.c");
    }
}
