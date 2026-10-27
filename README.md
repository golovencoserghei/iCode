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

### Keep it honest

```sh
icode verify status /path/to/project   # recent test/build runs + how much is unproven
icode verify stale  /path/to/project   # files changed since the last run covering them
icode curate <project-name>            # report stale + duplicate memory (--apply to archive)
```

The index lives at `<project>/.icode/index.db`; cross-session memory lives in the
central `~/.icode/icode.db`; the verification ledger in `~/.icode/verify.db`.

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

The server exposes **13 tools**. Most take an `op` argument — the surface was
consolidated from 41 separate tools because their schemas cost ~6.7k tokens of
context on every single request.

| Tool | What it answers |
|------|-----------------|
| `search` | Find code by words or identifier. Identifier-aware (`Handler` finds `HttpRequestHandler`); `mode=lexical` (default) needs no model. |
| `symbol` | Everything about one symbol: `context` / `def` / `callers` / `callees` / `references` (classified, not grep) / `tests` (which tests reach it, transitively) / `implementations` / `similar`. |
| `graph` | `chain` (does A reach B?), `impact` (reverse deps of a file), `deps`. |
| `map` | Architecture overview; in a monorepo, call it without `scope` first to list members. |
| `file` | `outline` / `list` / `stat` / `read`. |
| `grep` | Regex over stored symbol bodies. |
| `audit` | `dead` / `unreachable` / `complex` / `routes` / `health` (index drift). |
| `verify` | The verification ledger: `record` a run, `status`, `stale` (files changed since the last green run covering them, with the tests to re-run), `tests` (tests covering one file). |
| `check_exists` | Existence oracle: EXISTS / WEAK / ABSENT **with evidence**. Run it before claiming a feature is missing. |
| `recall` | One query → relevant CODE and relevant MEMORY, ranked separately. |
| `memory` | `add` / `search` / `list` / `update` / `delete` / `resolve` / `search_all` / `projects` / `for_symbol` / `why` / `curate`. |
| `session` | `start` (developer profile + project context) / `end` (persist the session). |
| `developer` | Cross-project profile: `note` / `profile`. |

Semantic modes (`mode=semantic|hybrid`) and every memory tool need an embedder;
they degrade with a clear error when Ollama is unavailable. The lexical and
structural surface — including `verify` and `memory op=curate` — works without one.

### Verification ledger

`verify` answers the question an agent cannot answer about itself: **were the
tests covering what I just changed actually run?**

iCode already knows which tests reach a symbol and what the tree looks like on
disk. The ledger adds the missing half — which checks actually ran, when, and
how wide — and joins the three:

```sh
icode verify stale .
# 2 of 83 files changed since the last run covering them:
#   crates/icode-engine/src/recall.rs — re-run: recall_returns_separate_code_and_memory_sections, …
```

Runs are filed automatically by the `PostToolUse` hook (see `icode hook config`),
which watches Bash calls for test/build/lint commands. Three rules keep it honest:

- A **narrowed** run (`cargo test one_case`, `pytest -k auth`) never counts as
  project-wide coverage.
- A **failing** run is kept as history but never counts as verification.
- When the output does not clearly say pass or fail, **nothing is recorded** — a
  guessed pass is worse than a missing entry.

### Memory curation

Memory only ever grew: stale records and near-duplicates kept competing for the
slots `session_start` injects. `memory op=curate` (or `icode curate <project>`)
reports records nobody has read in months and pairs that say the same thing.

It reports by default and writes only with `apply` / `--apply`; it **never
deletes** (archiving is `resolve`, reversible and visible with
`include_resolved`); and always-on (L0) rules plus anything at importance ≥ 4 are
immune regardless of age. Duplicate detection is lexical, so it needs no model.

### Secrets: kept out of memory, kept in the vault

Memory is the wrong place for a secret — every record can come back into a
model's context, and `~/.icode/icode.db` is not encrypted. So every memory write
(`add`, `update`, the Stop-hook auto-summary, the WAL audit preview) replaces
recognised secrets with `[REDACTED:<kind>]`: private-key blocks, `age` secret
keys, GitHub / Anthropic / OpenAI / AWS / Slack / Google / Stripe tokens, JWTs,
URL passwords, and `password=…`-style assignments whose value looks generated.
There is no generic entropy rule (it would eat commit SHAs and ids), so a secret
with no recognisable shape still gets through — it is a seatbelt, not a vault.

The vault is `icode vault`: a place for the secrets you must get back after
losing this machine — a server's break-glass SSH key, recovery codes.

```sh
icode vault init                                   # new passphrase, asked twice
icode vault add prod-breakglass --note "web1, db1" < ~/.ssh/id_breakglass
icode vault list                                   # names + fingerprints, never values
icode vault get prod-breakglass --out ~/.ssh/id_breakglass   # mode 0600, never overwrites
icode vault check                                  # proves it opens, prints recovery steps
```

The file (default `~/.icode/vault.age`; `--file` or `$ICODE_VAULT` to move it) is
a standard passphrase-encrypted **age** file in ASCII armour. Recovery needs only
the file and the passphrase — on a machine without iCode, the stock
`age -d vault.age` prints every entry as JSON. Being text, it can be copied to a
private repo, a cloud folder, a USB stick or paper; keep at least one copy off
this machine, or it dies with it.

The passphrase is read from the terminal only — never argv, never stdin — so an
agent's non-interactive shell cannot open the vault, and the vault is **not**
exposed over MCP. Saves are atomic (temp file, fsync, rename), re-read before
they replace anything, and keep the previous version as `vault.age.bak`.

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
The vault file is the one thing meant to leave it — and only as ciphertext you
copy yourself.

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
