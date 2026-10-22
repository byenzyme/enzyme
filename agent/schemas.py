"""Tool schemas for the enzyme Hermes plugin.

These schemas are presented to the LLM for tool selection and parameter filling.
"""

ENZYME_PETRI = {
    "name": "enzyme_petri",
    "description": (
        "What is this vault about? Returns the main topics, how recently active "
        "they are, and thematic questions running through them. Use this when the "
        "user asks what's here, what they've been thinking about, or to orient "
        "yourself in a workspace of notes. The questions (catalysts) are generated "
        "from the notes, not written by the user: use them to see what matters and "
        "what the user calls it. When you can only search once or twice, carry a "
        "relevant catalyst phrase into the enzyme_catalyze query."
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
                "description": (
                    "Number of top entities to return. Defaults to the CLI behavior: "
                    "all entities by recency, or 15 when query is set."
                ),
            },
        },
        "required": [],
    },
}

ENZYME_CATALYZE = {
    "name": "enzyme_catalyze",
    "description": (
        "Find notes and excerpts across the vault by keyword and meaning together. "
        "Searches a local index of the user's writing, so names, titles, plain "
        "words, and concepts all work. Returns excerpts with file paths, each with "
        "the catalyst that best connects it to the query. Use this for ideas, "
        "themes, or past thinking; for questions about the user's own thinking, "
        "search several times with different wording. Use grep when you need every "
        "occurrence of literal text."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": (
                    "What to look for, starting from the user's own wording. "
                    "Vary the wording across searches to cover different angles."
                ),
            },
            "limit": {
                "type": "integer",
                "description": "Max results to return. Default: 10.",
                "default": 10,
            },
            "target": {
                "type": "string",
                "description": (
                    "Optional path to a foreign corpus. When set, catalyze prepares "
                    "that corpus with the user's vault catalysts, then returns excerpts "
                    "from the target corpus."
                ),
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
