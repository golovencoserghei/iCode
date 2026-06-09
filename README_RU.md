# iCode

Instant code search for AI models. Replaces grep with millisecond queries.

[English version](README.md)

## What is it

**iCode** — a compiled Rust binary with one-writer / many-readers architecture:

1. Parses source code into AST via tree-sitter
2. Indexes everything into SQLite with FTS5 full-text search
3. A background **daemon** is the sole writer: one process watches folders and keeps `.icode/index.db` up to date
4. The **MCP server** is a thin read-only client: any number of Claude Code / VS Code / subagent sessions connect in parallel

## Problem it solves

AI models waste dozens of grep/find calls to navigate large codebases. Finding `RuntimeErrorProcessing` in a Java project takes 14 sequential grep calls, each scanning thousands of files. With iCode — one query, result in <1ms.

## Supported Languages

| Language | Extensions |
|----------|------------|
| Python | `.py` |
| JavaScript | `.js`, `.jsx` |
| TypeScript | `.ts`, `.tsx` |
| Java | `.java` |
| Rust | `.rs` |
| Go | `.go` |
| PHP | `.php`, `.phtml`, `.php8`, `.php7` |
| HTML | `.html`, `.htm` |

Text files (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env`, etc.) are indexed for full-text search.

## Quick Start

### Build from source

```bash
git clone <your-repo-url>
cd icode
cargo build --release -p icode
```

Binary: `target/release/icode`

### Daemon setup

1. Create `daemon.toml`:

```toml
[daemon]
http_port = 0

[[paths]]
path = "/path/to/your/project"
```

2. Start the daemon:

```bash
icode daemon run
```

3. Connect Claude Code via `.mcp.json` in the project root:

```json
{
  "mcpServers": {
    "icode": {
      "type": "stdio",
      "command": "/path/to/icode",
      "args": ["serve", "--path", "."]
    }
  }
}
```

### One-shot indexing (no daemon)

```bash
icode index /path/to/project
```

### Project config

Auto-created at `.icode/config.json`:

```json
{
  "exclude_dirs": ["vendor", "node_modules", ".git", "var", "cache"],
  "languages": ["php"],
  "storage_mode": "disk",
  "debounce_ms": 1500,
  "batch_ms": 2000
}
```

## MCP Tools

### Search & Navigation

| Tool | Description |
|------|-------------|
| `search_function` | Full-text search for functions by name/body |
| `search_class` | Full-text search for classes |
| `search_text` | Search in text files |
| `find_symbol` | Exact name lookup (functions, classes, variables) |
| `grep_body` | Regex/literal search inside function/class bodies |
| `grep_text` | Regex search in text files |
| `grep_code` | Search in stored file contents |

### Precise Queries

| Tool | Description |
|------|-------------|
| `get_function` | Get function by exact name |
| `get_class` | Get class by exact name |
| `get_callers` | Who calls a given function |
| `get_callees` | What a function calls |
| `get_imports` | File or module imports |
| `get_file_summary` | Full file map: functions, classes, imports, variables |

### Repository Overview

| Tool | Description |
|------|-------------|
| `list_files` | List files with filtering |
| `stat_file` | File metadata |
| `get_stats` | Index statistics |
| `read_file` | Read file contents (by line range) |
| `health` | MCP server and daemon status |

## Response Format

All tools return data wrapped in:

```json
{
  "result": [...],
  "_meta": {
    "dependent_files": ["src/foo.php", "src/bar.php"]
  }
}
```

`_meta.dependent_files` — list of files the response depends on. Used for cache invalidation.

## CLI Commands

```bash
icode serve --path .                  # start MCP server
icode serve --path alias=/path        # with alias
icode index /path                     # one-shot indexing
icode index --force /path             # force full re-index
icode stats                           # index statistics
icode search-function <name>          # search function
icode get-callers <name>              # call graph
icode daemon run                      # start daemon
icode daemon status                   # daemon status
icode daemon stop                     # stop daemon
```

## Architecture

```
[File System]
      ↓ (notify / inotify)
[Daemon — background indexer]    ← sole writer
      ↓ (SQLite + FTS5)
[MCP Server — read-only]         ← many parallel readers
      ↓ (stdio / HTTP)
[Claude Code / VS Code / subagents]
```

### Project structure

```
crates/
  code-index-core/   # core: parsers, storage, MCP server, daemon
  code-index/        # icode binary
```

## Tech Stack

- **Rust** — implementation language
- **tree-sitter** — AST parsing (8 languages)
- **SQLite + FTS5** — index and full-text search
- **tokio** — async runtime
- **rayon** — parallel parsing
- **rmcp** — Rust MCP SDK
- **zstd** — file content compression
- **notify** — filesystem change watching
