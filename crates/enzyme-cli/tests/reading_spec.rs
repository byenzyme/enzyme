mod support;
use std::{fs, time::Duration};
use support::{fixture::VaultFixture, mock::MockLlm};

fn write_spec(f: &VaultFixture, marker: &str) {
    let dir = f.enzyme_home().join("configs");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("vault.enzyme"),format!(r#"
settings {{ generation hosted updates disabled }}
profile decisions {{ seek "{marker}: recover why one approach won" notice ["tradeoffs", "rejected alternatives"] }}
learning {{ question budget 3 sample across time favor recent periods }}
vault {:?} {{
 leave out folders ["private"]
 learn questions from tag "focus" about decisions
 when asked {{ "Use grep for exact phrases." retrieve passages through learned questions answer with sources }}
}}
"#,f.vault_path().to_string_lossy())).unwrap();
}
#[test]
fn init_and_refresh_use_rust_spec_and_regenerate_after_profile_edit() {
    let f = VaultFixture::empty();
    let llm = MockLlm::start();
    llm.set_ok_content(r#"{"catalysts":["Which migration tradeoff preserves source evidence?","What rejected alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#);
    for i in 0..3 {
        f.write_note(&format!("note-{i}.md"),&format!("---\ntags: [focus]\ncreated: 2026-08-0{}\n---\n# Migration evidence\n{}",i+1,"The team chose a reversible migration with auditable source evidence. Rejected alternatives traded traceability for speed. The decision depends on keeping old records available for comparison. ".repeat(20)));
    }
    f.write_note(
        "private/secret.md",
        "This excluded note must not be indexed. #focus",
    );
    write_spec(&f, "FIRST_PROFILE");
    let out = f
        .cli()
        .args(["init", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    out.assert_success();
    assert!(
        llm.requests().iter().any(|s| s.contains("FIRST_PROFILE")),
        "custom seek never reached LLM: {}",
        out.combined()
    );
    assert!(!f.enzyme_home().join("config.toml").exists());
    let db_path = f.vault_enzyme_dir().join("enzyme.db");
    // Locate the canonical database filename used by this checkout.
    let db_path = if db_path.exists() {
        db_path
    } else {
        f.vault_enzyme_dir().join("index.db")
    };
    let db = enzyme_core::db::Database::open(&db_path).unwrap();
    assert!(
        !db.get_all_document_hashes()
            .unwrap()
            .keys()
            .any(|p| p.contains("secret"))
    );
    let count = llm.request_count();
    write_spec(&f, "SECOND_PROFILE");
    let refreshed = f
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    refreshed.assert_success();
    let payload: serde_json::Value = serde_json::from_str(&refreshed.stdout).unwrap();
    let epoch = support::epoch::build_epoch_id(&payload);
    let worker = f
        .cli()
        .args(["refresh", "--detached", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    worker.assert_success();
    let state = support::epoch::wait_for_epoch_terminal(&db_path, &epoch, Duration::from_secs(60));
    assert_eq!(state, "active");
    assert!(llm.request_count() > count);
    assert!(
        llm.requests()
            .iter()
            .skip(count)
            .any(|s| s.contains("SECOND_PROFILE"))
    );
    let inspect = f
        .cli()
        .args(["spec", "inspect"])
        .arg(f.vault_path().to_string_lossy())
        .arg("--instructions")
        .run();
    inspect.assert_success();
    assert!(inspect.stdout.contains("petri --query"));
    let search = f
        .cli()
        .args(["catalyze", "reversible migration audit evidence"])
        .run();
    search.assert_success();
    assert!(search.stdout.contains("note-"));
}
#[test]
fn invalid_spec_fails_before_init_writes() {
    let f = VaultFixture::empty();
    fs::create_dir(f.enzyme_home().join("configs")).unwrap();
    fs::write(f.enzyme_home().join("configs/bad.enzyme"), "vault oops").unwrap();
    let out = f.cli().args(["init", "--stage", "index"]).run();
    out.assert_failure();
    assert!(!f.vault_enzyme_dir().join("enzyme.db").exists());
}

#[test]
fn compile_creates_setup_before_init_and_preserves_edits() {
    let f = VaultFixture::new();
    let first = f.cli().args(["compile", "-v", "--color", "never"]).run();
    first.assert_success();
    assert!(first.stdout.contains("learn questions from"));
    assert!(!first.stdout.contains('\u{1b}'));
    assert!(!f.vault_path().join(".enzyme").exists());
    assert!(!f.enzyme_home().join("config.toml").exists());
    let files = enzyme_spec::source_files(&f.enzyme_home().join("configs")).unwrap();
    assert_eq!(files.len(), 1);
    let path = &files[0].0;
    let original = format!("// Keep my comment\n{}", fs::read_to_string(path).unwrap());
    fs::write(path, &original).unwrap();
    let again = f.cli().args(["compile", "-v", "--color", "always"]).run();
    again.assert_success();
    assert!(again.stdout.contains("\x1b[1;36m"));
    assert_eq!(fs::read_to_string(path).unwrap(), original);
    let no_color = f.cli().args(["compile", "-v"]).env("NO_COLOR", "1").run();
    no_color.assert_success();
    assert!(!no_color.stdout.contains('\u{1b}'));
    fs::write(path, "vault broken {").unwrap();
    let invalid = f.cli().args(["compile", "-v"]).run();
    assert!(!invalid.success);
    assert_eq!(fs::read_to_string(path).unwrap(), "vault broken {");
}

#[test]
fn compile_highlights_complete_phrases_and_profile_strings() {
    let f = VaultFixture::empty();
    write_spec(&f, "highlight me");
    let output = f.cli().args(["compile", "-v", "--color", "always"]).run();
    output.assert_success();
    for word in [
        "learning",
        "question",
        "budget",
        "sample",
        "across",
        "time",
        "favor",
        "recent",
        "periods",
        "learn",
        "questions",
        "from",
        "tag",
        "about",
        "when",
        "asked",
        "retrieve",
        "passages",
        "through",
        "learned",
        "answer",
        "with",
        "sources",
        "generation",
        "hosted",
        "updates",
        "disabled",
    ] {
        assert!(
            output.stdout.contains(&format!("\x1b[1;36m{word}\x1b[0m")),
            "uncolored keyword: {word}"
        );
    }
    assert!(
        output
            .stdout
            .contains("\x1b[32m\"highlight me: recover why one approach won\"\x1b[0m")
    );
    assert!(output.stdout.contains("\x1b[32m\"tradeoffs\"\x1b[0m"));
    assert!(output.stdout.contains("\x1b[35mdecisions\x1b[0m"));
    let plain = f.cli().args(["compile", "-v", "--color", "never"]).run();
    plain.assert_success();
    assert!(!plain.stdout.contains('\u{1b}'));
}
