# LIP — Linked Incremental Protocol

LIP is a language-agnostic open protocol for **streaming, incremental code intelligence**. It combines the runtime-query model of LSP with the static-snapshot model of SCIP, adding a lazy query graph, blast-radius indexing, and a content-addressed dependency registry.

## Status

v0.1 — reference implementation. Wire format is JSON; FlatBuffers IPC is planned for v0.2.

## Repository layout

```
schema/
  lip.fbs               # Canonical FlatBuffers schema (wire format)

bindings/
  rust/                 # Rust reference implementation (this crate)
    src/
      schema/           # Owned types mirroring lip.fbs
      query_graph/      # Salsa-inspired incremental query database
      indexer/          # Tier 1 tree-sitter indexer
      daemon/           # Unix-socket IPC daemon
      bridge/           # LIP-to-LSP bridge (tower-lsp)
      registry/         # Dependency slice cache + registry client
  dart/                 # Dart/Flutter bindings

tools/
  lip-cli/              # `lip` command-line tool

spec/
  symbol-uri.md         # Symbol URI grammar reference
docs/
  LIP_SPEC.mdx          # Full protocol specification
```

## CLI

```bash
# Index a directory (Tier 1, tree-sitter)
lip index ./src

# Start the daemon
lip daemon --socket /tmp/lip.sock

# Query a running daemon
lip query definition file:///src/main.rs 42 10
lip query hover      file:///src/main.rs 42 10
lip query references lip://npm/react@18.0.0/src/index.js#createElement
lip query blast-radius lip://npm/react@18.0.0/src/index.js#createElement
lip query symbols useState

# Import a SCIP index (upgrades confidence to Tier 2 / score 90)
lip import --from-scip index.scip

# Start the LSP bridge (connect your editor to stdin/stdout)
lip lsp --socket /tmp/lip.sock

# Fetch a dependency slice from the registry
lip fetch <sha256-content-hash> --registry https://registry.lip.dev
```

## Symbol URIs

```
lip://scope/package@version/path#descriptor
       │      │        │      │     │
       │      │        │      │     └── symbol name (optional)
       │      │        │      └──────── file path relative to package root
       │      │        └─────────────── semver string
       │      └──────────────────────── package name
       └─────────────────────────────── scope: npm | cargo | pub | pip | …
```

Example: `lip://npm/react@18.2.0/src/jsx-runtime.js#createElement`

## Confidence tiers

| Tier | Score range | Source |
|------|-------------|--------|
| 1    | 1–50        | Tree-sitter (syntax only) |
| 2    | 51–90       | Compiler / type-checker |
| 3    | 100         | Federated CAS registry slice |

## Performance

Measured on the Rust reference implementation (`cargo bench -p lip`), optimised
build, Apple Silicon. Fixtures are ~60–80 line source files per language.

### Tier 1 indexer — `index_file` (symbols + occurrences)

| Language | Measured | Spec budget | Margin |
|---|---|---|---|
| Rust | 205 µs | < 10 ms | 49× under budget |
| TypeScript | 234 µs | < 10 ms | 42× under budget |
| Python | 279 µs | < 10 ms | 35× under budget |

A 500-line production file extrapolates to roughly 1.5–2 ms — still well
within budget. Symbols-only and occurrences-only passes each run in ~100 µs.

### Query graph

| Operation | Measured | Notes |
|---|---|---|
| `upsert_file` | 92–104 ns | O(1) — HashMap insert + cache invalidation |
| `file_symbols` cache hit | **24 ns** | Arc clone only |
| `file_symbols` cache miss | 26 µs | Full tree-sitter re-parse |
| `file_api_surface` early-cutoff | 29–34 µs | Re-parse + hash compare |
| `blast_radius` (50 files) | 5.6 µs | Warm cache |
| `workspace_symbols` (100 files) | 14.6 µs | Warm cache |

The 24 ns cache hit is the steady-state path for every query after the first
parse. The spec claims "< 5 ms in steady state" — the measured 24 ns is
**208× under that budget**.

### Wire framing (Unix socket, length-prefix)

| Scenario | Time | Throughput |
|---|---|---|
| Round-trip, 64 B payload | 6 µs | 10 MiB/s |
| Round-trip, 1 KB payload | 6.3 µs | 154 MiB/s |
| Round-trip, 64 KB payload | 43 µs | 1.4 GiB/s |
| Burst, 1 000 × 256 B messages | 1.47 ms | 680K msg/s |
| JSON serialisation only (64 B) | 58 ns | — |

The ~6 µs floor on small messages is two `write()` + two `read()` syscalls,
not serialisation cost. JSON serialisation alone is 58 ns for a typical
hover response. At 64 KB the socket saturates at 1.4 GiB/s.

Run benchmarks yourself:

```bash
cargo bench -p lip
# Results land in bindings/rust/target/criterion/
```

## Building

```bash
cargo build --workspace
cargo test  --workspace
```

Requires Rust 1.78+. No system `protoc` needed — `protoc-bin-vendored` provides a precompiled binary.

## Running the daemon + LSP bridge

```bash
# Terminal 1 — daemon
cargo run -p lip-cli -- daemon --socket /tmp/lip.sock

# Terminal 2 — LSP bridge (connect your editor to this process via stdio)
cargo run -p lip-cli -- lsp --socket /tmp/lip.sock
```

Configure your editor to launch `lip lsp` as the language server command.

## Architecture

```
Editor
  │ LSP (JSON-RPC, stdio)
  ▼
LipLspBackend  ─── bridge/lsp_server.rs
  │ LIP JSON (Unix socket, length-prefixed)
  ▼
LipDaemon  ─── daemon/server.rs + session.rs
  │
  ▼
LipDatabase  ─── query_graph/db.rs
  │  ▲
  │  └── early-cutoff: file_api_surface unchanged → skip downstream recompute
  ▼
Tier1Indexer  ─── indexer/tier1.rs  (tree-sitter)
```

## License

Apache 2.0
