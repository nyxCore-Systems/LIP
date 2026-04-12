# Changelog

All notable changes to this project are documented here.

---

## [1.4.0] — 2026-04-12

### Added

- **`textDocument/typeDefinition` in all 4 Tier 2 backends** — rust-analyzer, typescript-language-server, pyright-langserver/pylsp, and dart language-server now call `typeDefinition` for each symbol after the hover pass. When the response points to a different file, an `OwnedRelationship { is_type_definition: true, target_uri }` is attached to the symbol. This gives LIP a cross-file type dependency graph — the blast-radius engine can now identify all symbols whose type is `Foo` when `Foo`'s definition changes.
- **`textDocument/inlayHints` in rust-analyzer backend** — after `documentSymbol`, the rust-analyzer backend fetches all Type-kind inlay hints for the file. Each inferred local variable binding that isn't already exposed by `documentSymbol` becomes a new `Variable` symbol with `signature: "name: InferredType"`. These are indexed at `lip://local/<path>#<name>@<line>:<col>` URIs. SCIP indexers do not capture local variable types; this is additive coverage.
- **SCIP signature extraction** — `lip import --from-scip` now extracts type signatures from SCIP documentation. SCIP indexers (scip-rust, scip-typescript, scip-java, …) place the rendered signature as `documentation[0]`. The importer now splits this correctly: `doc[0]` → `OwnedSymbolInfo.signature`, remaining entries → `OwnedSymbolInfo.documentation`. A keyword heuristic handles single-entry arrays. Imported symbols now have their type signatures populated rather than `None`.

### Changed

- **Tier 2 confidence score: 70 → 90** across all 4 backends (rust-analyzer, typescript-language-server, pyright-langserver/pylsp, dart language-server). Aligns with spec §3.3 ("score 51–90") and the roadmap v1.2 target. SCIP imports already used 90; Tier 2 was incorrectly lower.
- **`LipDatabase::upgrade_file_symbols` confidence floor** — upgrades now apply only when `incoming.confidence_score >= existing.confidence_score`. A racing Tier 2 job can no longer silently downgrade a symbol that was previously pushed at a higher confidence (e.g. a SCIP import with `--confidence 95`). The floor also propagates `relationships` from incoming upgrades.
- **`pub(super) file_uri_to_lip_uri`** extracted as a shared helper in `rust_analyzer.rs`, re-exported by the other three Tier 2 backends.

---

## [1.3.0] — 2026-04-11

### Added

- **ABI surface fingerprinting** (`is_exported` field on `OwnedSymbolInfo`) — formal exported-symbol tracking per language. Rust: `pub` keyword; TypeScript: `export` statement; Python/Dart: non-underscore name convention. `file_api_surface()` now filters by `is_exported` instead of heuristics.
- **Function-level blast radius** — `blast_radius_for` now emits one `ImpactItem` per distinct caller symbol, not one per file. Enables per-function impact analysis when multiple callers in the same file depend on the changed symbol. `direct_items` and `transitive_items` are deduplicated by `(file_uri, symbol_uri)`.
- **Kotlin-IC name consumption index** — `LipDatabase` tracks which external display-names each file references (`file_consumed_names: HashMap<String, HashSet<String>>`). New query: `files_consuming_names(&[name])` returns files that must be re-verified when a symbol is renamed or deleted. Matches Kotlin's incremental compilation invalidation model.
- **SCIP CI batch layer** — `lip import --from-scip` extended with `--push-to-daemon <socket>` and `--confidence <1–100>`. Streams each SCIP document as a `ClientMessage::Delta` directly to a running daemon, enabling nightly CI to push compiler-accurate symbols into the live graph without a restart.
- **Semantic embedding support** — new subsystem for dense vector search:
  - `ClientMessage::EmbeddingBatch { uris, model }` — batch file embeddings via any OpenAI-compatible HTTP endpoint. Already-cached vectors are returned without a network call; new source upserts invalidate stale embeddings. Configure with `LIP_EMBEDDING_URL` and `LIP_EMBEDDING_MODEL`.
  - `ClientMessage::QueryNearest { uri, top_k }` — find the `top_k` most similar files to `uri` by cosine similarity of stored embedding vectors.
  - `ClientMessage::QueryNearestByText { text, top_k, model }` — embed `text` on the fly and run cosine search. Useful for "find files related to authentication" queries.
  - `ServerMessage::EmbeddingBatchResult`, `NearestResult`, `NearestItem` — corresponding response types.
  - New `daemon::embedding::EmbeddingClient` module — thin async HTTP wrapper with `from_env()` and `embed_texts()`.
- **Index and file observability** — daemon health endpoints for `ckb doctor` integration:
  - `ClientMessage::QueryIndexStatus` → `ServerMessage::IndexStatusResult` — indexed file count, pending embedding count, last upsert timestamp (ms), configured embedding model.
  - `ClientMessage::QueryFileStatus { uri }` → `ServerMessage::FileStatusResult` — per-file indexed/has_embedding/age_seconds.
- **5 new MCP tools**: `lip_embedding_batch`, `lip_index_status`, `lip_file_status`, `lip_nearest`, `lip_nearest_by_text`.

### Changed

- **`file_api_surface()` filter** — replaced `_`-prefix heuristic + `SymbolKind` check with `s.is_exported` field (set by extractors at parse time and by Tier 2 backends via signature prefix).
- **`blast_radius_for` Phase 3/4** — `sym_impacts: HashMap<String, (String, u32)>` replaced with `sym_items: Vec<(String, String, u32)>` (one entry per caller symbol). `direct_dependents` / `transitive_dependents` still count unique files for backwards compatibility.
- **`LipDatabase::upsert_file`** — additionally records `file_indexed_at` timestamp and invalidates stale `file_embeddings` on source change.
- **`LipDatabase::remove_file`** — clears `file_consumed_names`, `file_embeddings`, `file_indexed_at` for the removed URI.

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
