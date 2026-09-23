use enzyme_spec::*;
use std::path::Path;
fn compile(s: &str) -> Program {
    resolve(vec![parse(s).unwrap()], Path::new("/home/demo")).unwrap()
}
#[test]
fn inheritance_and_inline_profiles() {
    let p = compile(
        r#"
learning { question budget 40 sample across time favor recent periods }
profile relationships { seek "connections" notice ["people", "projects"] }
vault "~/notes" {
 learning { question budget 30 }
 learn questions from folder "people" including linked pages about relationships
 learn questions from folder "archive" about profile { seek "choices" notice ["alternatives"] } { favor all periods equally question budget 12 }
}"#,
    );
    let v = &p.vaults[0];
    assert_eq!(v.path, "/home/demo/notes");
    assert_eq!(v.readings[0].learning.budget, Some(30));
    assert_eq!(v.readings[1].learning.budget, Some(12));
    assert_eq!(v.readings[1].learning.favor.as_deref(), Some("equal"));
    assert_eq!(
        v.readings[0].definition.as_ref().unwrap().seek,
        "connections"
    );
    assert!(v.readings[0].include_linked_pages);
}
#[test]
fn reusable_profiles_across_files() {
    let a = parse("profile p { seek \"continuity\" notice [\"decisions\"] }").unwrap();
    let b =
        parse("vault \"/notes\" { learn questions from log \"observations\" about p }").unwrap();
    let r = resolve(vec![a, b], Path::new("/home/demo")).unwrap();
    assert_eq!(
        r.vaults[0].readings[0].definition.as_ref().unwrap().seek,
        "continuity"
    );
}
#[test]
fn builtin_names_are_resolved() {
    let p = compile(
        "vault \"/notes\" { learn questions from tags [\"startup\", \"design\"] about decisions }",
    );
    assert_eq!(p.vaults[0].readings[0].profile, "decision_trace");
}
#[test]
fn invalid_programs_fail() {
    for s in [
        "vault \"oops",
        "vault \"/notes\" { surprise }",
        "learning { question budget 0 }",
        "profile x { seek \"x\" }",
        "vault \"/notes\" { learn questions from tag \"x\" including linked pages about auto }",
        "\"vault\" \"/notes\" {}",
        "vault \"/notes\" { expire after 90 days }",
    ] {
        assert!(parse(s).is_err(), "{s}");
    }
}
#[test]
fn resolution_rejects_ambiguity() {
    for s in [
        "vault \"/notes\" { learn questions from tag \"x\" about unknown }",
        "vault \"/notes\" {} vault \"/notes\" {}",
        "vault \"/notes\" { learn questions from tag \"x\" about auto learn questions from tag \"#x\" about auto }",
        "vault \"/notes\" { leave out folders [\"private\"] learn questions from folder \"private/people\" about relationships }",
    ] {
        assert!(
            resolve(vec![parse(s).unwrap()], Path::new("/home/demo")).is_err(),
            "{s}"
        );
    }
}
#[test]
fn error_location() {
    assert!(
        parse("vault \"/notes\" {\n surprise }")
            .unwrap_err()
            .to_string()
            .contains("2:2")
    );
}
#[test]
fn memory_compiles_guidance_without_writing() {
    let p = compile(
        r#"vault "/notes" { remember in "observations.md" when { "A preference is corrected." append observation { "Keep the evidence." } } learn questions from log "observations" about preferences when asked { "Use grep for exact phrases." retrieve passages through learned questions answer with sources } }"#,
    );
    let text = instructions(&p.vaults[0]);
    assert!(text.contains("petri --query"));
    assert!(text.contains("catalyze"));
    assert!(text.contains("refresh --quiet"));
    assert!(text.contains("Keep the evidence"));
}
#[test]
fn render_roundtrip() {
    for name in ["vault", "memory", "current-engine", "overrides"] {
        let s = std::fs::read_to_string(format!("../../tools/reading-spec/examples/{name}.enzyme"))
            .unwrap();
        let ast = parse(&s).unwrap();
        let rendered = render_program(&ast);
        assert_eq!(
            resolve(vec![ast], Path::new("/home/demo")).unwrap(),
            resolve(vec![parse(&rendered).unwrap()], Path::new("/home/demo")).unwrap(),
            "{name}"
        );
    }
}
#[test]
fn overrides_do_not_mutate_siblings() {
    let p = compile(
        "learning { favor recent periods } vault \"/notes\" { learn questions from folder \"a\" about auto { favor all periods equally } learn questions from folder \"b\" about auto }",
    );
    assert_eq!(
        p.vaults[0].readings[1].learning.favor.as_deref(),
        Some("recent")
    );
}
#[test]
fn quotes_are_data() {
    let p = compile(r#"vault "/notes" { when asked { "// <script> { }" } }"#);
    assert!(instructions(&p.vaults[0]).contains("// <script> { }"));
}
#[test]
fn directory_is_deterministic_and_rejects_duplicates() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("a.enzyme"),
        "profile p { seek \"x\" notice [\"y\"] }",
    )
    .unwrap();
    std::fs::write(
        d.path().join("b.enzyme"),
        "vault \"/notes\" { learn questions from tag \"x\" about p }",
    )
    .unwrap();
    assert_eq!(
        load_directory(d.path(), Path::new("/home/demo")).unwrap(),
        load_directory(d.path(), Path::new("/home/demo")).unwrap()
    );
    std::fs::write(
        d.path().join("c.enzyme"),
        "profile p { seek \"changed\" notice [\"y\"] }",
    )
    .unwrap();
    assert!(load_directory(d.path(), Path::new("/home/demo")).is_err());
}
#[test]
fn full_profile_roundtrips() {
    let p = Profile {
        seek: "purpose".into(),
        notice: vec!["evidence".into()],
        ask: vec!["What shifted?".into()],
        note: Some("Preserve context".into()),
        recognize: vec!["meeting notes".into()],
    };
    assert_eq!(parse(&render_profile("p", &p)).unwrap().profiles["p"], p);
}

#[test]
fn append_log_runtime_kind_resolves_its_reading() {
    let p = compile(
        "vault \"/notes\" { learn questions from log \"observations\" about preferences { question budget 8 } }",
    );
    assert_eq!(
        p.vaults[0]
            .reading("observations", "append_log")
            .unwrap()
            .learning
            .budget,
        Some(8)
    );
}

#[test]
fn newline_blocks_preserve_array_semantics_in_migrated_programs() {
    let source = r#"
profile people {
  seek "relationships"
  notice ["shared interests", "changing priorities"]
  ask ["What matters now?"]
  recognize ["meeting notes", "personal notes"]
}
vault "/notes" {
  leave out folders ["archive", "templates"]
  leave out tags ["private", "draft"]
  leave out links ["Excluded"]
  references in fields ["people", "attendees"]
  learn questions from tags ["friends", "work"] about people
}
"#;
    let parsed = parse(source).unwrap();
    let rendered = render_program(&parsed);
    // Prose elements keep one per line: a long guidance string stays legible only
    // as a block, whatever construct holds it.
    let prose = parse(
        r#"profile p {
  seek "x"
  notice ["Make trade-offs explicit-what does choosing A foreclose about B?"]
}
vault "/notes" { learn questions from tag "a" about p }
"#,
    )
    .unwrap();
    let prose_rendered = render_program(&prose);
    assert!(prose_rendered.contains(
        "notice {\n    \"Make trade-offs explicit-what does choosing A foreclose about B?\"\n  }"
    ));
    assert_eq!(parse(&prose_rendered).unwrap(), prose);
    assert_eq!(parse(&rendered).unwrap(), parsed);
    assert_eq!(compile(&rendered), compile(source));
    assert!(parse("profile p { seek \"x\" notice { } }").is_err());
    assert!(parse("profile p { seek \"x\" notice { \"y\" ] }").is_err());
    let grouped =
        compile("vault \"/notes\" { learn questions from tags { \"a\" \"b\" } about auto }");
    assert_eq!(grouped.vaults[0].readings.len(), 2);
}

#[test]
fn short_token_lists_fill_inline_below_the_readings() {
    let long: Vec<String> = [
        ".agents", ".claude", ".codex", ".conversations", ".enzyme", ".git", ".obsidian", ".trash",
        "__pycache__", "build", "dist", "node_modules", "target", "templates",
    ]
    .iter()
    .map(|s| format!("\"{s}\""))
    .collect();
    let source = format!(
        r#"vault "/notes" {{
  leave out folders [{}]
  references in fields ["people", "attendees"]
  learn questions from folder "people" about relationships
  learn questions from folder "inbox"
}}
"#,
        long.join(", ")
    );
    let parsed = parse(&source).unwrap();
    let rendered = render_program(&parsed);

    // Short scalar tokens fill inline rather than taking a line each.
    assert!(rendered.contains("leave out folders [\".agents\", \".claude\""));
    assert!(rendered.contains("references in fields [\"people\", \"attendees\"]"));
    // A list too long for one line wraps instead of reverting to a block.
    assert!(!rendered.contains("leave out folders {"));
    assert!(rendered.lines().all(|l| l.chars().count() <= 84), "{rendered}");

    // The file opens on the readings; bookkeeping follows them.
    let readings = rendered.find("learn questions").unwrap();
    let exclusions = rendered.find("leave out folders").unwrap();
    let fields = rendered.find("references in fields").unwrap();
    assert!(readings < exclusions, "{rendered}");
    assert!(readings < fields, "{rendered}");

    // Wrapping is presentation only.
    assert_eq!(parse(&rendered).unwrap(), parsed);
    assert_eq!(compile(&rendered), compile(&source));
}

#[test]
fn identical_exported_profiles_share_a_namespace_but_conflicts_fail() {
    let one = enzyme_spec::parse(r#"profile decisions { seek "why" notice { "tradeoffs" } } vault "/one" { learn questions from tag "focus" about decisions }"#).unwrap();
    let two = enzyme_spec::parse(r#"profile decisions { seek "why" notice { "tradeoffs" } } vault "/two" { learn questions from tag "focus" about decisions }"#).unwrap();
    let resolved = enzyme_spec::resolve(
        vec![one.clone(), two.clone()],
        std::path::Path::new("/home/test"),
    )
    .unwrap();
    assert_eq!(resolved.profiles.len(), 1);
    assert_eq!(resolved.vaults.len(), 2);
    let mut changed = two;
    changed.profiles.get_mut("decisions").unwrap().seek = "different".into();
    assert!(enzyme_spec::resolve(vec![one, changed], std::path::Path::new("/home/test")).is_err());
}

#[test]
fn omitted_profiles_default_to_auto_and_roundtrip_with_reading_options() {
    let source = r#"vault "/notes" {
        learn questions from tag "spirit"
        learn questions from tags { "taste" "comms" } about auto
        learn questions from folder "people" including linked pages { question budget 12 }
        learn questions from log "journal" { sample by time }
        learn questions from tag "bestill" about reflective
    }"#;
    let parsed = parse(source).unwrap();
    let readings = &parsed.vaults[0].readings;
    assert_eq!(readings.len(), 6);
    assert!(
        readings[..5]
            .iter()
            .all(|r| r.profile == "auto" && r.definition.is_none())
    );
    assert!(readings[3].include_linked_pages);
    assert_eq!(readings[3].learning.budget, Some(12));
    let rendered = render_program(&parsed);
    assert!(!rendered.contains("about auto"));
    assert!(rendered.contains("about reflective"));
    assert_eq!(parse(&rendered).unwrap(), parsed);
    assert_eq!(compile(&rendered), compile(source));
    assert!(parse(r#"vault "/notes" { learn questions from tag "x" about }"#).is_err());
}

#[test]
fn implicit_exclusions_are_omitted_but_real_choices_survive() {
    // Naming an index-gate default is inert: discovery appends the user's list to
    // its own defaults rather than replacing them, so suppressing these cannot
    // change what is indexed. Folders it does not enforce must stay explicit.
    assert!(enzyme_spec::is_implicit_exclusion("node_modules"));
    assert!(enzyme_spec::is_implicit_exclusion(".git"));
    assert!(enzyme_spec::is_implicit_exclusion("Node_Modules"));
    assert!(enzyme_spec::is_implicit_exclusion("target/"));
    assert!(!enzyme_spec::is_implicit_exclusion("templates"));
    assert!(!enzyme_spec::is_implicit_exclusion(".aside"));
    assert!(!enzyme_spec::is_implicit_exclusion("archive"));

    // Suppression happens where the program is generated, so rendering stays
    // faithful: anything a program does say still round-trips verbatim.
    let source = r#"vault "/notes" {
  learn questions from folder "people"
  leave out folders ["node_modules", "archive"]
}
"#;
    let parsed = parse(source).unwrap();
    let rendered = render_program(&parsed);
    assert!(rendered.contains("leave out folders [\"node_modules\", \"archive\"]"));
    assert_eq!(parse(&rendered).unwrap(), parsed);
}

#[test]
fn selection_grammar_inheritance_roundtrip_and_limits() {
    let source = r#"
learning { select 50 by frequency }
vault "/notes" {
 learning { select 50% by recency }
 learn questions from folder "people" including linked pages
 learn questions from folders ["projects", "efforts"] including linked pages
   select 10% by frequency up to 100 sample across time question budget 4
 learn questions from folder "small" including linked pages select 0.5% by frequency
}"#;
    let p = compile(source);
    let readings = &p.vaults[0].readings;
    assert_eq!(
        readings[0].learning.selection.as_ref().unwrap().by,
        SelectionMode::Recency
    );
    assert_eq!(
        readings[0].learning.selection.as_ref().unwrap().limit(15),
        8
    );
    let policy = readings[1].learning.selection.as_ref().unwrap();
    for (eligible, expected) in [(0, 0), (1, 1), (11, 2), (1000, 100), (2000, 100)] {
        assert_eq!(policy.limit(eligible), expected);
    }
    assert_eq!(
        readings[2].learning.selection,
        readings[1].learning.selection
    );
    assert_eq!(
        readings[3].learning.selection.as_ref().unwrap().limit(201),
        2
    );
    for s in [
        "select 50 by frequency",
        "select 50 by recency",
        "select 100% by frequency",
        "select 7% by frequency",
        "select 0.07% by frequency",
    ] {
        let p = compile(&format!(
            r#"vault "/notes" {{ learn questions from folder "efforts" including linked pages {s} }}"#
        ));
        let selection = p.vaults[0].readings[0].learning.selection.as_ref().unwrap();
        assert_eq!(selection.limit(0), 0);
        assert_eq!(selection.limit(1), 1);
        if s.starts_with("select 0.07%") {
            assert_eq!(selection.limit(10000), 7);
        }
        if s.starts_with("select 7%") {
            assert_eq!(selection.limit(100), 7);
        }
    }
    let count = p.learning.selection.as_ref().unwrap();
    assert_eq!(count.limit(100), 50);
    assert_eq!(count.limit(3), 3);
    let normalized = render_program(&parse(source).unwrap());
    assert_eq!(compile(&normalized), p);
    assert_eq!(compile(&render_program(&p)), p);
    assert_eq!(
        serde_json::from_str::<Program>(&serde_json::to_string(&p).unwrap()).unwrap(),
        p
    );
    let plan = playground(source);
    assert_eq!(
        plan["plan"]["vaults"][0]["readings"][1]["learning"]["selection"]["by"],
        "frequency"
    );
    assert!(
        compile(r#"vault "/notes" { learn questions from folder "people" }"#).vaults[0].readings[0]
            .learning
            .selection
            .is_none()
    );
}

#[test]
fn selection_rejects_ambiguous_or_invalid_policies() {
    for clause in [
        "select",
        "select 50.% by frequency",
        "select 0 by frequency",
        "select 0% by frequency",
        "select 101% by frequency",
        "select 100.000000000000000001% by frequency",
        "select -1 by frequency",
        "select -1% by frequency",
        "select 0.5 by frequency",
        "select half by frequency",
        "select 2 by coverage",
        "select 50% frequency",
        "select 1 by frequency up to 3",
        "select 10% by frequency up to 0",
        "select 10% up to 3 by frequency",
        "select 10% by frequency up to 1.5",
        "select 2 by frequency select 3 by recency",
        "select 1e2% by frequency",
        "select 10%% by frequency",
        "select NaN% by frequency",
        "select 1..2% by recency",
    ] {
        let source = format!(
            r#"vault "/notes" {{ learn questions from folder "people" including linked pages {clause} }}"#
        );
        assert!(parse(&source).is_err(), "accepted {clause}");
    }
    assert!(parse("learning { select").is_err());
    assert!(
        parse(r#"vault "/notes" { learn questions from tag "people" select 3 by frequency }"#)
            .is_err()
    );
}

#[test]
fn concise_sampling_normalizes_legacy_and_preserves_inheritance() {
    for (mode, legacy) in [
        ("time", "sample across time favor all periods equally"),
        ("time", "sample across time"),
        ("time", "favor all periods equally"),
        ("recency", "sample across time favor recent periods"),
        ("recency", "favor recent periods"),
    ] {
        let source = format!(
            r#"learning {{ question budget 4 {legacy} }} vault "/notes" {{ learn questions from folder "a" }}"#
        );
        let old = compile(&source);
        let concise = source.replace(legacy, &format!("sample by {mode}"));
        assert_eq!(old, compile(&concise));
        for program in [parse(&source).unwrap(), old.clone()] {
            let rendered = render_program(&program);
            assert!(rendered.contains(&format!("sample by {mode}")));
            assert!(!rendered.contains("sample across time") && !rendered.contains("favor "));
            assert_eq!(compile(&rendered), old);
        }
        let plan = playground(&concise);
        assert_eq!(
            plan["plan"]["vaults"][0]["readings"][0]["learning"]["sampling_mode"],
            mode
        );
    }
    for source in [
        r#"learning { sample by recency question budget 4 } vault "/notes" { learn questions from folder "a" sample by time learn questions from link "child" sample by recency learn questions from folder "b" }"#,
        r#"learning { favor recent periods question budget 4 } vault "/notes" { sample across time learn questions from folder "a" favor all periods equally learn questions from link "child" sample by recency learn questions from folder "b" sample across time }"#,
    ] {
        let p = compile(source);
        let v = &p.vaults[0];
        assert_eq!(v.learning.sampling_mode(), Some("recency"));
        assert_eq!(v.readings[0].learning.sampling_mode(), Some("time"));
        assert_eq!(v.readings[1].learning.sampling_mode(), Some("recency"));
        assert_eq!(v.readings[2].learning.sampling_mode(), Some("recency"));
        assert!(v.readings.iter().all(|r| r.learning.budget == Some(4)));
        assert_eq!(compile(&render_program(&parse(source).unwrap())), p);
        assert_eq!(compile(&render_program(&p)), p);
        assert_eq!(
            serde_json::from_str::<Program>(&serde_json::to_string(&p).unwrap()).unwrap(),
            p
        );
    }
}

#[test]
fn concise_sampling_rejects_mixed_duplicate_and_unknown_modes() {
    for clause in [
        "sample by",
        "sample by recent",
        "sample by frequency",
        "sample by half",
        "sample time",
        "sample by time by recency",
        "sample by time sample by recency",
        "sample by time sample by time",
        "sample by recency sample across time",
        "sample across time sample by time",
        "sample by time favor recent periods",
        "favor recent periods sample by time",
        "sample by recency favor recent periods",
        "favor all periods equally sample by time",
    ] {
        assert!(
            parse(&format!("learning {{ {clause} }}")).is_err(),
            "{clause}"
        );
    }
    let a = parse("learning { sample by time }").unwrap();
    let b = parse("learning { favor recent periods }").unwrap();
    for files in [vec![a.clone(), b.clone()], vec![b, a]] {
        assert!(resolve(files, Path::new("/home/demo")).is_err());
    }
}

#[test]
fn as_of_scopes_roundtrip_and_inherit_by_vault() {
    let source = r#"
learning { as of "2024-10-01" sample by recency }
vault "/old" {
  learn questions from folder "people" including linked pages
}
vault "/current" {
  as of latest evidence
  learn questions from folder "people" { question budget 5 }
}
vault "/block" {
  learning { as of "2024-02-29" }
}"#;
    let p = compile(source);
    let date = |d: &str| Some(AsOf::Date { date: d.into() });
    assert_eq!(p.learning.as_of, date("2024-10-01"));
    assert_eq!(p.vaults[0].learning.as_of, date("2024-10-01"));
    assert_eq!(p.vaults[0].readings[0].learning.as_of, date("2024-10-01"));
    assert_eq!(p.vaults[1].learning.as_of, Some(AsOf::LatestEvidence));
    assert_eq!(
        p.vaults[1].readings[0].learning.as_of,
        Some(AsOf::LatestEvidence)
    );
    assert_eq!(p.vaults[2].learning.as_of, date("2024-02-29"));
    assert!(
        compile(r#"vault "/n" {}"#).vaults[0]
            .learning
            .as_of
            .is_none()
    );

    let normalized = render_program(&parse(source).unwrap());
    assert!(normalized.contains("learning {\n  as of \"2024-10-01\"\n"));
    assert!(normalized.contains("  as of latest evidence\n"));
    assert_eq!(normalized.matches("as of").count(), 4, "{normalized}");
    assert_eq!(compile(&normalized), p);
    let effective = render_vault(&p.vaults[1]);
    assert_eq!(effective.matches("as of").count(), 1, "{effective}");
    assert_eq!(compile(&render_program(&p)), p);
    let json = serde_json::to_value(&p.vaults[1].learning).unwrap();
    assert_eq!(json["as_of"]["kind"], "latest_evidence");
    assert_eq!(
        serde_json::to_value(&p.vaults[0].learning).unwrap()["as_of"],
        serde_json::json!({"kind": "date", "date": "2024-10-01"})
    );
    assert!(
        serde_json::to_value(&compile(r#"vault "/n" {}"#).vaults[0].learning)
            .unwrap()
            .get("as_of")
            .is_none()
    );
    assert_eq!(
        serde_json::from_str::<Program>(&serde_json::to_string(&p).unwrap()).unwrap(),
        p
    );
    let plan = playground(source);
    assert_eq!(
        plan["plan"]["vaults"][1]["learning"]["as_of"]["kind"],
        "latest_evidence"
    );
}

#[test]
fn as_of_rejects_reading_scope_duplicates_and_bad_dates() {
    for s in [
        r#"vault "/n" { learn questions from folder "people" { as of "2024-10-01" } }"#,
        r#"vault "/n" { learn questions from folder "people" as of latest evidence }"#,
    ] {
        let error = parse(s).unwrap_err().to_string();
        assert!(error.contains("as of applies to a whole vault"), "{error}");
    }
    for s in [
        r#"learning { as of "2024-10-01" as of latest evidence }"#,
        r#"vault "/n" { as of "2024-10-01" learning { as of "2024-10-02" } }"#,
        r#"vault "/n" { as of latest evidence as of latest evidence }"#,
        r#"learning { as of "2024-02-30" }"#,
        r#"learning { as of "2023-02-29" }"#,
        r#"learning { as of "24-10-01" }"#,
        r#"learning { as of "2024-10-1" }"#,
        r#"learning { as of "0000-01-01" }"#,
        r#"learning { as of "2024-10-01T00:00" }"#,
        r#"learning { as of latest }"#,
        r#"learning { as of now }"#,
        r#"learning { as of 2024 }"#,
    ] {
        assert!(parse(s).is_err(), "{s}");
    }
    let a = parse(r#"learning { as of "2024-10-01" }"#).unwrap();
    let b = parse(r#"learning { as of latest evidence }"#).unwrap();
    assert!(resolve(vec![a, b], Path::new("/home/demo")).is_err());
}
