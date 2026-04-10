# LIP — Linked Incremental Protocol

LIP is a persistent, incremental code intelligence daemon with an open protocol. It keeps a live query graph of your entire repository and updates only the **blast radius** of each change.

---

## Why LIP?

**LSP is the editor protocol. SCIP is the audit format. LIP is the live intelligence layer that AI agents need and nobody has standardized yet.**

Every AI coding tool — Cursor, Copilot, Claude Code, Cody — needs to answer questions that LSP was never designed for:

- *What is the blast radius of this change?*
- *Where is `AuthService.verifyToken` called from across the whole repo?*
- *What is the exported API surface of this module?*
- *Which symbols depend on this external package version?*

All of them have built custom, proprietary, incompatible code graph layers to answer these. LIP is the open protocol for that layer: a standardized, streamable, live code intelligence graph that editors, AI agents, CI systems, and developer tools can all speak.

### Why not just use LSP?

LSP is in-memory and stateless. It restarts cold, has no cross-repository index, and its incremental sync (`textDocument/didChange`) is a known source of client/server state drift. For anything beyond single-file interactive queries it requires a custom layer on top. That custom layer is what everyone is building in private.

### Why not just use SCIP?

SCIP has compiler-accurate, repository-wide symbol data — but it treats the repository as a monolithic snapshot. One changed function signature means re-running the full toolchain over every file. On a large repo that is 30–90 minutes per CI push, repeated on every developer machine, repeated for every `git clone`, repeated for every external dependency even though `react@18.2.0` has not changed since the last run.

### What makes LIP practical

Three ideas that make a live, always-current graph actually feasible:

**1. Blast-radius indexing.**
LIP tracks a reverse dependency graph per symbol. When a file is saved, it checks whether the exported API surface changed. If not — true for the vast majority of edits (bug fixes, refactors, comment changes) — zero downstream recomputation happens. If yes, only the files that import the changed symbols are re-verified.

```
Single-file edit, 500k-file repo:
  SCIP → re-index all 500k files      (~60 min)
  LIP  → re-verify ~10–200 files      (~200–800 ms)
```

**2. Federated dependency slices.**
External packages are content-addressed blobs. `react@18.2.0` is indexed once — by anyone, anywhere — and stored in the registry keyed by its hash. Every subsequent machine downloads it in seconds. `node_modules` / `target/` / `.pub-cache` are never re-indexed locally again.

**3. Progressive confidence.**
Tree-sitter (< 1 ms/file) gives symbol search and go-to-definition immediately on file open. The compiler runs on the blast radius in the background (~500 ms) and silently upgrades those results. The IDE never blocks waiting for indexing to finish.

---

## How LIP relates to LSP and SCIP

| | LSP 3.17 | SCIP | LIP |
|---|---|---|---|
| Scope | Open files only | Full repo snapshot | Full repo + deps, streaming |
| Indexing model | Volatile (in-memory) | Batch artifact (.scip file) | Persistent lazy query graph |
| Change handling | Full re-parse per file | Full re-index O(N) | Blast radius O(Δ + depth) |
| Dependency indexing | None | Re-indexes every run | Federated CAS slices (once, shared) |
| Incremental sync | Fire-and-forget, drift bugs | None | Acknowledged deltas + Merkle |
| Cold start | < 1 s | 30–90 min | < 30 s shallow + background |
| Progressive results | No | No | Yes (Tier 1 → 2 → 3) |
| Team cache sharing | No | No | Yes |
| AI / agent queries | Limited | Read-only batch | Streaming graph queries |
| CPG / call graph | No | No | Yes (Tier 2+) |

**LIP is not a replacement for LSP at the editor layer.** It ships an LSP bridge (`lip lsp`) so any editor sees it as a standard language server. LSP remains the editor-facing protocol; LIP is the persistent layer behind it.

**LIP is not a replacement for SCIP either.** It can import SCIP indexes (`lip import --from-scip index.scip`), upgrading those symbols to Tier 2 confidence (score 90). If you already run SCIP in CI, feed those artifacts into LIP and get incremental updates from that point forward.

```
External deps            SCIP index (from CI)
      │                          │
      │  lip push / fetch         │  lip import --from-scip
      ▼                          ▼
  LIP Registry  ────→  LIP Daemon  (persistent query graph)
                             │
             ┌───────────────┴───────────────┐
             ▼                               ▼
        lip lsp bridge                AI agent / CLI
        editors see standard          direct LIP protocol
        LSP — no changes needed       blast radius, CPG, etc.
```

---

## Confidence tiers

| Tier | Score | Source | Latency | When |
|------|-------|--------|---------|------|
| 1 | 1–50 | Tree-sitter | < 1 ms/file | Immediately on file open |
| 2 | 51–90 | Compiler / analyzer | 200–500 ms | After file save, blast radius only |
| 3 | 100 | Federated registry slice | Instant (cached) | On startup for external deps |

---

## Status

v1.0 — reference implementation. Wire format is JSON; FlatBuffers IPC is planned for v1.1.

---

## Repository layout

```
schema/
  lip.fbs               # Canonical FlatBuffers schema (wire format)

bindings/
  rust/                 # Rust reference implementation (this crate)
    src/
      schema/           # Owned types mirroring lip.fbs
      query_graph/      # Salsa-inspired incremental query database
      indexer/          # Tier 1 tree-sitter indexer (Rust, TypeScript, Python, Dart)
      daemon/           # Unix-socket IPC daemon + per-file filesystem watcher
      bridge/           # LIP-to-LSP bridge (tower-lsp)
      registry/         # Dependency slice cache + registry client
  dart/                 # Dart/Flutter bindings

tools/
  lip-cli/              # `lip` command-line tool
  lip-registry/         # Registry server + Docker image

spec/
  symbol-uri.md         # Symbol URI grammar reference
docs/
  LIP_SPEC.mdx          # Full protocol specification
  user/                 # User documentation
    getting-started.md
    cli-reference.md
    mcp-integration.md
    daemon.md
    registry.md
```

---

## CLI

```bash
# Index a directory (Tier 1, tree-sitter)
lip index ./src

# Start the daemon (watches files, updates blast radius on save)
lip daemon --socket /tmp/lip.sock

# Query a running daemon
lip query definition   file:///src/main.rs 42 10
lip query hover        file:///src/main.rs 42 10
lip query references   lip://cargo/myapp@0.1.0/src/auth.rs#AuthService.verifyToken
lip query blast-radius lip://cargo/myapp@0.1.0/src/auth.rs#AuthService.verifyToken
lip query symbols      verifyToken

# Import a SCIP index (upgrades confidence to Tier 2 / score 90)
lip import --from-scip index.scip

# Start the LSP bridge (editors connect here)
lip lsp --socket /tmp/lip.sock

# Start the MCP server (AI agents and tools connect here)
lip mcp --socket /tmp/lip.sock

# Fetch / publish dependency slices
lip fetch <sha256-hash> --registry https://registry.lip.dev
lip push slice.json    --registry https://registry.lip.dev

# Build dependency slices from your lockfiles
lip slice --cargo                          # uses ./Cargo.toml
lip slice --npm                            # uses ./package.json
lip slice --pub                            # uses ./pubspec.yaml
lip slice --cargo --push --registry https://registry.lip.dev
```

---

## Supported languages (Tier 1)

| Language | Extension | Symbols extracted |
|----------|-----------|-------------------|
| Rust | `.rs` | Functions, structs, enums, traits, impls, consts |
| TypeScript | `.ts`, `.tsx` | Functions, classes, interfaces, type aliases |
| Python | `.py` | Functions, classes, async functions |
| Dart | `.dart` | Functions, classes, methods, constructors, mixins, extensions |

---

## Symbol URIs

```
lip://scope/package@version/path#descriptor
       │      │        │      │     │
       │      │        │      │     └── symbol name
       │      │        │      └──────── file path within package
       │      │        └─────────────── semver
       │      └──────────────────────── package name
       └─────────────────────────────── scope: npm | cargo | pub | pip | local | team | scip
```

Examples:
```
lip://cargo/tokio@1.35.1/runtime#Runtime.spawn
lip://npm/react@18.2.0/src/jsx-runtime.js#createElement
lip://local/myapp/src/auth.rs#AuthService.verifyToken
```

---

## Performance

Measured on the Rust reference implementation (`cargo bench -p lip`), optimised build, Apple Silicon. Fixtures are ~60–80 line source files.

### Tier 1 indexer — `index_file`

| Language | Measured | Budget | Margin |
|---|---|---|---|
| Rust | 205 µs | < 10 ms | 49× under |
| TypeScript | 234 µs | < 10 ms | 42× under |
| Python | 279 µs | < 10 ms | 35× under |

A 500-line file extrapolates to ~1.5–2 ms.

### Query graph

| Operation | Measured | Notes |
|---|---|---|
| `upsert_file` | 92–104 ns | O(1) — HashMap insert + cache invalidation |
| `file_symbols` cache hit | **24 ns** | Arc clone only |
| `file_symbols` cache miss | 26 µs | Full tree-sitter re-parse |
| `file_api_surface` early-cutoff | 29–34 µs | Re-parse + hash compare |
| `blast_radius` (50 files) | 5.6 µs | Warm cache |
| `workspace_symbols` (100 files) | 14.6 µs | Warm cache |

The spec budget for steady-state queries is < 5 ms. The measured 24 ns cache hit is 208× under that budget.

### Wire framing (Unix socket)

| Scenario | Time | Throughput |
|---|---|---|
| Round-trip, 64 B | 6 µs | — |
| Round-trip, 64 KB | 43 µs | 1.4 GiB/s |
| Burst 1 000 × 256 B | 1.47 ms | 680K msg/s |

```bash
cargo bench -p lip
# Results in bindings/rust/target/criterion/
```

---

## Building

```bash
cargo build --workspace
cargo test  --workspace
```

Requires Rust 1.78+. No system `protoc` needed — `protoc-bin-vendored` bundles a precompiled binary.

---

## Running

```bash
# Terminal 1 — daemon
cargo run -p lip-cli -- daemon --socket /tmp/lip.sock

# Terminal 2 — LSP bridge
cargo run -p lip-cli -- lsp --socket /tmp/lip.sock
```

Configure your editor to launch `lip lsp` as the language server command. It speaks standard LSP; no editor plugin needed beyond what you already have.

### Registry

```bash
# From source
cargo run -p lip-registry -- serve --store /tmp/slices --port 8080

# Docker
docker build -f tools/lip-registry/Dockerfile -t lip-registry .
docker run -p 8080:8080 -v lip-slices:/slices lip-registry
```

---

## Architecture

```
Editor                     AI agent / CKB / Cursor
  │ LSP (stdio)               │ MCP (stdio, JSON-RPC)
  ▼                           ▼
LipLspBackend            lip mcp server
  │                           │
  │   LIP JSON (Unix socket, length-prefixed)
  └──────────────┬────────────┘
                 ▼
          LipDaemon  ─── daemon/server.rs
                 │
                 ├── FileWatcher   per-file notify, re-indexes on change
                 ▼
          LipDatabase  ─── query_graph/db.rs
                 │  blast-radius · CPG · name_to_symbols
                 │  early-cutoff: API surface unchanged → zero work
                 ▼
          Tier1Indexer  Rust · TypeScript · Python · Dart
```

---

## License

Apache 2.0
