"""enzyme-cli — pip-installable wrapper for the enzyme binary.

On first run (or `enzyme-cli install`), downloads the correct platform binary
and embedding model from GitHub releases. Subsequent calls proxy directly to
the binary.
"""

from __future__ import annotations

import hashlib
import os
import platform
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

REPO = "useenzyme/enzyme"
VERSION = "0.4.5"

# Install binary to ~/.local/bin (standard user binary location)
_INSTALL_DIR = os.path.join(os.path.expanduser("~"), ".local", "bin")
_BIN_PATH = os.path.join(_INSTALL_DIR, "enzyme")


def _detect_target() -> str:
    """Detect platform target string matching GitHub release asset names."""
    system = platform.system()
    machine = platform.machine()

    if system == "Darwin" and machine == "arm64":
        return "macos-arm64"
    elif system == "Darwin" and machine == "x86_64":
        return "macos-x86_64"
    elif system == "Linux" and machine == "x86_64":
        return "linux-x86_64"
    elif system == "Linux" and machine in ("aarch64", "arm64"):
        return "linux-arm64"
    else:
        raise RuntimeError(
            f"Unsupported platform: {system}-{machine}. "
            "Enzyme supports macOS arm64/x86_64, Linux x86_64, and Linux arm64."
        )


def _download_url(url: str, dest: str) -> None:
    """Download a URL to a file path."""
    print(f"  Downloading {url.split('/')[-1]}...")
    urllib.request.urlretrieve(url, dest)


def _verify_sha256(filepath: str, sha256_url: str) -> None:
    """Verify file against a .sha256 checksum URL."""
    try:
        with urllib.request.urlopen(sha256_url) as resp:
            expected = resp.read().decode().strip().split()[0]
        h = hashlib.sha256()
        with open(filepath, "rb") as f:
            for chunk in iter(lambda: f.read(8192), b""):
                h.update(chunk)
        actual = h.hexdigest()
        if actual != expected:
            raise RuntimeError(
                f"SHA256 mismatch: expected {expected}, got {actual}"
            )
    except urllib.error.URLError:
        # If checksum file unavailable, skip verification
        pass


def _is_installed() -> bool:
    """Check if the enzyme binary is installed and current."""
    if not os.path.isfile(_BIN_PATH):
        return False
    # Check version
    try:
        result = subprocess.run(
            [_BIN_PATH, "--version"],
            capture_output=True, text=True, timeout=5,
        )
        return VERSION in result.stdout
    except Exception:
        return False


def install() -> None:
    """Download and install the enzyme binary and embedding model."""
    target = _detect_target()
    base_url = f"https://github.com/{REPO}/releases/download/v{VERSION}"
    asset = f"enzyme-{target}.tar.gz"
    asset_url = f"{base_url}/{asset}"
    sha256_url = f"{asset_url}.sha256"

    os.makedirs(_INSTALL_DIR, exist_ok=True)

    print(f"Installing enzyme {VERSION} ({target})...")

    with tempfile.TemporaryDirectory() as tmpdir:
        tarball = os.path.join(tmpdir, asset)
        _download_url(asset_url, tarball)
        _verify_sha256(tarball, sha256_url)

        with tarfile.open(tarball, "r:gz") as tar:
            tar.extractall(tmpdir)

        # Move binary
        src = os.path.join(tmpdir, "enzyme")
        if not os.path.isfile(src):
            raise RuntimeError(f"Binary not found in archive at {src}")

        # Replace atomically
        tmp_bin = _BIN_PATH + ".tmp"
        with open(src, "rb") as sf, open(tmp_bin, "wb") as df:
            df.write(sf.read())
        os.chmod(tmp_bin, os.stat(tmp_bin).st_mode | stat.S_IEXEC)
        os.replace(tmp_bin, _BIN_PATH)

        # Copy bundled libs if present (Linux builds)
        lib_dir = os.path.join(tmpdir, "lib")
        if os.path.isdir(lib_dir):
            dest_lib = os.path.join(_INSTALL_DIR, "lib")
            os.makedirs(dest_lib, exist_ok=True)
            for f in os.listdir(lib_dir):
                src_f = os.path.join(lib_dir, f)
                dst_f = os.path.join(dest_lib, f)
                with open(src_f, "rb") as sf, open(dst_f, "wb") as df:
                    df.write(sf.read())

    print(f"  Binary installed to {_BIN_PATH}")

    # Download embedding model
    print("  Downloading embedding model (~23 MB)...")
    subprocess.run(
        [_BIN_PATH, "setup"],
        timeout=120,
        check=True,
    )
    print("  Done.")


def main() -> None:
    """Entry point — install if needed, then proxy to the binary."""
    # Handle explicit install command
    if len(sys.argv) > 1 and sys.argv[1] == "install":
        install()
        return

    # Auto-install on first run
    if not _is_installed():
        install()

    # Proxy all args to the real binary
    try:
        result = subprocess.run([_BIN_PATH] + sys.argv[1:])
        sys.exit(result.returncode)
    except KeyboardInterrupt:
        sys.exit(130)
