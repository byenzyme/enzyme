---
name: release
description: >
  Bump enzyme version, tag, and push to trigger the release pipeline.
  Builds binaries for 3 platforms, publishes to jshph/enzyme, syncs plugin
  files, and updates the Homebrew formula.
allowed-tools: Bash, Read, Edit, Grep
argument-hint: <version or bump-type>
---

# Release

Bump version and trigger the release pipeline.

## Usage

`/release <version>` — e.g. `/release 0.2.0` or `/release patch`

## Steps

1. **Parse the argument**:
   - If a semver like `0.2.0` → use it directly
   - If `patch` / `minor` / `major` → read current version from `Cargo.toml` workspace and bump accordingly

2. **Read the current version** from `[workspace.package]` in `Cargo.toml`:
   ```
   grep 'version = ' Cargo.toml | head -1
   ```

3. **Update version** in `Cargo.toml` (the `[workspace.package]` version field only — crates inherit it)

4. **Run `cargo check`** to make sure it builds and to update `Cargo.lock`

5. **Show the user** what will happen:
   - Current version → new version
   - Files changed
   - What the tag push will trigger (build 3 targets, publish to jshph/enzyme, sync plugin files, update Homebrew)

6. **Commit, tag, and push** (only after user confirms):
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "Release v<version>"
   git tag v<version>
   git push origin master --tags
   ```

## What the CI does after push

The `v*` tag triggers `.github/workflows/release.yml`:
- Builds binaries: macOS ARM, Linux x86_64, Linux ARM
- Publishes GitHub Release on `jshph/enzyme` with tarballs + checksums
- Syncs plugin files (skills, hooks, scripts) to `jshph/enzyme`
- Updates Homebrew formula SHA256 in `jshph/homebrew-enzyme`
