<div align="center">

# 🧬 Enzyme

[![Discord](https://img.shields.io/discord/1191288276536008745?label=Discord&logo=discord&style=flat-square)](https://discord.gg/nhvsqtKjQd)
[![License](https://img.shields.io/github/license/byenzyme/enzyme?style=flat-square)](LICENSE)
[![Release](https://img.shields.io/github/v/release/byenzyme/enzyme?style=flat-square)](https://github.com/byenzyme/enzyme/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/byenzyme/enzyme/total?style=flat-square&label=Downloads&color=6c757d)](https://github.com/byenzyme/enzyme/releases)

</div>

Enzyme orients your coding agent to your wiki. It reads how your Markdown
notes are already organized (folders, tags, links, and dates) and builds a local
index, so Claude Code or Codex can pull up related notes from months of writing
and cite them.

## Get started

Open your Markdown notes folder in Claude Code, Codex, or another agent that can
run terminal commands, and paste this prompt. If your agent is open elsewhere,
give it the path to your notes.

```text
Set up Enzyme for the Markdown notes folder I have open. If you cannot identify it, ask me for its path.

Install Enzyme if needed using https://raw.githubusercontent.com/byenzyme/enzyme/main/install.sh, and install the instructions for this agent. Read and follow the installed enzyme-workspace-setup skill.

You may use Enzyme's online service to suggest settings from my notes and prepare search. Show me what you propose to include or exclude, and let me correct it before building the local search index.

Then answer one useful question using my notes, with links to the sources, and suggest a question I can ask next. Keep my existing notes in place and unchanged. Ask separately before any cleanup, model download, or recurring task.
```

Your agent installs Enzyme and follows its setup skill. It proposes which
folders, tags, and links to read, shows you the settings so you can correct
them, builds the index, and answers one real question with links to the notes
behind it. Your notes stay where they are, and you don't need to clean them up
first.

After setup, keep working with your agent as usual. The installed skill tells it
when to search with Enzyme, when plain search is better, and when to run
`enzyme refresh` so new notes are included.

Prefer to install it yourself first? Run this, then paste the same prompt:

```bash
curl -fsSL https://raw.githubusercontent.com/byenzyme/enzyme/main/install.sh | bash
cd /path/to/your/notes
enzyme install claude # or: codex, hermes, openclaw
```

## What it's for

Enzyme was built for knowledge bases that grow quickly:

* Obsidian vaults that accumulate dated notes in a few folders
* Meeting notes and AI-native, Markdown CRM setups
* Agent memory written as Markdown

It helps most when a question spans many notes written over time: what changed
in a project, what keeps coming up with a person, why a decision went the way
it did. For a name, title, or exact phrase, plain search is faster, and the
installed skill tells your agent to use it.

## How it works

Setup saves a short, editable settings file (an `.enzyme` program) that says
what to read. When the index is built, Enzyme does two things:

1. **Maps what matters.** It samples excerpts across time from each folder,
   tag, or link you chose and writes questions from them, called
   **catalysts**, about what you decided, changed, or left open there. `enzyme
   petri` shows this map.
2. **Indexes every note for search.** `enzyme catalyze` ranks notes by wording
   and meaning together, so a search finds the passage whether your agent names
   something exactly or only describes it. Each result carries the catalyst that
   best connects it to the query. Search runs entirely on your machine.

Your agent orients on the map, searches from several angles, and answers from
your own passages, so it can tell you how your thinking moved, not just what one
note says. Catalysts are generated, so they're hints, never evidence: judge setup
by the notes your agent brings back.

A small settings file looks like this. Your agent writes and edits it with you
during setup:

```
workspace "notes" {
  source markdown "notes" { path "~/notes" }

  learn questions from folder "people" including linked pages about relational
  learn questions from folder "meetings" about operational
  learn questions from tags ["startup", "design"] about decision_trace
}
```

Each reading uses a profile that sets what to look for. The built-in profiles
were refined over [2 years of personal use](https://joshpham.com/kit/how-i-use-enzyme):

| Read...                  | For...                                          | Profile               |
|--------------------------|-------------------------------------------------|-----------------------|
| Project notes            | what's stuck and what keeps blocking            | `operational`         |
| Decision records         | why a choice won, what would change it          | `decision_trace`      |
| Saved articles           | connections to what you're already working on   | `resonance_trace`     |
| Journals                 | what keeps returning across entries             | `reflective`          |
| People notes             | what matters in these relationships             | `relational`          |
| Feedback / activity logs | what works for you, under what constraints      | `preference_evidence` |
| *(default)*              | costs, assumptions, live tensions               | `tension_trace`       |

## Reference

### Setup without an agent

```bash
enzyme compile -v
# Review the .enzyme settings file at the path printed by compile.
enzyme init --quiet
enzyme doctor
enzyme catalyze "a question about your notes"
```

`compile` proposes settings and saves them; `init` builds the local index from
them; `doctor` reports what was indexed and any problems.

### Other sources

For Apple Notes, a preset builds a named `workspace` program from your Notes
database. Review the saved `.enzyme` program before running `init`:

```bash
enzyme --workspace apple-notes compile --preset apple-notes \
  "$HOME/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite"
enzyme --workspace apple-notes init
```

For Are.na, use `enzyme --workspace <name> compile --preset arena <channel-url>`
and then `enzyme --workspace <name> init`. Run `refresh` with the
same `--workspace` name to pick up later changes (`--collection` is an accepted
alias).

### Retrieval commands

`enzyme petri` shows what Enzyme found worth asking about: for each recurring tag, link, or folder, questions about what you decided, changed, or left open, sampled across time. In a terminal it renders a tree; piped, it emits JSON.

```bash
enzyme petri -n 2
```

```text
Petri
├─ #working-with-others · tag · active · 11 mentions
│  └─ catalysts
│     ├─ Why did every auth change still route through me after I kept promising to write it down?
│     ├─ Why did the third auth extraction attempt stall when nobody wanted to own the session model?
│     ├─ Which reasons for handing the session model to Maya had I stopped believing by then?
│     ├─ Did extraction stay paused once Maya's team owned the session model?
│     └─ …
└─ #system-design · tag · active · 10 mentions
   └─ catalysts
      ├─ Why did I pick a monolith with inner module boundaries over splitting services yet?
      ├─ Did I ever settle who owns the session model once auth moves out?
      ├─ Which part of auth extraction kept adding layers instead of removing one?
      ├─ Does my view that independence means who can change what still hold?
      └─ …
```

An agent reads this map first, then searches. `enzyme catalyze` ranks notes by wording and meaning together, and each result carries the catalyst that best connects it to the query:

```bash
enzyme catalyze -n 3 "why we keep rewriting the auth layer" \
  | jq '{search_strategy, results: [.results[] | {file_path, similarity, via_catalyst_text, content}]}'
```

```json
{
  "search_strategy": "hybrid",
  "results": [
    {
      "file_path": "retros/2025-q3-platform-retro.md",
      "similarity": 0.817,
      "via_catalyst_text": "Why did I keep scoping auth extraction as a two-week project, and what kept stalling it?",
      "content": "# Q3 platform retro\n#system-design #working-with-others\n\nScoped auth extraction as a two-week project for the third time. The real blocker was not the token service; nobody wanted to own the session model. Every proposal added a layer instead of removing one. Action: decide ownership before touching code again."
    },
    {
      "file_path": "journal/2025-05-12.md",
      "similarity": 0.693,
      "via_catalyst_text": "Why did every auth change still route through me after I kept promising to write it down?",
      "content": "#working-with-others\n\nReviewed eleven PRs today and wrote none. Every auth change still routes through me because I wrote the session code and nobody else wants to touch it. I keep saying I will write it down so someone else can own it. I have not."
    },
    {
      "file_path": "1on1s/2026-01-maya.md",
      "similarity": 0.579,
      "via_catalyst_text": "What keeps pulling me back to rewriting when Maya's smaller version already worked?",
      "content": "# 1:1 with Maya\n#working-with-others\n\nMaya replaced the token refresh path with something half the size of mine. She asked me twice and then stopped asking, which is the point. I want to do this with billing before I start rewriting it myself."
    }
  ]
}
```

*Captured from a 17-note example vault of a team lead's ADRs, retros, 1:1s, and journal. Catalyst wording comes from model generation, so it varies between runs.*

Scripts and host apps can rely on defined exit codes (0 ok, 1 runtime failure,
2 usage, 3 configuration, 4 workspace busy, 5 `init` indexed but catalysts
failed their quality gate). Concurrent `init`/`refresh` calls
on one workspace wait for each other, bounded by `--lock-timeout`.

### Message archives (_experimental_)

Enzyme indexes SQLite tables alongside Markdown — iMessage, WhatsApp, Mail, or any table of dated rows.

Declare a `source sqlite` inside a named `workspace` program, then run `init`
and periodic `refresh`. The [SQLite source setup skill](https://github.com/byenzyme/enzyme/blob/main/skills/enzyme-sqlite-source/SKILL.md)
shows the source declaration and source-scoped readings.

None of these is a special case. A `handle_id` column is repeated person values across dated rows, the same way `[[links]]` are repeated person values across dated notes. Both collapse to entity occurrences with effective dates, and nothing downstream knows which one it came from: the same profiles, budgets, and catalysts apply to a message thread and a folder of meeting notes.

A source names columns by the role they play rather than by the app they came from:

| Role     | Means                                           |
|----------|-------------------------------------------------|
| `id`     | the row's identity                              |
| `who`    | participants; scalar, JSON array, or delimited  |
| `when`   | the row's timestamp                             |
| `what`   | the text to read                                |
| `where`  | the container the row belongs to (optional)     |
| `weight` | numeric significance per occurrence (optional)  |

### API keys

Enzyme needs an API key only for catalyst generation and for Markdown `enzyme compile`'s selection step. Source preset compilation does not call a model. By default `enzyme init` uses Enzyme's hosted bootstrap and ignores inherited `OPENAI_*` variables so it does not spend your personal key. Credential resolution is explicit key → configured local model → anonymous brokered free config, with no additional configuration.

The first configured vault on a machine initializes without login. Refresh, publishing, account credits, and additional vaults require `enzyme login`.

To bring your own OpenAI-compatible key, pass `--llm env` (alias `--use-env-llm`), which reads only `OPENAI_API_KEY` plus optional `OPENAI_BASE_URL` and `OPENAI_MODEL`. `init` and `refresh` also take `--llm local` (the installed local model; fails if none is installed), `--llm hosted`, and `--llm none` (index, embed, and select without generating catalysts). Without `--llm` and without any hosted or env key, catalyst generation is skipped and indexing, embedding, and local search still work.

Markdown `enzyme compile` is an explicit OpenRouter Decisions operation. It reuses the hosted lease from `enzyme login` and the free-config broker; an explicit `OPENAI_API_KEY` with `OPENAI_BASE_URL=https://openrouter.ai/api/v1` takes precedence. Catalyst generation uses `OPENAI_MODEL`; Decisions uses `ENZYME_JEV_MODEL` (default `typesafe/jev-1.13`).
