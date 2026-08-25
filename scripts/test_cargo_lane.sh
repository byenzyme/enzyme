#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/enzyme-cargo-lane-test.XXXXXX")"

cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

fail() {
  echo "test_cargo_lane: $*" >&2
  exit 1
}

read_config_target() {
  python3 - "$1" <<'PY'
from pathlib import Path
import sys
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

config = tomllib.loads(Path(sys.argv[1]).read_text())
print(config["build"]["target-dir"])
PY
}

checksum() {
  cksum "$1" | awk '{print $1 ":" $2}'
}

assert_removed_isolated_target() {
  local stderr_path="$1"
  local target
  target="$(sed -n 's/.*; target \([^ ]*\) .*/\1/p' "$stderr_path" | tail -1)"
  [[ -n "$target" ]] || fail "could not parse isolated target from $stderr_path"
  [[ ! -e "$target" ]] || fail "isolated target still exists: $target"
}

fixture="$tmp_root/enzyme-rust"
git -c init.defaultBranch=main init "$fixture" >/dev/null
mkdir -p "$fixture/scripts"
cp "$repo_root/scripts/cargo-lane" "$fixture/scripts/cargo-lane"
cp "$repo_root/.bb-env-setup.sh" "$fixture/.bb-env-setup.sh"
chmod +x "$fixture/scripts/cargo-lane" "$fixture/.bb-env-setup.sh"
git -C "$fixture" add scripts/cargo-lane .bb-env-setup.sh
git -C "$fixture" -c user.name="Cargo Lane Test" -c user.email="cargo-lane@example.invalid" commit -m init >/dev/null

worktree="$tmp_root/worktree"
git -C "$fixture" worktree add "$worktree" >/dev/null 2>&1

expected_target="$tmp_root/enzyme-rust-cargo-target"
actual_target="$(cd "$worktree" && scripts/cargo-lane target-dir)"
[[ "$actual_target" == "$expected_target" ]] || fail "target-dir mismatch: $actual_target != $expected_target"

status_output="$(cd "$worktree" && scripts/cargo-lane status)"
[[ "$status_output" == *"cargo-lane: shared target $expected_target"* ]] || fail "status did not report expected target"
[[ "$status_output" == *"cargo-lane: isolated slot 1 free"* ]] || fail "status did not report isolated slot 1"
[[ "$status_output" == *"cargo-lane: isolated slot 2 free"* ]] || fail "status did not report isolated slot 2"

(cd "$worktree" && env bash .bb-env-setup.sh) >/dev/null
config_path="$worktree/.cargo/config.toml"
[[ -f "$config_path" ]] || fail "setup did not write .cargo/config.toml"
configured_target="$(read_config_target "$config_path")"
[[ "$configured_target" == "$expected_target" ]] || fail "configured target mismatch"
before_idempotent="$(checksum "$config_path")"
(cd "$worktree" && env bash .bb-env-setup.sh) >/dev/null
after_idempotent="$(checksum "$config_path")"
[[ "$before_idempotent" == "$after_idempotent" ]] || fail "setup is not idempotent"

existing_config_tree="$tmp_root/worktree-existing-config"
git -C "$fixture" worktree add "$existing_config_tree" >/dev/null 2>&1
mkdir -p "$existing_config_tree/.cargo"
cat > "$existing_config_tree/.cargo/config.toml" <<'CONFIG'
[build]
target-dir = "custom-target"
CONFIG
existing_before="$(checksum "$existing_config_tree/.cargo/config.toml")"
existing_stderr="$tmp_root/existing-config.stderr"
(cd "$existing_config_tree" && env bash .bb-env-setup.sh) >/dev/null 2>"$existing_stderr"
existing_after="$(checksum "$existing_config_tree/.cargo/config.toml")"
[[ "$existing_before" == "$existing_after" ]] || fail "existing Cargo config was modified"
grep -q "preserving existing Cargo config" "$existing_stderr" || fail "existing Cargo config diagnostic missing"

fake_bin="$tmp_root/fake-bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/sccache" <<'SH'
#!/usr/bin/env sh
echo "$*" >> "$SCCACHE_FAKE_LOG"
if [ "$1" = "--show-stats" ]; then
  echo "fake sccache stats"
  exit 0
fi
exit 0
SH
chmod +x "$fake_bin/sccache"

sccache_config_tree="$tmp_root/worktree-sccache-config"
git -C "$fixture" worktree add "$sccache_config_tree" >/dev/null 2>&1
(cd "$sccache_config_tree" && SCCACHE_FAKE_LOG="$tmp_root/sccache-config.log" PATH="$fake_bin:$PATH" env bash .bb-env-setup.sh) >/dev/null
python3 - "$sccache_config_tree/.cargo/config.toml" "$fake_bin/sccache" <<'PY'
from pathlib import Path
import sys
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

config = tomllib.loads(Path(sys.argv[1]).read_text())
actual = config["build"].get("rustc-wrapper")
expected = sys.argv[2]
if actual != expected:
    raise SystemExit(f"rustc-wrapper mismatch: {actual!r} != {expected!r}")
PY

sccache_status="$(cd "$worktree" && SCCACHE_FAKE_LOG="$tmp_root/sccache-status.log" PATH="$fake_bin:$PATH" scripts/cargo-lane status)"
[[ "$sccache_status" == *"cargo-lane: sccache available yes ($fake_bin/sccache)"* ]] || fail "sccache availability missing from status"
[[ "$sccache_status" == *"cargo-lane: sccache use yes ($fake_bin/sccache)"* ]] || fail "sccache use missing from status"

stats_log="$tmp_root/sccache-stats.log"
stats_output="$(cd "$worktree" && SCCACHE_FAKE_LOG="$stats_log" PATH="$fake_bin:$PATH" scripts/cargo-lane sccache-stats)"
[[ "$stats_output" == "fake sccache stats" ]] || fail "unexpected sccache stats output: $stats_output"
grep -q -- "--show-stats" "$stats_log" || fail "sccache --show-stats was not invoked"

missing_bin="$tmp_root/missing-bin"
mkdir -p "$missing_bin"
ln -s "$(command -v python3)" "$missing_bin/python3"
ln -s "$(command -v git)" "$missing_bin/git"
missing_output="$(cd "$worktree" && ENZYME_RUST_SCCACHE=auto PATH="$missing_bin" scripts/cargo-lane sccache-stats)"
[[ "$missing_output" == *"sccache unavailable"* ]] || fail "missing sccache auto mode should be a no-op"

required_stderr="$tmp_root/sccache-required.stderr"
set +e
(cd "$worktree" && ENZYME_RUST_SCCACHE=required PATH="$missing_bin" scripts/cargo-lane shared -- sh -c true) >/dev/null 2>"$required_stderr"
required_code=$?
set -e
[[ "$required_code" -eq 2 ]] || fail "required missing sccache returned $required_code, expected 2"
grep -q "required but sccache was not found on PATH" "$required_stderr" || fail "required missing sccache diagnostic missing"

wrapper_stderr="$tmp_root/sccache-wrapper-precedence.stderr"
(cd "$worktree" && ENZYME_RUST_SCCACHE=auto RUSTC_WRAPPER=/custom/wrapper SCCACHE_FAKE_LOG="$tmp_root/sccache-wrapper.log" PATH="$fake_bin:$PATH" \
  scripts/cargo-lane shared -- sh -c 'test "$RUSTC_WRAPPER" = /custom/wrapper') >/dev/null 2>"$wrapper_stderr"
wrapper_status="$(cd "$worktree" && ENZYME_RUST_SCCACHE=auto RUSTC_WRAPPER=/custom/wrapper SCCACHE_FAKE_LOG="$tmp_root/sccache-wrapper-status.log" PATH="$fake_bin:$PATH" scripts/cargo-lane status)"
[[ "$wrapper_status" == *"cargo-lane: sccache use no (RUSTC_WRAPPER is already set to /custom/wrapper)"* ]] || fail "explicit RUSTC_WRAPPER precedence missing from status"

serialization_log="$tmp_root/serialization.log"
second_stderr="$tmp_root/second-shared.stderr"
(
  cd "$worktree"
  scripts/cargo-lane shared -- sh -c "printf '%s\n' first-start >> '$serialization_log'; sleep 1; printf '%s\n' first-end >> '$serialization_log'" >/dev/null 2>"$tmp_root/first-shared.stderr" &
  first_pid=$!
  for _ in $(seq 1 50); do
    [[ -f "$tmp_root/.enzyme-rust-cargo-build-owner.json" ]] && break
    sleep 0.1
  done
  scripts/cargo-lane shared -- sh -c "printf '%s\n' second >> '$serialization_log'" >/dev/null 2>"$second_stderr"
  wait "$first_pid"
)
grep -q "waiting for shared lane" "$second_stderr" || fail "second shared lane did not report waiting"
expected_order=$'first-start\nfirst-end\nsecond'
actual_order="$(cat "$serialization_log")"
[[ "$actual_order" == "$expected_order" ]] || fail "shared lane did not serialize commands"

success_stderr="$tmp_root/isolated-success.stderr"
(cd "$worktree" && scripts/cargo-lane isolated -- sh -c 'test "$CARGO_INCREMENTAL" = 0 && test -d "$CARGO_TARGET_DIR" && touch "$CARGO_TARGET_DIR/probe"') >/dev/null 2>"$success_stderr"
assert_removed_isolated_target "$success_stderr"

failure_stderr="$tmp_root/isolated-failure.stderr"
set +e
(cd "$worktree" && scripts/cargo-lane isolated -- sh -c 'touch "$CARGO_TARGET_DIR/probe"; exit 7') >/dev/null 2>"$failure_stderr"
failure_code=$?
set -e
[[ "$failure_code" -eq 7 ]] || fail "isolated failure returned $failure_code, expected 7"
assert_removed_isolated_target "$failure_stderr"

signal_stderr="$tmp_root/isolated-signal.stderr"
(
  cd "$worktree"
  scripts/cargo-lane isolated -- sh -c 'touch "$CARGO_TARGET_DIR/probe"; sleep 30' >/dev/null 2>"$signal_stderr" &
  signal_pid=$!
  for _ in $(seq 1 50); do
    grep -q "isolated slot" "$signal_stderr" && break
    sleep 0.1
  done
  kill -TERM "$signal_pid"
  set +e
  wait "$signal_pid"
  set -e
)
assert_removed_isolated_target "$signal_stderr"

echo "test_cargo_lane: ok"
