# Changelog

All notable changes to this project are documented here.

---

## [2.1.0] — 2026-04-15

### Added

**v2.1 — Capability discovery + graceful unknown-variant handling**

- **`HandshakeResult.supported_messages: Vec<String>`** — handshake response now lists every `ClientMessage` `type` tag this daemon understands. Lets clients probe for an individual message (e.g. `stream_context`, `embed_text`) without writing "handshake then pray" code or comparing `protocol_version` integers. Field is `#[serde(default)]`; older daemons predating this field yield an empty vector, which clients should treat as "fall back to `protocol_version`."
- **`ServerMessage::UnknownMessage { message_type, supported }`** — when a client sends a well-formed JSON envelope whose `type` tag is unknown, the daemon now replies with `UnknownMessage` (carrying the tag plus the same supported list as handshake) *and keeps the socket open*, instead of closing after a generic parse `Error`. Lets forward-compatible clients downgrade gracefully to a supported call instead of reconnecting.

**v2.1 — `embed_text`: unary text-to-vector embedding**

- **`EmbedText { text, model? }`** — embed an arbitrary text string and return the raw vector. Closes the gap left by `EmbeddingBatch` (URI-only) and `QueryNearestByText` (embeds internally but discards the vector). Callers re-ranking with their own scoring (centroid arithmetic, federated nearest-neighbour, lexical-then-semantic re-rank) get the embedding directly instead of building a centroid out of nearest-neighbour seeds. Returns `EmbedTextResult { vector: Vec<f32>, embedding_model: String }`. Not permitted inside `BatchQuery` (requires async HTTP).

**v2.1 — `stream_context`: token-budgeted RAG context streaming**

- **`StreamContext { file_uri, cursor_position, max_tokens, model? }`** — new streaming wire message. Daemon ranks symbols relevant to the cursor and emits one `SymbolInfo { symbol_info, relevance_score, token_cost }` frame at a time, terminating with exactly one `EndStream { reason, emitted, total_candidates, error? }` frame. Reasons: `budget_reached`, `exhausted`, `error`. Replaces the broken "fetch top-k, locally truncate to prompt budget" pattern with stream-until-full. Spec §9.2.
- **Relevance ordering** (spec §2.3): direct symbol at cursor → callers (from blast-radius CPG walk) → callees / references → related types.
- **Token-cost estimate**: conservative `ceil((len(signature) + len(documentation)) / 4) + 8` per symbol.
- **Back-pressure**: daemon does not buffer ahead of the socket. `BrokenPipe` from a closing client aborts the ranking walk cleanly. `StreamContext` is rejected from `Batch` / `BatchQuery`.
- **`protocol_version` bumped from `1` → `2`** in `HandshakeResult`. Clients can detect streaming support via handshake.
- **`lip stream-context <file_uri> <line:col> --max-tokens N [--model M]`** — new CLI subcommand prints frames as JSON for manual testing.

---

## [2.0.0] — 2026-04-13

### Added

**v2.0 — Semantic explainability + model provenance**

- **`ExplainMatch { query, result_uri, top_k, chunk_lines, model }`** — explain *why* `result_uri` ranked as a strong semantic match. Chunks `result_uri`'s source into `chunk_lines`-line windows, embeds each in one batch call, cosine-scores each against the query embedding (cached for URI queries; embedded on the fly for text queries), and returns the top-`top_k` chunks with `(start_line, end_line, chunk_text, score)`. Turns "this file is relevant" into "these specific lines are relevant." Not permitted inside `BatchQuery` (requires HTTP). Returns `ExplainMatchResult { chunks: Vec<ExplanationChunk>, query_model }`.
- **Model provenance** — every `set_file_embedding` and `set_symbol_embedding` now records the model name that produced the vector. The name is supplied by the `EmbeddingBatch` handler from `embed_texts`'s return value, so it reflects the model actually used (not just what was configured). `QueryFileStatus` now returns `embedding_model: Option<String>`. `QueryIndexStatus` now returns `mixed_models: bool` and `models_in_index: Vec<String>` — clients can warn users when a model upgrade left the index with mixed-model vectors, making cosine scores unreliable across the boundary.
- **New wire types**: `ExplanationChunk { start_line, end_line, chunk_text, score }`.
- **1 new MCP tool**: `lip_explain_match`.
- **MCP updates**: `lip_file_status` response now includes `embedding_model`; `lip_index_status` response now includes `mixed_models` flag and `models_in_index` list with a `⚠ MIXED MODELS` warning in text output.

---

## [1.9.0] — 2026-04-13

### Added

**v1.9 — Search precision + server-side aggregation (4 features)**

- **`filter: Option<String>` on `QueryNearest`, `QueryNearestByText`, `BatchQueryNearestByText`, `QueryNearestByContrast`, `FindSemanticCounterpart`, `QueryNearestInStore`** — restrict candidate URIs with a glob pattern before scoring. Patterns containing `/` are matched against the full path; patterns without are matched against the filename only (e.g. `"internal/auth/**"` or `"*_test.go"`). Implemented via the `glob 0.3` crate in `nearest_by_vector`.
- **`min_score: Option<f32>` on all search calls above** — quality gate that drops results scoring below the threshold rather than returning low-confidence noise. Clients can fall back cleanly to FTS instead of surfacing the least-bad result.
- **`GetCentroid { uris: Vec<String> }`** — compute and return the embedding centroid (component-wise mean) of a file set without shipping all raw vectors to the caller. Returns `CentroidResult { vector: Vec<f32>, included: usize }`. Safe inside `BatchQuery`.
- **`QueryStaleEmbeddings { root: String }`** — report files under `root` whose stored embedding is older than their current filesystem mtime (uses `file_indexed_at` vs `tokio::fs::metadata`). Files with no `indexed_at` record are conservatively reported as stale. Returns `StaleEmbeddingsResult { uris: Vec<String> }`. Not permitted inside `BatchQuery` (requires filesystem I/O).
- **2 new db methods**: `LipDatabase::centroid()`, `LipDatabase::file_embeddings_in_root()`.
- **Filter/min_score logic duplicated in `FindSemanticCounterpart` and `QueryNearestInStore`** sync and async paths (those use inline scoring rather than `nearest_by_vector`).
- **4 new MCP tools**: `lip_nearest` / `lip_nearest_by_text` / `lip_nearest_by_contrast` / `lip_find_counterpart` / `lip_nearest_in_store` gain `filter` + `min_score` optional params; `lip_get_centroid`, `lip_stale_embeddings` are new tools.

**v1.7 — Semantic retrieval primitives (6 new wire messages)**

- **`QueryNearestByContrast { like_uri, unlike_uri, top_k }`** — contrastive nearest-neighbour search using vector arithmetic: computes `normalize(embed(like) − embed(unlike))` then finds the `top_k` files most similar to that direction. Both URIs must have cached embeddings. Returns `NearestResult`. Safe inside `BatchQuery`.
- **`QueryOutliers { uris, top_k }`** — return the `top_k` files from `uris` that are most semantically dissimilar from the rest of the group. Uses leave-one-out mean cosine similarity: files with the lowest mean similarity to peers are returned first. Returns `OutliersResult { outliers: Vec<NearestItem> }`. Safe inside `BatchQuery`.
- **`QuerySemanticDrift { uri_a, uri_b }`** — compute the cosine distance `1 − similarity` between two stored embeddings. Range `[0.0, 2.0]`. Returns `SemanticDriftResult { distance: Option<f32> }`. Safe inside `BatchQuery`.
- **`SimilarityMatrix { uris }`** — compute all pairwise cosine similarities for a list of URIs in one call. URIs without cached embeddings are silently excluded. Returns `SimilarityMatrixResult { uris: Vec<String>, matrix: Vec<Vec<f32>> }`. Safe inside `BatchQuery`.
- **`FindSemanticCounterpart { uri, candidates, top_k }`** — given a source URI and a pool of candidates, return the `top_k` candidates most semantically similar to the source. Finds test files that cover a changed implementation even when naming conventions differ. Returns `NearestResult`. Safe inside `BatchQuery`.
- **`QueryCoverage { root }`** — report embedding coverage under a filesystem path. Shows the fraction of indexed files that have embeddings, broken down by directory. Returns `CoverageResult`. Safe inside `BatchQuery`.
- **6 new MCP tools**: `lip_nearest_by_contrast`, `lip_outliers`, `lip_semantic_drift`, `lip_similarity_matrix`, `lip_find_counterpart`, `lip_coverage`.

**v1.8 — Higher-order semantic analysis (6 new wire messages)**

- **`FindBoundaries { uri, chunk_lines, threshold, model }`** — detect semantic boundaries within a file by splitting it into `chunk_lines`-line windows, embedding each in one batch HTTP call, and returning positions where the cosine distance between adjacent windows exceeds `threshold`. Defaults: 30 lines, 0.3 threshold. Returns `BoundariesResult { uri, boundaries: Vec<BoundaryRange> }`. Not permitted inside `BatchQuery` (requires HTTP).
- **`SemanticDiff { content_a, content_b, top_k, model }`** — measure how much the meaning of a file changed between two versions. Returns `SemanticDiffResult { distance, moving_toward }`: `distance` is the cosine distance between the two content embeddings; `moving_toward` is the `top_k` nearest files to `normalize(new − old)`, naming the concepts the content moved toward. Not permitted inside `BatchQuery` (requires HTTP).
- **`QueryNearestInStore { uri, store, top_k }`** — nearest-neighbour search against a caller-provided `HashMap<String, Vec<f32>>`. Enables cross-repo federation: export embeddings from each root via `ExportEmbeddings`, merge the maps, then search across all repos in one call. The query URI must have a cached local embedding. Returns `NearestResult`. Safe inside `BatchQuery`.
- **`QueryNoveltyScore { uris }`** — quantify how semantically novel a set of files is relative to the existing codebase. For each URI finds its nearest neighbour outside the input set and returns `1 − similarity` as that file's novelty score. Returns `NoveltyScoreResult { score: f32, per_file: Vec<NoveltyItem> }`, sorted by descending novelty. Safe inside `BatchQuery`.
- **`ExtractTerminology { uris, top_k }`** — extract the domain vocabulary most semantically central to a set of files. Computes the centroid of the input files' embeddings, then scores each symbol by its embedding's proximity to that centroid. Returns `TerminologyResult { terms: Vec<TermItem> }`. Requires symbol embeddings — call `EmbeddingBatch` with `lip://` URIs first. Safe inside `BatchQuery`.
- **`PruneDeleted`** — remove index entries (including embeddings) for files that no longer exist on disk. On repos with high churn, ghost embeddings accumulate and pollute nearest-neighbour results. Fires `IndexChanged` after removal. Returns `PruneDeletedResult { checked, removed }`. Not permitted inside `BatchQuery` (requires filesystem I/O).
- **6 new MCP tools**: `lip_find_boundaries`, `lip_semantic_diff`, `lip_nearest_in_store`, `lip_novelty_score`, `lip_extract_terminology`, `lip_prune_deleted`.
- **New wire types**: `BoundaryRange`, `NoveltyItem`, `TermItem`, `DirectoryCoverage` (v1.7).
- **Bumped `recursion_limit` to 512** in `lip-cli` to accommodate the expanded `json!` manifest.

---

## [1.6.0] — 2026-04-13

### Added

- **`ReindexFiles { uris }`** — force a targeted re-index of specific file URIs from disk, bypassing the directory scan. The daemon reads each file, detects its language from the URI, and calls `upsert_file`. Useful when the client knows exactly which files changed out-of-band (selective git checkout, build artifact regeneration). Returns `DeltaAck`. Not permitted inside `BatchQuery`.
- **`Similarity { uri_a, uri_b }`** — pairwise cosine similarity of two stored embeddings. Returns `SimilarityResult { score: Option<f32> }` — `None` when either URI has no cached embedding. Routes `lip://` URIs to the symbol embedding store and `file://` URIs to the file embedding store. Safe inside `BatchQuery`.
- **`QueryExpansion { query, top_k, model }`** — embed `query`, find the `top_k` nearest symbols in the symbol embedding store, and return their display names as expansion terms. Designed for CKB's compound-search path: expand a short query string into related symbol names before running `QueryWorkspaceSymbols`. Requires `LIP_EMBEDDING_URL`. Returns `QueryExpansionResult { terms }`. Not permitted inside `BatchQuery` (requires async HTTP).
- **`Cluster { uris, radius }`** — group `uris` by embedding proximity within a cosine-similarity radius. Uses greedy single-link assignment: each URI is placed in the first existing group containing a member with similarity ≥ `radius`, or starts a new group. URIs without a cached embedding are silently excluded. Returns `ClusterResult { groups }`. Not permitted inside `BatchQuery` (requires a coupling pass over embeddings that may trigger HTTP for missing vectors).
- **`ExportEmbeddings { uris }`** — return the raw stored embedding vectors for `uris` as `ExportEmbeddingsResult { embeddings: HashMap<String, Vec<f32>> }`. URIs with no cached vector are omitted. Routes `lip://` / `file://` by prefix. Safe inside `BatchQuery`.

---

## [1.5.0] — 2026-04-13

### Added

- **`BatchQueryNearestByText`** — embed N query strings in a single round-trip and return one nearest-neighbor list per query. Replaces N sequential `QueryNearestByText` calls used by CKB's compound search operations.
- **`QueryNearestBySymbol`** — find symbols similar to a given symbol URI. The daemon embeds the symbol's text (display_name + signature + doc) on demand and searches against a new per-symbol embedding store. `EmbeddingBatch` now routes `lip://` URIs to `symbol_embeddings` and `file://` URIs to `file_embeddings`.
- **`BatchAnnotationGet`** — retrieve an annotation key for multiple symbol URIs under a single db lock. Safe inside `BatchQuery`. Replaces N sequential `AnnotationGet` calls used by CKB's agent-lock check at change time.
- **`IndexChanged` push notification** — emitted to all active sessions after every `Delta::Upsert` via the existing broadcast channel. Carries `indexed_files` count and `affected_uris`. Enables precise cache invalidation without polling `QueryIndexStatus`.
- **`Handshake` / `HandshakeResult`** — clients send `Handshake { client_version }` on connect; daemon replies with `daemon_version` (semver) and `protocol_version` (monotonic integer, currently `1`). Version drift between independently updated daemon and clients is now detectable at connect time rather than producing silent bad results.
- **`--managed` flag** (`lip daemon start --managed`) — spawns a background watchdog that polls the parent process every 2 s and calls `std::process::exit(0)` when the parent has exited. Designed for IDE integrations (CKB, VS Code extension) that manage the daemon as a subprocess.

### Changed

- `EmbeddingBatch` URI routing: `lip://` URIs are now stored in `symbol_embeddings` (new field on `LipDatabase`); `file://` URIs continue to use `file_embeddings`. The response format is unchanged.

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
