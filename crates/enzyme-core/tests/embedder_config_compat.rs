//! Black-box regressions for caller-owned embedder configuration.

use enzyme_core::config::enzyme_config::{CatalyzedEntity, EnzymeConfig};

const CALLER_HEADER: &str = "# Margins search configuration\n# caller-owned header\n\n";

fn entity_strings(config: &EnzymeConfig) -> Vec<&str> {
    config
        .catalyzed_entities
        .iter()
        .filter_map(|entity| match entity {
            CatalyzedEntity::Simple(value) => Some(value.as_str()),
            CatalyzedEntity::WithOptions(_) => None,
        })
        .collect()
}

#[test]
fn explicit_path_initializes_caller_header_and_raw_vault_entry() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("raw-vault");
    std::fs::create_dir_all(&vault).unwrap();
    let config_path = root.path().join("product/settings.toml");
    let enzyme_home_before = std::env::var_os("ENZYME_HOME");

    EnzymeConfig::default()
        .set_auto_selected_entities_at(
            &vault,
            &config_path,
            vec!["[[Raw Vault]]".to_string()],
            CALLER_HEADER,
        )
        .unwrap();

    assert_eq!(std::env::var_os("ENZYME_HOME"), enzyme_home_before);
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.starts_with(CALLER_HEADER));
    assert!(raw.contains("[vaults."));
    assert!(!raw.contains("[workspaces."));
    let loaded = EnzymeConfig::load_at(&vault, &config_path)
        .unwrap()
        .unwrap();
    assert_eq!(entity_strings(&loaded), vec!["[[Raw Vault]]"]);
}

#[test]
fn explicit_path_owns_named_workspace_and_preserves_existing_document() {
    let root = tempfile::tempdir().unwrap();
    let config_home = root.path().join("caller-home");
    let workspace = config_home.join("workspaces/imessage");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_path = config_home.join("product.toml");
    std::fs::write(
        &config_path,
        r#"# Existing caller header -- keep exactly
[workspaces.imessage]
# product extension
mystery_key = "keep me"
entities = ["[[Old]]"]

[workspaces.imessage.sources.notes]
# caller-owned source comment
path = "/notes"

[vaults."/unrelated/raw-vault"]
entities = ["[[Untouched]]"]
"#,
    )
    .unwrap();
    let enzyme_home_before = std::env::var_os("ENZYME_HOME");
    let mut config = EnzymeConfig::load_at(&workspace, &config_path)
        .unwrap()
        .unwrap();

    config
        .set_auto_selected_entities_at(
            &workspace,
            &config_path,
            vec!["folder:conversations".to_string()],
            CALLER_HEADER,
        )
        .unwrap();

    assert_eq!(std::env::var_os("ENZYME_HOME"), enzyme_home_before);
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.starts_with("# Existing caller header -- keep exactly\n"));
    assert_eq!(
        raw.matches("# Existing caller header -- keep exactly")
            .count(),
        1
    );
    assert!(raw.contains("mystery_key = \"keep me\""));
    assert!(raw.contains("# caller-owned source comment"));
    assert!(raw.contains("[vaults.\"/unrelated/raw-vault\"]"));
    assert!(raw.contains("[[Untouched]]"));
    assert!(!raw.contains(&format!("[vaults.\"{}\"]", workspace.display())));

    let loaded = EnzymeConfig::load_at(&workspace, &config_path)
        .unwrap()
        .unwrap();
    assert_eq!(entity_strings(&loaded), vec!["folder:conversations"]);
    assert_eq!(
        loaded.notes_path.as_deref(),
        Some(std::path::Path::new("/notes"))
    );
}
