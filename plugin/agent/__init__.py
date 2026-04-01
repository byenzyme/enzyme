"""Enzyme — Hermes agent plugin.

Registers enzyme tools and lifecycle hooks with the Hermes runtime.
Tools shell out to the enzyme CLI; no core logic is reimplemented here.
"""

from . import hooks, schemas, setup, tools


def register(ctx):
    """Called by Hermes to register this plugin's tools and hooks."""

    # Lifecycle hooks
    ctx.register_hook("on_session_start", hooks.on_session_start)
    ctx.register_hook("pre_llm_call", hooks.pre_llm_call)

    # Tools — gated by check_fn so they're hidden until enzyme is installed
    check = lambda: setup.is_enzyme_available()

    ctx.register_tool(
        name="enzyme_petri",
        schema=schemas.ENZYME_PETRI,
        handler=tools.handle_petri,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_catalyze",
        schema=schemas.ENZYME_CATALYZE,
        handler=tools.handle_catalyze,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_refresh",
        schema=schemas.ENZYME_REFRESH,
        handler=tools.handle_refresh,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_status",
        schema=schemas.ENZYME_STATUS,
        handler=tools.handle_status,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_init",
        schema=schemas.ENZYME_INIT,
        handler=tools.handle_init,
        check_fn=check,
    )
