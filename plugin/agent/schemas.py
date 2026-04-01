"""Tool schemas for the enzyme Hermes plugin.

These schemas are presented to the LLM for tool selection and parameter filling.
"""

ENZYME_PETRI = {
    "name": "enzyme_petri",
    "description": (
        "Get a vault overview: trending entities (tags, links, folders) with their "
        "catalysts (thematic phrases) and activity trends. With --query, results are "
        "ranked by relevance to the query. Use catalyst phrases from results to "
        "compose enzyme_catalyze queries."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": (
                    "Optional query to rank entities by relevance. "
                    "Pass the user's message to get focused results."
                ),
            },
            "top": {
                "type": "integer",
                "description": "Number of top entities to return. Default: 10.",
                "default": 10,
            },
        },
        "required": [],
    },
}

ENZYME_CATALYZE = {
    "name": "enzyme_catalyze",
    "description": (
        "Semantic search by concept — finds content through pre-computed catalyst "
        "questions, not keyword matching. Compose queries using catalyst vocabulary "
        "from enzyme_petri rather than the user's raw words."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "The conceptual query to search for.",
            },
            "limit": {
                "type": "integer",
                "description": "Max results to return. Default: 10.",
                "default": 10,
            },
            "register": {
                "type": "string",
                "enum": ["explore", "continuity", "reference"],
                "description": (
                    "Presentation register. 'explore' (default) for wonder/patterns, "
                    "'continuity' for restoring context, 'reference' for capture patterns."
                ),
                "default": "explore",
            },
        },
        "required": ["query"],
    },
}

ENZYME_REFRESH = {
    "name": "enzyme_refresh",
    "description": (
        "Re-index vault content. Fast: skips if nothing changed. "
        "Use --full to force complete re-index if results seem off."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "full": {
                "type": "boolean",
                "description": "Force full re-index. Default: false.",
                "default": False,
            },
        },
        "required": [],
    },
}

ENZYME_STATUS = {
    "name": "enzyme_status",
    "description": "Show vault stats: doc count, entity count, catalyst count, embedding coverage.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": [],
    },
}

ENZYME_INIT = {
    "name": "enzyme_init",
    "description": (
        "Initialize enzyme on a vault. Run after scanning vault structure and "
        "composing a guide (entity list). Takes 10-30s depending on vault size."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "guide": {
                "type": "string",
                "description": (
                    "Freeform guide string: entity names, folder: prefixes, "
                    "excludedTags: block. Tells enzyme which entities to focus on."
                ),
            },
            "quiet": {
                "type": "boolean",
                "description": "Return compact JSON with petri data. Default: true.",
                "default": True,
            },
        },
        "required": [],
    },
}
