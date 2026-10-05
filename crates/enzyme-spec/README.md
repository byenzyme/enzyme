# enzyme-spec

Parser, resolver, and renderer for the readable `.enzyme` workspace language.
It is pure: parsing, resolution, lowering to the engine's configuration value,
canonical rendering, and agent instructions, with no engine or I/O dependency
beyond reading `.enzyme` files from a directory.

The language reference is `tools/reading-spec/README.md` in the enzyme-rust
repository.

## Embedding hosts

A host that defines its own source kinds (for example `source google-mail
"mail" { account "me@example.com" }`) parses them as `Source::Host`, then
replaces each one with a native Markdown, SQLite, or Are.na source before
resolution:

```rust
let mut program = enzyme_spec::parse(text)?;
program.lower_host_sources(|workspace, host| {
    Ok(match host.kind.as_str() {
        "google-mail" => Some(my_mail_source(workspace, host)?),
        _ => None, // left in place; resolve() rejects it
    })
})?;
let resolved = enzyme_spec::resolve(vec![program], &user_home)?;
```

Create-note policies (`remember in folder "inbox" create note`) are exposed as
`Workspace::note_policies`; `Workspace::note_policy_source` names the Markdown
source a policy writes into, and resolution fills `NotePolicy::root`.

## Publishing

The manifest is self-contained (explicit version, edition, and pinned
dependency versions, no `*.workspace = true` keys), so the crate builds and
packages outside the enzyme-rust workspace. Check it with:

```bash
cargo package -p enzyme-spec --allow-dirty --no-verify --list
```

Not yet published. Before publishing:

- License: the manifest keeps the workspace license (`BUSL-1.1`). Choosing the
  publication license is an open decision for the maintainers.
- Repository: `repository` points at the enzyme-rust repository as a
  placeholder until the crate's public home is decided.
- Version: the crate version no longer follows the workspace version
  automatically; bump it deliberately when the language changes.
