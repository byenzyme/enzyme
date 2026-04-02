"""Binary bootstrap for the enzyme agent plugin.

Downloads the enzyme CLI binary and embedding model if not already installed.
Uses the same install.sh script as the main distribution.
"""

import os
import platform
import shutil
import subprocess

EXPECTED_VERSION = "0.4.1"
INSTALL_SCRIPT_URL = "https://raw.githubusercontent.com/jshph/enzyme/main/install.sh"


def is_enzyme_available() -> bool:
    """Check if enzyme binary is on PATH and executable."""
    return shutil.which("enzyme") is not None


def is_enzyme_current() -> bool:
    """Check if installed enzyme matches the expected version."""
    try:
        result = subprocess.run(
            ["enzyme", "--version"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        installed = result.stdout.strip()
        return installed == EXPECTED_VERSION or installed == f"enzyme {EXPECTED_VERSION}"
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False


def get_platform() -> str | None:
    """Detect platform. Returns None if unsupported."""
    system = platform.system()
    machine = platform.machine()

    if system == "Darwin" and machine == "arm64":
        return "macos-arm64"
    elif system == "Linux" and machine == "x86_64":
        return "linux-x86_64"
    elif system == "Linux" and machine in ("aarch64", "arm64"):
        return "linux-arm64"
    return None


def ensure_enzyme_installed() -> bool:
    """Install enzyme binary + model if not present or outdated.

    Returns True if enzyme is available after this call.
    """
    if is_enzyme_available() and is_enzyme_current():
        return True

    target = get_platform()
    if target is None:
        return False

    # Try install.sh bundled with the plugin first, fall back to remote
    plugin_dir = os.path.dirname(os.path.abspath(__file__))
    local_script = os.path.join(plugin_dir, "install.sh")

    if os.path.exists(local_script):
        result = subprocess.run(
            ["bash", local_script],
            capture_output=True,
            text=True,
            timeout=120,
        )
    else:
        result = subprocess.run(
            ["bash", "-c", f"curl -fsSL {INSTALL_SCRIPT_URL} | bash"],
            capture_output=True,
            text=True,
            timeout=120,
        )

    if result.returncode != 0:
        return False

    return is_enzyme_available()
