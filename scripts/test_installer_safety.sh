#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run_installer_test() (
    local installer="$1"
    local sandbox home fake_bin vault target

    sandbox="$(mktemp -d "${TMPDIR:-/tmp}/enzyme-installer-safety.XXXXXX")"
    trap 'rm -rf "$sandbox"' EXIT
    home="$sandbox/home"
    fake_bin="$sandbox/bin"
    vault="$sandbox/vault"
    target="$home/.enzyme/symlinked-vault"
    mkdir -p "$fake_bin" "$home/.enzyme/existing-index" "$target" "$vault"

    printf '%s\n' 'auth-sentinel' > "$home/.enzyme/auth.json"
    printf '%s\n' 'config-sentinel' > "$home/.enzyme/config.toml"
    printf '%s\n' 'index-sentinel' > "$home/.enzyme/existing-index/enzyme.db"
    printf '%s\n' 'symlink-target-sentinel' > "$target/enzyme.db"
    printf '%s\n' 'unrelated-sentinel' > "$home/.enzyme/unrelated.txt"
    ln -s "$target" "$vault/.enzyme"

    cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) echo Linux ;;
    -m) echo x86_64 ;;
    *) exit 1 ;;
esac
EOF
    cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
case " $* " in
    *"releases/latest"*) printf '%s' 'https://github.com/byenzyme/enzyme/releases/tag/v-test' ;;
esac
exit 0
EOF
    cat > "$fake_bin/tar" <<'EOF'
#!/bin/sh
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-C" ]; then
        shift
        mkdir -p "$1"
        printf '%s\n' '#!/bin/sh' 'exit 0' > "$1/enzyme"
        exit 0
    fi
    shift
done
exit 1
EOF
    chmod +x "$fake_bin/uname" "$fake_bin/curl" "$fake_bin/tar"

    HOME="$home" PATH="$fake_bin:/usr/bin:/bin" bash "$installer" >/dev/null

    test -x "$home/.local/bin/enzyme"
    grep -qx 'auth-sentinel' "$home/.enzyme/auth.json"
    grep -qx 'config-sentinel' "$home/.enzyme/config.toml"
    grep -qx 'index-sentinel' "$home/.enzyme/existing-index/enzyme.db"
    grep -qx 'unrelated-sentinel' "$home/.enzyme/unrelated.txt"
    test -L "$vault/.enzyme"
    grep -qx 'symlink-target-sentinel' "$vault/.enzyme/enzyme.db"

)

run_installer_test "$REPO_ROOT/install.sh"
run_installer_test "$REPO_ROOT/agent/install.sh"

echo "installer-safety: both installers preserved existing ~/.enzyme data"
