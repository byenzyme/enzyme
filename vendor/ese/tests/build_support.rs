#[path = "../build_support.rs"]
mod build_support;

use std::path::{Path, PathBuf};

use build_support::resolve_shared_model_dir;

#[test]
fn explicit_model_cache_takes_precedence() {
    let out = Path::new("/work/custom-target/debug/build/ese-hash/out");
    let cache = Path::new("/shared/models/ese");

    assert_eq!(
        resolve_shared_model_dir(out, Some(cache), "ese-cache").unwrap(),
        cache
    );
}

#[test]
fn finds_custom_target_dir_by_suffix() {
    let out = Path::new("/work/aside-cargo-target/debug/build/ese-hash/out");

    assert_eq!(
        resolve_shared_model_dir(out, None, "ese-cache").unwrap(),
        PathBuf::from("/work/aside-cargo-target/ese-cache")
    );
}

#[test]
fn falls_back_to_cargo_out_dir_layout_for_arbitrary_target_name() {
    let out = Path::new("/work/build-artifacts/debug/build/ese-hash/out");

    assert_eq!(
        resolve_shared_model_dir(out, None, "ese-cache").unwrap(),
        PathBuf::from("/work/build-artifacts/ese-cache")
    );
}

#[test]
fn shallow_out_dir_returns_a_diagnostic() {
    let error = resolve_shared_model_dir(Path::new("out"), None, "ese-cache").unwrap_err();

    assert!(error.contains("could not resolve Cargo target directory"));
    assert!(error.contains("out"));
}
