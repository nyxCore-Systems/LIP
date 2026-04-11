# Changelog

All notable changes to this project are documented here.

---

## [1.2.0] — 2026-04-11

### Added

- **Slice mounting** — `db.mount_slice()` loads a pre-built `OwnedDependencySlice` into the daemon graph at Tier 3 confidence (score=100). Symbols are visible to `workspace_symbols`, `similar_symbols`, `symbol_by_uri`, and blast-radius lookups. Idempotent: re-mounting the same package replaces prior symbols without accumulation.
- **`ClientMessage::LoadSlice`** — new wire message; session handler logs package name and symbol count on mount.
- **`lip fetch --mount`** — downloads a slice and immediately loads it into a running daemon in one command.
- **`lip_load_slice` MCP tool** — exposes slice mounting to AI agents.
- **LSP bridge tests** — first test coverage for the bridge: seq counter monotonicity, `CARGO_PKG_VERSION` version propagation, u64 type invariant.
- **8 new db tests** — slice mounting: workspace_symbols visibility, Tier 3 confidence, idempotency, def_index population, similar_symbols search, re-mount replace-not-append.
- **Workspace metadata** — `[workspace.package]` in root `Cargo.toml` with author (`Lisa Welsch <lisa@tastehub.io>`), homepage, repository. All crates inherit via `.workspace = true`.
- **`keywords` and `categories`** on all three crates for crates.io discoverability.
- **`LICENSE`** file (MIT) at repo root.
- **`CHANGELOG.md`** (this file).
- **CI publish job** — triggered on `v*` tag push after release builds pass; publishes `lip`, `lip-cli`, `lip-registry` to crates.io in dependency order. Requires `CARGO_REGISTRY_TOKEN` in GitHub Secrets.

### Fixed

- **LSP bridge `did_change`** hardcoded `seq: 0` — now calls `self.next_seq()`.
- **LSP bridge version** was hardcoded `"0.1.0"` in ManifestRequest and ServerInfo — now `env!("CARGO_PKG_VERSION")`.
- **License** corrected from Apache-2.0 to MIT across all Cargo.toml files and README.

### Changed

- **`docs/user/cli-reference.md`** — added `query similar`, `query stale-files`, `fetch --mount`, `slice --pip`, full `annotate` subcommand docs, updated MCP tools list.

---

## [1.1.0] — 2026-04-11

### Added

- **Dart Tier 2 backend** — `dart language-server --protocol=lsp` wired into the Tier 2 manager. Symbols in `.dart` files are upgraded to confidence 70–90 on save.
- **`lip query similar`** — trigram (Jaccard 3-gram) fuzzy search across all symbol names and documentation. Returns ranked hits with score ≥ 0.2.
- **`lip query stale-files`** — Merkle sync probe. Sends `[(uri, sha256_hex)]` pairs; daemon returns stale/unknown URIs in one round-trip. Clients use this on reconnect to send only changed deltas.
- **`lip annotate search`** — workspace-wide annotation search by key prefix (e.g. `lip:fragile`, `agent:`, empty string for all).
- **`lip slice --pip`** — indexes pip-installed packages from the active Python environment using `pip list` + `pip show`.
- **`lip fetch --mount`** — after downloading a slice, optionally sends `LoadSlice` to a running daemon in a single command.
- **`ClientMessage::LoadSlice`** — new wire message to mount a pre-built `OwnedDependencySlice` into the daemon graph at Tier 3 confidence (score=100). Idempotent: re-mounting the same package replaces prior symbols.
- **`lip_load_slice` MCP tool** — exposes slice mounting to AI agents.
- **`lip_annotation_workspace_list` MCP tool** — workspace-wide annotation search for AI agents.
- **`lip_stale_files` MCP tool** — Merkle sync probe for AI agents.
- **Annotation expiry** — `expires_ms` field is now enforced in `annotation_get`, `annotation_list`, and `all_annotations`. `purge_expired_annotations()` sweeps stale entries on startup.
- **Tier 3 confidence (score=100)** — all symbols built by `lip slice` are stamped at 100 rather than inheriting the Tier 1 score of 30.
- **FlatBuffers schema v1.1.0** (`schema/lip.fbs`) — extended with the full IPC query layer: `BlastRadiusResult`, `ImpactItem`, `RiskLevel`, `SimilarSymbol`, all request/response tables, `FileHashEntry`, `QueryStaleFiles`, `StaleFilesResult`.

### Fixed

- **LSP bridge `did_change`** was hardcoding `seq: 0` on every change notification instead of incrementing the monotonic counter. This caused DeltaAck sequence tracking to desync. Now calls `self.next_seq()`.
- **LSP bridge version** was hardcoded as `"0.1.0"` in both the ManifestRequest and the `initialize` ServerInfo response. Now reads from `env!("CARGO_PKG_VERSION")`.

### Changed

- **`LipDatabase`** — `symbol_by_uri`, `workspace_symbols`, and `similar_symbols` now search mounted slice symbols in addition to tracked source files.
- **Roadmap in `LIP_SPEC.mdx`** updated from stale v0.x checklist to accurate v1.1 shipped list + v1.2/v1.3/v2.0 future items.
- **`README.md`** — Dart Tier 2 row updated to ✓, MCP tools table updated, `lip slice --pip` added.
- **`docs/user/cli-reference.md`** — `query similar`, `query stale-files`, `fetch --mount`, `slice --pip`, and full `annotate` subcommand docs added.

---

## [1.0.0] — 2026-03-xx

### Added

- **Blast-radius indexing** — CPG call edges stored and queried. `blast_radius_for` combines file-level reverse-dep BFS with symbol-level CPG BFS for precise impact analysis.
- **`lip push`** — publishes a dependency slice to the registry via HTTP PUT.
- **Dart Tier 1** — tree-sitter-dart grammar; `dart_symbols`, `dart_occurrences`, `dart_calls` extractors.
- **Docker image** for `lip-registry` — multi-stage Alpine build, scratch runtime.
- **`lip_similar_symbols` MCP tool** — trigram fuzzy search exposed to AI agents.
- **`lip_batch_query` MCP tool** — multiple queries in one round-trip.
- **Workspace annotation search** (`AnnotationWorkspaceList`) — scan all symbols by key prefix.
- **SCIP import** (`lip import --from-scip`) — bootstraps the graph from a CI-generated SCIP index at confidence 90.
- **SCIP export** (`lip export --to-scip`) — snapshots the live graph back to SCIP format.
- **LSP bridge** (`lip lsp`) — standard LSP server (stdio) backed by the LIP daemon. Supports `textDocument/definition`, `textDocument/references`, `textDocument/hover`, `workspace/symbol`, `textDocument/documentSymbol`.
- **WAL journal** with compaction and full replay on daemon startup.
- **Filesystem watcher** — OS-native per-file notifications trigger incremental re-index on out-of-band changes.
- **Tier 2 backends**: rust-analyzer, typescript-language-server, pyright-langserver, with graceful degradation when binary absent.
- **Registry client + cache** — content-addressable local cache; federated fetch from multiple registry URLs.
- **`BatchQuery`** — N queries under a single db lock acquisition.

### Initial languages

| Language | Tier 1 | Tier 2 |
|----------|--------|--------|
| Rust | ✓ | ✓ rust-analyzer |
| TypeScript | ✓ | ✓ typescript-language-server |
| Python | ✓ | ✓ pyright-langserver |
| Dart | ✓ | — (added Tier 2 in v1.1) |
