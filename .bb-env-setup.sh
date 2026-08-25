#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# A killed agent can leave a fixed scratch target behind. The lane pruner only
# removes slots whose advisory lock is free, so it is safe during bb startup.
"$repo_root/scripts/cargo-lane" prune >/dev/null

sentinel="# enzyme-rust-bb-cargo-config-v1"
config_dir="$repo_root/.cargo"
config_toml="$config_dir/config.toml"
legacy_config="$config_dir/config"

is_generated_config() {
  local path="$1"
  [[ -f "$path" ]] && grep -Fqx "$sentinel" "$path"
}

for existing in "$legacy_config" "$config_toml"; do
  if [[ -e "$existing" ]] && ! is_generated_config "$existing"; then
    echo ".bb-env-setup.sh: preserving existing Cargo config at $existing" >&2
    echo ".bb-env-setup.sh: raw cargo target sharing was not installed; use scripts/cargo-lane shared -- cargo <args> for the shared lane" >&2
    exit 0
  fi
done

mkdir -p "$config_dir"
tmp_config="$(mktemp "$config_dir/config.toml.XXXXXX")"
cleanup_tmp_config() {
  rm -f "$tmp_config"
}
trap cleanup_tmp_config EXIT
"$repo_root/scripts/cargo-lane" cargo-config > "$tmp_config"
mv "$tmp_config" "$config_toml"
trap - EXIT

if is_generated_config "$legacy_config"; then
  rm -f "$legacy_config"
fi

target_dir="$("$repo_root/scripts/cargo-lane" target-dir)"
echo ".bb-env-setup.sh: configured Cargo target-dir $target_dir"
