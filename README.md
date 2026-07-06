# iCode

A local Retrieval-Augmented Generation (RAG) backend for coding agents. iCode
gives Claude Code two things it normally lacks, behind a single MCP server:

- **Graph-level code understanding** — a per-project code graph (functions,
  classes, calls, imports, routes, inheritance) over 8 languages, queryable by
  structure *and* by meaning.
- **Cross-session project memory** — durable decisions, progress, bugs and TODOs
  that survive across sessions and across projects, semantically searchable.

Everything runs locally. The only network call is to a localhost Ollama instance
for embeddings — no code, queries, or memory ever leave the machine.

The flagship tool, `recall`, answers one query with both halves at once:
the relevant code and the relevant memory, in separate, independently-ranked
sections.

---

## Requirements

- **Rust** (stable toolchain; see `rust-toolchain.toml`).
- **[Ollama](https://ollama.com/)** running at `http://127.0.0.1:11434` with the
  embedding model `qwen3-embedding:0.6b` (dim 1024).
  - `icode setup` will pull the model for you if it's missing.
  - Ollama is **optional at runtime**: without it, the lexical code-graph tools
    still work — only semantic search and memory are disabled.

---

## Installation

```sh
./install.sh
```

This builds a release binary and installs it to `${PREFIX:-$HOME/.local/bin}/icode`
(no sudo). If that directory isn't on your `PATH`, the script tells you how to
add it.

---

## Setup

```sh
icode setup [project_path]
```

The onboarding wizard:

1. Probes Ollama and pulls `qwen3-embedding:0.6b` if it's missing. Starting
   `ollama serve` is left to you — the wizard prints that hint if Ollama is down.
2. Prints the Claude Code MCP registration snippet for this binary.
3. Lists the next steps.

It never fails: a down Ollama just degrades to lexical-only and the rest of the
guidance still prints.

---

## Usage

### Index a project

```sh
icode index  /path/to/project   # build the code graph + embeddings
icode embed  /path/to/project   # catch-up embed pass (if Ollama was down at index time)
icode stats  /path/to/project   # code-graph statistics
icode doctor /path/to/project   # read-only health check (index drift vs disk, exit 1 on drift)
```

The index lives at `<project>/.icode/index.db`; cross-session memory lives in the
central `~/.icode/icode.db`.

### Connect to Claude Code (MCP)

`icode serve <path>` speaks the MCP protocol over stdio. Register it with Claude
Code by adding a snippet to `.mcp.json` in your project root (or your `~/.claude`
settings):

```json
{
  "mcpServers": {
    "icode": {
      "command": "/abs/path/to/icode",
      "args": ["serve", "/path/to/project"]
    }
  }
}
```

Generate this snippet (with the correct absolute binary path filled in) with:

```sh
icode mcp-config /path/to/project > .mcp.json
```

### MCP tools

The server exposes 41 tools, grouped roughly as:

- **Code navigation** — `get_function` / `get_class` / `list_file_symbols`,
  `get_callers` / `get_callees` / `call_path`, `get_dependencies` /
  `get_dependents`, `find_routes`, `find_implementations`, `find_dead_code`,
  `find_unreachable`, `complex_functions`, `grep_symbols`, `overview`,
  `symbol_context`, `stats`, `doctor`.
- **Semantic code search** — `search_semantic`, `find_similar`, `find_existing`
  (semantic + lexical RRF fusion to surface "this already exists, don't rewrite
  it"). These require an embedder; they error clearly when Ollama is unavailable.
- **Memory** — `session_start` / `session_end`, `add_memory`, `search_memory`,
  `search_all`, `list_memories`, `list_projects`, `update_memory`,
  `delete_memory`, `resolve_memory`, `add_developer_note`,
  `get_developer_profile`.
- **Recall (synergy)** — `recall` (code + memory in one call), `code_to_memory`
  ("what do we remember about this symbol?"), `why_this_exists` (the DECISION
  memory behind a symbol).

---

## Languages

8 languages, via tree-sitter:

Rust · Python · PHP (+ Laravel route extraction) · JavaScript · TypeScript · Go
· Java · HTML.

---

## Architecture

Five crates:

| Crate          | Responsibility |
|----------------|----------------|
| `icode-core`   | Frozen contracts: traits (`Embedder`, `VectorIndex`, store traits), config, error, models. Framework-free. |
| `icode-engine` | Parse / index / store / memory / search. SQLite + sqlite-vec + FTS5. |
| `icode-embed`  | Concrete `Embedder` implementations (Ollama today). |
| `icode-serve`  | MCP server (rmcp, stdio). `#[tool]` functions are thin dispatchers — no logic. |
| `icode`        | Thin CLI dispatcher (clap). |

Storage is **SQLite** throughout — `sqlite-vec` (vec0) for vector KNN and FTS5
for lexical search, in the same database. The code graph is walked with
recursive CTEs (no external graph DB). Two databases:

- **per-project** `<root>/.icode/index.db` — code graph + code-chunk vectors.
- **central** `~/.icode/icode.db` — memory, developer profile, and their vectors.

Embeddings come from Ollama (`qwen3-embedding:0.6b`, dim 1024) behind the
`Embedder` trait. Hybrid retrieval fuses vector and lexical hits with
Reciprocal Rank Fusion (RRF). `recall` is the synergy point: it runs code and
memory retrieval independently and returns them in separate sections so neither
can drown out the other.

The bin builds the embedder and memory store **best-effort** before serving: a
down Ollama degrades `serve` to lexical-only (code tools stay fully useful)
rather than failing.

---

## Locality

Everything is local. The single external call is to **localhost Ollama** for
embeddings. No telemetry, no cloud, no code or memory leaving the machine.

---

## Honest boundaries

What works today, and what's deliberately still ahead:

- **Call-graph resolution is by name.** Method/call edges resolve on symbol
  names, with the call receiver captured alongside each edge (`self`/`cls`/
  `this`/`$this` and explicit class names). There is no type inference — typed
  properties are not resolved to their class — so full type-based OOP resolution
  (M3b) is planned.
- **`find_existing` is a hybrid** (semantic + lexical RRF). Without an embedder
  it falls back to lexical-only, which misses semantic duplicates that share no
  vocabulary (`fetchUser` vs `getUserById`).
- **`find_unreachable` is approximate** — it surfaces candidate dead clusters
  from entry points; not a proof of dead code.
- **Not yet built / planned:** cross-encoder reranker and a golden-set quality
  harness, the knowledge-graph `facts` section in `recall` (currently empty), a
  daemon + filesystem watcher for live incremental indexing, and a web
  dashboard.
