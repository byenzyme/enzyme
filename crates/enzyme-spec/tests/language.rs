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
        "profile p { seek \"x\" notice [\"y\"] }",
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
