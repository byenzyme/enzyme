use enzyme_core::kernel::Generator;
use enzyme_core::llm::ConfigProvider;

#[tokio::test]
async fn explicit_home_env_credentials_win_over_persisted_local_mode() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[llm]\nmode = \"local\"\nlocal_model = \"missing-model\"\n",
    )
    .unwrap();

    let previous_key = std::env::var("OPENAI_API_KEY").ok();
    let previous_model = std::env::var("OPENAI_MODEL").ok();
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "fixture-key");
        std::env::set_var("OPENAI_MODEL", "fixture-model");
    }
    let result = Generator::resolve_in(home.path()).await;
    unsafe {
        match previous_key {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match previous_model {
            Some(value) => std::env::set_var("OPENAI_MODEL", value),
            None => std::env::remove_var("OPENAI_MODEL"),
        }
    }

    let generator = result.unwrap();
    assert!(matches!(generator.provider(), ConfigProvider::Env));
    assert_eq!(generator.model(), "fixture-model");
    assert!(!home.path().join("bootstrap.json").exists());
}
