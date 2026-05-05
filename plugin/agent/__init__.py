"""Enzyme — Hermes agent plugin.

Registers enzyme tools and lifecycle hooks with the Hermes runtime.
Tools shell out to the enzyme CLI; no core logic is reimplemented here.
"""

from . import hooks, schemas, tools


def register(ctx):
    """Called by Hermes to register this plugin's tools and hooks."""

    ctx.register_hook("on_session_start", hooks.on_session_start)
    ctx.register_hook("pre_llm_call", hooks.pre_llm_call)
    ctx.register_hook("on_session_end", hooks.on_session_end)

    check = lambda: hooks.is_enzyme_available()

    ctx.register_tool(
        name="enzyme_petri",
        toolset="enzyme",
        schema=schemas.ENZYME_PETRI,
        handler=tools.handle_petri,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_catalyze",
        toolset="enzyme",
        schema=schemas.ENZYME_CATALYZE,
        handler=tools.handle_catalyze,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_refresh",
        toolset="enzyme",
        schema=schemas.ENZYME_REFRESH,
        handler=tools.handle_refresh,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_scan",
        toolset="enzyme",
        schema=schemas.ENZYME_SCAN,
        handler=tools.handle_scan,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_status",
        toolset="enzyme",
        schema=schemas.ENZYME_STATUS,
        handler=tools.handle_status,
        check_fn=check,
    )

    ctx.register_tool(
        name="enzyme_init",
        toolset="enzyme",
        schema=schemas.ENZYME_INIT,
        handler=tools.handle_init,
        check_fn=check,
    )
