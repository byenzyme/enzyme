use enzyme_core::config::EnzymeConfig;
use std::fs;

#[test]
fn malformed_spec_does_not_fall_back_to_legacy() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("configs")).unwrap();
    fs::write(home.path().join("config.toml"), "[defaults]").unwrap();
    fs::write(home.path().join("configs/a.enzyme"), "vault bad").unwrap();
    assert!(
        EnzymeConfig::load_at(
            std::path::Path::new("/notes"),
            &home.path().join("config.toml")
        )
        .is_err()
    );
}
