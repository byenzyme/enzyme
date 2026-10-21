use enzyme_core::config::{EnzymeConfig, migrate_readable_configs, readable_builtin_profiles};
use std::fs;
#[test]
fn migration_preserves_settings_and_makes_spec_authoritative() {
    let home = tempfile::tempdir().unwrap();
    let vault = tempfile::tempdir().unwrap();
    let key = vault.path().to_string_lossy();
    fs::write(
        home.path().join("config.toml"),
        format!(
            r##"[defaults]
minimum_tags=8
max_embedding_files=2048
[llm]
mode="hosted"
[update]
auto=false
[vaults.{key:?}]
min_top_catalysts=40
entities=[{{"folder:people"={{profile="relational",expandable=true}}}},"#craft"]
excluded_folders=["templates"]
"##
        ),
    )
    .unwrap();
    let before = EnzymeConfig::load_at(vault.path(), &home.path().join("config.toml"))
        .unwrap()
        .unwrap();
    let files = migrate_readable_configs(home.path(), Some(vault.path())).unwrap();
    assert!(files.iter().any(|p| p.ends_with("obsidian.enzyme")));
    assert!(!home.path().join("config.toml").exists());
    let after = EnzymeConfig::load_at(vault.path(), &home.path().join("config.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(after.excluded_folders, before.excluded_folders);
    assert_eq!(after.effective_max_embedding_files(), Some(2048));
    assert_eq!(
        after.reading_spec.as_ref().unwrap().profiles.len(),
        readable_builtin_profiles().len()
    );
    assert_eq!(
        EnzymeConfig::configured_vault_keys_at(&home.path().join("config.toml")).unwrap(),
        vec![key.to_string()]
    );
    let file = home.path().join("configs/obsidian.enzyme");
    let source = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        source.replace(
            "You create retrieval handles for what passes between the user",
            "READABLE_PROFILE_EDIT: You create retrieval handles for what passes between the user",
        ),
    )
    .unwrap();
    let changed = EnzymeConfig::load_at(vault.path(), &home.path().join("config.toml"))
        .unwrap()
        .unwrap();
    assert_ne!(
        serde_json::to_string(&after).unwrap(),
        serde_json::to_string(&changed).unwrap()
    );
    assert_eq!(
        EnzymeConfig::catalyst_llm_config_at(&home.path().join("config.toml"))
            .unwrap()
            .mode
            .to_string(),
        "hosted"
    );
    assert!(!EnzymeConfig::automatic_updates_enabled_at(&home.path().join("config.toml")).unwrap());
}
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
#[test]
fn unsupported_migration_keeps_original() {
    let home = tempfile::tempdir().unwrap();
    let content = "[workspaces.test.sources.notes]\npath='/notes'\n";
    fs::write(home.path().join("config.toml"), content).unwrap();
    assert!(migrate_readable_configs(home.path(), None).is_err());
    assert_eq!(
        fs::read_to_string(home.path().join("config.toml")).unwrap(),
        content
    );
    assert!(!home.path().join("configs").exists());
}
