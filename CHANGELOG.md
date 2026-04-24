# Changelog

All notable changes to this project are documented here.

---

## [Unreleased]

## [2.3.4] — 2026-04-24

**Module-level grouping on impact items.** Adds `module_id: Option<String>` to `ImpactItem` and `SemanticImpactItem` so consumers whose risk classifier weights cross-module blast (CKB's `RecomputeBlastRadius.ModuleCount`) get a useful value instead of the conservative zero that the unioned static-plus-LIP set previously collapsed to. `protocol_version` stays at `2`; the field is `#[serde(default, skip_serializing_if = "Option::is_none")]`, so the wire shape stays byte-identical for emitters that don't populate it and deserialises cleanly on pre-v2.3.4 clients.

### Added

- **`ImpactItem.module_id` and `SemanticImpactItem.module_id`** — resolved once at upsert time, stored on `FileInput`, surfaced on every `ImpactItem` / `SemanticImpactItem` built by `blast_radius_for`, `blast_radius_for_symbol`, `blast_radius_batch`, and `outgoing_impact_for`. Three-tier resolution, first hit wins:

  1. **Slice URI prefix** — `lip://<manager>/<package>@<version>/...` → `"<manager>/<package>"`. Covers mounted dependency slices.
  2. **SCIP package descriptor** — the first `<manager> <name>` pair parsed from any SCIP symbol attached to the file → `"<manager>/<name>"`. Covers every file imported via `upsert_file_precomputed` whose symbols carry real SCIP metadata (scip-go, scip-typescript, scip-rs, etc.). Rejects `local <id>` sentinels and empty-package descriptors.
  3. **Manifest walk** — upward walk (depth-capped at 12) from the file's directory, looking for a language-appropriate manifest: `Cargo.toml` (Rust, `[package] name`), `go.mod` (Go, `module <path>`), `package.json` (TypeScript / JavaScript / TSX / JSX, top-level `"name"`), `pyproject.toml` (Python, `[project].name` or `[tool.poetry].name`), `setup.py` (Python, `name="…"` in setup call), `pubspec.yaml` (Dart, top-level `name:`). Parse failures, I/O failures, and unsupported languages (C / C++ / Kotlin / Swift / Java) return `None` rather than propagating.

  Unit-tested per parser (`parse_cargo_toml_extracts_crate_name`, `parse_go_mod_extracts_module_path`, `parse_package_json_ignores_name_inside_values`, `parse_pyproject_toml_{project,poetry}_section`, `parse_setup_py_double_and_single_quotes`, `parse_pubspec_yaml_{name,ignores_nested_name}`, plus Cargo's workspace-only and dependency-section edge cases). Resolver-level tests cover the priority ordering (`resolve_prefers_slice_uri_over_scip`, `resolve_falls_back_to_scip_when_no_slice`, `resolve_walks_manifest_for_tier1_rust_file`, `resolve_returns_none_when_unsupported_language_and_no_scip`). Integration tests (`blast_radius_surfaces_module_id_from_scip_descriptor`, `blast_radius_surfaces_module_id_from_cargo_toml_walk`, `outgoing_impact_surfaces_module_id`) confirm the field reaches the wire through both RPCs.

---

## [2.3.3] — 2026-04-24

**Outgoing-impact symmetry.** Adds a single additive RPC so CKB can query the forward direction of the call graph with the same enriched envelope and provenance gating that `QueryBlastRadiusSymbol` already provides for the reverse direction. `protocol_version` stays at `2`; pre-v2.3.3 daemons reply `UnknownMessage`.

### Added

- **`QueryOutgoingImpact { symbol_uri, depth?, min_score? }` → `OutgoingImpactResult { result: Option<EnrichedOutgoingImpact> }`** — forward-direction twin of `QueryBlastRadiusSymbol`. BFS walks `caller_to_callees` starting from `symbol_uri`, splits direct vs. transitive hops, and wraps the static result in an envelope flattened with `#[serde(flatten)] static_result: OutgoingImpactStatic` so `edges_source` lives on the inner struct (matching the v2.3.2 shape for blast radius). `depth` is clamped to `1..=8` with a default of 8; `NODE_LIMIT=200` bounds the BFS frontier and trips `truncated: true` on overflow. Semantic enrichment reuses `SemanticImpactItem { source: Static | Semantic | Both }`: symbol-level embedding is preferred, with file-level embedding as the fallback seed, and static-hit files are tagged `Both` when their URI also appears in the nearest-embedding set. The Bug-D-style `#<name>`-strip fallback from v2.3.2 Phase 3 is applied symmetrically on the callee side, so tier-1 URIs with no `def_index` entry still resolve to their file URI instead of producing blank `symbol_uri`. `edges_source: Option<EdgesSource>` on `OutgoingImpactStatic` mirrors blast radius so CKB can apply the same `EdgesSourceEmpty → skip fold` provenance gate. Advertised as `query_outgoing_impact` in `HandshakeResult.supported_messages`; round-trip tests `query_outgoing_impact_round_trips`, `query_outgoing_impact_is_batchable`, `outgoing_impact_result_round_trips` cover the wire shape, and db-level tests `outgoing_impact_direct_and_transitive` + `outgoing_impact_phase3_fallback_for_tier1_callee_uri` cover BFS correctness and the Phase-3 fallback.

---

## [2.3.2] — 2026-04-24

**CKB testdrive follow-up.** Six correctness fixes discovered after v2.3.1 shipped and CKB began consuming `EnrichedBlastRadius` end-to-end. `protocol_version` stays at `2`; the only schema change moves an existing field between structurally-nested records and is wire-compatible via `#[serde(flatten)]`.

### Changed

- **`edges_source` moved from `EnrichedBlastRadius` onto `BlastRadiusResult`** — so non-enriched `QueryBlastRadius` (not just `QueryBlastRadiusBatch` / `QueryBlastRadiusSymbol`) carries call-edge provenance. `EnrichedBlastRadius` still surfaces it through `#[serde(flatten)] static_result: BlastRadiusResult`, so the JSON wire shape is unchanged. Rust callers that field-access `enriched.edges_source` must switch to `enriched.static_result.edges_source`. Round-trip test `edges_source_survives_all_response_envelopes` asserts the field is emitted in every response variant.

### Fixed

- **Tier-1 back-fill URIs now translate to SCIP descriptor form — same-file and cross-file.** v2.3.1 Feature #5 re-ran the tree-sitter tier-1 extractor on disk when a SCIP import carried no call edges, but tier-1 emits fragment URIs in plain-identifier form (`#NewExporter`) while scip-go / scip-typescript emit descriptor form (`#NewExporter()`, `#Component.`). Every tier-1-emitted caller and callee missed `def_index.get(caller_sym)` in `blast_radius_for` Phase 3, so Phase 4 fell through to the file-level fallback and produced `ImpactItem`s with blank `symbol_uri` — breaking dedup on the CKB side. The back-fill now builds a same-file `display_name → SCIP-uri` map (plus URI fragment as secondary key) and, for cross-file callees whose definition lives in another SCIP document, falls back to the global `name_to_symbols` index (populated at SCIP-import time from each symbol's `display_name`, with symmetric cleanup on re-upsert and file removal). Both sides of every tier-1 edge are translated in-place when an unambiguous match exists. Regression tests `tier1_backfill_translates_caller_uri_to_scip_fragment` (same-file) and `tier1_backfill_resolves_cross_file_callee_via_name_index` (cross-file) seed SCIP-descriptor defs against real on-disk sources and assert every `ImpactItem` carries a non-empty, translated `symbol_uri`.

- **Path-traversal guard on SCIP document ingestion.** scip-go ships documents whose `relative_path` points outside the project tree (e.g. `../../../../Library/Caches/go-build/…`). `build_document_uri` previously joined them literally, producing URIs like `lip://local//Users/lisa/Work/Projects/CKB/src/../../../../Library/Caches/go-build/…` that the daemon's path-based indexer happily ingested. `convert_document` now rejects any document whose net depth falls below the project root under pure string-level normalization (no filesystem access, so the check works on machines other than the one that produced the index). Emits `warning: skipped N SCIP document(s) whose relative_path escapes project_root` when any documents are dropped.

- **Double `lip://local/` prefix in `callee_to_callers` keys.** `SymbolExtractor::lip_uri` stripped the `file://` scheme but not an existing `lip://local/` prefix. When `upsert_file_precomputed`'s tier-1 back-fill replayed the tree-sitter extractor against a file imported with its canonical key (`lip://local//<abs>/<rel>`), every emitted edge URI was double-prefixed (`lip://local/lip://local//<abs>/<rel>#name`), making URI-exact BFS lookups impossible in `blast_radius_for` Phase 2. Fixed by detecting the `lip://local/` prefix and appending `#<name>` directly; the `file://` / bare-path branch is unchanged for tier-1 callers that pass raw paths. Confirmed via `LIP_DEBUG_EDGES=1` diagnostic trace from a CKB testdrive.

- **SCIP-descriptor / tier-1-identifier name-fragment mismatch in `callee_name_to_callers`.** Tier-1 extractor indexes plain identifiers (`SearchSymbols`); SCIP descriptors carry suffix sigils (`SearchSymbols().`, `MyField.`, `Foo:`). Phase-2 BFS in `blast_radius_for` did `extract_name(callee)` without stripping the sigils, so cross-provider lookups always missed even when both providers had indexed the same function. Added `normalize_callee_name(fragment)` — truncates at the first `(`, then trims trailing non-identifier chars — and applied it at all four `callee_name_to_callers` insert sites plus the BFS lookup site, so SCIP and tier-1 callees now share keys. Unit test `normalize_callee_name_strips_scip_descriptor_suffixes` covers the six canonical SCIP descriptor shapes.

- **Blank `symbol_uri` when tier-1 back-fill preserves a raw caller URI.** After the back-fill resolver falls back to `edge.from_uri` (both `translate` and `name_to_symbols` miss for the caller name — e.g. a caller function not captured as a SCIP `SymbolInformation`), `callee_to_callers` stores the raw tier-1 URI `lip://local//<abs>#<name>`. `def_index` was never populated for that URI (it only records SCIP occurrences), so Phase 3 of `blast_radius_for` skipped every such caller and Phase 4 fell through to the file-level fallback — producing 100 % blank `symbol_uri` in the CKB testdrive against real projects. Phase 3 now derives the file URI by stripping the `#<name>` fragment when `def_index` misses and the caller URI has the `lip://local/` scheme, using the caller URI verbatim as `symbol_uri`. No double-indexing required. Regression test `blast_radius_phase3_fallback_for_tier1_caller_uri` imports a caller file with no SCIP symbols (forcing the resolver miss) against an on-disk source whose tier-1 edge walks to a SCIP-indexed target, then asserts the `ImpactItem` carries the full tier-1 caller URI rather than a blank.

### Added

- **`LIP_DEBUG_EDGES=1` diagnostic gating.** `upsert_file_precomputed`, `blast_radius_for` Phase-2 BFS, and `write_message` (wire output) emit focused `[lip-debug-edges]` traces to stderr when the env var is set. Zero-cost and silent when unset. The wire log now reports `has_edges_source` / `edges_source_count` / `body_bytes` + 500-char head instead of a truncated 2 KB tail, so edges_source presence on the wire can be confirmed without scrolling through multi-kilobyte bodies.

---

## [2.3.1] — 2026-04-21

**CKB import landing fix.** Addresses the "`lip import --push-to-daemon` prints success but every file shows `indexed: false`" class of bug by making client- and daemon-side URI conventions converge, and by back-filling call edges when SCIP imports carry none. `protocol_version` stays at `2`; every change is either additive or limited to CLI behaviour.

### Added

- **`RegisterProjectRoot { root: String }`** — idempotent client message that registers a filesystem root with the daemon. The daemon uses its registered roots to resolve relative `lip://local/<rel>` URIs against the absolute-form records emitted by the tier-1 indexer and by `lip import` (`lip://local//<abs>/<rel>`). Longest matching root wins when multiple are registered. Advertised as `register_project_root` in `HandshakeResult.supported_messages`; pre-v2.3.1 daemons reply `UnknownMessage`, in which case clients must send absolute URIs. `RegisterTier3Source` now auto-registers its `source.project_root` when non-empty, so SCIP imports that carry a project root need no second round-trip.

- **`EdgesSource` provenance on blast radius results** — `EnrichedBlastRadius` gains `edges_source: Option<EdgesSource>` with four variants (`Tier1 | ScipWithTier1Edges | ScipOnly | Empty`). Consumers that maintain their own fallback path (e.g. CKB's native SCIP backend) can now detect when LIP has no structural edges for a file and route around us. `#[serde(default, skip_serializing_if = Option::is_none)]`, so the field is invisible to pre-v2.3.1 clients.

- **Tier-1 edge back-fill on SCIP imports** — `upsert_file_precomputed` now falls back to running the tree-sitter tier-1 extractor over the file on disk when the incoming SCIP document has no call edges. Produces `edges_source = ScipWithTier1Edges` when the fallback succeeds, `Empty` when the file is unreadable or yields no calls. Fills the gap where `scip-go` inconsistently emits call edges and `scip-clang` omits them entirely.

- **`lip import --verify`** — after pushing deltas, samples up to 10 documents and round-trips `QueryFileStatus` (expecting `indexed = true`) plus a `QueryWorkspaceSymbols` probe scoped to the file when the document carries an exported `Function` / `Class` / `Interface` definition. Exits non-zero on any mismatch so CI catches silent import drops instead of printing success and returning 0. Requires `--push-to-daemon`.

### Changed

- **`lip import` URI scheme** — imported documents now use `lip://local/<rel>` (when `Metadata.project_root` is absent) or `lip://local/<abs>/<rel>` with the canonical doubled slash (when it is present), replacing the previous `file:///<rel>` form that silently failed to match any CKB query. Requires CKB to call `RegisterProjectRoot` for its workspace root before querying by relative path.

- **`LipDatabase::canonicalize_uri`** — every public query- and mutation-surface method now canonicalises its URI argument through `registered_roots` before hitting the input/embedding/def/sym/occ maps, so relative and absolute lip-local forms of the same file resolve to the same record. Non-lip-local URIs (`file://…`, `scip://external`, bare paths) are returned unchanged.

### Fixed

- **CKB "printed success but nothing landed"** — the combined effect of the import URI change + `RegisterProjectRoot` + daemon-side canonicalisation means `lip import` records are now discoverable by a CKB client that queries `lip://local/<rel>`, which was the class of bug reported after v2.3.0.

- **Self-echo deadlock on bulk precomputed imports.** Each session subscribes to the daemon's push-notification broadcast and, in the drain loop that runs after every response, writes every pending notification back to its client. Every `Delta { Upsert }` also emitted an `IndexChanged` onto that same broadcast — so the session received an echo of its own emission and wrote it as a *second* frame on top of the `DeltaAck`. The import loop read one frame per iteration, so frame production ran one frame ahead of consumption; after ~65 deltas the 8 KB macOS `AF_UNIX` send buffer filled, `write_message` parked mid-frame, `read_message` never ran, and every worker sat idle with both processes at 0 % CPU. Fixed by tagging every broadcast message with the emitting session's id (`Notification { source_session: Option<u64>, message: ServerMessage }`) and having the drain loop skip envelopes whose `source_session` matches its own. Tier 2 upgrades emit with `source_session: None` so they still reach every session. Regression test `daemon_bulk_precomputed_import_does_not_deadlock` pushes 200 precomputed deltas through a single session and fails fast if any `IndexChanged` echo reaches the client.

---

## [2.3.0] — 2026-04-21

**CKB structural-parity bundle.** Five additive features so CKB (and any other consumer) can retire its duplicate SCIP parser and query LIP for everything structural. `protocol_version` stays at `2`; every new field is `#[serde(default, skip_serializing_if = …)]` and every new message is advertised via `HandshakeResult.supported_messages`, so older clients see no change.

### Added

- **Rich symbol metadata** — `OwnedSymbolInfo` now carries `signature_normalized`, `modifiers: Vec<String>`, `visibility: Option<Visibility>` + `visibility_confidence: Option<u8>`, `container_name: Option<String>`, `extraction_tier: ExtractionTier`, and `modifiers_source: Option<ModifiersSource>`. Tier-1 extractors populate the structural fields for Rust / TypeScript / Python / Swift / Kotlin (`extraction_tier = Tier1`, `modifiers_source = None`). The SCIP importer (`lip import --from-scip`) parses upstream-compatible `enclosing_symbol = 8` and derives modifiers via prefix-parse (`extraction_tier = Tier3Scip`, `modifiers_source = PrefixParse`). Wire round-trip covered end-to-end by `daemon_tier1_emits_v23_metadata` and `daemon_precomputed_preserves_v23_metadata`.

- **Reference classification** — `OwnedOccurrence` gains `kind: ReferenceKind` (`Unknown` / `Call` / `Read` / `Write` / `Type` / `Implements` / `Extends`) and `is_test: bool`. Tier-1 classifier in `symbol_extractor::classify_ref_kind` uses tree-sitter parent-node and field-name lookup — call / method targets → `Call`, assignment-LHS → `Write`, otherwise `Read`; `is_test` stamps occurrences from paths under `/tests/`, `_test.rs`, `_test.py`, `.spec.ts`, etc. SCIP import/export round-trips via `SymbolRole::ReadAccess | WriteAccess | Test` (Call has no SCIP equivalent and maps to `Unknown` on re-export).

- **`QueryBlastRadiusSymbol { symbol_uri, min_score?: f32 } → BlastRadiusSymbolResult { result: Option<EnrichedBlastRadius> }`** — single-symbol wrapper around `blast_radius_for_symbol`. Resolves the symbol's `def_index` entry, runs the same structural BFS + semantic-enrichment loop as `QueryBlastRadiusBatch`, and returns `None` for unknown or unindexed symbols so the caller can distinguish "zero impact" from "no data." Safe inside `BatchQuery`.

- **`QueryOutgoingCalls { symbol_uri, depth: u32 } → OutgoingCallsResult { edges: Vec<OutgoingCallEdge>, truncated: bool }`** — forward call-graph traversal. New `caller_to_callees: HashMap<String, Vec<String>>` index mirrors the existing reverse map, populated in both `upsert_file` and `upsert_file_precomputed` and cleaned in `remove_file_call_edges`. BFS is depth-clamped to `[1, 8]` with `NODE_LIMIT = 200`; the `truncated` flag reports when the cap fired. Safe inside `BatchQuery`.

- **Ranked & filtered workspace symbols** — `QueryWorkspaceSymbols` adds three optional filters (`kind_filter: Option<Vec<SymbolKind>>`, `scope: Option<String>`, `modifier_filter: Option<Vec<String>>`); `WorkspaceSymbolsResult` adds `ranked: Vec<RankedSymbol>` (parallel to `symbols`). Tiered scoring: `Exact = 1.0`, case-insensitive `Prefix = 0.8`, case-insensitive substring `Fuzzy = 0.5` — not BM25. `MatchType` is a discriminator only, not a ranking signal; callers sort by `score`. `ranked` is `skip_if_empty`, and an empty query preserves pre-v2.3 behaviour (empty `ranked`). Pre-v2.3 clients that pattern-match `{ symbols }` keep working unchanged.

- **`OutgoingCallEdge`, `RankedSymbol`, `MatchType`** — new public types in `lip_core::query_graph::types`.

- **Drift guard** — the two new client messages (`query_blast_radius_symbol`, `query_outgoing_calls`) are registered in both `supported_messages()` and `variant_tag()`; the `supported_messages_covers_all_variants` test now covers them.

### Changed

- **`LipDatabase::workspace_symbols`** now delegates to `workspace_symbols_ranked`; the old signature is preserved for callers that do not need the ranked tier (LSP bridge, MCP adapter, legacy CLI). No behaviour change for existing callers.

---

## [2.2.0] — 2026-04-21

### Added

- **`NearestItem.embedding_model`** — every nearest-neighbour hit now carries the model name that produced its stored embedding. Field is optional / `skip_serializing_if = None`; older clients see no change. Populated by `nearest_by_vector`, `nearest_symbol_by_vector`, and `outliers`. Useful for debugging mixed-model indexes and confirming which model was used for a specific result.

- **Function-level blast radius** (`QueryBlastRadiusBatch`) — semantic enrichment now uses per-symbol embeddings when available. If `EmbeddingBatch` has been called with `lip://` URIs (function-level chunks), `semantic_items[].symbol_uri` is populated and results are at function granularity. Falls back to file-level embeddings when no symbol embeddings exist, so the upgrade is transparent.

- **`ReindexStale`** — atomic "reindex if stale" operation. Accepts `uris` and `max_age_seconds`; re-reads from disk only the URIs that are not indexed or whose last-indexed timestamp exceeds the threshold. Returns `ReindexStaleResult { reindexed, skipped }`. Pass `max_age_seconds = 0` to force unconditional reindex. Replaces the manual `QueryFileStatus` → `ReindexFiles` race.

- **`BatchFileStatus`** — query index status for multiple files in one round-trip. Equivalent to issuing `QueryFileStatus` inside a `Batch`, but without message-per-file overhead. Batchable. Returns `BatchFileStatusResult { entries: Vec<FileStatusEntry> }`.

- **`QueryAbiHash`** — stable hex hash (SHA-256) over a file's exported API surface (exported symbol URIs + kinds + signatures, sorted). A change in hash means the public interface changed — safe as a downstream recompilation or re-verification trigger (Kotlin IC model). Returns `AbiHashResult { uri, hash: Option<String> }`. Batchable.

- **Tier 1.5 Datalog inference** — `LipDatabase::run_tier1_5_inference()` runs a fixed-point inference loop applying two rules: (1) if every direct caller of a symbol is at confidence ≥ 80 (Tier 2 / SCIP quality), raise the callee to confidence 65; (2) exported symbols with no local callers are raised by 5 points (capped at 65). Never lowers confidence; never exceeds the Tier 1.5 ceiling, leaving headroom for Tier 2.

- **Tier 2 backoff recovery** — language server backends now recover from transient crashes with exponential backoff (2–300 s, up to 8 failures) instead of being permanently disabled for the session lifetime. `disabled_*` flags are kept for hard failures (binary not installed). A `BackoffState` struct tracks `failure_count` and `available_after` per backend. Tests: `backoff_fresh_is_available`, `backoff_fail_makes_unavailable`, `backoff_reset_clears_state`, `backoff_permanent_after_8_failures`, `backoff_not_permanent_before_8_failures`.

- **`FileStatusEntry`** — new public struct carrying the same fields as `FileStatusResult` but suitable for use inside `BatchFileStatusResult`.

- **`QueryBlastRadiusBatch`** — batch blast radius for all exported symbols in changed files, with optional semantic enrichment via file embeddings. Accepts `changed_file_uris` and optional `min_score` threshold. Resolves symbols server-side (filtered to Function, Method, Class, Interface, Constructor, Macro), runs structural BFS per symbol, and when `min_score` is set, augments results with cosine-similarity neighbours from the file embedding index. Each semantic hit carries a `source` field (`"semantic"` or `"both"`) so consumers can distinguish certainty tiers. Spec §8.1.1.
- **`QueryInvalidatedFiles`** — name-based dependency tracking query. Given a set of changed symbol URIs, returns file URIs that consumed those names externally (Kotlin-IC inspired). Enables symbol-level re-verification without full reindex.
- **`JournalEntry::UpsertFilePrecomputed`** — journal variant that persists pre-computed symbols, occurrences, and CPG edges from SCIP imports. Fixes data loss on daemon restart for SCIP-imported files.

### Fixed

- **SCIP proto field numbers** — `SymbolInformation.relationships` (2→4), `kind` (4→5), `display_name` (5→6) aligned with upstream SCIP. Fixes protobuf decode crash (`LengthDelimited where Varint expected`) when importing any index produced by a spec-compliant SCIP emitter.
- **SCIP proto `Relationship.is_override`** → `is_definition` to match upstream field 5 semantics.
- **SCIP import pre-computed symbol persistence** — Delta handler now routes pre-computed documents through `upsert_file_precomputed`, populating sym_cache, occ_cache, def_index, name_to_symbols, and call-edge indexes. Previously, SCIP-imported symbols were silently dropped.
- **Journal replay for SCIP imports** — pre-computed symbols now survive daemon restart via `UpsertFilePrecomputed` journal entry.
- **Merkle stale_files** — uses stored `content_hash` instead of hashing empty text for pre-computed files. Fixes infinite re-sync loop.
- **file_source_text** — falls back to disk read for precomputed `file://` URIs. Fixes stream_context, embeddings, and explain-match for SCIP-imported files.

- **`EndStreamReason::CursorOutOfRange`** and **`EndStreamReason::FileNotIndexed`** — split the previously-conflated `Error + "cursor_out_of_range"` emission into two typed reasons. Before, a cursor past EOF and a URI the daemon had never indexed both surfaced as `reason: error, error: "cursor_out_of_range"`; clients could not distinguish "user gave bad coordinates" from "daemon has nothing for this path." Now:
  - `CursorOutOfRange` — the file is indexed but the cursor line is outside its range. Error message reports the actual line count.
  - `FileNotIndexed` — the daemon has no record of the URI. Error message names the URI. Callers should upsert or reindex, then retry.
  - `Error` remains for any other stream-terminating failure. Clients should branch on specific reasons first and fall through to `Error` for the rest.

  Non-breaking for the happy path (`BudgetReached`, `Exhausted`) and for the free-form `error` string. Clients that strictly matched `reason == Error` on both failure modes now need one extra arm — all v2.1.x CKB builds ship in lockstep so this lands as a coordinated change.

---

## [2.1.0] — 2026-04-15

### Added

**v2.1 — Streaming context + forward-compat primitives**

**Streaming**

- **`StreamContext { file_uri, cursor_position, max_tokens, model? }`** — new streaming wire message. Daemon ranks symbols relevant to the cursor and emits one `SymbolInfo { symbol_info, relevance_score, token_cost }` frame at a time, terminating with exactly one `EndStream { reason, emitted, total_candidates, error? }` frame. Reasons: `budget_reached`, `exhausted`, `error`. Replaces the broken "fetch top-k, locally truncate to prompt budget" pattern with stream-until-full. Spec §9.2.
- **Relevance ordering** (spec §2.3): direct symbol at cursor → callers (from blast-radius CPG walk) → callees / references → related types. Conservative token-cost estimate `ceil((len(signature) + len(documentation)) / 4) + 8` per symbol. Daemon does not buffer ahead of the socket; `BrokenPipe` from a closing client aborts the ranking walk cleanly. `StreamContext` is rejected from `Batch` / `BatchQuery`.
- **`protocol_version` bumped from `1` → `2`** in `HandshakeResult`. Clients can detect streaming support via handshake.
- **`lip stream-context <file_uri> <line:col> --max-tokens N [--model M]`** — new CLI subcommand prints frames as JSON for manual testing.

**New primitives**

- **`EmbedText { text, model? }`** — embed an arbitrary text string and return the raw vector. Closes the gap left by `EmbeddingBatch` (URI-only) and `QueryNearestByText` (embeds internally but discards the vector). Callers re-ranking with their own scoring (centroid arithmetic, federated nearest-neighbour, lexical-then-semantic re-rank) get the embedding directly instead of building a centroid out of nearest-neighbour seeds. Returns `EmbedTextResult { vector: Vec<f32>, embedding_model: String }`. Not permitted inside `BatchQuery` (requires async HTTP).
- **`RegisterTier3Source { source: Tier3Source }`** + **`IndexStatusResult.tier3_sources`** — expose provenance for Tier 3 ingestion batches (SCIP imports). `Tier3Source { source_id, tool_name, tool_version, project_root, imported_at_ms }` records *what* producer generated the symbols and *when* the daemon accepted them. Re-registering the same `source_id` overwrites in place, refreshing `imported_at_ms`. The daemon deliberately does no staleness detection: stale Tier 3 symbols remain in the graph at their original confidence until the caller re-imports. Surfacing provenance lets clients decide when to warn a user that imported data has aged (e.g. `scip-rust imported 3 days ago`). `lip import --push-to-daemon` now sends this before streaming SCIP deltas, with `source_id = sha256(tool_name + ":" + project_root)`. `IndexStatusResult.tier3_sources` is `#[serde(default)]`; older daemons yield an empty vector. Ack'd with `DeltaAck`. Not permitted inside `BatchQuery` (mutation).
- **`lip import --no-provenance`** — opt out of Tier 3 provenance registration for ephemeral or test imports that should not pollute a long-lived daemon's `tier3_sources` list. No effect on the default EventStream-JSON output path.

**Forward-compat & capability discovery**

- **`HandshakeResult.supported_messages: Vec<String>`** — handshake response now lists every `ClientMessage` `type` tag this daemon understands. Lets clients probe for an individual message (e.g. `stream_context`, `embed_text`) without writing "handshake then pray" code or comparing `protocol_version` integers. Field is `#[serde(default)]`; older daemons yield an empty vector, which clients should treat as "fall back to `protocol_version`."
- **`ServerMessage::UnknownMessage { message_type, supported }`** — when a client sends a well-formed JSON envelope whose `type` tag is unknown, the daemon now replies with `UnknownMessage` (carrying the tag plus the same supported list as handshake) *and keeps the socket open*, instead of closing after a generic parse `Error`. Lets forward-compatible clients downgrade gracefully to a supported call instead of reconnecting.
- **`ServerMessage::Error { message, code }`** — `code: ErrorCode` is a stable, machine-readable category. Clients branch on this instead of string-matching `message`. `#[serde(default)]`; older daemons deserialize as `ErrorCode::Internal`.
- **`ErrorCode`** enum — small, stable set: `unknown_message_type`, `unknown_model`, `embedding_not_configured`, `no_embedding`, `cursor_out_of_range`, `index_locked`, `invalid_request`, `internal` (default). Adding a code is non-breaking; renaming or removing one is breaking.
  - `embedding_not_configured` — daemon has no embedding service (`LIP_EMBEDDING_URL` unset).
  - `no_embedding` — URI has no cached embedding yet; call `EmbeddingBatch` first.
  - `unknown_model` — the embedding endpoint rejected the requested model. Emitted by the daemon when the HTTP backend returns 404 or a 4xx body matching `model_not_found` / `"unknown model"` / `"model … not found/invalid/unsupported"`. Transport, rate-limit, and auth errors stay on `internal` — retrying with the same model only makes sense after a real config change. Classification lives in `daemon/embedding.rs::classify_http_error`.
  - `invalid_request` — request was well-formed on the wire but used incorrectly (e.g. nested `Batch`, or `StreamContext` inside a `Batch`). Distinct from `internal` so clients can avoid retry loops on caller-side mistakes.

**Drift guard**

- **`ClientMessage::variant_tag`** + `supported_messages_covers_all_variants` test — exhaustive-match helper plus paired test that fails compilation when a new `ClientMessage` variant is added without being advertised in `supported_messages()`. Prevents capability-list drift from silently shrinking the handshake surface.

### Fixed

- **`QueryExpansion` handler contract pinned by a db-level test.** The post-embedding ranking is now encapsulated in `LipDatabase::query_expansion_terms(query_vec, actual_model, top_k)`, which the handler calls in one line. A regression that drops the model filter would cause `query_expansion_terms_rejects_cross_model_scoring` (db.rs) to fail, closing the earlier gap where the fix shipped without a paired assertion.
- **`QueryExpansion` now honors the caller's model pin.** Previously the handler embedded the query with the requested model but then ranked candidates across *all* stored symbol embeddings regardless of which model produced them — cross-model cosine scores are not meaningful, so the returned "expansion terms" were effectively noise whenever the index held mixed-model vectors. Handler now captures the actual model returned by `embed_texts` and passes it through a new `model_filter: Option<&str>` parameter on `LipDatabase::nearest_symbol_by_vector`, restricting candidates to symbols embedded with the same model. `SimilarSymbols` (which resolves from a URI's own cached embedding) keeps the old unfiltered behavior by passing `None`.

---

## [2.0.1] — 2026-04-13

### Changed

- Library crate renamed from `lip` to `lip-core` on crates.io (name was taken)
- All three crates now published: `lip-core`, `lip-cli`, `lip-registry`
- Crates metadata: homepage → `https://lip-sigma.vercel.app`, docs linked, `rust-version = "1.78"`, READMEs added
- Author email updated to `lisa@nyxcore.cloud`

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
