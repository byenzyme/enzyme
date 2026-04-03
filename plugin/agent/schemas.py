"""Tool schemas for the enzyme Hermes plugin.

These schemas are presented to the LLM for tool selection and parameter filling.
"""

ENZYME_PETRI = {
    "name": "enzyme_petri",
    "description": (
        "What is this vault about? Returns the main topics, how recently active "
        "they are, and thematic questions running through them. Use this when the "
        "user asks what's here, what they've been thinking about, or to orient "
        "yourself in a workspace of notes. The thematic questions (catalysts) are "
        "the vocabulary for enzyme_catalyze queries — pass them as-is or adapt them."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": (
                    "Rank topics by relevance to this query. "
                    "Pass the user's message to focus results."
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
        "Find notes and excerpts across the vault by concept. Searches a "
        "pre-built index of the user's writing — finds things they forgot they "
        "wrote, connects notes from different time periods, surfaces patterns "
        "across hundreds of files. Returns quoted excerpts with file paths and "
        "dates. Use this instead of grep/search_files when looking for ideas, "
        "themes, or past thinking — it finds content that keyword search misses."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": (
                    "What to search for. Use catalyst phrases from enzyme_petri "
                    "for best results — they match the vault's own language."
                ),
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
                    "'explore' (default): surface patterns and tensions. "
                    "'continuity': restore prior decisions and context. "
                    "'reference': show what the user chose to capture."
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
