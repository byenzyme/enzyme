"""Lifecycle hooks for the enzyme Hermes plugin.

Maps Hermes session events to enzyme CLI operations.
"""

import subprocess

from . import setup


def on_session_start(**kwargs) -> None:
    """Bootstrap enzyme and refresh the vault index.

    Called at the start of every Hermes session. First session:
    downloads binary + model (~38 MB), then refreshes. Subsequent
    sessions: skips download, runs the fast staleness check.
    """
    if not setup.is_enzyme_available():
        setup.ensure_enzyme_installed()

    if not setup.is_enzyme_available():
        return  # install failed — tools will be gated by check_fn

    # Update binary if plugin version is newer
    if not setup.is_enzyme_current():
        setup.ensure_enzyme_installed()

    # Refresh vault index (fast: skips if nothing changed)
    subprocess.run(
        ["enzyme", "refresh", "--quiet"],
        capture_output=True,
        timeout=60,
    )


def pre_llm_call(user_message: str, **kwargs) -> dict:
    """Inject query-aware vault context before each model call.

    Runs enzyme petri with the user's message as a query, returning
    relevant entities and catalysts as ephemeral context.
    """
    if not setup.is_enzyme_available():
        return {}

    try:
        result = subprocess.run(
            ["enzyme", "petri", "--query", user_message],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            return {"context": result.stdout.strip()}
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    return {}
