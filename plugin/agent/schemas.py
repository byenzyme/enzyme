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
                    "A conceptual query inspired by the user's intent. Use the "
                    "petri catalysts and entity themes as context to infer a few "
                    "different angles — don't copy catalyst phrases verbatim."
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
    "description": "Incrementally refresh vault content and catalyst-derived similarities.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": [],
    },
}

ENZYME_SCAN = {
    "name": "enzyme_scan",
    "description": (
        "Scan vault structure before initialization. Returns suggested config entities "
        "from folders, tags, and wikilinks without reading or creating the enzyme DB. "
        "By default this is a preview only. Audit the vault independently and confirm "
        "the final entity/exclusion list with the user before setting write_config. "
        "write_config persists the suggestion only when the vault has no configured "
        "entities yet."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "write_config": {
                "type": "boolean",
                "description": "Write suggested entities to config only if none exist for this vault. Default: false.",
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
        "persisting the reviewed config. Takes 10-30s depending on vault size."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "quiet": {
                "type": "boolean",
                "description": "Return compact JSON with petri data. Default: true.",
                "default": True,
            },
        },
        "required": [],
    },
}
