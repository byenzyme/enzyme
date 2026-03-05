fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        cc::Build::new()
            .file("compat/glibc_compat.c")
            .compile("glibc_compat");
    }
}
