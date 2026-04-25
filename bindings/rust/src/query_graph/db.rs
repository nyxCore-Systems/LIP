/// # LIP Query Graph
///
/// A revision-based incremental query graph inspired by Salsa (spec §3.1).
/// Implements the core invariant: a stable API surface shields all callers
/// from internal changes.
///
/// Salsa's proc-macro API has changed across versions; v0.1 implements the
/// pattern manually. A v0.2 migration to the salsa crate is tracked in the
/// roadmap.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::indexer::{language::Language, Tier1Indexer};
use crate::query_graph::types::{
    ApiSurface, BlastRadiusResult, EdgesSource, EnrichedBlastRadius, EnrichedOutgoingImpact,
    ImpactItem, ImpactSource, OutgoingImpactStatic, RiskLevel, SemanticImpactItem, SimilarSymbol,
};
use crate::schema::EdgeKind;
use crate::schema::{
    sha256_hex, OwnedAnnotationEntry, OwnedDependencySlice, OwnedGraphEdge, OwnedOccurrence,
    OwnedRange, OwnedSymbolInfo, Role,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Returns `true` if `(line, col)` falls inside `range` (start inclusive, end exclusive).
fn range_contains(r: &OwnedRange, line: i32, col: i32) -> bool {
    if line < r.start_line || line > r.end_line {
        return false;
    }
    if line == r.start_line && col < r.start_char {
        return false;
    }
    if line == r.end_line && col >= r.end_char {
        return false;
    }
    true
}

/// Extracts the display name from a symbol URI (the fragment after `#`).
///
/// `lip://local/src/main.rs#helper` → `"helper"`
/// Returns `""` for URIs without a `#`.
fn extract_name(uri: &str) -> &str {
    uri.rfind('#').map(|i| &uri[i + 1..]).unwrap_or("")
}

/// Strip SCIP descriptor suffix characters so a fragment like
/// `SearchSymbols().` reduces to `SearchSymbols` — matching the plain
/// identifier form the Tier-1 extractor emits. SCIP descriptors end in
/// `()` for methods/functions, `.` for terms, `#` for types, `:` for
/// macros, or `[T]` for type parameters; tier-1 emits the bare name.
/// Indexing and lookup must go through this normaliser or the two
/// providers will store disjoint keys in `callee_name_to_callers`.
fn normalize_callee_name(fragment: &str) -> &str {
    // Truncate at the first `(` — SCIP's `name(<disambiguator>).` form.
    let head = match fragment.find('(') {
        Some(i) => &fragment[..i],
        None => fragment,
    };
    // Strip trailing SCIP sigils / whitespace.
    head.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
}

/// Returns `true` if the annotation entry has expired (past its `expires_ms` timestamp).
/// An `expires_ms` of 0 means the entry is permanent and never expires.
fn is_expired(entry: &crate::schema::OwnedAnnotationEntry) -> bool {
    if entry.expires_ms == 0 {
        return false;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX);
    entry.expires_ms < now_ms
}

/// Jaccard similarity of 3-character windows (trigrams) between two strings.
///
/// Both inputs are lowercased before comparison. Returns 1.0 for two empty
/// strings, 0.0 when exactly one is empty.
fn trigram_similarity(a: &str, b: &str) -> f32 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    let a_tris: std::collections::HashSet<&str> = a_lower
        .as_bytes()
        .windows(3)
        .map(|w| std::str::from_utf8(w).unwrap_or(""))
        .collect();
    let b_tris: std::collections::HashSet<&str> = b_lower
        .as_bytes()
        .windows(3)
        .map(|w| std::str::from_utf8(w).unwrap_or(""))
        .collect();
    if a_tris.is_empty() && b_tris.is_empty() {
        return 1.0;
    }
    if a_tris.is_empty() || b_tris.is_empty() {
        return 0.0;
    }
    let intersection = a_tris.intersection(&b_tris).count();
    let union = a_tris.union(&b_tris).count();
    intersection as f32 / union as f32
}

/// Normalise a project-root string to an absolute filesystem path (v2.3.1).
///
/// Accepts `file:///abs`, `lip://local/abs`, and bare `/abs` forms. Trailing
/// slashes are stripped. Returns `""` for unrecognised inputs or empty roots.
fn normalise_root(raw: &str) -> String {
    let stripped = if let Some(rest) = raw.strip_prefix("file://") {
        // file:///abs — the first '/' is part of the path
        let trimmed = rest.trim_start_matches('/');
        format!("/{trimmed}")
    } else if let Some(rest) = raw.strip_prefix("lip://local") {
        // lip://local/abs → /abs; handle lip://local (no slash) as ""
        let trimmed = rest.trim_start_matches('/');
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("/{trimmed}")
        }
    } else if raw.starts_with('/') {
        raw.to_owned()
    } else {
        String::new()
    };
    let trimmed = stripped.trim_end_matches('/');
    // Never leave the bare string "" ambiguous with root "/". Empty input → empty root.
    if trimmed.is_empty() && !stripped.starts_with('/') {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

// ─── Internal types ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct FileInput {
    text: String,
    language: String,
    /// Revision at which this input was last changed.
    revision: u64,
    /// `true` when symbols/occurrences were supplied externally (SCIP import)
    /// rather than derived from `text` by Tier 1.
    precomputed: bool,
    /// Content hash supplied by the caller (e.g. from `OwnedDocument.content_hash`).
    /// Used by `stale_files` so Merkle sync works even when `text` is empty.
    content_hash: String,
    /// v2.3.4 — module grouping identifier, resolved at upsert time.
    /// Source priority: slice URI > SCIP descriptor > language-appropriate
    /// manifest walk. See [`crate::query_graph::module_id`]. `None` for
    /// files whose language has no manifest convention and whose URI carries
    /// no slice or SCIP metadata.
    module_id: Option<String>,
}

#[derive(Debug)]
struct Cached<T> {
    value: T,
    /// Revision of the source `FileInput` when this was computed.
    revision: u64,
}

impl<T> Cached<T> {
    fn new(value: T, revision: u64) -> Self {
        Self { value, revision }
    }
}

// ─── LipDatabase ─────────────────────────────────────────────────────────────

/// The LIP in-memory incremental query database.
///
/// Design (spec §3.1):
/// - Global `revision` counter increments O(1) on each file change.
/// - Derived caches store the revision at which they were computed.
/// - A stale cache entry (cached.revision < file.revision) triggers recomputation.
/// - `file_api_surface` is the primary early-cutoff node: if the API surface hash
///   is unchanged, downstream callers skip recomputation entirely.
pub struct LipDatabase {
    /// Global revision counter — increments on every `upsert_file`.
    revision: u64,
    file_inputs: HashMap<String, FileInput>,
    sym_cache: HashMap<String, Cached<Arc<Vec<OwnedSymbolInfo>>>>,
    occ_cache: HashMap<String, Cached<Arc<Vec<OwnedOccurrence>>>>,
    api_cache: HashMap<String, Cached<Arc<ApiSurface>>>,
    /// Reverse index: symbol_uri → (file_uri, definition range). O(1) definition lookup.
    def_index: HashMap<String, (String, OwnedRange)>,
    /// Last Merkle root sent by the client. Drives lifecycle state reporting.
    merkle_root: Option<String>,
    /// Repo root from the last ManifestRequest. Used by Tier 2 to locate rust-analyzer's workspace.
    workspace_root: Option<PathBuf>,
    /// Persistent annotations keyed by (symbol_uri, annotation_key).
    annotations: HashMap<String, HashMap<String, OwnedAnnotationEntry>>,
    /// CPG reverse call graph: callee_uri → [caller_uris].
    /// Populated eagerly in `upsert_file`; used by `blast_radius_for`.
    callee_to_callers: HashMap<String, Vec<String>>,
    /// CPG forward call graph: caller_uri → [callee_uris] (v2.3 Feature #4).
    /// Symmetric mirror of `callee_to_callers`; populated from the same edges
    /// during upsert, cleared during `remove_file_call_edges`. Used by
    /// `QueryOutgoingCalls` to answer "what does this symbol call?".
    caller_to_callees: HashMap<String, Vec<String>>,
    /// Per-file call edge index for cleanup on re-upsert or remove.
    file_call_edges: HashMap<String, Vec<(String, String)>>,
    /// Global name index: display_name → [definition symbol_uris].
    /// Enables cross-file symbol lookup by unqualified name.
    name_to_symbols: HashMap<String, Vec<String>>,
    /// CPG name index: callee display_name → [caller symbol_uris].
    /// Bridges file-local callee URIs to definitions in other files during
    /// `blast_radius_for`. A call edge `to_uri = lip://local/X#foo` from any
    /// file is stored here under key `"foo"`, so blast_radius on the canonical
    /// definition `lip://local/Y#foo` still finds all callers.
    callee_name_to_callers: HashMap<String, Vec<String>>,
    /// Forward-direction twin of `callee_name_to_callers` (v2.3.5).
    /// CPG name index: caller display_name → [callee symbol_uris]. Bridges
    /// the tier-1 back-fill's URI-form mismatches during `outgoing_impact_for`
    /// the same way the reverse bridge serves `blast_radius_for`: when the
    /// SCIP descriptor caller URI (`Engine#AnalyzeImpact().`) misses the
    /// URI-exact `caller_to_callees` key (which the back-fill ended up
    /// writing under the raw tier-1 form), the BFS falls through to this
    /// name-fragment keyed bridge. Keys are normalised via
    /// `normalize_callee_name(extract_name(from_uri))`.
    caller_name_to_callees: HashMap<String, Vec<String>>,
    /// Pre-built symbols from mounted dependency slices (Tier 3, score=100).
    /// Keyed by symbol URI. Not derived from source text — set directly by `mount_slice`.
    mounted_symbols: HashMap<String, OwnedSymbolInfo>,
    /// Tracks which package keys (e.g. "cargo/serde@1.0.0") have been mounted.
    mounted_packages: HashMap<String, (String, String, String)>,
    /// Kotlin-IC model: file_uri → set of external display-names referenced by that file.
    /// Enables precise re-verification when a symbol is renamed or deleted — only files
    /// whose `file_consumed_names` set contains the changed name need re-checking.
    file_consumed_names: HashMap<String, HashSet<String>>,
    /// Cached embedding vectors: file_uri → dense float vector.
    /// Set by the daemon after an `EmbeddingBatch` call; never derived from source.
    file_embeddings: HashMap<String, Vec<f32>>,
    /// Model name that produced each file embedding: file_uri → model string.
    file_embedding_models: HashMap<String, String>,
    /// Cached embedding vectors for individual symbols: symbol_uri → dense float vector.
    /// Keyed by `lip://` URIs. Populated on demand by `QueryNearestBySymbol` or by
    /// `EmbeddingBatch` when called with `lip://` URIs.
    symbol_embeddings: HashMap<String, Vec<f32>>,
    /// Model name that produced each symbol embedding: symbol_uri → model string.
    symbol_embedding_models: HashMap<String, String>,
    /// Unix timestamps (ms) recording when each URI was last upserted.
    file_indexed_at: HashMap<String, i64>,
    /// Provenance for Tier 3 ingestion batches (typically SCIP imports),
    /// keyed by caller-supplied `source_id`. Surfaced through
    /// `QueryIndexStatus` so clients can implement their own staleness
    /// policy; the daemon never reasons about freshness itself.
    tier3_sources: HashMap<String, crate::query_graph::types::Tier3Source>,
    /// Absolute filesystem paths registered as project roots (v2.3.1).
    /// Used by [`LipDatabase::canonicalize_uri`] to resolve relative
    /// `lip://local/<rel>` URIs from clients against absolute keys
    /// produced by SCIP import. Populated by `RegisterProjectRoot` and
    /// implicitly by `RegisterTier3Source` when `project_root` is set.
    /// Resolution prefers the longest prefix when multiple roots match.
    registered_roots: HashSet<String>,
    /// Per-file provenance of call edges (v2.3.1). Surfaced back to
    /// clients through [`crate::query_graph::types::EnrichedBlastRadius::edges_source`]
    /// so CKB can decide whether to fall back to its own SCIP backend.
    file_edges_source: HashMap<String, EdgesSource>,
}

impl LipDatabase {
    pub fn new() -> Self {
        Self {
            revision: 0,
            file_inputs: HashMap::new(),
            sym_cache: HashMap::new(),
            occ_cache: HashMap::new(),
            api_cache: HashMap::new(),
            def_index: HashMap::new(),
            merkle_root: None,
            workspace_root: None,
            annotations: HashMap::new(),
            callee_to_callers: HashMap::new(),
            caller_to_callees: HashMap::new(),
            file_call_edges: HashMap::new(),
            name_to_symbols: HashMap::new(),
            callee_name_to_callers: HashMap::new(),
            caller_name_to_callees: HashMap::new(),
            mounted_symbols: HashMap::new(),
            mounted_packages: HashMap::new(),
            file_consumed_names: HashMap::new(),
            file_embeddings: HashMap::new(),
            file_embedding_models: HashMap::new(),
            symbol_embeddings: HashMap::new(),
            symbol_embedding_models: HashMap::new(),
            file_indexed_at: HashMap::new(),
            tier3_sources: HashMap::new(),
            registered_roots: HashSet::new(),
            file_edges_source: HashMap::new(),
        }
    }

    // ── v2.3.1 project-root registration + URI canonicalisation ─────────

    /// Register an absolute project root for URI canonicalisation.
    ///
    /// Accepts either a bare filesystem path (`/repo`), a `file:///` URI
    /// (`file:///repo`), or a `lip://local/` URI (`lip://local/repo`).
    /// Trailing slashes are trimmed. Duplicates are no-ops.
    ///
    /// Returns `true` when a new root was inserted, `false` when the root
    /// was already registered (or the input normalised to an empty path).
    pub fn register_project_root(&mut self, raw: &str) -> bool {
        let path = normalise_root(raw);
        if path.is_empty() {
            return false;
        }
        self.registered_roots.insert(path)
    }

    /// All currently-registered project roots, sorted for deterministic
    /// output. Primarily exposed for diagnostics and tests.
    pub fn registered_roots(&self) -> Vec<String> {
        let mut out: Vec<String> = self.registered_roots.iter().cloned().collect();
        out.sort();
        out
    }

    /// Canonicalise a URI for lookup inside the query graph (v2.3.1).
    ///
    /// Only `lip://local/<rel>` (a *relative* URI — no leading slash after
    /// the scheme) is rewritten: the daemon prepends a registered project
    /// root so `<rel>` lands on the absolute form that the tier-1 importer
    /// and the v2.3.1 SCIP importer both emit (`lip://local//abs/path`).
    /// All other URIs — including `file:///abs/path`, `lip://local//abs/...`,
    /// `scip://external`, etc. — are returned unchanged.
    ///
    /// Fragments (`#symbol`) are preserved across the rewrite. Multiple
    /// matching roots are tried longest-first; the longest root also wins
    /// when no file-match exists so write paths still produce a stable key
    /// before the file is first upserted.
    ///
    /// This method never mutates state — safe to call on the read path.
    pub fn canonicalize_uri(&self, uri: &str) -> String {
        let Some(body_and_frag) = uri.strip_prefix("lip://local/") else {
            return uri.to_owned();
        };
        let (body, frag) = match body_and_frag.find('#') {
            Some(i) => (&body_and_frag[..i], &body_and_frag[i..]),
            None => (body_and_frag, ""),
        };
        if body.starts_with('/') {
            // Already absolute — canonical.
            return uri.to_owned();
        }

        // Relative path — try each registered root, longest first.
        // Root starts with `/`, so the extra slash in the format string
        // produces the double-slash convention used by tier-1 extractors
        // (`lip://local//abs/path`).
        let mut roots: Vec<&String> = self.registered_roots.iter().collect();
        roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
        for root in &roots {
            let candidate = format!("lip://local/{}/{}", root, body);
            if self.file_inputs.contains_key(&candidate) {
                return if frag.is_empty() {
                    candidate
                } else {
                    format!("{}{}", candidate, frag)
                };
            }
        }

        // No file-match: fall back to the longest root anyway, so write
        // paths still produce a stable canonical key even before the file
        // is first upserted.
        if let Some(longest) = roots.first() {
            let candidate = format!("lip://local/{}/{}", longest, body);
            return if frag.is_empty() {
                candidate
            } else {
                format!("{}{}", candidate, frag)
            };
        }
        uri.to_owned()
    }

    /// Record (or refresh) provenance for a Tier 3 ingestion batch.
    /// Re-registering the same `source_id` overwrites the prior entry,
    /// which is how clients refresh `imported_at_ms` after a re-import.
    pub fn register_tier3_source(&mut self, source: crate::query_graph::types::Tier3Source) {
        self.tier3_sources.insert(source.source_id.clone(), source);
    }

    /// All currently-registered Tier 3 provenance records, sorted by
    /// `source_id` for deterministic output.
    pub fn tier3_sources(&self) -> Vec<crate::query_graph::types::Tier3Source> {
        let mut out: Vec<_> = self.tier3_sources.values().cloned().collect();
        out.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        out
    }

    // ── ABI surface fingerprinting ────────────────────────────────────────

    /// Compute a stable hash over the file's exported API surface.
    ///
    /// The hash is SHA-256 (hex) over the newline-joined list of
    /// `"URI|kind|signature"` entries for all exported symbols in `uri`,
    /// sorted by URI for determinism. Returns `None` when the file is not
    /// in the daemon's index.
    ///
    /// A change in hash means the public interface changed — safe as a
    /// downstream recompilation / re-verification trigger (Kotlin IC model).
    pub fn abi_hash(&mut self, uri: &str) -> Option<String> {
        let uri = self.canonicalize_uri(uri);
        if !self.file_inputs.contains_key(&uri) {
            return None;
        }
        let syms = self.file_symbols(&uri);
        let mut surface: Vec<String> = syms
            .iter()
            .filter(|s| s.is_exported)
            .map(|s| {
                format!(
                    "{}|{}|{}",
                    s.uri,
                    s.kind as u8,
                    s.signature.as_deref().unwrap_or("")
                )
            })
            .collect();
        surface.sort();
        let payload = surface.join("\n");
        Some(sha256_hex(payload.as_bytes()))
    }

    // ── Datalog Tier 1.5 inference ────────────────────────────────────────

    /// Run a single fixed-point inference pass and return the number of
    /// symbols whose confidence was raised.
    ///
    /// Rules applied (one iteration; caller loops to fixpoint):
    ///
    /// **Rule 1 — Callee elevation**: if every direct caller of a symbol
    /// has confidence ≥ 80 (Tier 2 / SCIP quality), and the symbol itself
    /// is below 65, raise it to 65 (Tier 1.5 level). The intuition: if
    /// all callers have been verified to compiler accuracy, the callee is
    /// unlikely to have been left dangling; the call site itself acts as
    /// implicit type evidence.
    ///
    /// **Rule 2 — Exported leaf stability**: an exported symbol with no
    /// callers in the local graph is a stable leaf if its confidence is
    /// ≥ 40. Raise it by 5 points (capped at 65) — exported with no
    /// internal callers means it is part of the public API, which is
    /// typically more carefully maintained than internal helpers.
    ///
    /// Both rules are conservative: they never lower confidence and never
    /// exceed the Tier 1.5 ceiling (65), leaving room for Tier 2 / SCIP
    /// to raise further.
    fn inference_step(&mut self) -> usize {
        const TIER2_THRESHOLD: u8 = 80;
        const TIER1_5_CEILING: u8 = 65;

        // Snapshot caller confidence per symbol before mutating.
        // Build: callee_uri → vec of caller confidence scores.
        let mut callee_caller_confs: HashMap<String, Vec<u8>> = HashMap::new();
        let all_file_uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        for file_uri in &all_file_uris {
            let syms = self.file_symbols(file_uri).to_vec();
            for sym in &syms {
                // For each callee edge, record this caller's confidence.
                if let Some(callers) = self.callee_to_callers.get(&sym.uri).cloned() {
                    for caller_uri in callers {
                        // Look up confidence of the caller symbol.
                        if let Some((caller_file, _)) = self.def_index.get(&caller_uri).cloned() {
                            let caller_syms = self.file_symbols(&caller_file.clone()).to_vec();
                            if let Some(caller_sym) =
                                caller_syms.iter().find(|s| s.uri == caller_uri)
                            {
                                callee_caller_confs
                                    .entry(sym.uri.clone())
                                    .or_default()
                                    .push(caller_sym.confidence_score);
                            }
                        }
                    }
                }
            }
        }

        // Apply rules and collect upgrades.
        let mut upgrades: Vec<(String, String, u8)> = Vec::new(); // (file_uri, sym_uri, new_conf)
        for file_uri in &all_file_uris {
            let syms = self.file_symbols(file_uri).to_vec();
            for sym in &syms {
                if sym.confidence_score >= TIER1_5_CEILING {
                    continue;
                }
                let caller_confs = callee_caller_confs.get(&sym.uri);
                let new_conf = if let Some(confs) = caller_confs {
                    if !confs.is_empty()
                        && confs.iter().all(|&c| c >= TIER2_THRESHOLD)
                        && sym.confidence_score < TIER1_5_CEILING
                    {
                        // Rule 1: all callers are Tier 2+.
                        Some(TIER1_5_CEILING)
                    } else {
                        None
                    }
                } else if sym.is_exported && sym.confidence_score >= 40 {
                    // Rule 2: exported leaf, no local callers.
                    Some((sym.confidence_score + 5).min(TIER1_5_CEILING))
                } else {
                    None
                };
                if let Some(conf) = new_conf {
                    if conf > sym.confidence_score {
                        upgrades.push((file_uri.clone(), sym.uri.clone(), conf));
                    }
                }
            }
        }

        let updated = upgrades.len();
        for (file_uri, sym_uri, new_conf) in upgrades {
            let syms = self.file_symbols(&file_uri).to_vec();
            if let Some(sym) = syms.iter().find(|s| s.uri == sym_uri) {
                let mut upgraded = sym.clone();
                upgraded.confidence_score = new_conf;
                self.upgrade_file_symbols(&file_uri, &[upgraded]);
            }
        }
        updated
    }

    /// Run the Tier 1.5 Datalog inference loop to fixpoint.
    ///
    /// Returns the total number of symbol confidence scores raised.
    pub fn run_tier1_5_inference(&mut self) -> usize {
        let mut total = 0;
        loop {
            let changed = self.inference_step();
            total += changed;
            if changed == 0 {
                break;
            }
        }
        total
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Register or update a file. Bumps the global revision and invalidates
    /// cached derived data for `uri`.
    /// Return the module grouping id stored for `file_uri`, if any (v2.3.4).
    fn module_id_for(&self, file_uri: &str) -> Option<String> {
        self.file_inputs
            .get(file_uri)
            .and_then(|fi| fi.module_id.clone())
    }

    pub fn upsert_file(&mut self, uri: String, text: String, language: String) {
        let uri = self.canonicalize_uri(&uri);
        self.revision += 1;
        let rev = self.revision;
        let content_hash = sha256_hex(text.as_bytes());
        let module_id = crate::query_graph::module_id::resolve_module_id(&uri, &language, &[]);
        self.file_inputs.insert(
            uri.clone(),
            FileInput {
                text,
                language,
                revision: rev,
                precomputed: false,
                content_hash,
                module_id,
            },
        );
        // Invalidate the direct derived caches. api_cache is intentionally kept
        // so file_api_surface can compare the new hash against the previous one and
        // fire an early-cutoff (returning the same Arc) when the API surface is stable.
        self.sym_cache.remove(&uri);
        self.occ_cache.remove(&uri);

        // Eagerly rebuild the definition reverse index for this file.
        // First remove stale name_to_symbols entries for symbols this file defined.
        let stale_defs: Vec<String> = self
            .def_index
            .iter()
            .filter(|(_, (furi, _))| furi == &uri)
            .map(|(sym_uri, _)| sym_uri.clone())
            .collect();
        for sym_uri in &stale_defs {
            let name = extract_name(sym_uri);
            if let Some(uris) = self.name_to_symbols.get_mut(name) {
                uris.retain(|u| u != sym_uri);
                if uris.is_empty() {
                    self.name_to_symbols.remove(name);
                }
            }
        }
        self.def_index.retain(|_, (furi, _)| furi != &uri);
        let occs = self.compute_occurrences(&uri);
        for occ in occs.iter() {
            if occ.role == Role::Definition {
                self.def_index
                    .insert(occ.symbol_uri.clone(), (uri.clone(), occ.range.clone()));
                // Populate global name index.
                let name = extract_name(&occ.symbol_uri).to_owned();
                if !name.is_empty() {
                    self.name_to_symbols
                        .entry(name)
                        .or_default()
                        .push(occ.symbol_uri.clone());
                }
            }
        }
        // Cache the occurrences we just computed to avoid a redundant parse on first query.
        self.occ_cache
            .insert(uri.to_owned(), Cached::new(occs, rev));

        // Eagerly rebuild the CPG call-edge reverse index for this file.
        self.remove_file_call_edges(&uri);
        if let Some(input) = self.file_inputs.get(&uri) {
            let lang = Language::detect(&uri, &input.language.clone());
            let text = input.text.clone();
            let edges = Tier1Indexer::new().edges_for_source(&uri, &text, lang);
            let mut pairs: Vec<(String, String)> = Vec::new();
            for edge in edges.iter().filter(|e| e.kind == EdgeKind::Calls) {
                self.callee_to_callers
                    .entry(edge.to_uri.clone())
                    .or_default()
                    .push(edge.from_uri.clone());
                self.caller_to_callees
                    .entry(edge.from_uri.clone())
                    .or_default()
                    .push(edge.to_uri.clone());
                // Name-based index: enables cross-file resolution in blast_radius_for.
                // Normalise to the plain-identifier form so SCIP-descriptor
                // callees (`Foo().`) share a key with tier-1 (`Foo`).
                let callee_name = normalize_callee_name(extract_name(&edge.to_uri)).to_owned();
                if !callee_name.is_empty() {
                    self.callee_name_to_callers
                        .entry(callee_name)
                        .or_default()
                        .push(edge.from_uri.clone());
                }
                // Forward twin: enables outgoing_impact_for to seed from a
                // SCIP descriptor URI when the back-fill kept the raw
                // tier-1 caller URI. v2.3.5.
                let caller_name = normalize_callee_name(extract_name(&edge.from_uri)).to_owned();
                if !caller_name.is_empty() {
                    self.caller_name_to_callees
                        .entry(caller_name)
                        .or_default()
                        .push(edge.to_uri.clone());
                }
                pairs.push((edge.from_uri.clone(), edge.to_uri.clone()));
            }
            let src = if pairs.is_empty() {
                EdgesSource::Empty
            } else {
                EdgesSource::Tier1
            };
            self.file_edges_source.insert(uri.clone(), src);
            self.file_call_edges.insert(uri.clone(), pairs);
        }

        // Rebuild Kotlin-IC consumed-names index: Reference occurrences whose
        // symbol is defined in a different file (or unresolved) are "external names".
        {
            let cached_occs = self
                .occ_cache
                .get(&uri)
                .map(|c| c.value.clone())
                .unwrap_or_default();
            let mut consumed: HashSet<String> = HashSet::new();
            for occ in cached_occs.iter().filter(|o| o.role == Role::Reference) {
                let name = extract_name(&occ.symbol_uri);
                if name.is_empty() {
                    continue;
                }
                let is_external = self
                    .def_index
                    .get(&occ.symbol_uri)
                    .map(|(def_file, _)| def_file != &uri)
                    .unwrap_or(true); // unresolved → treat as external
                if is_external {
                    consumed.insert(name.to_owned());
                }
            }
            self.file_consumed_names.insert(uri.clone(), consumed);
        }

        // Record the timestamp of this upsert; invalidate stale embedding.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.file_indexed_at.insert(uri.clone(), now_ms);
        // A new source version invalidates the old embedding.
        self.file_embeddings.remove(&uri);
    }

    /// Upsert a file whose symbols and occurrences are already computed
    /// (e.g. SCIP import). Populates the same indexes as `upsert_file` but
    /// skips the Tier 1 parser since the caller already provides the data.
    pub fn upsert_file_precomputed(
        &mut self,
        uri: String,
        language: String,
        content_hash: String,
        symbols: Vec<OwnedSymbolInfo>,
        occurrences: Vec<OwnedOccurrence>,
        edges: Vec<OwnedGraphEdge>,
    ) {
        let uri = self.canonicalize_uri(&uri);
        self.revision += 1;
        let rev = self.revision;
        let module_id = crate::query_graph::module_id::resolve_module_id(&uri, &language, &symbols);
        self.file_inputs.insert(
            uri.clone(),
            FileInput {
                text: String::new(),
                language: language.clone(),
                revision: rev,
                precomputed: true,
                content_hash,
                module_id,
            },
        );

        // Snapshot old display_names before clearing sym_cache so we can
        // remove both the fragment-keyed and display_name-keyed entries from
        // `name_to_symbols` (v2.3.2 Issue #1 adds display_name indexing).
        let stale_display_names: Vec<(String, String)> = self
            .sym_cache
            .get(&uri)
            .map(|c| {
                c.value
                    .iter()
                    .filter(|s| !s.display_name.is_empty())
                    .map(|s| (s.uri.clone(), s.display_name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        // Clear stale caches + def_index entries for this file.
        self.sym_cache.remove(&uri);
        self.occ_cache.remove(&uri);
        let stale_defs: Vec<String> = self
            .def_index
            .iter()
            .filter(|(_, (furi, _))| furi == &uri)
            .map(|(sym_uri, _)| sym_uri.clone())
            .collect();
        for sym_uri in &stale_defs {
            let name = extract_name(sym_uri);
            if let Some(uris) = self.name_to_symbols.get_mut(name) {
                uris.retain(|u| u != sym_uri);
                if uris.is_empty() {
                    self.name_to_symbols.remove(name);
                }
            }
        }
        for (sym_uri, display_name) in &stale_display_names {
            if let Some(uris) = self.name_to_symbols.get_mut(display_name) {
                uris.retain(|u| u != sym_uri);
                if uris.is_empty() {
                    self.name_to_symbols.remove(display_name);
                }
            }
        }
        self.def_index.retain(|_, (furi, _)| furi != &uri);

        // Build def_index + name_to_symbols from pre-computed occurrences.
        let occs = Arc::new(occurrences);
        for occ in occs.iter() {
            if occ.role == Role::Definition {
                self.def_index
                    .insert(occ.symbol_uri.clone(), (uri.clone(), occ.range.clone()));
                let name = extract_name(&occ.symbol_uri).to_owned();
                if !name.is_empty() {
                    self.name_to_symbols
                        .entry(name)
                        .or_default()
                        .push(occ.symbol_uri.clone());
                }
            }
        }
        self.occ_cache
            .insert(uri.clone(), Cached::new(occs.clone(), rev));

        // Seed sym_cache so file_symbols() returns the pre-computed symbols.
        let syms = Arc::new(symbols);
        self.sym_cache
            .insert(uri.clone(), Cached::new(syms.clone(), rev));

        // v2.3.2 Issue #1 — also index SCIP defs by their `display_name`
        // (not just URI fragment) so tier-1 back-fill's cross-file callee
        // translation can resolve plain-identifier names to SCIP URIs. The
        // descriptor suffix scip-go emits (`NewExporter()` in the fragment)
        // otherwise hides cross-file matches from the tier-1 extractor's
        // plain-identifier view.
        for sym in syms.iter() {
            if sym.display_name.is_empty() {
                continue;
            }
            let frag = extract_name(&sym.uri);
            if sym.display_name == frag {
                continue; // already indexed by upstream occurrence loop
            }
            let entry = self
                .name_to_symbols
                .entry(sym.display_name.clone())
                .or_default();
            if !entry.contains(&sym.uri) {
                entry.push(sym.uri.clone());
            }
        }

        // Consumed-names index (same as upsert_file).
        {
            let mut consumed: HashSet<String> = HashSet::new();
            for occ in occs.iter().filter(|o| o.role == Role::Reference) {
                let name = extract_name(&occ.symbol_uri);
                if name.is_empty() {
                    continue;
                }
                let is_external = self
                    .def_index
                    .get(&occ.symbol_uri)
                    .map(|(def_file, _)| def_file != &uri)
                    .unwrap_or(true);
                if is_external {
                    consumed.insert(name.to_owned());
                }
            }
            self.file_consumed_names.insert(uri.clone(), consumed);
        }

        // Call-edge indexes from pre-computed edges.
        self.remove_file_call_edges(&uri);
        let mut pairs: Vec<(String, String)> = Vec::new();
        for edge in edges.iter().filter(|e| e.kind == EdgeKind::Calls) {
            self.callee_to_callers
                .entry(edge.to_uri.clone())
                .or_default()
                .push(edge.from_uri.clone());
            self.caller_to_callees
                .entry(edge.from_uri.clone())
                .or_default()
                .push(edge.to_uri.clone());
            let callee_name = normalize_callee_name(extract_name(&edge.to_uri)).to_owned();
            if !callee_name.is_empty() {
                self.callee_name_to_callers
                    .entry(callee_name)
                    .or_default()
                    .push(edge.from_uri.clone());
            }
            let caller_name = normalize_callee_name(extract_name(&edge.from_uri)).to_owned();
            if !caller_name.is_empty() {
                self.caller_name_to_callees
                    .entry(caller_name)
                    .or_default()
                    .push(edge.to_uri.clone());
            }
            pairs.push((edge.from_uri.clone(), edge.to_uri.clone()));
        }

        // v2.3.1 Feature #5 — SCIP-imported files often have no call edges
        // (scip-clang omits `SymbolRole::Call`; scip-go is inconsistent).
        // When the disk source is reachable, re-run the Tier-1 tree-sitter
        // edge extractor to populate the forward + reverse call graph so
        // `QueryBlastRadiusSymbol` returns non-empty `direct_items`.
        //
        // v2.3.2 Issue #1 — tier-1 emits file-local URIs (`lip://local/…#Name`)
        // that won't match the SCIP-style def_index keys (`lip://scip-go/…#Name`).
        // Without translation, Phase 3 of `blast_radius_for` can't resolve the
        // caller symbol and every ImpactItem degrades to blank `symbol_uri`.
        // We translate the caller side only: the current file's SCIP defs are
        // already in `def_index`, so we can look them up by name and rewrite
        // the tier-1 caller URI in-place. Callees stay tier-1 because the BFS
        // walks `callee_name_to_callers` via name fragment regardless.
        // Snapshot the SCIP-origin count before any back-fill can append.
        // The diagnostic at the end splits scip_pairs vs tier1_pairs so the
        // log unambiguously explains which branch produced which edges.
        let scip_pairs = pairs.len();
        let edges_src = if !pairs.is_empty() {
            EdgesSource::ScipOnly
        } else if let Some(path) = crate::daemon::watcher::uri_to_path(&uri) {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let lang = Language::detect(&uri, &language);
                    let tier1_edges = Tier1Indexer::new().edges_for_source(&uri, &text, lang);

                    // Build identifier → SCIP-uri map for defs in this file.
                    // Used to translate tier-1-emitted caller/callee URIs into
                    // SCIP keys so that def_index and name-based BFS lookups in
                    // blast_radius_for succeed. Keyed by both `display_name`
                    // and the URI fragment — SCIP descriptors (`NewExporter()`
                    // in scip-go, `Component.` in scip-typescript) differ from
                    // tier-1's plain-identifier fragment extraction.
                    let mut translate: HashMap<String, String> = HashMap::new();
                    let this_file_syms = self.file_symbols(&uri);
                    for sym in this_file_syms.iter() {
                        if !sym.display_name.is_empty() {
                            translate
                                .entry(sym.display_name.clone())
                                .or_insert_with(|| sym.uri.clone());
                        }
                        let frag = extract_name(&sym.uri);
                        if !frag.is_empty() {
                            translate
                                .entry(frag.to_owned())
                                .or_insert_with(|| sym.uri.clone());
                        }
                    }

                    // Cross-file fallback: when the same-file `translate` map
                    // misses (callee/caller defined in another SCIP document),
                    // fall back to the global `name_to_symbols` index. Only
                    // accept unambiguous hits (single URI) so we don't alias
                    // unrelated homonyms across packages. v2.3.2 Issue #1.
                    let name_to_symbols = &self.name_to_symbols;
                    let resolve = |name: &str, fallback: &str| -> String {
                        if let Some(u) = translate.get(name) {
                            return u.clone();
                        }
                        if let Some(uris) = name_to_symbols.get(name) {
                            if uris.len() == 1 {
                                return uris[0].clone();
                            }
                        }
                        fallback.to_owned()
                    };

                    let mut filled = false;
                    let calls: Vec<(String, String)> = tier1_edges
                        .iter()
                        .filter(|e| e.kind == EdgeKind::Calls)
                        .map(|edge| {
                            let caller_name = extract_name(&edge.from_uri);
                            let callee_name_raw = extract_name(&edge.to_uri);
                            (
                                resolve(caller_name, &edge.from_uri),
                                resolve(callee_name_raw, &edge.to_uri),
                            )
                        })
                        .collect();
                    for (from_uri, to_uri) in calls {
                        self.callee_to_callers
                            .entry(to_uri.clone())
                            .or_default()
                            .push(from_uri.clone());
                        self.caller_to_callees
                            .entry(from_uri.clone())
                            .or_default()
                            .push(to_uri.clone());
                        let callee_name = normalize_callee_name(extract_name(&to_uri)).to_owned();
                        if !callee_name.is_empty() {
                            self.callee_name_to_callers
                                .entry(callee_name)
                                .or_default()
                                .push(from_uri.clone());
                        }
                        let caller_name = normalize_callee_name(extract_name(&from_uri)).to_owned();
                        if !caller_name.is_empty() {
                            self.caller_name_to_callees
                                .entry(caller_name)
                                .or_default()
                                .push(to_uri.clone());
                        }
                        pairs.push((from_uri, to_uri));
                        filled = true;
                    }
                    if filled {
                        EdgesSource::ScipWithTier1Edges
                    } else {
                        EdgesSource::Empty
                    }
                }
                Err(_) => EdgesSource::Empty,
            }
        } else {
            EdgesSource::Empty
        };
        // v2.3.2 diagnostic — gated on LIP_DEBUG_EDGES=1. Confirms which
        // back-fill branch fired per file and the exact key written into
        // `file_edges_source` (for pairing against the lookup-side log in
        // `blast_radius_for`).
        if std::env::var("LIP_DEBUG_EDGES")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            let tier1_pairs = pairs.len().saturating_sub(scip_pairs);
            eprintln!(
                "[lip-debug-edges] upsert_precomputed uri={} edges_src={:?} pairs={} scip_pairs={} tier1_pairs={}",
                uri,
                edges_src,
                pairs.len(),
                scip_pairs,
                tier1_pairs
            );
        }
        self.file_edges_source.insert(uri.clone(), edges_src);
        self.file_call_edges.insert(uri.clone(), pairs);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.file_indexed_at.insert(uri.clone(), now_ms);
        self.file_embeddings.remove(&uri);
    }

    pub fn remove_file(&mut self, uri: &str) {
        let uri = self.canonicalize_uri(uri);
        let uri = uri.as_str();
        self.revision += 1;
        self.file_inputs.remove(uri);
        // Snapshot display_names before removing sym_cache (v2.3.2 Issue #1
        // display_name indexing needs symmetric cleanup on file removal).
        let stale_display_names: Vec<(String, String)> = self
            .sym_cache
            .get(uri)
            .map(|c| {
                c.value
                    .iter()
                    .filter(|s| !s.display_name.is_empty())
                    .map(|s| (s.uri.clone(), s.display_name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.sym_cache.remove(uri);
        self.occ_cache.remove(uri);
        self.api_cache.remove(uri);
        // Clean name_to_symbols before clearing def_index.
        let stale_defs: Vec<String> = self
            .def_index
            .iter()
            .filter(|(_, (furi, _))| furi.as_str() == uri)
            .map(|(sym_uri, _)| sym_uri.clone())
            .collect();
        for sym_uri in &stale_defs {
            let name = extract_name(sym_uri);
            if let Some(uris) = self.name_to_symbols.get_mut(name) {
                uris.retain(|u| u != sym_uri);
                if uris.is_empty() {
                    self.name_to_symbols.remove(name);
                }
            }
        }
        for (sym_uri, display_name) in &stale_display_names {
            if let Some(uris) = self.name_to_symbols.get_mut(display_name) {
                uris.retain(|u| u != sym_uri);
                if uris.is_empty() {
                    self.name_to_symbols.remove(display_name);
                }
            }
        }
        self.def_index.retain(|_, (furi, _)| furi.as_str() != uri);
        self.remove_file_call_edges(uri);
        self.file_consumed_names.remove(uri);
        self.file_embeddings.remove(uri);
        self.file_embedding_models.remove(uri);
        self.file_indexed_at.remove(uri);
    }

    fn remove_file_call_edges(&mut self, uri: &str) {
        if let Some(pairs) = self.file_call_edges.remove(uri) {
            for (from, to) in pairs {
                if let Some(callers) = self.callee_to_callers.get_mut(&to) {
                    callers.retain(|c| *c != from);
                }
                if let Some(callees) = self.caller_to_callees.get_mut(&from) {
                    callees.retain(|c| *c != to);
                    if callees.is_empty() {
                        self.caller_to_callees.remove(&from);
                    }
                }
                let callee_name = normalize_callee_name(extract_name(&to));
                if let Some(callers) = self.callee_name_to_callers.get_mut(callee_name) {
                    callers.retain(|c| *c != from);
                    if callers.is_empty() {
                        self.callee_name_to_callers.remove(callee_name);
                    }
                }
                let caller_name = normalize_callee_name(extract_name(&from));
                if let Some(callees) = self.caller_name_to_callees.get_mut(caller_name) {
                    callees.retain(|c| *c != to);
                    if callees.is_empty() {
                        self.caller_name_to_callees.remove(caller_name);
                    }
                }
            }
        }
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────

    pub fn set_merkle_root(&mut self, root: String) {
        self.merkle_root = Some(root);
    }

    pub fn current_merkle_root(&self) -> Option<&str> {
        self.merkle_root.as_deref()
    }

    pub fn file_count(&self) -> usize {
        self.file_inputs.len()
    }

    /// Returns the source text stored for `uri`, or `None` if not indexed.
    pub fn file_source_text(&self, uri: &str) -> Option<String> {
        let canon = self.canonicalize_uri(uri);
        let fi = self.file_inputs.get(&canon)?;
        if fi.precomputed && fi.text.is_empty() {
            if let Some(path) = crate::daemon::watcher::uri_to_path(&canon) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    return Some(text);
                }
            }
            return None;
        }
        Some(fi.text.clone())
    }

    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = Some(root);
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// Merge Tier 2 symbol upgrades into the cached symbol list for `uri`.
    ///
    /// For each upgraded symbol (matched by URI), replaces `confidence_score`
    /// and `signature` / `documentation` with the Tier 2 values. The file
    /// input revision is NOT bumped — this is a quality enhancement on existing
    /// data, not a new source input. The API surface cache is invalidated so
    /// downstream callers see the improved data on their next access.
    pub fn upgrade_file_symbols(&mut self, uri: &str, upgrades: &[OwnedSymbolInfo]) {
        if upgrades.is_empty() {
            return;
        }
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let Some(cached) = self.sym_cache.get(uri) else {
            return;
        };
        let rev = cached.revision;
        let existing: Vec<OwnedSymbolInfo> = cached.value.as_ref().clone();

        let merged: Vec<OwnedSymbolInfo> = existing
            .into_iter()
            .map(|mut sym| {
                if let Some(up) = upgrades.iter().find(|u| u.uri == sym.uri) {
                    // Only apply if the incoming upgrade is at least as confident as
                    // the current value. This prevents a racing Tier 2 job from
                    // silently downgrading a symbol that was already upgraded by a
                    // SCIP push at a higher confidence score.
                    if up.confidence_score >= sym.confidence_score {
                        sym.confidence_score = up.confidence_score;
                        if up.signature.is_some() {
                            sym.signature = up.signature.clone();
                        }
                        if up.documentation.is_some() {
                            sym.documentation = up.documentation.clone();
                        }
                        if !up.relationships.is_empty() {
                            sym.relationships = up.relationships.clone();
                        }
                    }
                }
                sym
            })
            .collect();

        self.sym_cache
            .insert(uri.to_owned(), Cached::new(Arc::new(merged), rev));
        // Invalidate api_cache so the content_hash reflects updated signatures.
        self.api_cache.remove(uri);
    }

    // ── Raw accessors ─────────────────────────────────────────────────────

    pub fn file_text(&self, uri: &str) -> Option<&str> {
        let canon = self.canonicalize_uri(uri);
        self.file_inputs.get(&canon).map(|f| f.text.as_str())
    }

    pub fn file_language(&self, uri: &str) -> Option<&str> {
        let canon = self.canonicalize_uri(uri);
        self.file_inputs.get(&canon).map(|f| f.language.as_str())
    }

    pub fn tracked_uris(&self) -> Vec<String> {
        self.file_inputs.keys().cloned().collect()
    }

    pub fn is_precomputed(&self, uri: &str) -> bool {
        let canon = self.canonicalize_uri(uri);
        self.file_inputs.get(&canon).is_some_and(|f| f.precomputed)
    }

    pub fn file_content_hash(&self, uri: &str) -> Option<&str> {
        let canon = self.canonicalize_uri(uri);
        self.file_inputs
            .get(&canon)
            .map(|f| f.content_hash.as_str())
    }

    /// Read-only access to cached symbols (for journal compaction).
    pub fn cached_symbols(&self, uri: &str) -> Arc<Vec<OwnedSymbolInfo>> {
        self.sym_cache
            .get(uri)
            .map(|c| c.value.clone())
            .unwrap_or_default()
    }

    /// Read-only access to cached occurrences (for journal compaction).
    pub fn cached_occurrences(&self, uri: &str) -> Arc<Vec<OwnedOccurrence>> {
        self.occ_cache
            .get(uri)
            .map(|c| c.value.clone())
            .unwrap_or_default()
    }

    /// Return stored call-edge pairs for a file (for journal compaction).
    pub fn file_call_edges_raw(&self, uri: &str) -> Vec<OwnedGraphEdge> {
        self.file_call_edges
            .get(uri)
            .map(|pairs| {
                pairs
                    .iter()
                    .map(|(from, to)| OwnedGraphEdge {
                        from_uri: from.clone(),
                        to_uri: to.clone(),
                        kind: EdgeKind::Calls,
                        at_range: OwnedRange::default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Merkle sync probe: given a slice of `(uri, client_content_hash)` pairs,
    /// returns URIs that are stale (daemon hash ≠ client hash) or unknown to
    /// the daemon (never indexed). The client should re-Delta each returned URI.
    pub fn stale_files(&self, entries: &[(String, String)]) -> Vec<String> {
        entries
            .iter()
            .filter(|(uri, client_hash)| {
                match self.file_inputs.get(uri) {
                    None => true, // daemon has never seen this file
                    Some(fi) => fi.content_hash != *client_hash,
                }
            })
            .map(|(uri, _)| uri.clone())
            .collect()
    }

    /// All definition symbol URIs with the given display name across all tracked files.
    ///
    /// Useful for workspace-wide "go to definition by name" without a full scan.
    pub fn symbols_by_name(&self, name: &str) -> Vec<&str> {
        self.name_to_symbols
            .get(name)
            .map(|uris| uris.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn current_revision(&self) -> u64 {
        self.revision
    }

    // ── Derived queries ───────────────────────────────────────────────────

    /// Tier 1 symbols for a file, lazily computed and cached.
    pub fn file_symbols(&mut self, uri: &str) -> Arc<Vec<OwnedSymbolInfo>> {
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None => return Arc::new(vec![]),
        };

        if let Some(cached) = self.sym_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        // Precomputed files (SCIP imports) have no source text — Tier 1 parsing
        // on empty text returns nothing anyway, but bail early to make the
        // invariant explicit: precomputed symbols live only in sym_cache.
        if self.file_inputs.get(uri).is_some_and(|f| f.precomputed) {
            return Arc::new(vec![]);
        }

        let result = self.compute_symbols(uri);
        self.sym_cache
            .insert(uri.to_owned(), Cached::new(result.clone(), file_rev));
        result
    }

    /// Tier 1 occurrences for a file, lazily computed and cached.
    pub fn file_occurrences(&mut self, uri: &str) -> Arc<Vec<OwnedOccurrence>> {
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None => return Arc::new(vec![]),
        };

        if let Some(cached) = self.occ_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let result = self.compute_occurrences(uri);
        self.occ_cache
            .insert(uri.to_owned(), Cached::new(result.clone(), file_rev));
        result
    }

    /// Exported API surface — the primary early-cutoff node.
    ///
    /// If `content_hash` is identical to the last-cached value, downstream
    /// callers can skip their own recomputation (see spec §3.1 "early cutoff").
    pub fn file_api_surface(&mut self, uri: &str) -> Arc<ApiSurface> {
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None => {
                return Arc::new(ApiSurface {
                    content_hash: String::new(),
                    symbols: vec![],
                })
            }
        };

        // Early-cutoff check: if the API surface is fresh, return it.
        if let Some(cached) = self.api_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let symbols = self.file_symbols(uri);
        let public: Vec<OwnedSymbolInfo> =
            symbols.iter().filter(|s| s.is_exported).cloned().collect();

        let surface_text = public
            .iter()
            .map(|s| {
                format!(
                    "{}:{}",
                    s.uri,
                    s.signature.as_deref().unwrap_or(&s.display_name)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Compare new hash to cached; if equal, restore the old cached entry so
        // callers can detect the early-cutoff (same Arc pointer or same hash).
        let new_hash = sha256_hex(surface_text.as_bytes());
        if let Some(cached) = self.api_cache.get(uri) {
            if cached.value.content_hash == new_hash {
                // API surface unchanged — early cutoff. Update revision in place.
                let old_val = cached.value.clone();
                self.api_cache
                    .insert(uri.to_owned(), Cached::new(old_val.clone(), file_rev));
                return old_val;
            }
        }

        let surface = Arc::new(ApiSurface {
            content_hash: new_hash,
            symbols: public,
        });
        self.api_cache
            .insert(uri.to_owned(), Cached::new(surface.clone(), file_rev));
        surface
    }

    /// Files that directly reference any exported symbol from `uri`.
    pub fn reverse_deps(&mut self, uri: &str) -> Vec<String> {
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let target = self.file_api_surface(uri);
        let target_uris: Vec<String> = target.symbols.iter().map(|s| s.uri.clone()).collect();

        uris.into_iter()
            .filter(|other| other != uri)
            .filter(|other| {
                let occs = self.file_occurrences(other);
                occs.iter().any(|occ| target_uris.contains(&occ.symbol_uri))
            })
            .collect()
    }

    /// Compute blast radius for a symbol URI (spec §8.1).
    pub fn blast_radius_for(&mut self, symbol_uri: &str) -> BlastRadiusResult {
        // BFS limits — keeps response time bounded on highly-connected symbols.
        const DEPTH_LIMIT: u32 = 4;
        const NODE_LIMIT: usize = 200;

        let canon_symbol = self.canonicalize_uri(symbol_uri);
        let symbol_uri = canon_symbol.as_str();

        let all_uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let def_uri = all_uris
            .iter()
            .find(|uri| self.file_symbols(uri).iter().any(|s| s.uri == symbol_uri))
            .cloned();

        let Some(def_uri) = def_uri else {
            return BlastRadiusResult {
                symbol_uri: symbol_uri.to_owned(),
                ..Default::default()
            };
        };

        // ── Phase 1: file-level reverse-dependency BFS ────────────────────
        //
        // Walks `reverse_deps` (file → files that import it) with depth
        // tracking. Produces a map of file_uri → minimum distance from def_uri.
        //
        // NOTE: def_uri is intentionally NOT seeded at distance 0. If a
        // same-file symbol calls the target (e.g. main() calls helper() in
        // the same file), the CPG phase will discover def_uri at distance 1
        // and include it correctly. Seeding at 0 would suppress that.
        let mut truncated = false;
        let mut file_distance: HashMap<String, u32> = HashMap::new();
        {
            let mut queue: VecDeque<(String, u32)> = VecDeque::new();
            // Start from def_uri's direct dependents at distance 1.
            for dep_file in self.reverse_deps(&def_uri) {
                if !file_distance.contains_key(&dep_file) {
                    file_distance.insert(dep_file.clone(), 1);
                    queue.push_back((dep_file, 1));
                }
            }

            while let Some((file_uri, depth)) = queue.pop_front() {
                if depth >= DEPTH_LIMIT {
                    truncated = true;
                    continue;
                }
                if file_distance.len() > NODE_LIMIT {
                    truncated = true;
                    break;
                }
                for dep_file in self.reverse_deps(&file_uri) {
                    if !file_distance.contains_key(&dep_file) {
                        file_distance.insert(dep_file.clone(), depth + 1);
                        queue.push_back((dep_file, depth + 1));
                    }
                }
            }
        }

        // ── Phase 2: CPG (call-graph) BFS ─────────────────────────────────
        //
        // Walks `callee_to_callers` and `callee_name_to_callers` with depth
        // tracking, then maps each caller symbol to its defining file.
        //
        // Cross-file resolution: Tier 1 generates file-local URIs, so a call
        // to `helper` in file A produces `to_uri = lip://local/A#helper` while
        // the definition in file B has URI `lip://local/B#helper`.
        // `callee_name_to_callers` bridges this by keying on the name fragment.
        //
        // caller_sym → minimum distance from symbol_uri
        let mut cpg_distance: HashMap<String, u32> = HashMap::new();
        let debug_edges = std::env::var("LIP_DEBUG_EDGES")
            .map(|v| v == "1")
            .unwrap_or(false);
        // Phase-2 hit/miss counters (v2.3.2 diagnostic). `uri_*` covers the
        // exact-URI `callee_to_callers` index; `name_*` covers the
        // name-fragment `callee_name_to_callers` bridge.
        let (mut uri_hits, mut uri_misses, mut name_hits, mut name_misses) =
            (0u32, 0u32, 0u32, 0u32);
        {
            let mut queue: VecDeque<(String, u32)> = VecDeque::new();
            cpg_distance.insert(symbol_uri.to_owned(), 0);
            queue.push_back((symbol_uri.to_owned(), 0));

            while let Some((callee, depth)) = queue.pop_front() {
                if depth >= DEPTH_LIMIT {
                    truncated = true;
                    continue;
                }
                if cpg_distance.len() > NODE_LIMIT {
                    truncated = true;
                    break;
                }
                // URI-exact callers (same-file or pre-resolved edges).
                if let Some(callers) = self.callee_to_callers.get(&callee).cloned() {
                    uri_hits += 1;
                    for caller in callers {
                        if !cpg_distance.contains_key(&caller) {
                            cpg_distance.insert(caller.clone(), depth + 1);
                            queue.push_back((caller, depth + 1));
                        }
                    }
                } else {
                    uri_misses += 1;
                }
                // Name-based callers: catches file-local URIs from other files.
                // Normalise the SCIP descriptor fragment so `SearchSymbols().`
                // collides with the tier-1-indexed `SearchSymbols`.
                let name = normalize_callee_name(extract_name(&callee));
                if !name.is_empty() {
                    if let Some(callers) = self.callee_name_to_callers.get(name).cloned() {
                        name_hits += 1;
                        for caller in callers {
                            if !cpg_distance.contains_key(&caller) {
                                cpg_distance.insert(caller.clone(), depth + 1);
                                queue.push_back((caller, depth + 1));
                            }
                        }
                    } else {
                        name_misses += 1;
                    }
                }
            }
        }

        if debug_edges {
            eprintln!(
                "[lip-debug-edges] blast_radius_for Phase-2 symbol={} uri_hits={} uri_misses={} name_hits={} name_misses={} cpg_nodes={}",
                symbol_uri, uri_hits, uri_misses, name_hits, name_misses, cpg_distance.len()
            );
            // If nothing hit, dump a few representative keys from each index
            // so we can eyeball the URI-form mismatch.
            if uri_hits == 0 && name_hits == 0 {
                let uri_keys: Vec<&String> = self.callee_to_callers.keys().take(3).collect();
                let name_keys: Vec<&String> = self.callee_name_to_callers.keys().take(10).collect();
                let raw = extract_name(symbol_uri);
                let normalized = normalize_callee_name(raw);
                eprintln!(
                    "[lip-debug-edges]   query_name_raw={:?} normalized={:?} callee_to_callers_total={} sample={:?}",
                    raw,
                    normalized,
                    self.callee_to_callers.len(),
                    uri_keys
                );
                eprintln!(
                    "[lip-debug-edges]   callee_name_to_callers_total={} sample={:?}",
                    self.callee_name_to_callers.len(),
                    name_keys
                );
            }
        }

        // ── Phase 3: merge ────────────────────────────────────────────────
        //
        // For each caller symbol from the CPG pass, resolve its defining file,
        // merge into `file_distance`, and collect one entry per distinct
        // (caller_sym, file) pair — giving function-level granularity.
        //
        // sym_items: Vec<(caller_sym_uri, file_uri, distance)>
        // Multiple caller symbols in the same file each produce their own entry.
        let mut sym_items: Vec<(String, String, u32)> = Vec::new();
        for (caller_sym, &sym_dist) in &cpg_distance {
            if caller_sym == symbol_uri {
                continue; // skip the target itself
            }
            // Prefer def_index when present — it's the authoritative mapping.
            // Fall back to deriving the file URI from the caller URI itself when
            // the caller came from tier-1 back-fill (which inserts edges keyed
            // by `lip://local/<abs>#<name>` without populating def_index).
            let file_uri_opt = self
                .def_index
                .get(caller_sym)
                .map(|(f, _)| f.clone())
                .or_else(|| {
                    if caller_sym.starts_with("lip://local/") {
                        let hash_idx = caller_sym.rfind('#')?;
                        let candidate = &caller_sym[..hash_idx];
                        if self.file_inputs.contains_key(candidate) {
                            Some(candidate.to_owned())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });
            if let Some(file_uri) = file_uri_opt {
                let prev_dist = file_distance.get(&file_uri).copied().unwrap_or(u32::MAX);
                file_distance.insert(file_uri.clone(), sym_dist.min(prev_dist));
                sym_items.push((caller_sym.clone(), file_uri, sym_dist));
            }
        }

        // ── Phase 4: build result ─────────────────────────────────────────
        let mut direct_items: Vec<ImpactItem> = vec![];
        let mut transitive_items: Vec<ImpactItem> = vec![];
        let mut affected_files_set: HashSet<String> = HashSet::new();

        // Per-symbol items from CPG — one ImpactItem per (file, symbol) pair.
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (sym, file, dist) in &sym_items {
            if dist == &0 {
                continue; // the symbol's own file is not an "affected" file
            }
            if seen.insert((file.clone(), sym.clone())) {
                let item = ImpactItem {
                    file_uri: file.clone(),
                    symbol_uri: sym.clone(),
                    distance: *dist,
                    confidence: ImpactItem::confidence_at(*dist),
                    module_id: self.module_id_for(file),
                };
                affected_files_set.insert(file.clone());
                if *dist == 1 {
                    direct_items.push(item);
                } else {
                    transitive_items.push(item);
                }
            }
        }

        // File-level items for files reachable only via reverse-deps (no CPG symbol matched).
        for (file_uri, &distance) in &file_distance {
            if distance == 0 {
                continue; // the symbol's own file is not "affected"
            }
            if sym_items.iter().any(|(_, f, _)| f == file_uri) {
                continue; // already covered by CPG pass above
            }
            let item = ImpactItem {
                file_uri: file_uri.clone(),
                symbol_uri: String::new(),
                distance,
                confidence: ImpactItem::confidence_at(distance),
                module_id: self.module_id_for(file_uri),
            };
            affected_files_set.insert(file_uri.clone());
            if distance == 1 {
                direct_items.push(item);
            } else {
                transitive_items.push(item);
            }
        }

        let mut affected_files: Vec<String> = affected_files_set.into_iter().collect();

        // Sort for deterministic output.
        direct_items.sort_by(|a, b| {
            a.file_uri
                .cmp(&b.file_uri)
                .then(a.symbol_uri.cmp(&b.symbol_uri))
        });
        transitive_items.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then(a.file_uri.cmp(&b.file_uri))
                .then(a.symbol_uri.cmp(&b.symbol_uri))
        });
        affected_files.sort();

        // direct/transitive counts are unique files, not items.
        let direct_count = direct_items
            .iter()
            .map(|i| &i.file_uri)
            .collect::<HashSet<_>>()
            .len() as u32;
        let transitive_count = direct_items
            .iter()
            .chain(transitive_items.iter())
            .map(|i| &i.file_uri)
            .collect::<HashSet<_>>()
            .len() as u32;

        // Risk: Low ≤3 direct / ≤5 total · Medium ≤10 direct / ≤20 total · High otherwise
        let risk_level = if direct_count > 10 || transitive_count > 20 {
            RiskLevel::High
        } else if direct_count > 3 || transitive_count > 5 {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        };

        let edges_source = self.file_edges_source.get(&def_uri).copied();
        // v2.3.2 diagnostic — gated on LIP_DEBUG_EDGES=1. Prints the
        // symbol_uri / def_uri / hit-or-miss triple so we can spot
        // canonicalisation asymmetry between upsert-time and query-time.
        if std::env::var("LIP_DEBUG_EDGES")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            let has = edges_source.is_some();
            eprintln!(
                "[lip-debug-edges] blast_radius_for symbol={} def_uri={} edges_source_hit={} value={:?} sym_items={} file_items={}",
                symbol_uri,
                def_uri,
                has,
                edges_source,
                sym_items.len(),
                file_distance.len(),
            );
            if !has {
                // Dump up to 5 keys so we can compare against the insert log.
                let keys: Vec<&String> = self.file_edges_source.keys().take(5).collect();
                eprintln!(
                    "[lip-debug-edges]   file_edges_source sample keys (total={}): {:?}",
                    self.file_edges_source.len(),
                    keys
                );
            }
        }
        BlastRadiusResult {
            symbol_uri: symbol_uri.to_owned(),
            direct_dependents: direct_count,
            transitive_dependents: transitive_count,
            affected_files,
            direct_items,
            transitive_items,
            truncated,
            risk_level,
            edges_source,
        }
    }

    /// Batch blast-radius for all symbols defined in the given files,
    /// optionally enriched with embedding-based semantic coupling.
    ///
    /// When `min_score` is `Some(threshold)`, each changed file's embedding
    /// is compared against the index and neighbours above the threshold are
    /// returned as `semantic_items`. Omit to get static-only results.
    pub fn blast_radius_batch(
        &mut self,
        changed_file_uris: &[String],
        min_score: Option<f32>,
    ) -> (Vec<EnrichedBlastRadius>, Vec<String>) {
        let mut results = Vec::new();
        let mut not_indexed_uris = Vec::new();
        let mut seen_symbols: HashSet<String> = HashSet::new();
        let threshold = min_score.unwrap_or(0.6);

        // Only resolve symbols whose kind produces meaningful blast-radius
        // results. Variables, constants, parameters, and type aliases are
        // excluded — they're dominated by framework wiring noise.
        use crate::schema::SymbolKind;
        let interesting = |k: SymbolKind| {
            matches!(
                k,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::Constructor
                    | SymbolKind::Macro
            )
        };

        for file_uri in changed_file_uris {
            let canon_file = self.canonicalize_uri(file_uri);
            if !self.file_inputs.contains_key(canon_file.as_str()) {
                not_indexed_uris.push(file_uri.clone());
                continue;
            }
            let syms = self.file_symbols(&canon_file);
            for sym in syms.iter() {
                if !interesting(sym.kind) {
                    continue;
                }
                if !seen_symbols.insert(sym.uri.clone()) {
                    continue;
                }
                let static_result = self.blast_radius_for(&sym.uri);

                let mut semantic_items = Vec::new();
                if min_score.is_some() {
                    let static_files: HashSet<String> =
                        static_result.affected_files.iter().cloned().collect();

                    // Prefer per-symbol embeddings (function-level granularity) when
                    // available. Fall back to file-level embeddings when the symbol has
                    // no stored vector. This degrades gracefully for callers that have
                    // not yet run `EmbeddingBatch` with `lip://` URIs.
                    if let Some(sym_embedding) = self.symbol_embeddings.get(&sym.uri).cloned() {
                        let sym_neighbours =
                            self.nearest_symbol_by_vector(&sym_embedding, 20, Some(&sym.uri), None);
                        for n in sym_neighbours {
                            if n.score < threshold {
                                continue;
                            }
                            // Map symbol hit back to its defining file.
                            let hit_file = self
                                .def_index
                                .get(&n.uri)
                                .map(|(f, _)| f.clone())
                                .unwrap_or_else(|| n.uri.clone());
                            let source = if static_files.contains(&hit_file) {
                                ImpactSource::Both
                            } else {
                                ImpactSource::Semantic
                            };
                            let module_id = self.module_id_for(&hit_file);
                            semantic_items.push(SemanticImpactItem {
                                file_uri: hit_file,
                                symbol_uri: n.uri,
                                similarity: n.score,
                                source,
                                module_id,
                            });
                        }
                    } else if let Some(file_embedding) =
                        self.file_embeddings.get(canon_file.as_str()).cloned()
                    {
                        let neighbours = self.nearest_by_vector(
                            &file_embedding,
                            20,
                            Some(&canon_file),
                            None,
                            Some(threshold),
                        );
                        for neighbour in neighbours {
                            let source = if static_files.contains(&neighbour.uri) {
                                ImpactSource::Both
                            } else {
                                ImpactSource::Semantic
                            };
                            let module_id = self.module_id_for(&neighbour.uri);
                            semantic_items.push(SemanticImpactItem {
                                file_uri: neighbour.uri,
                                symbol_uri: String::new(),
                                similarity: neighbour.score,
                                source,
                                module_id,
                            });
                        }
                    }
                }

                results.push(EnrichedBlastRadius {
                    file_uri: canon_file.clone(),
                    static_result,
                    semantic_items,
                });
            }
        }
        (results, not_indexed_uris)
    }

    /// Symbol-scoped blast radius with optional semantic enrichment (v2.3).
    ///
    /// Returns `None` when the symbol has no known defining file (either the
    /// URI doesn't resolve or the file isn't indexed). The semantic-enrichment
    /// path mirrors [`blast_radius_batch`]: per-symbol embeddings preferred,
    /// file-level fallback. When `min_score` is `None`, enrichment is skipped.
    pub fn blast_radius_for_symbol(
        &mut self,
        symbol_uri: &str,
        min_score: Option<f32>,
    ) -> Option<EnrichedBlastRadius> {
        let canon_symbol = self.canonicalize_uri(symbol_uri);
        let symbol_uri = canon_symbol.as_str();
        let file_uri = self.def_index.get(symbol_uri).map(|(f, _)| f.clone())?;
        if !self.file_inputs.contains_key(file_uri.as_str()) {
            return None;
        }
        let threshold = min_score.unwrap_or(0.6);
        let static_result = self.blast_radius_for(symbol_uri);

        let mut semantic_items = Vec::new();
        if min_score.is_some() {
            let static_files: HashSet<String> =
                static_result.affected_files.iter().cloned().collect();

            if let Some(sym_embedding) = self.symbol_embeddings.get(symbol_uri).cloned() {
                let neighbours =
                    self.nearest_symbol_by_vector(&sym_embedding, 20, Some(symbol_uri), None);
                for n in neighbours {
                    if n.score < threshold {
                        continue;
                    }
                    let hit_file = self
                        .def_index
                        .get(&n.uri)
                        .map(|(f, _)| f.clone())
                        .unwrap_or_else(|| n.uri.clone());
                    let source = if static_files.contains(&hit_file) {
                        ImpactSource::Both
                    } else {
                        ImpactSource::Semantic
                    };
                    let module_id = self.module_id_for(&hit_file);
                    semantic_items.push(SemanticImpactItem {
                        file_uri: hit_file,
                        symbol_uri: n.uri,
                        similarity: n.score,
                        source,
                        module_id,
                    });
                }
            } else if let Some(file_embedding) = self.file_embeddings.get(&file_uri).cloned() {
                let neighbours = self.nearest_by_vector(
                    &file_embedding,
                    20,
                    Some(&file_uri),
                    None,
                    Some(threshold),
                );
                for neighbour in neighbours {
                    let source = if static_files.contains(&neighbour.uri) {
                        ImpactSource::Both
                    } else {
                        ImpactSource::Semantic
                    };
                    let module_id = self.module_id_for(&neighbour.uri);
                    semantic_items.push(SemanticImpactItem {
                        file_uri: neighbour.uri,
                        symbol_uri: String::new(),
                        similarity: neighbour.score,
                        source,
                        module_id,
                    });
                }
            }
        }

        Some(EnrichedBlastRadius {
            file_uri,
            static_result,
            semantic_items,
        })
    }

    /// Forward-call BFS starting at `symbol_uri` (v2.3 Feature #4).
    ///
    /// Walks `caller_to_callees` up to `depth` hops. Returns a flat
    /// `(caller, callee)` edge list and a `truncated` flag that is `true`
    /// when the node cap was hit.
    pub fn outgoing_calls(&self, symbol_uri: &str, depth: u32) -> (Vec<(String, String)>, bool) {
        const NODE_LIMIT: usize = 200;
        let depth = depth.clamp(1, 8);

        let canon = self.canonicalize_uri(symbol_uri);
        let symbol_uri = canon.as_str();

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut seen_edges: HashSet<(String, String)> = HashSet::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(symbol_uri.to_owned());

        let mut frontier: Vec<String> = vec![symbol_uri.to_owned()];
        let mut truncated = false;

        for _ in 0..depth {
            let mut next: Vec<String> = Vec::new();
            for caller in &frontier {
                let Some(callees) = self.caller_to_callees.get(caller) else {
                    continue;
                };
                for callee in callees {
                    let edge = (caller.clone(), callee.clone());
                    if seen_edges.insert(edge.clone()) {
                        if edges.len() >= NODE_LIMIT {
                            truncated = true;
                            return (edges, truncated);
                        }
                        edges.push(edge);
                    }
                    if visited.insert(callee.clone()) {
                        next.push(callee.clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        (edges, truncated)
    }

    /// Forward-direction symbol impact with optional semantic enrichment (v2.3.3).
    ///
    /// Symmetric to [`Self::blast_radius_for_symbol`]. Runs a forward BFS
    /// over `caller_to_callees` starting at `symbol_uri`, groups callees
    /// by distance (direct = 1, transitive >= 2), and surfaces the same
    /// [`EdgesSource`] provenance as the incoming-direction query.
    ///
    /// `depth` clamps to `[1, 8]`; `None` defaults to 8. `min_score` gates
    /// semantic enrichment (embedding NN on the target's embedding). The
    /// same tier-1-URI fallback used in Phase 3 of `blast_radius_for`
    /// (strip `#<name>` when `def_index` misses) applies here so callees
    /// emitted by the Tier-1 back-fill resolve to `ImpactItem` rows with
    /// a non-empty `symbol_uri`.
    ///
    /// Returns `None` when the symbol has no known defining file.
    pub fn outgoing_impact_for(
        &mut self,
        symbol_uri: &str,
        depth: Option<u32>,
        min_score: Option<f32>,
    ) -> Option<EnrichedOutgoingImpact> {
        const NODE_LIMIT: usize = 200;
        let depth = depth.unwrap_or(8).clamp(1, 8);

        let canon_symbol = self.canonicalize_uri(symbol_uri);
        let symbol_uri = canon_symbol.as_str();

        // Resolve the symbol's defining file. Fail closed if we can't —
        // downstream enrichment needs it, and the caller can't distinguish
        // "zero callees" from "unknown symbol" without this signal.
        let file_uri = self.def_index.get(symbol_uri).map(|(f, _)| f.clone())?;
        if !self.file_inputs.contains_key(file_uri.as_str()) {
            return None;
        }
        let threshold = min_score.unwrap_or(0.6);

        // ── Forward BFS over caller_to_callees ───────────────────────────
        //
        // Mirrors Phase 2 of `blast_radius_for`: at each hop, consult both
        // the URI-exact `caller_to_callees` index and the name-bridge
        // `caller_name_to_callees`. The bridge catches the case where the
        // seed (or an intermediate caller) is a SCIP descriptor URI while
        // the tier-1 back-fill kept the raw tier-1 form as the index key —
        // symmetric to how the reverse direction bridges file-local callee
        // URIs to scip-form seeds. v2.3.5.
        let mut callee_distance: HashMap<String, u32> = HashMap::new();
        let mut truncated = false;
        {
            let mut frontier: Vec<String> = vec![symbol_uri.to_owned()];
            let mut visited: HashSet<String> = HashSet::new();
            visited.insert(symbol_uri.to_owned());

            'bfs: for hop in 1..=depth {
                let mut next: Vec<String> = Vec::new();
                for caller in &frontier {
                    // URI-exact callees.
                    let mut direct_callees: Vec<String> = self
                        .caller_to_callees
                        .get(caller)
                        .cloned()
                        .unwrap_or_default();
                    // Name-bridge callees: normalise the caller's fragment
                    // so a SCIP descriptor (`AnalyzeImpact().`) collides
                    // with the tier-1-indexed `AnalyzeImpact`.
                    let caller_name = normalize_callee_name(extract_name(caller));
                    if !caller_name.is_empty() {
                        if let Some(extra) = self.caller_name_to_callees.get(caller_name) {
                            direct_callees.extend(extra.iter().cloned());
                        }
                    }
                    for callee in &direct_callees {
                        if callee == symbol_uri {
                            continue; // skip self-cycles back to the seed
                        }
                        if callee_distance.len() > NODE_LIMIT {
                            truncated = true;
                            break 'bfs;
                        }
                        let prev = callee_distance.get(callee).copied().unwrap_or(u32::MAX);
                        if hop < prev {
                            callee_distance.insert(callee.clone(), hop);
                        }
                        if visited.insert(callee.clone()) {
                            next.push(callee.clone());
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
        }

        // ── Map each callee to its defining file ─────────────────────────
        //
        // Mirrors Phase 3 of `blast_radius_for`: prefer `def_index`, fall
        // back to stripping `#<name>` from a `lip://local/` URI when the
        // tier-1 back-fill kept a raw caller/callee URI that was never
        // registered in `def_index` (v2.3.2 Bug D, symmetric direction).
        let mut direct_items: Vec<ImpactItem> = Vec::new();
        let mut transitive_items: Vec<ImpactItem> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (callee_sym, &dist) in &callee_distance {
            let resolved_file = self
                .def_index
                .get(callee_sym)
                .map(|(f, _)| f.clone())
                .or_else(|| {
                    if callee_sym.starts_with("lip://local/") {
                        let hash_idx = callee_sym.rfind('#')?;
                        let candidate = &callee_sym[..hash_idx];
                        if self.file_inputs.contains_key(candidate) {
                            Some(candidate.to_owned())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });
            let Some(callee_file) = resolved_file else {
                continue; // unresolved external symbol — drop rather than emit blank
            };
            if !seen.insert((callee_file.clone(), callee_sym.clone())) {
                continue;
            }
            let module_id = self.module_id_for(&callee_file);
            let item = ImpactItem {
                file_uri: callee_file,
                symbol_uri: callee_sym.clone(),
                distance: dist,
                confidence: ImpactItem::confidence_at(dist),
                module_id,
            };
            if dist == 1 {
                direct_items.push(item);
            } else {
                transitive_items.push(item);
            }
        }

        // Deterministic ordering.
        direct_items.sort_by(|a, b| {
            a.file_uri
                .cmp(&b.file_uri)
                .then(a.symbol_uri.cmp(&b.symbol_uri))
        });
        transitive_items.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then(a.file_uri.cmp(&b.file_uri))
                .then(a.symbol_uri.cmp(&b.symbol_uri))
        });

        let edges_source = self.file_edges_source.get(&file_uri).copied();

        let static_result = OutgoingImpactStatic {
            target_uri: symbol_uri.to_owned(),
            direct_items,
            transitive_items,
            edges_source,
            truncated,
        };

        // ── Semantic enrichment ──────────────────────────────────────────
        //
        // Same seed as `blast_radius_for_symbol`: the target's own
        // embedding. Per-symbol preferred, file-level fallback. The
        // `source` tagging (`Static | Semantic | Both`) references the set
        // of *callee files* we already reached statically, so a semantic
        // hit already confirmed by the call graph flips to `Both`.
        let mut semantic_items: Vec<SemanticImpactItem> = Vec::new();
        if min_score.is_some() {
            let static_files: HashSet<String> = static_result
                .direct_items
                .iter()
                .chain(static_result.transitive_items.iter())
                .map(|i| i.file_uri.clone())
                .collect();

            if let Some(sym_embedding) = self.symbol_embeddings.get(symbol_uri).cloned() {
                let neighbours =
                    self.nearest_symbol_by_vector(&sym_embedding, 20, Some(symbol_uri), None);
                for n in neighbours {
                    if n.score < threshold {
                        continue;
                    }
                    let hit_file = self
                        .def_index
                        .get(&n.uri)
                        .map(|(f, _)| f.clone())
                        .unwrap_or_else(|| n.uri.clone());
                    let source = if static_files.contains(&hit_file) {
                        ImpactSource::Both
                    } else {
                        ImpactSource::Semantic
                    };
                    let module_id = self.module_id_for(&hit_file);
                    semantic_items.push(SemanticImpactItem {
                        file_uri: hit_file,
                        symbol_uri: n.uri,
                        similarity: n.score,
                        source,
                        module_id,
                    });
                }
            } else if let Some(file_embedding) = self.file_embeddings.get(&file_uri).cloned() {
                let neighbours = self.nearest_by_vector(
                    &file_embedding,
                    20,
                    Some(&file_uri),
                    None,
                    Some(threshold),
                );
                for neighbour in neighbours {
                    let source = if static_files.contains(&neighbour.uri) {
                        ImpactSource::Both
                    } else {
                        ImpactSource::Semantic
                    };
                    let module_id = self.module_id_for(&neighbour.uri);
                    semantic_items.push(SemanticImpactItem {
                        file_uri: neighbour.uri,
                        symbol_uri: String::new(),
                        similarity: neighbour.score,
                        source,
                        module_id,
                    });
                }
            }
        }

        Some(EnrichedOutgoingImpact {
            static_result,
            semantic_items,
        })
    }

    /// Find the symbol URI whose occurrence range contains `(line, col)` in `uri`.
    ///
    /// Returns `None` if no occurrence covers the given position.
    pub fn symbol_at_position(&mut self, uri: &str, line: i32, col: i32) -> Option<String> {
        let canon = self.canonicalize_uri(uri);
        let occs = self.file_occurrences(&canon);
        occs.iter()
            .find(|occ| range_contains(&occ.range, line, col))
            .map(|occ| occ.symbol_uri.clone())
    }

    /// Find the definition occurrence location for `symbol_uri`.
    ///
    /// O(1) via the definition reverse index maintained in `upsert_file`.
    pub fn symbol_definition_location(&self, symbol_uri: &str) -> Option<(String, OwnedRange)> {
        let canon = self.canonicalize_uri(symbol_uri);
        self.def_index.get(canon.as_str()).cloned()
    }

    /// Files that reference any of the given display-name strings (Kotlin IC model).
    ///
    /// Use this after a rename or delete: pass the old symbol name(s) to find every
    /// file that consumed them and needs re-verification.
    pub fn files_consuming_names(&self, names: &[&str]) -> Vec<String> {
        let name_set: HashSet<&str> = names.iter().copied().collect();
        self.file_consumed_names
            .iter()
            .filter(|(_, consumed)| consumed.iter().any(|n| name_set.contains(n.as_str())))
            .map(|(f, _)| f.clone())
            .collect()
    }

    /// Given a set of changed symbol URIs, return the deduplicated set of file
    /// URIs that need re-verification because they consume at least one of the
    /// changed names.
    ///
    /// This is the public entry-point for symbol-level invalidation (Kotlin IC
    /// model). It extracts the display name from each symbol URI via
    /// `extract_name`, then delegates to the `file_consumed_names` index.
    pub fn invalidated_files_for(&self, changed_symbol_uris: &[String]) -> Vec<String> {
        let names: HashSet<&str> = changed_symbol_uris
            .iter()
            .map(|uri| extract_name(uri))
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            return vec![];
        }
        let name_refs: Vec<&str> = names.into_iter().collect();
        self.files_consuming_names(&name_refs)
    }

    // ── Embedding / observability ─────────────────────────────────────────

    /// Store a pre-computed embedding vector for a file, recording which model produced it.
    pub fn set_file_embedding(&mut self, uri: &str, vector: Vec<f32>, model: &str) {
        let uri = self.canonicalize_uri(uri);
        self.file_embeddings.insert(uri.clone(), vector);
        self.file_embedding_models.insert(uri, model.to_owned());
    }

    /// Retrieve the stored embedding vector for a file, if any.
    pub fn get_file_embedding(&self, uri: &str) -> Option<&Vec<f32>> {
        let canon = self.canonicalize_uri(uri);
        self.file_embeddings.get(canon.as_str())
    }

    /// Retrieve the model that produced the stored embedding for a file, if any.
    pub fn file_embedding_model(&self, uri: &str) -> Option<&str> {
        let canon = self.canonicalize_uri(uri);
        self.file_embedding_models
            .get(canon.as_str())
            .map(String::as_str)
    }

    /// Store a pre-computed embedding vector for a symbol URI (`lip://` scheme),
    /// recording which model produced it.
    pub fn set_symbol_embedding(&mut self, uri: &str, vector: Vec<f32>, model: &str) {
        let uri = self.canonicalize_uri(uri);
        self.symbol_embeddings.insert(uri.clone(), vector);
        self.symbol_embedding_models.insert(uri, model.to_owned());
    }

    /// Retrieve the stored embedding vector for a symbol URI, if any.
    pub fn get_symbol_embedding(&self, uri: &str) -> Option<&Vec<f32>> {
        let canon = self.canonicalize_uri(uri);
        self.symbol_embeddings.get(canon.as_str())
    }

    /// Return the distinct model names present across all stored file embeddings.
    /// Used to detect mixed-model indexes after a model upgrade.
    pub fn file_embedding_model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .file_embedding_models
            .values()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        names
    }

    /// Rank symbols semantically related to `query_vec` (produced by
    /// `actual_model`) and return their display names as query-expansion
    /// terms.
    ///
    /// Encapsulates the post-embedding work of the `QueryExpansion`
    /// handler so the daemon-side wiring (filter results to symbols
    /// embedded with `actual_model`, then resolve display names) is
    /// pinned by a db-level test and cannot silently regress in the
    /// session handler. Cross-model cosine scores are meaningless —
    /// mixing them would rank random symbols highest.
    pub fn query_expansion_terms(
        &mut self,
        query_vec: &[f32],
        actual_model: &str,
        top_k: usize,
    ) -> Vec<String> {
        let hits = self.nearest_symbol_by_vector(query_vec, top_k, None, Some(actual_model));
        let uris: Vec<String> = hits.into_iter().map(|item| item.uri).collect();
        uris.into_iter()
            .map(|uri| match self.symbol_by_uri(&uri) {
                Some(s) => s.display_name,
                None => uri
                    .rfind('#')
                    .map(|i| uri[i + 1..].to_owned())
                    .unwrap_or(uri),
            })
            .collect()
    }

    /// Find the `top_k` symbols whose embedding is most similar (cosine) to `query_vec`.
    ///
    /// Mirrors `nearest_by_vector` but operates over `symbol_embeddings`.
    pub fn nearest_symbol_by_vector(
        &self,
        query_vec: &[f32],
        top_k: usize,
        exclude_uri: Option<&str>,
        model_filter: Option<&str>,
    ) -> Vec<crate::query_graph::types::NearestItem> {
        let q_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if q_norm == 0.0 || top_k == 0 {
            return vec![];
        }
        let mut scored: Vec<(String, f32)> = self
            .symbol_embeddings
            .iter()
            .filter(|(uri, _)| exclude_uri.map(|e| e != uri.as_str()).unwrap_or(true))
            .filter(|(uri, _)| {
                // When `model_filter` is set, skip any symbol whose stored
                // embedding was produced by a different model — cross-model
                // cosine scores are not meaningful.
                model_filter.map_or(true, |want| {
                    self.symbol_embedding_models
                        .get(uri.as_str())
                        .map(|m| m == want)
                        .unwrap_or(false)
                })
            })
            .filter_map(|(uri, vec)| {
                if vec.len() != query_vec.len() {
                    return None;
                }
                let dot: f32 = query_vec.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                let v_norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if v_norm == 0.0 {
                    return None;
                }
                Some((uri.clone(), dot / (q_norm * v_norm)))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(uri, score)| {
                let embedding_model = self.symbol_embedding_models.get(&uri).cloned();
                crate::query_graph::types::NearestItem {
                    uri,
                    score,
                    embedding_model,
                }
            })
            .collect()
    }

    /// Number of files that have been indexed but whose embedding is not yet stored.
    pub fn pending_embedding_count(&self) -> usize {
        self.file_inputs
            .keys()
            .filter(|uri| !self.file_embeddings.contains_key(*uri))
            .count()
    }

    /// Unix timestamp (ms) of the most recent `upsert_file` call, or `None` if empty.
    pub fn last_updated_ms(&self) -> Option<i64> {
        self.file_indexed_at.values().copied().max()
    }

    /// Find the `top_k` files whose embedding is most similar (cosine) to `query_vec`.
    ///
    /// Files without an embedding are skipped. The query vector is assumed to be
    /// non-zero; if it is all-zeros the result is undefined.
    pub fn nearest_by_vector(
        &self,
        query_vec: &[f32],
        top_k: usize,
        exclude_uri: Option<&str>,
        filter: Option<&str>,
        min_score: Option<f32>,
    ) -> Vec<crate::query_graph::types::NearestItem> {
        let q_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if q_norm == 0.0 || top_k == 0 {
            return vec![];
        }
        let pat = filter.and_then(|f| glob::Pattern::new(f).ok());
        let threshold = min_score.unwrap_or(f32::NEG_INFINITY);
        let mut scored: Vec<(String, f32)> = self
            .file_embeddings
            .iter()
            .filter(|(uri, _)| exclude_uri.map(|e| e != uri.as_str()).unwrap_or(true))
            .filter(|(uri, _)| match &pat {
                None => true,
                Some(p) => {
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    if p.as_str().contains('/') {
                        p.matches(path)
                    } else {
                        let fname = path.rsplit('/').next().unwrap_or(path);
                        p.matches(fname)
                    }
                }
            })
            .filter_map(|(uri, vec)| {
                if vec.len() != query_vec.len() {
                    return None;
                }
                let dot: f32 = query_vec.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                let v_norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if v_norm == 0.0 {
                    return None;
                }
                let score = dot / (q_norm * v_norm);
                if score < threshold {
                    return None;
                }
                Some((uri.clone(), score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(uri, score)| {
                let embedding_model = self.file_embedding_models.get(&uri).cloned();
                crate::query_graph::types::NearestItem {
                    uri,
                    score,
                    embedding_model,
                }
            })
            .collect()
    }

    /// Compute the embedding centroid (component-wise mean) of `uris`.
    ///
    /// Returns `(vector, included)` where `included` is the number of URIs
    /// that had a cached embedding.  Returns an empty vector when no URI
    /// had an embedding.
    pub fn centroid(&self, uris: &[String]) -> (Vec<f32>, usize) {
        let vecs: Vec<&Vec<f32>> = uris
            .iter()
            .filter_map(|u| self.file_embeddings.get(u))
            .collect();
        let n = vecs.len();
        if n == 0 {
            return (vec![], 0);
        }
        let dim = vecs[0].len();
        let mut result = vec![0.0f32; dim];
        for v in &vecs {
            if v.len() == dim {
                for (r, x) in result.iter_mut().zip(v.iter()) {
                    *r += x;
                }
            }
        }
        let nf = n as f32;
        for r in result.iter_mut() {
            *r /= nf;
        }
        (result, n)
    }

    /// Return `(uri, indexed_at_ms)` pairs for all files under `root` that have
    /// a cached embedding.  Files with no `file_indexed_at` entry get `0`
    /// (treated as stale by callers).
    pub fn file_embeddings_in_root(&self, root: &str) -> Vec<(String, i64)> {
        self.file_embeddings
            .keys()
            .filter(|uri| {
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                path.starts_with(root)
            })
            .map(|uri| {
                let ts = self.file_indexed_at.get(uri).copied().unwrap_or(0);
                (uri.clone(), ts)
            })
            .collect()
    }

    /// Overall index health snapshot.
    ///
    /// Returns `(indexed_files, pending_embedding_files, last_updated_ms)`.
    pub fn index_status(&self) -> (usize, usize, Option<i64>) {
        (
            self.file_inputs.len(),
            self.pending_embedding_count(),
            self.last_updated_ms(),
        )
    }

    /// Per-file status snapshot.
    ///
    /// Returns `(indexed, has_embedding, age_seconds)`.
    pub fn file_status(&self, uri: &str) -> (bool, bool, Option<u64>) {
        let canon = self.canonicalize_uri(uri);
        let uri = canon.as_str();
        let indexed = self.file_inputs.contains_key(uri);
        let has_embedding = self.file_embeddings.contains_key(uri);
        let age_seconds = self.file_indexed_at.get(uri).and_then(|&ts_ms| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            u64::try_from(now_ms.saturating_sub(ts_ms) / 1000).ok()
        });
        (indexed, has_embedding, age_seconds)
    }

    /// Return the `top_k` URIs from `uris` that are most semantically dissimilar from the rest.
    ///
    /// For each URI with a cached embedding compute its leave-one-out mean cosine similarity
    /// to all other URIs in the set. The URIs with the **lowest** mean similarity are returned
    /// first — they are the semantic outliers. URIs without a cached embedding are skipped.
    pub fn outliers(
        &self,
        uris: &[String],
        top_k: usize,
    ) -> Vec<crate::query_graph::types::NearestItem> {
        use crate::query_graph::types::NearestItem;

        let pairs: Vec<(&str, &Vec<f32>)> = uris
            .iter()
            .filter_map(|uri| {
                let v = if uri.starts_with("lip://") {
                    self.get_symbol_embedding(uri)
                } else {
                    self.get_file_embedding(uri)
                };
                v.map(|vec| (uri.as_str(), vec))
            })
            .collect();

        if pairs.is_empty() {
            return vec![];
        }
        if pairs.len() == 1 {
            let uri = pairs[0].0.to_owned();
            let embedding_model = if uri.starts_with("lip://") {
                self.symbol_embedding_models.get(&uri).cloned()
            } else {
                self.file_embedding_models.get(&uri).cloned()
            };
            return vec![NearestItem {
                uri,
                score: 0.0,
                embedding_model,
            }];
        }

        let norms: Vec<f32> = pairs
            .iter()
            .map(|(_, v)| v.iter().map(|x| x * x).sum::<f32>().sqrt())
            .collect();

        let mut scores: Vec<(String, f32)> = pairs
            .iter()
            .enumerate()
            .map(|(i, (uri, vi))| {
                if norms[i] == 0.0 {
                    return (uri.to_string(), 0.0_f32);
                }
                let total_sim: f32 = pairs
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .filter_map(|(j, (_, vj))| {
                        if vj.len() != vi.len() || norms[j] == 0.0 {
                            return None;
                        }
                        let dot: f32 = vi.iter().zip(vj.iter()).map(|(a, b)| a * b).sum();
                        Some(dot / (norms[i] * norms[j]))
                    })
                    .sum();
                let mean_sim = total_sim / (pairs.len() - 1) as f32;
                (uri.to_string(), mean_sim)
            })
            .collect();

        // Ascending: lowest mean similarity = most outlier-like.
        scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scores
            .into_iter()
            .take(top_k)
            .map(|(uri, score)| {
                let embedding_model = if uri.starts_with("lip://") {
                    self.symbol_embedding_models.get(&uri).cloned()
                } else {
                    self.file_embedding_models.get(&uri).cloned()
                };
                NearestItem {
                    uri,
                    score,
                    embedding_model,
                }
            })
            .collect()
    }

    /// Compute all pairwise cosine similarities for `uris`.
    ///
    /// URIs without a cached embedding are excluded from the result. Returns the filtered URI
    /// list and a row-major N×N matrix where `matrix[i][j]` = cosine similarity of `uris[i]`
    /// and `uris[j]`. Diagonal entries are `1.0`.
    pub fn similarity_matrix(&self, uris: &[String]) -> (Vec<String>, Vec<Vec<f32>>) {
        let pairs: Vec<(String, &Vec<f32>)> = uris
            .iter()
            .filter_map(|uri| {
                let v = if uri.starts_with("lip://") {
                    self.get_symbol_embedding(uri)
                } else {
                    self.get_file_embedding(uri)
                };
                v.map(|vec| (uri.clone(), vec))
            })
            .collect();

        let n = pairs.len();
        let norms: Vec<f32> = pairs
            .iter()
            .map(|(_, v)| v.iter().map(|x| x * x).sum::<f32>().sqrt())
            .collect();

        let mut matrix = vec![vec![0.0f32; n]; n];
        for i in 0..n {
            matrix[i][i] = 1.0;
            for j in (i + 1)..n {
                let (_, vi) = &pairs[i];
                let (_, vj) = &pairs[j];
                if vi.len() != vj.len() || norms[i] == 0.0 || norms[j] == 0.0 {
                    continue;
                }
                let dot: f32 = vi.iter().zip(vj.iter()).map(|(a, b)| a * b).sum();
                let sim = dot / (norms[i] * norms[j]);
                matrix[i][j] = sim;
                matrix[j][i] = sim;
            }
        }

        let result_uris = pairs.into_iter().map(|(uri, _)| uri).collect();
        (result_uris, matrix)
    }

    /// Report embedding coverage for files whose URI starts with `root`.
    ///
    /// `root` is matched as a path prefix. Both bare paths (`/project/src`) and
    /// `file://` URIs are accepted — bare paths are normalised to `file:///path`.
    ///
    /// Returns `(total_files, embedded_files, per_directory_breakdown)`.
    /// The per-directory list is sorted by directory URI.
    pub fn coverage(
        &self,
        root: &str,
    ) -> (
        usize,
        usize,
        Vec<crate::query_graph::types::DirectoryCoverage>,
    ) {
        use crate::query_graph::types::DirectoryCoverage;
        use std::collections::HashMap;

        let prefix = if root.starts_with("file://") {
            root.to_owned()
        } else {
            format!("file://{root}")
        };

        let mut by_dir: HashMap<String, (usize, usize)> = HashMap::new();

        for uri in self.file_inputs.keys() {
            if !uri.starts_with(&prefix) {
                continue;
            }
            let has_embedding = self.file_embeddings.contains_key(uri.as_str());
            let dir = uri[..uri.rfind('/').unwrap_or(uri.len())].to_owned();
            let entry = by_dir.entry(dir).or_default();
            entry.0 += 1;
            if has_embedding {
                entry.1 += 1;
            }
        }

        let total_files: usize = by_dir.values().map(|(t, _)| t).sum();
        let embedded_files: usize = by_dir.values().map(|(_, e)| e).sum();
        let mut dirs: Vec<DirectoryCoverage> = by_dir
            .into_iter()
            .map(|(directory, (total, embedded))| DirectoryCoverage {
                directory,
                total_files: total,
                embedded_files: embedded,
            })
            .collect();
        dirs.sort_by(|a, b| a.directory.cmp(&b.directory));

        (total_files, embedded_files, dirs)
    }

    /// Compute per-file novelty scores for `uris` relative to the rest of the indexed files.
    ///
    /// For each URI that has a cached embedding, finds its nearest neighbour *outside* `uris`
    /// in the file embedding store.  The novelty score is `1 − nearest_similarity`.
    /// Returns `(mean_score, per_file_items)` sorted by descending novelty.
    pub fn novelty_scores(
        &self,
        uris: &[String],
    ) -> (f32, Vec<crate::query_graph::types::NoveltyItem>) {
        use crate::query_graph::types::NoveltyItem;
        use std::collections::HashSet;

        let input_set: HashSet<&str> = uris.iter().map(String::as_str).collect();

        let mut items: Vec<NoveltyItem> = uris
            .iter()
            .filter_map(|uri| {
                let qv = self.get_file_embedding(uri)?;
                let q_norm: f32 = qv.iter().map(|x| x * x).sum::<f32>().sqrt();
                if q_norm == 0.0 {
                    return None;
                }
                let best = self
                    .file_embeddings
                    .iter()
                    .filter(|(u, _)| !input_set.contains(u.as_str()))
                    .filter_map(|(u, v)| {
                        if v.len() != qv.len() {
                            return None;
                        }
                        let vn: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if vn == 0.0 {
                            return None;
                        }
                        let dot: f32 = qv.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
                        Some((u.clone(), dot / (q_norm * vn)))
                    })
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

                Some(NoveltyItem {
                    uri: uri.clone(),
                    score: best.as_ref().map(|(_, s)| 1.0 - s).unwrap_or(1.0),
                    nearest_existing: best.map(|(u, _)| u),
                })
            })
            .collect();

        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mean = if items.is_empty() {
            0.0
        } else {
            items.iter().map(|i| i.score).sum::<f32>() / items.len() as f32
        };

        (mean, items)
    }

    /// Extract the domain vocabulary most semantically central to a set of files.
    ///
    /// Computes the centroid of the input files' embeddings, then scores each symbol
    /// defined in those files by its symbol embedding's similarity to that centroid.
    /// Returns the `top_k` most central terms (symbol display names), deduplicated.
    ///
    /// Requires symbol embeddings — call `EmbeddingBatch` with `lip://` URIs first.
    pub fn extract_terminology(
        &mut self,
        uris: &[String],
        top_k: usize,
    ) -> Vec<crate::query_graph::types::TermItem> {
        use crate::query_graph::types::TermItem;
        use std::collections::HashSet;

        // Collect file embeddings for input URIs.
        let file_vecs: Vec<(String, Vec<f32>)> = uris
            .iter()
            .filter_map(|uri| {
                self.get_file_embedding(uri)
                    .cloned()
                    .map(|v| (uri.clone(), v))
            })
            .collect();

        if file_vecs.is_empty() {
            return vec![];
        }

        let dim = file_vecs[0].1.len();

        // Compute centroid.
        let mut centroid = vec![0.0f32; dim];
        let mut count = 0usize;
        for (_, v) in &file_vecs {
            if v.len() != dim {
                continue;
            }
            for (c, x) in centroid.iter_mut().zip(v.iter()) {
                *c += x;
            }
            count += 1;
        }
        if count == 0 {
            return vec![];
        }
        for c in centroid.iter_mut() {
            *c /= count as f32;
        }
        let c_norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
        if c_norm == 0.0 {
            return vec![];
        }

        let mut scored: Vec<(String, f32, String)> = Vec::new(); // (term, score, source_uri)
        let mut seen: HashSet<String> = HashSet::new();

        for uri in uris {
            let syms = self.file_symbols(uri).to_vec();
            for sym in &syms {
                if seen.contains(&sym.display_name) {
                    continue;
                }
                if let Some(sv) = self.symbol_embeddings.get(&sym.uri) {
                    if sv.len() != dim {
                        continue;
                    }
                    let sn: f32 = sv.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if sn == 0.0 {
                        continue;
                    }
                    let dot: f32 = centroid.iter().zip(sv.iter()).map(|(a, b)| a * b).sum();
                    let sim = dot / (c_norm * sn);
                    seen.insert(sym.display_name.clone());
                    scored.push((sym.display_name.clone(), sim, uri.clone()));
                }
            }
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
            .into_iter()
            .map(|(term, score, source_uri)| TermItem {
                term,
                score,
                source_uri,
            })
            .collect()
    }

    /// Find `OwnedSymbolInfo` for a given symbol URI across all tracked files and mounted slices.
    pub fn symbol_by_uri(&mut self, symbol_uri: &str) -> Option<OwnedSymbolInfo> {
        let canon = self.canonicalize_uri(symbol_uri);
        let symbol_uri = canon.as_str();
        // Fast path: check mounted slice symbols first (O(1)).
        if let Some(sym) = self.mounted_symbols.get(symbol_uri) {
            return Some(sym.clone());
        }
        let uris = self.tracked_uris();
        for uri in &uris {
            let syms = self.file_symbols(&uri.clone());
            if let Some(sym) = syms.iter().find(|s| s.uri == symbol_uri) {
                return Some(sym.clone());
            }
        }
        None
    }

    /// Symbols that are defined but never referenced within the tracked workspace.
    ///
    /// Only considers `Role::Definition` occurrences in the definition index and
    /// cross-references them against all `Role::Reference` occurrences. Symbols
    /// with no reference occurrence are considered dead for the current workspace.
    pub fn dead_symbols(&mut self, limit: Option<usize>) -> Vec<OwnedSymbolInfo> {
        let uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let referenced: HashSet<String> = uris
            .iter()
            .flat_map(|u| {
                self.file_occurrences(u)
                    .iter()
                    .filter(|o| o.role == Role::Reference)
                    .map(|o| o.symbol_uri.clone())
                    .collect::<Vec<_>>()
            })
            .collect();

        let cap = limit.unwrap_or(usize::MAX);
        let mut result = vec![];
        'outer: for uri in &uris {
            for sym in self.file_symbols(uri).iter() {
                if !referenced.contains(&sym.uri) {
                    result.push(sym.clone());
                    if result.len() >= cap {
                        break 'outer;
                    }
                }
            }
        }
        result
    }

    // ── Annotations ───────────────────────────────────────────────────────

    /// Set (or overwrite) an annotation on a symbol. Annotations survive file upserts/removes.
    pub fn annotation_set(&mut self, entry: OwnedAnnotationEntry) {
        self.annotations
            .entry(entry.symbol_uri.clone())
            .or_default()
            .insert(entry.key.clone(), entry);
    }

    /// Get the annotation for `(symbol_uri, key)`, if present and not expired.
    pub fn annotation_get(&self, symbol_uri: &str, key: &str) -> Option<&OwnedAnnotationEntry> {
        let entry = self.annotations.get(symbol_uri)?.get(key)?;
        if is_expired(entry) {
            None
        } else {
            Some(entry)
        }
    }

    /// List all non-expired annotations for `symbol_uri`.
    pub fn annotation_list(&self, symbol_uri: &str) -> Vec<OwnedAnnotationEntry> {
        self.annotations
            .get(symbol_uri)
            .map(|m| m.values().filter(|e| !is_expired(e)).cloned().collect())
            .unwrap_or_default()
    }

    /// Remove all annotations whose `expires_ms` has passed.
    ///
    /// Called periodically by the daemon (e.g. on journal compaction) to keep
    /// the in-memory annotation table from growing unboundedly.
    pub fn purge_expired_annotations(&mut self) -> usize {
        let mut removed = 0usize;
        for sym_map in self.annotations.values_mut() {
            let before = sym_map.len();
            sym_map.retain(|_, e| !is_expired(e));
            removed += before - sym_map.len();
        }
        // Remove empty inner maps to avoid accumulating empty HashMaps.
        self.annotations.retain(|_, m| !m.is_empty());
        removed
    }

    /// All non-expired annotations whose key starts with `prefix`, across every
    /// symbol URI. Pass `""` to return all annotations workspace-wide.
    ///
    /// Useful for finding all `lip:fragile` symbols, all `agent:note` entries, etc.
    pub fn annotations_by_key_prefix(&self, prefix: &str) -> Vec<OwnedAnnotationEntry> {
        self.annotations
            .values()
            .flat_map(|m| m.values())
            .filter(|e| !is_expired(e) && e.key.starts_with(prefix))
            .cloned()
            .collect()
    }

    // ── Slice mounting ────────────────────────────────────────────────────

    /// Mount a pre-built dependency slice into the database.
    ///
    /// All symbols in the slice are inserted into the in-memory symbol store
    /// at Tier 3 confidence (score=100). Definitions are registered in
    /// `def_index` and `name_to_symbols` so blast-radius and symbol search
    /// queries can resolve them cross-file.
    ///
    /// Mounting is idempotent: re-mounting the same package key overwrites
    /// all prior symbols from that package rather than duplicating them.
    pub fn mount_slice(&mut self, slice: &OwnedDependencySlice) {
        let pkg_key = format!("{}/{}@{}", slice.manager, slice.package_name, slice.version);

        // Remove stale mounted symbols for this package so re-mount is idempotent.
        if self.mounted_packages.contains_key(&pkg_key) {
            self.mounted_symbols.retain(|uri, _| {
                !uri.starts_with(&format!("lip://{}/{}", slice.manager, slice.package_name))
            });
            self.def_index.retain(|uri, (file_uri, _)| {
                let is_slice_sym =
                    uri.starts_with(&format!("lip://{}/{}", slice.manager, slice.package_name));
                let is_slice_file = file_uri
                    .starts_with(&format!("lip://{}/{}", slice.manager, slice.package_name));
                !(is_slice_sym || is_slice_file)
            });
            self.name_to_symbols.values_mut().for_each(|uris| {
                uris.retain(|u| {
                    !u.starts_with(&format!("lip://{}/{}", slice.manager, slice.package_name))
                });
            });
            self.name_to_symbols.retain(|_, uris| !uris.is_empty());
        }

        // Insert symbols, then register definitions.
        for sym in &slice.symbols {
            let mut sym = sym.clone();
            sym.confidence_score = 100; // Tier 3
            let name = extract_name(&sym.uri).to_owned();
            // Synthetic file URI: everything up to the # fragment.
            let file_uri = sym
                .uri
                .find('#')
                .map(|i| sym.uri[..i].to_owned())
                .unwrap_or_else(|| sym.uri.clone());
            self.def_index.insert(
                sym.uri.clone(),
                (
                    file_uri,
                    OwnedRange {
                        start_line: 0,
                        start_char: 0,
                        end_line: 0,
                        end_char: 0,
                    },
                ),
            );
            if !name.is_empty() {
                self.name_to_symbols
                    .entry(name)
                    .or_default()
                    .push(sym.uri.clone());
            }
            self.mounted_symbols.insert(sym.uri.clone(), sym);
        }

        self.mounted_packages.insert(
            pkg_key,
            (
                slice.manager.clone(),
                slice.package_name.clone(),
                slice.version.clone(),
            ),
        );
    }

    /// Return the number of mounted packages.
    pub fn mounted_package_count(&self) -> usize {
        self.mounted_packages.len()
    }

    /// All annotations across every symbol URI — used by journal compaction.
    pub fn all_annotations(&self) -> Vec<OwnedAnnotationEntry> {
        self.annotations
            .values()
            .flat_map(|m| m.values().filter(|e| !is_expired(e)).cloned())
            .collect()
    }

    /// Symbol search across all tracked files and mounted slices.
    pub fn workspace_symbols(&mut self, query: &str, limit: usize) -> Vec<OwnedSymbolInfo> {
        let (symbols, _) = self.workspace_symbols_ranked(query, limit, None, None, None);
        symbols
    }

    /// Filtered + ranked workspace symbol search (v2.3 Feature #5).
    ///
    /// - `kind_filter`: if `Some`, only symbols whose kind is in the slice pass.
    /// - `scope`: if `Some`, only symbols whose def-file URI starts with the prefix.
    /// - `modifier_filter`: if `Some`, symbols must carry at least one modifier
    ///   from the slice.
    /// - Ranking tiers: `Exact` (case-sensitive equality) = 1.0;
    ///   case-insensitive prefix = 0.8; case-insensitive substring = 0.5.
    ///   An empty query is treated as "match all" with score 0.2 (no ranking
    ///   intent) and produces an empty `ranked` list.
    ///
    /// The two returned vecs are parallel: `ranked[i]` describes `symbols[i]`.
    /// When `query` is empty, `ranked` is empty (pre-v2.3 callers' behavior).
    pub fn workspace_symbols_ranked(
        &mut self,
        query: &str,
        limit: usize,
        kind_filter: Option<&[crate::schema::SymbolKind]>,
        scope: Option<&str>,
        modifier_filter: Option<&[String]>,
    ) -> (
        Vec<OwnedSymbolInfo>,
        Vec<crate::query_graph::types::RankedSymbol>,
    ) {
        use crate::query_graph::types::{MatchType, RankedSymbol};

        let q_lower = query.to_lowercase();
        let has_query = !query.is_empty();

        let passes_filters = |sym: &OwnedSymbolInfo, def_file: Option<&str>| -> bool {
            if let Some(kinds) = kind_filter {
                if !kinds.contains(&sym.kind) {
                    return false;
                }
            }
            if let Some(prefix) = scope {
                let file = def_file.unwrap_or("");
                if !file.starts_with(prefix) {
                    return false;
                }
            }
            if let Some(mods) = modifier_filter {
                if !mods.iter().any(|m| sym.modifiers.iter().any(|sm| sm == m)) {
                    return false;
                }
            }
            true
        };

        let classify = |name: &str| -> Option<(f32, MatchType)> {
            if !has_query {
                return Some((0.2, MatchType::Fuzzy));
            }
            if name == query {
                Some((1.0, MatchType::Exact))
            } else if name.to_lowercase().starts_with(&q_lower) {
                Some((0.8, MatchType::Prefix))
            } else if name.to_lowercase().contains(&q_lower) {
                Some((0.5, MatchType::Fuzzy))
            } else {
                None
            }
        };

        #[derive(Clone)]
        struct Hit {
            sym: OwnedSymbolInfo,
            score: f32,
            match_type: MatchType,
        }

        let uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let mut hits: Vec<Hit> = Vec::new();
        for uri in &uris {
            for sym in self.file_symbols(uri).iter() {
                if !passes_filters(sym, Some(uri.as_str())) {
                    continue;
                }
                if let Some((score, match_type)) = classify(&sym.display_name) {
                    hits.push(Hit {
                        sym: sym.clone(),
                        score,
                        match_type,
                    });
                }
            }
        }
        for sym in self.mounted_symbols.values() {
            let def_file = self.def_index.get(&sym.uri).map(|(f, _)| f.as_str());
            if !passes_filters(sym, def_file) {
                continue;
            }
            if let Some((score, match_type)) = classify(&sym.display_name) {
                hits.push(Hit {
                    sym: sym.clone(),
                    score,
                    match_type,
                });
            }
        }

        // Sort by score desc, then by display_name asc as a stable tiebreaker.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.sym.display_name.cmp(&b.sym.display_name))
        });
        hits.truncate(limit);

        let emit_ranked = has_query;
        let mut symbols = Vec::with_capacity(hits.len());
        let mut ranked = Vec::with_capacity(if emit_ranked { hits.len() } else { 0 });
        for h in hits {
            if emit_ranked {
                ranked.push(RankedSymbol {
                    symbol_uri: h.sym.uri.clone(),
                    score: h.score,
                    match_type: h.match_type,
                });
            }
            symbols.push(h.sym);
        }
        (symbols, ranked)
    }

    /// Trigram fuzzy search across all tracked symbols and mounted slices.
    ///
    /// Scores each symbol by Jaccard similarity of 3-char windows between
    /// `query` and the symbol name (and optionally its documentation at 0.6×
    /// weight). Returns up to `limit` results with score ≥ 0.2, sorted
    /// descending by score.
    pub fn similar_symbols(&mut self, query: &str, limit: usize) -> Vec<SimilarSymbol> {
        let uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let mut hits: Vec<SimilarSymbol> = Vec::new();

        let score_sym = |sym: &OwnedSymbolInfo| -> f32 {
            let name_score = trigram_similarity(query, &sym.display_name);
            let doc_score = sym
                .documentation
                .as_deref()
                .map(|d| trigram_similarity(query, d) * 0.6)
                .unwrap_or(0.0);
            name_score.max(doc_score)
        };

        for uri in &uris {
            for sym in self.file_symbols(uri).iter() {
                let score = score_sym(sym);
                if score >= 0.2 {
                    hits.push(SimilarSymbol {
                        uri: sym.uri.clone(),
                        name: sym.display_name.clone(),
                        kind: format!("{:?}", sym.kind).to_lowercase(),
                        score,
                        doc: sym.documentation.clone(),
                        confidence: sym.confidence_score,
                    });
                }
            }
        }

        for sym in self.mounted_symbols.values() {
            let score = score_sym(sym);
            if score >= 0.2 {
                hits.push(SimilarSymbol {
                    uri: sym.uri.clone(),
                    name: sym.display_name.clone(),
                    kind: format!("{:?}", sym.kind).to_lowercase(),
                    score,
                    doc: sym.documentation.clone(),
                    confidence: sym.confidence_score,
                });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        hits
    }

    // ── Private ───────────────────────────────────────────────────────────

    fn compute_symbols(&self, uri: &str) -> Arc<Vec<OwnedSymbolInfo>> {
        let Some(file) = self.file_inputs.get(uri) else {
            return Arc::new(vec![]);
        };
        let language = Language::detect(uri, &file.language);
        Arc::new(Tier1Indexer::new().symbols_for_source(uri, &file.text, language))
    }

    fn compute_occurrences(&self, uri: &str) -> Arc<Vec<OwnedOccurrence>> {
        let Some(file) = self.file_inputs.get(uri) else {
            return Arc::new(vec![]);
        };
        let language = Language::detect(uri, &file.language);
        Arc::new(Tier1Indexer::new().occurrences_for_source(uri, &file.text, language))
    }
}

impl Default for LipDatabase {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ReferenceKind, SymbolKind};

    fn make_rust_file(content: &str) -> (String, String, String) {
        (
            "lip://npm/pkg@1.0.0/src/lib.rs".to_owned(),
            content.to_owned(),
            "rust".to_owned(),
        )
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    #[test]
    fn normalize_callee_name_strips_scip_descriptor_suffixes() {
        // SCIP method / function descriptor form.
        assert_eq!(normalize_callee_name("SearchSymbols()."), "SearchSymbols");
        assert_eq!(normalize_callee_name("foo()"), "foo");
        // SCIP term form.
        assert_eq!(normalize_callee_name("MyField."), "MyField");
        // SCIP type form (trailing `#` already consumed by extract_name, but
        // defensively handle residual non-identifier trailers).
        assert_eq!(normalize_callee_name("Foo:"), "Foo");
        // Plain tier-1 identifier — unchanged.
        assert_eq!(normalize_callee_name("plain_name"), "plain_name");
        // Snake-case / digits preserved.
        assert_eq!(normalize_callee_name("do_thing_2()."), "do_thing_2");
        // Empty.
        assert_eq!(normalize_callee_name(""), "");
    }

    // ── Revision ──────────────────────────────────────────────────────────

    #[test]
    fn revision_starts_at_zero() {
        let db = LipDatabase::new();
        assert_eq!(db.current_revision(), 0);
    }

    #[test]
    fn revision_increments_on_upsert() {
        let mut db = LipDatabase::new();
        let (uri, text, lang) = make_rust_file("fn a() {}");
        db.upsert_file(uri.clone(), text, lang);
        assert_eq!(db.current_revision(), 1);
        db.upsert_file(uri, "fn b() {}".to_owned(), "rust".to_owned());
        assert_eq!(db.current_revision(), 2);
    }

    #[test]
    fn revision_increments_on_remove() {
        let mut db = LipDatabase::new();
        let (uri, text, lang) = make_rust_file("fn a() {}");
        db.upsert_file(uri.clone(), text, lang);
        db.remove_file(&uri);
        assert_eq!(db.current_revision(), 2);
    }

    // ── Cache invalidation ─────────────────────────────────────────────────

    #[test]
    fn symbols_cached_on_second_call() {
        let mut db = LipDatabase::new();
        let (uri, text, lang) = make_rust_file("pub fn foo() {}");
        db.upsert_file(uri.clone(), text, lang);

        let first = db.file_symbols(&uri);
        let second = db.file_symbols(&uri);
        // Exact same Arc pointer — no recomputation.
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn cache_invalidated_on_upsert() {
        let mut db = LipDatabase::new();
        let (uri, text, lang) = make_rust_file("pub fn foo() {}");
        db.upsert_file(uri.clone(), text, lang.clone());

        let first = db.file_symbols(&uri);
        db.upsert_file(uri.clone(), "pub fn bar() {}".to_owned(), lang);
        let second = db.file_symbols(&uri);
        // Different content → different Arc.
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn remove_file_returns_empty() {
        let mut db = LipDatabase::new();
        let (uri, text, lang) = make_rust_file("pub fn foo() {}");
        db.upsert_file(uri.clone(), text, lang);
        db.remove_file(&uri);

        assert!(db.file_symbols(&uri).is_empty());
        assert!(db.file_occurrences(&uri).is_empty());
        assert!(db.tracked_uris().is_empty());
    }

    // ── Early-cutoff ───────────────────────────────────────────────────────

    #[test]
    fn api_surface_early_cutoff_same_arc_on_same_content() {
        let mut db = LipDatabase::new();
        // Two identical upserts (same text) should yield the same Arc after
        // the second upsert because the API surface hash is unchanged.
        let (uri, text, lang) = make_rust_file("pub fn public_api() {}");
        db.upsert_file(uri.clone(), text.clone(), lang.clone());

        let first = db.file_api_surface(&uri);
        // Upsert the exact same text again.
        db.upsert_file(uri.clone(), text, lang);
        let second = db.file_api_surface(&uri);

        // Early-cutoff: same content_hash → same Arc.
        assert_eq!(first.content_hash, second.content_hash);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn api_surface_changes_when_content_changes() {
        let mut db = LipDatabase::new();
        let (uri, _, lang) = make_rust_file("");
        db.upsert_file(uri.clone(), "pub fn v1() {}".to_owned(), lang.clone());
        let first = db.file_api_surface(&uri);

        db.upsert_file(uri.clone(), "pub fn v2() {}".to_owned(), lang);
        let second = db.file_api_surface(&uri);

        assert_ne!(first.content_hash, second.content_hash);
    }

    // ── tracked_uris ──────────────────────────────────────────────────────

    #[test]
    fn tracked_uris_reflects_inserts_and_removes() {
        let mut db = LipDatabase::new();
        assert!(db.tracked_uris().is_empty());

        db.upsert_file(
            "lip://s/p@1/a.rs".to_owned(),
            String::new(),
            "rust".to_owned(),
        );
        db.upsert_file(
            "lip://s/p@1/b.rs".to_owned(),
            String::new(),
            "rust".to_owned(),
        );
        assert_eq!(db.tracked_uris().len(), 2);

        db.remove_file("lip://s/p@1/a.rs");
        assert_eq!(db.tracked_uris().len(), 1);
        assert_eq!(db.tracked_uris()[0], "lip://s/p@1/b.rs");
    }

    // ── workspace_symbols ────────────────────────────────────────────────

    #[test]
    fn workspace_symbols_empty_query_returns_up_to_limit() {
        let mut db = LipDatabase::new();
        // Empty source → no symbols; just verify it doesn't panic.
        db.upsert_file(
            "lip://s/p@1/a.rs".to_owned(),
            String::new(),
            "rust".to_owned(),
        );
        let syms = db.workspace_symbols("", 10);
        assert!(syms.len() <= 10);
    }

    // ── blast_radius ──────────────────────────────────────────────────────

    #[test]
    fn blast_radius_unknown_symbol_returns_zeros() {
        let mut db = LipDatabase::new();
        let result = db.blast_radius_for("lip://s/p@1/x.rs#ghost");
        assert_eq!(result.direct_dependents, 0);
        assert_eq!(result.transitive_dependents, 0);
        assert!(result.affected_files.is_empty());
    }

    // ── symbol_at_position ────────────────────────────────────────────────

    #[test]
    fn symbol_at_position_hits_occurrence_range() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/pos.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn greet() {}".to_owned(),
            "rust".to_owned(),
        );

        // Use the actual parsed occurrences so the test is not fragile to
        // tree-sitter range changes — pick the first occurrence and query at
        // its exact start position.
        let occs = db.file_occurrences(&uri);
        assert!(!occs.is_empty(), "expected at least one occurrence");
        let first = &occs[0];
        let result = db.symbol_at_position(&uri, first.range.start_line, first.range.start_char);
        assert_eq!(result.as_deref(), Some(first.symbol_uri.as_str()));
    }

    #[test]
    fn symbol_at_position_misses_outside_range() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/miss.rs".to_owned();
        db.upsert_file(uri.clone(), "pub fn f() {}".to_owned(), "rust".to_owned());
        // Line 9999 is never in any occurrence range.
        assert!(db.symbol_at_position(&uri, 9999, 0).is_none());
    }

    // ── symbol_definition_location ────────────────────────────────────────

    #[test]
    fn symbol_definition_location_found_after_upsert() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/def.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn defined_fn() {}".to_owned(),
            "rust".to_owned(),
        );

        // Find a definition occurrence to get a real symbol URI.
        let occs = db.file_occurrences(&uri);
        let def_occ = occs.iter().find(|o| o.role == Role::Definition);
        let Some(def_occ) = def_occ else {
            // tree-sitter produced no definition occurrences — skip rather than fail.
            return;
        };

        let loc = db.symbol_definition_location(&def_occ.symbol_uri);
        assert!(loc.is_some(), "expected definition location in def_index");
        let (loc_uri, loc_range) = loc.unwrap();
        assert_eq!(loc_uri, uri);
        assert_eq!(loc_range, def_occ.range);
    }

    #[test]
    fn symbol_definition_location_cleared_on_remove() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/clear.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn gone() {}".to_owned(),
            "rust".to_owned(),
        );

        let occs = db.file_occurrences(&uri);
        let def_occ = occs.iter().find(|o| o.role == Role::Definition);
        let Some(def_occ) = def_occ else {
            return;
        };
        let sym_uri = def_occ.symbol_uri.clone();

        assert!(db.symbol_definition_location(&sym_uri).is_some());
        db.remove_file(&uri);
        assert!(
            db.symbol_definition_location(&sym_uri).is_none(),
            "def_index should be pruned on remove_file"
        );
    }

    // ── dead_symbols ──────────────────────────────────────────────────────

    #[test]
    fn dead_symbols_detects_unreferenced_symbol() {
        let mut db = LipDatabase::new();
        // Two files: lib defines `helper`, main never references it.
        db.upsert_file(
            "lip://s/p@1/lib.rs".to_owned(),
            "pub fn helper() {}".to_owned(),
            "rust".to_owned(),
        );
        db.upsert_file(
            "lip://s/p@1/main.rs".to_owned(),
            "pub fn main() {}".to_owned(),
            "rust".to_owned(),
        );

        let dead = db.dead_symbols(None);
        // All symbols are unreferenced (no cross-file calls in these snippets).
        assert!(!dead.is_empty(), "expected dead symbols in isolated files");
    }

    #[test]
    fn dead_symbols_limit_respected() {
        let mut db = LipDatabase::new();
        db.upsert_file(
            "lip://s/p@1/a.rs".to_owned(),
            "pub fn a1() {} pub fn a2() {} pub fn a3() {}".to_owned(),
            "rust".to_owned(),
        );
        let dead = db.dead_symbols(Some(1));
        assert_eq!(dead.len(), 1);
    }

    #[test]
    fn dead_symbols_empty_when_no_files() {
        let mut db = LipDatabase::new();
        assert!(db.dead_symbols(None).is_empty());
    }

    // ── annotations ──────────────────────────────────────────────────────

    #[test]
    fn annotation_set_get_roundtrip() {
        use crate::schema::OwnedAnnotationEntry;
        let mut db = LipDatabase::new();
        let entry = OwnedAnnotationEntry {
            symbol_uri: "lip://s/p@1/f.rs#foo".to_owned(),
            key: "team:owner".to_owned(),
            value: "platform".to_owned(),
            author_id: "human:alice".to_owned(),
            confidence: 100,
            timestamp_ms: 0,
            expires_ms: 0,
        };
        db.annotation_set(entry.clone());

        let got = db.annotation_get("lip://s/p@1/f.rs#foo", "team:owner");
        assert!(got.is_some());
        assert_eq!(got.unwrap().value, "platform");
    }

    #[test]
    fn annotation_get_missing_returns_none() {
        let db = LipDatabase::new();
        assert!(db
            .annotation_get("lip://s/p@1/f.rs#no_sym", "key")
            .is_none());
    }

    #[test]
    fn annotation_list_returns_all_keys_for_symbol() {
        use crate::schema::OwnedAnnotationEntry;
        let mut db = LipDatabase::new();
        let sym = "lip://s/p@1/f.rs#bar";
        for key in ["k1", "k2", "k3"] {
            db.annotation_set(OwnedAnnotationEntry {
                symbol_uri: sym.to_owned(),
                key: key.to_owned(),
                value: key.to_owned(),
                author_id: "human:test".to_owned(),
                confidence: 100,
                timestamp_ms: 0,
                expires_ms: 0,
            });
        }
        let list = db.annotation_list(sym);
        assert_eq!(list.len(), 3);
        let keys: Vec<_> = list.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"k1") && keys.contains(&"k2") && keys.contains(&"k3"));
    }

    #[test]
    fn annotation_survives_file_upsert_and_remove() {
        use crate::schema::OwnedAnnotationEntry;
        let mut db = LipDatabase::new();
        let file_uri = "lip://s/p@1/f.rs".to_owned();
        let sym_uri = format!("{file_uri}#foo");

        db.upsert_file(
            file_uri.clone(),
            "pub fn foo() {}".to_owned(),
            "rust".to_owned(),
        );
        db.annotation_set(OwnedAnnotationEntry {
            symbol_uri: sym_uri.clone(),
            key: "note".to_owned(),
            value: "fragile".to_owned(),
            author_id: "human:test".to_owned(),
            confidence: 100,
            timestamp_ms: 0,
            expires_ms: 0,
        });

        // Re-upsert and then remove the file — annotation must survive both.
        db.upsert_file(
            file_uri.clone(),
            "pub fn foo() { /* changed */ }".to_owned(),
            "rust".to_owned(),
        );
        assert_eq!(
            db.annotation_get(&sym_uri, "note")
                .map(|e| e.value.as_str()),
            Some("fragile")
        );

        db.remove_file(&file_uri);
        assert_eq!(
            db.annotation_get(&sym_uri, "note")
                .map(|e| e.value.as_str()),
            Some("fragile"),
            "annotation must survive file removal"
        );
    }

    // ── upgrade_file_symbols ──────────────────────────────────────────────

    #[test]
    fn upgrade_file_symbols_raises_confidence() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/up.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn upgradable() {}".to_owned(),
            "rust".to_owned(),
        );

        let syms_before = db.file_symbols(&uri);
        // Tier 1 should give confidence 30.
        assert!(
            syms_before.iter().all(|s| s.confidence_score == 30),
            "Tier 1 symbols should start at confidence 30"
        );

        // Simulate Tier 2 upgrade.
        let upgrades: Vec<_> = syms_before
            .iter()
            .map(|s| {
                let mut up = s.clone();
                up.confidence_score = 90;
                up.signature = Some("fn upgradable()".to_owned());
                up
            })
            .collect();

        db.upgrade_file_symbols(&uri, &upgrades);

        let syms_after = db.file_symbols(&uri);
        assert!(
            syms_after.iter().all(|s| s.confidence_score == 90),
            "symbols should be upgraded to confidence 90"
        );
        assert!(
            syms_after.iter().any(|s| s.signature.is_some()),
            "upgraded symbols should carry signatures"
        );
    }

    #[test]
    fn upgrade_file_symbols_respects_confidence_floor() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/floor.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn floored() {}".to_owned(),
            "rust".to_owned(),
        );

        // Manually upgrade to 90 first (simulating a SCIP push).
        let syms = db.file_symbols(&uri);
        let scip_upgrades: Vec<_> = syms
            .iter()
            .map(|s| {
                let mut up = s.clone();
                up.confidence_score = 90;
                up.signature = Some("pub fn floored()".to_owned());
                up
            })
            .collect();
        db.upgrade_file_symbols(&uri, &scip_upgrades);

        // Now simulate a racing Tier 2 job at lower confidence (70).
        let syms2 = db.file_symbols(&uri);
        let tier2_upgrades: Vec<_> = syms2
            .iter()
            .map(|s| {
                let mut up = s.clone();
                up.confidence_score = 70;
                up.signature = Some("fn floored() — stale".to_owned());
                up
            })
            .collect();
        db.upgrade_file_symbols(&uri, &tier2_upgrades);

        let final_syms = db.file_symbols(&uri);
        assert!(
            final_syms.iter().all(|s| s.confidence_score == 90),
            "confidence floor must block downgrade from 90 to 70"
        );
        assert!(
            final_syms
                .iter()
                .all(|s| s.signature.as_deref() != Some("fn floored() — stale")),
            "stale signature from lower-confidence upgrade must not overwrite"
        );
    }

    #[test]
    fn upgrade_file_symbols_noop_on_unknown_uri() {
        // Should not panic when the uri isn't in the db.
        let mut db = LipDatabase::new();
        db.upgrade_file_symbols("lip://s/p@1/ghost.rs", &[]);
    }

    // ── reverse_deps ─────────────────────────────────────────────────────

    // ── impl block methods ────────────────────────────────────────────────

    #[test]
    fn impl_methods_extracted_as_symbols() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/impl.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            r#"
pub struct Greeter;
impl Greeter {
    pub fn hello(&self) -> &str { "hello" }
    fn private_method(&self) {}
}
"#
            .to_owned(),
            "rust".to_owned(),
        );
        let syms = db.file_symbols(&uri);
        let names: Vec<&str> = syms.iter().map(|s| s.display_name.as_str()).collect();
        assert!(
            names.contains(&"Greeter"),
            "struct should be extracted; got: {names:?}"
        );
        assert!(
            names.contains(&"hello"),
            "pub method should be extracted; got: {names:?}"
        );
        assert!(
            names.contains(&"private_method"),
            "private method should be extracted; got: {names:?}"
        );
    }

    // ── reverse_deps ─────────────────────────────────────────────────────

    #[test]
    fn reverse_deps_empty_for_isolated_file() {
        let mut db = LipDatabase::new();
        db.upsert_file(
            "lip://s/p@1/solo.rs".to_owned(),
            "pub fn solo() {}".to_owned(),
            "rust".to_owned(),
        );
        let deps = db.reverse_deps("lip://s/p@1/solo.rs");
        assert!(deps.is_empty(), "isolated file should have no reverse deps");
    }

    // ── blast_radius CPG augmentation ─────────────────────────────────────
    //
    // Tier 1 URIs are file-local (lip://local/<file>#<name>), so CPG edges
    // only resolve within the same file in v0.1. Cross-file resolution requires
    // a global name→file index (planned for v0.2).

    #[test]
    fn blast_radius_cpg_same_file_caller_included() {
        let mut db = LipDatabase::new();
        // Both helper and main are in the same file; main() calls helper().
        // The CPG edge main→helper should surface in blast_radius_for(helper).
        let uri = "file:///project/main.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn helper() {} pub fn main() { helper(); }".to_owned(),
            "rust".to_owned(),
        );

        let helper_sym = db
            .file_symbols(&uri)
            .iter()
            .find(|s| s.display_name == "helper")
            .map(|s| s.uri.clone())
            .expect("helper symbol not found");

        let result = db.blast_radius_for(&helper_sym);
        // The defining file appears because main (also in that file) calls helper.
        assert!(
            result.affected_files.contains(&uri),
            "blast radius should include the calling file via CPG edges; got {:?}",
            result.affected_files
        );
    }

    #[test]
    fn blast_radius_cpg_cleared_on_remove() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/main.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn helper() {} pub fn main() { helper(); }".to_owned(),
            "rust".to_owned(),
        );

        // Re-upsert with no callers — CPG edge should be removed.
        db.upsert_file(
            uri.clone(),
            "pub fn helper() {}".to_owned(),
            "rust".to_owned(),
        );

        // Re-fetch the helper sym URI (unchanged after re-upsert).
        let helper_sym2 = db
            .file_symbols(&uri)
            .iter()
            .find(|s| s.display_name == "helper")
            .map(|s| s.uri.clone())
            .expect("helper symbol not found after re-upsert");

        // blast_radius_for should now report an empty affected_files because the
        // caller was removed from the CPG index.
        let result = db.blast_radius_for(&helper_sym2);
        assert!(
            result.affected_files.is_empty(),
            "stale CPG edge should be purged on re-upsert; got {:?}",
            result.affected_files
        );
    }

    #[test]
    fn blast_radius_cpg_cross_file_via_name_index() {
        // lib.rs defines `helper`; caller.rs calls `helper`.
        // Tier 1 generates file-local URIs, so the call edge from caller.rs uses
        // lip://local/caller.rs#helper, while the definition in lib.rs has URI
        // lip://local/lib.rs#helper. The callee_name_to_callers index should bridge
        // this gap and include caller.rs in the blast radius for helper.
        let mut db = LipDatabase::new();
        let lib_uri = "file:///project/lib.rs".to_owned();
        let caller_uri = "file:///project/caller.rs".to_owned();

        db.upsert_file(
            lib_uri.clone(),
            "pub fn helper() {}".to_owned(),
            "rust".to_owned(),
        );
        db.upsert_file(
            caller_uri.clone(),
            "pub fn main() { helper(); }".to_owned(),
            "rust".to_owned(),
        );

        let helper_sym = db
            .file_symbols(&lib_uri)
            .iter()
            .find(|s| s.display_name == "helper")
            .map(|s| s.uri.clone())
            .expect("helper not found in lib.rs");

        let result = db.blast_radius_for(&helper_sym);
        assert!(
            result.affected_files.contains(&caller_uri),
            "cross-file CPG: caller.rs should be in blast radius of lib.rs#helper; got {:?}",
            result.affected_files
        );
    }

    #[test]
    fn symbols_by_name_finds_definition() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/api.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn my_func() {}".to_owned(),
            "rust".to_owned(),
        );

        let uris = db.symbols_by_name("my_func");
        assert!(!uris.is_empty(), "name_to_symbols should index my_func");
        assert!(
            uris.iter().any(|u| u.contains("my_func")),
            "returned URIs should contain my_func; got {uris:?}"
        );
    }

    #[test]
    fn symbols_by_name_cleared_on_remove() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/api.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn gone_fn() {}".to_owned(),
            "rust".to_owned(),
        );
        assert!(!db.symbols_by_name("gone_fn").is_empty());
        db.remove_file(&uri);
        assert!(
            db.symbols_by_name("gone_fn").is_empty(),
            "name_to_symbols should be pruned on remove_file"
        );
    }

    // ── Annotation expiry ─────────────────────────────────────────────────────

    fn make_annotation(
        sym: &str,
        key: &str,
        expires_ms: i64,
    ) -> crate::schema::OwnedAnnotationEntry {
        crate::schema::OwnedAnnotationEntry {
            symbol_uri: sym.to_owned(),
            key: key.to_owned(),
            value: "v".to_owned(),
            author_id: "test".to_owned(),
            confidence: 100,
            timestamp_ms: 0,
            expires_ms,
        }
    }

    #[test]
    fn annotation_zero_expires_never_expires() {
        let mut db = LipDatabase::new();
        db.annotation_set(make_annotation("lip://s#f", "k", 0));
        assert!(db.annotation_get("lip://s#f", "k").is_some());
    }

    #[test]
    fn annotation_future_expires_is_visible() {
        let mut db = LipDatabase::new();
        let far_future = i64::MAX;
        db.annotation_set(make_annotation("lip://s#f", "k", far_future));
        assert!(db.annotation_get("lip://s#f", "k").is_some());
    }

    #[test]
    fn annotation_past_expires_is_hidden() {
        let mut db = LipDatabase::new();
        // expires_ms = 1 (1 ms past the Unix epoch) — definitely expired
        db.annotation_set(make_annotation("lip://s#f", "k", 1));
        assert!(
            db.annotation_get("lip://s#f", "k").is_none(),
            "expired annotation should not be returned by get"
        );
        assert!(
            db.annotation_list("lip://s#f").is_empty(),
            "expired annotation should not be returned by list"
        );
    }

    #[test]
    fn purge_expired_removes_only_expired() {
        let mut db = LipDatabase::new();
        db.annotation_set(make_annotation("lip://s#f", "live", 0));
        db.annotation_set(make_annotation("lip://s#f", "expired", 1));
        let removed = db.purge_expired_annotations();
        assert_eq!(removed, 1);
        assert!(db.annotation_get("lip://s#f", "live").is_some());
        assert!(db.annotation_get("lip://s#f", "expired").is_none());
    }

    #[test]
    fn purge_removes_empty_symbol_maps() {
        let mut db = LipDatabase::new();
        db.annotation_set(make_annotation("lip://s#f", "k", 1));
        db.purge_expired_annotations();
        // The inner HashMap for "lip://s#f" should be gone, not just empty.
        assert!(!db.annotations.contains_key("lip://s#f"));
    }

    // ── Merkle sync ───────────────────────────────────────────────────────────

    #[test]
    fn stale_files_unknown_uri_is_stale() {
        let db = LipDatabase::new();
        let stale = db.stale_files(&[("file:///src/unknown.rs".into(), "deadbeef".into())]);
        assert_eq!(stale, vec!["file:///src/unknown.rs"]);
    }

    #[test]
    fn stale_files_matching_hash_is_clean() {
        let mut db = LipDatabase::new();
        let uri = "file:///src/main.rs".to_owned();
        let text = "fn main() {}".to_owned();
        db.upsert_file(uri.clone(), text.clone(), "rust".to_owned());
        let hash = sha256_hex(text.as_bytes());

        let stale = db.stale_files(&[(uri.clone(), hash)]);
        assert!(stale.is_empty(), "matching hash should not be stale");
    }

    #[test]
    fn stale_files_wrong_hash_is_stale() {
        let mut db = LipDatabase::new();
        let uri = "file:///src/main.rs".to_owned();
        db.upsert_file(uri.clone(), "fn main() {}".to_owned(), "rust".to_owned());

        let stale = db.stale_files(&[(uri.clone(), "wrong_hash".into())]);
        assert_eq!(stale, vec![uri]);
    }

    #[test]
    fn stale_files_mixed_returns_only_stale() {
        let mut db = LipDatabase::new();

        let clean_text = "fn clean() {}".to_owned();
        let stale_text = "fn stale() {}".to_owned();

        db.upsert_file(
            "file:///src/clean.rs".into(),
            clean_text.clone(),
            "rust".into(),
        );
        db.upsert_file(
            "file:///src/stale.rs".into(),
            stale_text.clone(),
            "rust".into(),
        );

        let clean_hash = sha256_hex(clean_text.as_bytes());
        let stale = db.stale_files(&[
            ("file:///src/clean.rs".into(), clean_hash),
            ("file:///src/stale.rs".into(), "outdated_hash".into()),
            ("file:///src/new.rs".into(), "any_hash".into()),
        ]);
        assert_eq!(stale.len(), 2);
        assert!(stale.contains(&"file:///src/stale.rs".to_owned()));
        assert!(stale.contains(&"file:///src/new.rs".to_owned()));
        assert!(!stale.contains(&"file:///src/clean.rs".to_owned()));
    }

    // ── Slice mounting ────────────────────────────────────────────────────

    fn make_slice(
        manager: &str,
        pkg: &str,
        ver: &str,
        syms: Vec<(&str, &str)>,
    ) -> OwnedDependencySlice {
        OwnedDependencySlice {
            manager: manager.to_owned(),
            package_name: pkg.to_owned(),
            version: ver.to_owned(),
            package_hash: "abc123".to_owned(),
            content_hash: "def456".to_owned(),
            symbols: syms
                .into_iter()
                .map(|(uri, name)| OwnedSymbolInfo {
                    uri: uri.to_owned(),
                    display_name: name.to_owned(),
                    kind: SymbolKind::Function,
                    documentation: None,
                    signature: None,
                    confidence_score: 30,
                    relationships: vec![],
                    runtime_p99_ms: None,
                    call_rate_per_s: None,
                    taint_labels: vec![],
                    blast_radius: 0,
                    is_exported: true,
                    ..Default::default()
                })
                .collect(),
            slice_url: String::new(),
            built_at_ms: 0,
        }
    }

    #[test]
    fn mount_slice_symbols_visible_in_workspace_symbols() {
        let mut db = LipDatabase::new();
        let slice = make_slice(
            "cargo",
            "serde",
            "1.0.0",
            vec![
                (
                    "lip://cargo/serde@1.0.0/src/lib.rs#Deserialize",
                    "Deserialize",
                ),
                ("lip://cargo/serde@1.0.0/src/lib.rs#Serialize", "Serialize"),
            ],
        );
        db.mount_slice(&slice);
        assert_eq!(db.mounted_package_count(), 1);

        let results = db.workspace_symbols("Deserialize", 10);
        assert!(
            results.iter().any(|s| s.display_name == "Deserialize"),
            "mounted symbol should appear in workspace_symbols"
        );
    }

    #[test]
    fn mount_slice_confidence_is_tier3() {
        let mut db = LipDatabase::new();
        let slice = make_slice(
            "npm",
            "react",
            "18.2.0",
            vec![("lip://npm/react@18.2.0/index.js#useState", "useState")],
        );
        db.mount_slice(&slice);
        let sym = db.symbol_by_uri("lip://npm/react@18.2.0/index.js#useState");
        assert!(sym.is_some(), "symbol_by_uri should find mounted symbol");
        assert_eq!(
            sym.unwrap().confidence_score,
            100,
            "Tier 3 score must be 100"
        );
    }

    #[test]
    fn mount_slice_is_idempotent() {
        let mut db = LipDatabase::new();
        let slice = make_slice(
            "cargo",
            "tokio",
            "1.0.0",
            vec![("lip://cargo/tokio@1.0.0/src/lib.rs#spawn", "spawn")],
        );
        db.mount_slice(&slice);
        db.mount_slice(&slice); // second mount of same package
        assert_eq!(
            db.mounted_package_count(),
            1,
            "re-mount should not double-count package"
        );
        let results = db.workspace_symbols("spawn", 10);
        assert_eq!(
            results.iter().filter(|s| s.display_name == "spawn").count(),
            1,
            "symbol should appear exactly once after idempotent re-mount"
        );
    }

    #[test]
    fn mount_slice_def_index_populated() {
        let mut db = LipDatabase::new();
        let slice = make_slice(
            "pub",
            "flutter",
            "3.0.0",
            vec![(
                "lip://pub/flutter@3.0.0/lib/src/widgets.dart#StatefulWidget",
                "StatefulWidget",
            )],
        );
        db.mount_slice(&slice);
        let loc = db.symbol_definition_location(
            "lip://pub/flutter@3.0.0/lib/src/widgets.dart#StatefulWidget",
        );
        assert!(loc.is_some(), "def_index must contain mounted symbol URI");
    }

    #[test]
    fn mount_slice_visible_in_similar_symbols() {
        let mut db = LipDatabase::new();
        let slice = make_slice(
            "npm",
            "lodash",
            "4.17.21",
            vec![
                ("lip://npm/lodash@4.17.21/src/index.js#debounce", "debounce"),
                ("lip://npm/lodash@4.17.21/src/index.js#throttle", "throttle"),
            ],
        );
        db.mount_slice(&slice);
        // "debounce" and "debounc" should both hit via trigram similarity.
        let hits = db.similar_symbols("debounc", 10);
        assert!(
            hits.iter().any(|s| s.name == "debounce"),
            "mounted symbol should appear in similar_symbols results"
        );
    }

    #[test]
    fn remount_replaces_symbols_not_duplicates() {
        let mut db = LipDatabase::new();
        // First mount: two symbols.
        let slice_v1 = make_slice(
            "cargo",
            "anyhow",
            "1.0.0",
            vec![
                ("lip://cargo/anyhow@1.0.0/src/lib.rs#Error", "Error"),
                ("lip://cargo/anyhow@1.0.0/src/lib.rs#Context", "Context"),
            ],
        );
        db.mount_slice(&slice_v1);

        // Re-mount with only one symbol (simulates a trimmed re-build).
        let slice_v2 = make_slice(
            "cargo",
            "anyhow",
            "1.0.0",
            vec![("lip://cargo/anyhow@1.0.0/src/lib.rs#Error", "Error")],
        );
        db.mount_slice(&slice_v2);

        // Only one package should be tracked.
        assert_eq!(db.mounted_package_count(), 1);
        // The re-mount should have replaced, not accumulated.
        let results = db.workspace_symbols("", 100);
        let anyhow_syms: Vec<_> = results
            .iter()
            .filter(|s| s.uri.contains("anyhow"))
            .collect();
        assert_eq!(anyhow_syms.len(), 1, "re-mount should replace, not append");
    }

    // ── WS1: is_exported / ABI surface fingerprinting ─────────────────────────

    #[test]
    fn api_surface_includes_only_exported_symbols() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/lib.rs".to_owned();
        // pub fn is exported; private fn is not.
        db.upsert_file(
            uri.clone(),
            "pub fn public_fn() {} fn private_fn() {}".to_owned(),
            "rust".to_owned(),
        );
        let surface = db.file_api_surface(&uri);
        assert!(
            surface
                .symbols
                .iter()
                .any(|s| s.display_name == "public_fn"),
            "public_fn must appear in API surface"
        );
        assert!(
            !surface
                .symbols
                .iter()
                .any(|s| s.display_name == "private_fn"),
            "private_fn must not appear in API surface"
        );
    }

    #[test]
    fn api_surface_hash_stable_when_private_changes() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/lib.rs".to_owned();
        db.upsert_file(
            uri.clone(),
            "pub fn api() {} fn internal() {}".to_owned(),
            "rust".to_owned(),
        );
        let hash1 = db.file_api_surface(&uri).content_hash.clone();

        // Change only the private function body — API surface must be unchanged.
        db.upsert_file(
            uri.clone(),
            "pub fn api() {} fn internal() { let _x = 1; }".to_owned(),
            "rust".to_owned(),
        );
        let hash2 = db.file_api_surface(&uri).content_hash.clone();
        assert_eq!(hash1, hash2, "private-only change must not alter API hash");
    }

    // ── WS2: function-level blast radius ──────────────────────────────────────

    #[test]
    fn blast_radius_emits_multiple_items_per_file() {
        let mut db = LipDatabase::new();
        let lib_uri = "file:///project/lib.rs".to_owned();
        let caller_uri = "file:///project/caller.rs".to_owned();

        // lib.rs defines `target`; caller.rs has two functions that both call it.
        db.upsert_file(
            lib_uri.clone(),
            "pub fn target() {}".to_owned(),
            "rust".to_owned(),
        );
        db.upsert_file(
            caller_uri.clone(),
            "pub fn caller_a() { target(); } pub fn caller_b() { target(); }".to_owned(),
            "rust".to_owned(),
        );

        let target_sym = db
            .file_symbols(&lib_uri)
            .iter()
            .find(|s| s.display_name == "target")
            .map(|s| s.uri.clone())
            .expect("target symbol not found");

        let result = db.blast_radius_for(&target_sym);
        assert!(
            result.affected_files.contains(&caller_uri),
            "caller.rs must be in blast radius"
        );
        // Function-level: both caller_a and caller_b should appear as separate items.
        let items_for_caller: Vec<_> = result
            .direct_items
            .iter()
            .chain(result.transitive_items.iter())
            .filter(|i| i.file_uri == caller_uri)
            .collect();
        assert!(
            items_for_caller.len() >= 2,
            "both caller_a and caller_b should produce separate ImpactItems; got {:?}",
            items_for_caller
        );
    }

    #[test]
    fn blast_radius_batch_not_indexed_uris_reported() {
        let mut db = LipDatabase::new();
        db.upsert_file(
            "file:///project/lib.rs".to_owned(),
            "pub fn f() {}".to_owned(),
            "rust".to_owned(),
        );
        let unknown = "file:///project/ghost.rs".to_owned();
        let (results, not_indexed) = db.blast_radius_batch(std::slice::from_ref(&unknown), None);
        assert!(results.is_empty());
        assert_eq!(not_indexed, vec![unknown]);
    }

    // v2.3.2 Issue #1 — tier-1 back-fill URI translation.
    //
    // SCIP-imported files carry SCIP descriptor fragments (`#NewExporter()`),
    // but tier-1 tree-sitter emits plain identifier fragments (`#NewExporter`).
    // Without translation, `blast_radius_for` Phase 3's `def_index.get(caller_sym)`
    // misses every tier-1-emitted caller URI → Phase 4 falls through to file-
    // level items with blank `symbol_uri`.
    //
    // This test upserts a precomputed file (SCIP-descriptor URIs, no edges)
    // pointing at a real on-disk source. The back-fill must translate the
    // tier-1 caller URI to the SCIP URI via the file's defs so the caller
    // `ImpactItem` carries a non-empty `symbol_uri`.
    #[test]
    fn tier1_backfill_translates_caller_uri_to_scip_fragment() {
        use crate::schema::{OwnedRange, OwnedSymbolInfo, SymbolKind};

        let dir = tempfile::tempdir().expect("tempdir");
        let src_path = dir.path().join("chain.rs");
        std::fs::write(&src_path, "fn caller() { target(); }\nfn target() {}\n").unwrap();
        let abs = src_path.to_string_lossy();
        let file_uri = format!("lip://local//{}", abs.trim_start_matches('/'));
        // SCIP descriptor fragments (distinct from tier-1's plain-identifier form).
        let sym_caller = format!("{file_uri}#caller()");
        let sym_target = format!("{file_uri}#target()");

        let syms = vec![
            OwnedSymbolInfo {
                uri: sym_caller.clone(),
                kind: SymbolKind::Function,
                display_name: "caller".to_owned(),
                confidence_score: 90,
                is_exported: true,
                ..Default::default()
            },
            OwnedSymbolInfo {
                uri: sym_target.clone(),
                kind: SymbolKind::Function,
                display_name: "target".to_owned(),
                confidence_score: 90,
                is_exported: true,
                ..Default::default()
            },
        ];
        let occs = vec![
            OwnedOccurrence {
                symbol_uri: sym_caller.clone(),
                range: OwnedRange {
                    start_line: 0,
                    start_char: 3,
                    end_line: 0,
                    end_char: 9,
                },
                confidence_score: 90,
                role: Role::Definition,
                override_doc: None,
                kind: ReferenceKind::Unknown,
                is_test: false,
            },
            OwnedOccurrence {
                symbol_uri: sym_target.clone(),
                range: OwnedRange {
                    start_line: 1,
                    start_char: 3,
                    end_line: 1,
                    end_char: 9,
                },
                confidence_score: 90,
                role: Role::Definition,
                override_doc: None,
                kind: ReferenceKind::Unknown,
                is_test: false,
            },
        ];

        let mut db = LipDatabase::new();
        db.upsert_file_precomputed(
            file_uri.clone(),
            "rust".to_owned(),
            "abc".to_owned(),
            syms,
            occs,
            vec![], // empty edges → triggers tier-1 back-fill
        );

        let result = db.blast_radius_for(&sym_target);
        let all_items: Vec<_> = result
            .direct_items
            .iter()
            .chain(result.transitive_items.iter())
            .collect();

        // At least one item should carry the SCIP caller URI — not a blank
        // symbol_uri from the file-level fallback path.
        assert!(
            all_items.iter().any(|i| i.symbol_uri == sym_caller),
            "tier-1 back-fill must translate the caller URI to the SCIP descriptor form; \
             got items: {:?}",
            all_items
        );
        assert!(
            all_items.iter().all(|i| !i.symbol_uri.is_empty()),
            "no ImpactItem should carry a blank symbol_uri when the caller is defined \
             in a SCIP-imported file; got items: {:?}",
            all_items
        );

        // edges_source (v2.3.2) must now surface on BlastRadiusResult (not just EnrichedBlastRadius).
        assert_eq!(
            result.edges_source,
            Some(EdgesSource::ScipWithTier1Edges),
            "BlastRadiusResult.edges_source must be populated when back-fill ran"
        );
    }

    // v2.3.2 Issue #1 cross-file — tier-1 back-fill callee translation across
    // SCIP documents. Tier-1 emits edges with `to_uri = lip://local/<caller_file>#<name>`
    // even when the callee is defined in a different file. The same-file
    // `translate` map misses, so we must fall back to the global
    // `name_to_symbols` index (populated with SCIP display_name entries) to
    // resolve cross-file callees. Without this, CKB's merge step sees
    // `symbol_uri: ""` on every transitive item.
    #[test]
    fn tier1_backfill_resolves_cross_file_callee_via_name_index() {
        use crate::schema::{OwnedRange, OwnedSymbolInfo, SymbolKind};

        let dir = tempfile::tempdir().expect("tempdir");
        let caller_path = dir.path().join("caller.rs");
        let target_path = dir.path().join("target.rs");
        std::fs::write(&caller_path, "fn caller() { target(); }\n").unwrap();
        std::fs::write(&target_path, "pub fn target() {}\n").unwrap();
        let caller_abs = caller_path.to_string_lossy();
        let target_abs = target_path.to_string_lossy();
        let caller_uri = format!("lip://local//{}", caller_abs.trim_start_matches('/'));
        let target_uri = format!("lip://local//{}", target_abs.trim_start_matches('/'));
        // SCIP descriptor fragments — different from tier-1 plain identifiers.
        let sym_caller = format!("{caller_uri}#caller()");
        let sym_target = format!("{target_uri}#target()");

        let caller_syms = vec![OwnedSymbolInfo {
            uri: sym_caller.clone(),
            kind: SymbolKind::Function,
            display_name: "caller".to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        }];
        let caller_occs = vec![OwnedOccurrence {
            symbol_uri: sym_caller.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 3,
                end_line: 0,
                end_char: 9,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        }];
        let target_syms = vec![OwnedSymbolInfo {
            uri: sym_target.clone(),
            kind: SymbolKind::Function,
            display_name: "target".to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        }];
        let target_occs = vec![OwnedOccurrence {
            symbol_uri: sym_target.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 7,
                end_line: 0,
                end_char: 13,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        }];

        let mut db = LipDatabase::new();
        // Import target first so `name_to_symbols["target"]` is populated
        // before the caller's tier-1 back-fill runs.
        db.upsert_file_precomputed(
            target_uri.clone(),
            "rust".to_owned(),
            "t1".to_owned(),
            target_syms,
            target_occs,
            vec![],
        );
        db.upsert_file_precomputed(
            caller_uri.clone(),
            "rust".to_owned(),
            "c1".to_owned(),
            caller_syms,
            caller_occs,
            vec![],
        );

        let result = db.blast_radius_for(&sym_target);
        let all_items: Vec<_> = result
            .direct_items
            .iter()
            .chain(result.transitive_items.iter())
            .collect();

        // Caller (cross-file) must be resolved to its SCIP URI, not emitted
        // as a file-level fallback item with empty symbol_uri.
        assert!(
            all_items.iter().any(|i| i.symbol_uri == sym_caller),
            "cross-file caller must resolve to SCIP URI via name_to_symbols fallback; \
             got items: {:?}",
            all_items
        );
        assert!(
            all_items.iter().all(|i| !i.symbol_uri.is_empty()),
            "no cross-file ImpactItem should carry a blank symbol_uri; got items: {:?}",
            all_items
        );
        // `result.edges_source` reflects the *target* file's edges (no outgoing
        // calls in `target.rs` → Empty). Verify the caller file recorded the
        // back-filled edge separately.
        let caller_edges_src = db.file_edges_source.get(&caller_uri).copied();
        assert_eq!(
            caller_edges_src,
            Some(EdgesSource::ScipWithTier1Edges),
            "caller file's back-fill must register ScipWithTier1Edges"
        );
    }

    // v2.3.2 Issue #2 / Bug D — Phase-3 fallback for tier-1-form caller URIs.
    //
    // When the tier-1 back-fill resolver's `translate` map AND the global
    // `name_to_symbols` index both miss for a caller name, the back-fill
    // preserves the raw tier-1 URI (`lip://local//<abs>#<name>`) as the
    // caller key in `callee_to_callers`. That URI is NOT in `def_index`
    // (def_index is populated only from SCIP occurrences). Phase 3 must
    // therefore fall back to deriving the file URI by stripping the
    // `#<name>` fragment, or the caller gets dropped and every ImpactItem
    // degrades to the file-level fallback with a blank `symbol_uri`.
    #[test]
    fn blast_radius_phase3_fallback_for_tier1_caller_uri() {
        use crate::schema::{OwnedRange, OwnedSymbolInfo, SymbolKind};

        let dir = tempfile::tempdir().expect("tempdir");
        let caller_path = dir.path().join("caller.rs");
        let target_path = dir.path().join("target.rs");
        // `orphan` is picked up by the tier-1 extractor but deliberately
        // omitted from the caller file's SCIP symbols below, so the back-fill
        // resolver for the caller side must fall back to the raw tier-1 URI.
        std::fs::write(&caller_path, "fn orphan() { target(); }\n").unwrap();
        std::fs::write(&target_path, "pub fn target() {}\n").unwrap();
        let caller_abs = caller_path.to_string_lossy();
        let target_abs = target_path.to_string_lossy();
        let caller_uri = format!("lip://local//{}", caller_abs.trim_start_matches('/'));
        let target_uri = format!("lip://local//{}", target_abs.trim_start_matches('/'));
        // SCIP descriptor form for target — matches what scip-go/scip-clang emit.
        let sym_target = format!("{target_uri}#target()");
        // The raw tier-1 URI that the back-fill will keep for the caller
        // because `orphan` is neither in caller's display_name set nor in
        // `name_to_symbols` from any other file.
        let tier1_caller_sym = format!("{caller_uri}#orphan");

        let target_syms = vec![OwnedSymbolInfo {
            uri: sym_target.clone(),
            kind: SymbolKind::Function,
            display_name: "target".to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        }];
        let target_occs = vec![OwnedOccurrence {
            symbol_uri: sym_target.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 7,
                end_line: 0,
                end_char: 13,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        }];

        let mut db = LipDatabase::new();
        db.upsert_file_precomputed(
            target_uri.clone(),
            "rust".to_owned(),
            "t1".to_owned(),
            target_syms,
            target_occs,
            vec![],
        );
        // Caller imported with NO SCIP symbols — forces the back-fill
        // resolver to miss for `orphan` and keep the raw tier-1 URI.
        db.upsert_file_precomputed(
            caller_uri.clone(),
            "rust".to_owned(),
            "c1".to_owned(),
            vec![],
            vec![],
            vec![],
        );

        let result = db.blast_radius_for(&sym_target);
        let all_items: Vec<_> = result
            .direct_items
            .iter()
            .chain(result.transitive_items.iter())
            .collect();

        // Option (b) fallback: the tier-1 caller URI survives Phase 3 and
        // is emitted as a symbol-level ImpactItem, not a file-level blank.
        assert!(
            all_items
                .iter()
                .any(|i| i.symbol_uri == tier1_caller_sym && i.file_uri == caller_uri),
            "Phase 3 must fall back to stripping `#<name>` when def_index misses \
             for a tier-1-form caller URI; got items: {:?}",
            all_items
        );
        assert!(
            all_items.iter().all(|i| !i.symbol_uri.is_empty()),
            "no ImpactItem should carry a blank symbol_uri under the option-(b) \
             fallback; got items: {:?}",
            all_items
        );
    }

    // v2.3.3 — QueryOutgoingImpact basic forward-BFS test. Two files with
    // a single cross-file call chain; assert direct vs transitive split
    // and that `edges_source` is surfaced from the target's file.
    #[test]
    fn outgoing_impact_direct_and_transitive() {
        use crate::schema::{OwnedGraphEdge, OwnedRange, OwnedSymbolInfo, SymbolKind};

        let mut db = LipDatabase::new();
        let root_uri = "lip://local//abs/root.rs".to_owned();
        let mid_uri = "lip://local//abs/mid.rs".to_owned();
        let leaf_uri = "lip://local//abs/leaf.rs".to_owned();
        let sym_root = format!("{root_uri}#root");
        let sym_mid = format!("{mid_uri}#mid");
        let sym_leaf = format!("{leaf_uri}#leaf");

        let defn_occ = |sym: &str, range: OwnedRange| OwnedOccurrence {
            symbol_uri: sym.to_owned(),
            range,
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        let mk_sym = |uri: &str, name: &str| OwnedSymbolInfo {
            uri: uri.to_owned(),
            kind: SymbolKind::Function,
            display_name: name.to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        };
        let range = OwnedRange {
            start_line: 0,
            start_char: 0,
            end_line: 0,
            end_char: 1,
        };
        // root → mid → leaf
        let root_edge = OwnedGraphEdge {
            from_uri: sym_root.clone(),
            to_uri: sym_mid.clone(),
            kind: EdgeKind::Calls,
            at_range: range.clone(),
        };
        let mid_edge = OwnedGraphEdge {
            from_uri: sym_mid.clone(),
            to_uri: sym_leaf.clone(),
            kind: EdgeKind::Calls,
            at_range: range.clone(),
        };

        db.upsert_file_precomputed(
            leaf_uri.clone(),
            "rust".to_owned(),
            "l1".to_owned(),
            vec![mk_sym(&sym_leaf, "leaf")],
            vec![defn_occ(&sym_leaf, range.clone())],
            vec![],
        );
        db.upsert_file_precomputed(
            mid_uri.clone(),
            "rust".to_owned(),
            "m1".to_owned(),
            vec![mk_sym(&sym_mid, "mid")],
            vec![defn_occ(&sym_mid, range.clone())],
            vec![mid_edge],
        );
        db.upsert_file_precomputed(
            root_uri.clone(),
            "rust".to_owned(),
            "r1".to_owned(),
            vec![mk_sym(&sym_root, "root")],
            vec![defn_occ(&sym_root, range.clone())],
            vec![root_edge],
        );

        let result = db
            .outgoing_impact_for(&sym_root, Some(4), None)
            .expect("root should resolve");

        let direct: Vec<_> = result.static_result.direct_items.iter().collect();
        let trans: Vec<_> = result.static_result.transitive_items.iter().collect();
        assert_eq!(direct.len(), 1, "expected one direct callee (mid)");
        assert_eq!(direct[0].symbol_uri, sym_mid);
        assert_eq!(direct[0].distance, 1);
        assert_eq!(trans.len(), 1, "expected one transitive callee (leaf)");
        assert_eq!(trans[0].symbol_uri, sym_leaf);
        assert_eq!(trans[0].distance, 2);
        assert_eq!(
            result.static_result.edges_source,
            Some(EdgesSource::ScipOnly),
            "edges_source should reflect root.rs (ScipOnly — pre-computed edges)"
        );
        assert!(
            result.semantic_items.is_empty(),
            "min_score=None → no enrichment"
        );
    }

    // v2.3.3 — Bug-D-symmetric test: when the tier-1 back-fill keeps a
    // callee in raw `lip://local//<abs>#<name>` form (resolver misses in
    // both translate + name_to_symbols), outgoing_impact_for must strip
    // the `#<name>` fragment to derive file_uri instead of dropping the
    // callee entirely.
    #[test]
    fn outgoing_impact_phase3_fallback_for_tier1_callee_uri() {
        use crate::schema::{OwnedRange, OwnedSymbolInfo, SymbolKind};

        let dir = tempfile::tempdir().expect("tempdir");
        let caller_path = dir.path().join("caller.rs");
        // `orphan` is a free-standing callee name unknown to name_to_symbols,
        // so the tier-1 back-fill resolver must fall through to the raw
        // tier-1 URI `caller_uri#orphan` as the callee key.
        std::fs::write(&caller_path, "fn entry() { orphan(); }\n").unwrap();
        let caller_abs = caller_path.to_string_lossy();
        let caller_uri = format!("lip://local//{}", caller_abs.trim_start_matches('/'));
        let sym_entry = format!("{caller_uri}#entry()");
        let tier1_callee = format!("{caller_uri}#orphan");

        let caller_syms = vec![OwnedSymbolInfo {
            uri: sym_entry.clone(),
            kind: SymbolKind::Function,
            display_name: "entry".to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        }];
        let caller_occs = vec![OwnedOccurrence {
            symbol_uri: sym_entry.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 3,
                end_line: 0,
                end_char: 8,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        }];

        let mut db = LipDatabase::new();
        // Caller: empty SCIP edges → tier-1 back-fill runs over on-disk
        // source, keeps `orphan` as a raw tier-1 URI since resolver misses.
        db.upsert_file_precomputed(
            caller_uri.clone(),
            "rust".to_owned(),
            "c1".to_owned(),
            caller_syms,
            caller_occs,
            vec![],
        );

        let result = db
            .outgoing_impact_for(&sym_entry, Some(2), None)
            .expect("caller entry symbol should resolve");
        let direct = &result.static_result.direct_items;
        assert!(
            direct
                .iter()
                .any(|i| i.symbol_uri == tier1_callee && i.file_uri == caller_uri),
            "tier-1-form callee must survive via #-strip fallback; got: {:?}",
            direct
        );
        assert!(
            direct.iter().all(|i| !i.symbol_uri.is_empty()),
            "outgoing direct items must never carry blank symbol_uri; got: {:?}",
            direct
        );
    }

    // v2.3.5 — forward-direction twin of the `callee_name_to_callers` bridge.
    // When a caller symbol is registered under a SCIP descriptor URI
    // (`pkg#Engine#AnalyzeImpact().`) but the pre-computed call edges key
    // the caller in tier-1 form (`...#AnalyzeImpact`), the seed lookup in
    // `caller_to_callees` misses. `outgoing_impact_for` must fall through
    // to `caller_name_to_callees` via the normalised name fragment.
    #[test]
    fn outgoing_impact_name_bridge_for_tier1_caller_uri() {
        use crate::schema::{OwnedGraphEdge, OwnedRange, OwnedSymbolInfo, SymbolKind};

        let mut db = LipDatabase::new();
        let caller_file = "lip://local//abs/engine.go".to_owned();
        let callee_file = "lip://local//abs/leaf.go".to_owned();
        // SCIP descriptor form for the caller (Go method on Engine receiver).
        // `extract_name` + `normalize_callee_name` collapses this to
        // `"AnalyzeImpact"` — matching the tier-1-form caller URI below.
        let scip_caller_sym = format!("{caller_file}#Engine#AnalyzeImpact().");
        // Raw tier-1 caller URI that the back-fill resolver's fallthrough
        // would keep when the same-file `translate` map and the global
        // `name_to_symbols` index both miss for an overloaded method name.
        let tier1_caller_sym = format!("{caller_file}#AnalyzeImpact");
        let callee_sym = format!("{callee_file}#leaf().");

        let range = OwnedRange {
            start_line: 0,
            start_char: 0,
            end_line: 0,
            end_char: 1,
        };
        let defn_occ = |sym: &str| OwnedOccurrence {
            symbol_uri: sym.to_owned(),
            range: range.clone(),
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        let mk_sym = |uri: &str, name: &str| OwnedSymbolInfo {
            uri: uri.to_owned(),
            kind: SymbolKind::Function,
            display_name: name.to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        };

        // Callee registered cleanly in SCIP form so Phase-3 resolution can
        // map it to a file via `def_index` rather than the #-strip fallback.
        db.upsert_file_precomputed(
            callee_file.clone(),
            "go".to_owned(),
            "c1".to_owned(),
            vec![mk_sym(&callee_sym, "leaf")],
            vec![defn_occ(&callee_sym)],
            vec![],
        );
        // Caller file: SCIP symbol + definition (feeds def_index with the
        // SCIP descriptor), but the pre-computed edge keys the caller in
        // tier-1 form. This is the shape the tier-1 back-fill produces
        // when the caller name is ambiguous across the codebase.
        let edge = OwnedGraphEdge {
            from_uri: tier1_caller_sym.clone(),
            to_uri: callee_sym.clone(),
            kind: EdgeKind::Calls,
            at_range: range.clone(),
        };
        db.upsert_file_precomputed(
            caller_file.clone(),
            "go".to_owned(),
            "e1".to_owned(),
            vec![mk_sym(&scip_caller_sym, "AnalyzeImpact")],
            vec![defn_occ(&scip_caller_sym)],
            vec![edge],
        );

        // Query using the SCIP descriptor URI — URI-exact seed lookup in
        // `caller_to_callees` misses (key is the tier-1 form), so the
        // forward BFS must bridge via `caller_name_to_callees`.
        let enriched = db
            .outgoing_impact_for(&scip_caller_sym, None, None)
            .expect("outgoing_impact_for must return Some for a known symbol");
        let direct = &enriched.static_result.direct_items;
        assert!(
            direct.iter().any(|i| i.symbol_uri == callee_sym),
            "forward name-bridge must surface the callee from a SCIP-form seed; \
             got direct_items: {:?}",
            direct
        );
    }

    #[test]
    fn blast_radius_batch_file_uri_populated() {
        let mut db = LipDatabase::new();
        let lib_uri = "file:///project/lib.rs".to_owned();
        db.upsert_file(
            lib_uri.clone(),
            "pub fn exported() {}".to_owned(),
            "rust".to_owned(),
        );
        let (results, not_indexed) = db.blast_radius_batch(std::slice::from_ref(&lib_uri), None);
        assert!(not_indexed.is_empty());
        for entry in &results {
            assert_eq!(entry.file_uri, lib_uri, "file_uri must trace back to input");
        }
    }

    #[test]
    fn file_symbols_precomputed_cold_cache_returns_empty() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/imported.go".to_owned();
        use crate::schema::{OwnedSymbolInfo, SymbolKind};
        let sym = OwnedSymbolInfo {
            uri: "lip://local/imported.go#Foo".to_owned(),
            kind: SymbolKind::Function,
            display_name: "Foo".to_owned(),
            confidence_score: 90,
            signature: None,
            documentation: None,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: true,
            ..Default::default()
        };
        db.upsert_file_precomputed(
            uri.clone(),
            "go".to_owned(),
            "abc123".to_owned(),
            vec![sym],
            vec![],
            vec![],
        );
        // Warm path: sym_cache is populated — must return the precomputed symbol.
        let syms = db.file_symbols(&uri);
        assert_eq!(syms.len(), 1, "warm path must return precomputed symbol");

        // Simulate cold cache; file_inputs still marks precomputed=true.
        db.sym_cache.remove(&uri);
        let syms_cold = db.file_symbols(&uri);
        // Must not fall through to Tier 1 (which would parse empty text).
        assert!(
            syms_cold.is_empty(),
            "cold precomputed cache must not run Tier-1 parser"
        );
    }

    // ── WS3: name consumption index ───────────────────────────────────────────

    #[test]
    fn files_consuming_names_finds_referencing_file() {
        let mut db = LipDatabase::new();
        let def_uri = "file:///project/lib.rs".to_owned();
        let ref_uri = "file:///project/main.rs".to_owned();

        // lib.rs defines `my_fn`; main.rs references it.
        db.upsert_file(
            def_uri.clone(),
            "pub fn my_fn() {}".to_owned(),
            "rust".to_owned(),
        );
        db.upsert_file(
            ref_uri.clone(),
            "fn caller() { my_fn(); }".to_owned(),
            "rust".to_owned(),
        );

        let consumers = db.files_consuming_names(&["my_fn"]);
        assert!(
            consumers.contains(&ref_uri),
            "main.rs references my_fn so should appear in consumers; got {:?}",
            consumers
        );
        assert!(
            !consumers.contains(&def_uri),
            "lib.rs defines my_fn (not an external reference); got {:?}",
            consumers
        );
    }

    #[test]
    fn files_consuming_names_cleared_on_remove() {
        let mut db = LipDatabase::new();
        let def_uri = "file:///project/lib.rs".to_owned();
        let ref_uri = "file:///project/main.rs".to_owned();

        db.upsert_file(
            def_uri.clone(),
            "pub fn my_fn() {}".to_owned(),
            "rust".to_owned(),
        );
        db.upsert_file(
            ref_uri.clone(),
            "fn caller() { my_fn(); }".to_owned(),
            "rust".to_owned(),
        );

        db.remove_file(&ref_uri);
        let consumers = db.files_consuming_names(&["my_fn"]);
        assert!(
            !consumers.contains(&ref_uri),
            "removed file must not appear in consumers; got {:?}",
            consumers
        );
    }

    // ── tier3 provenance ──────────────────────────────────────────────────

    #[test]
    fn tier3_sources_sorted_by_source_id() {
        use crate::query_graph::types::Tier3Source;
        let mut db = LipDatabase::new();
        db.register_tier3_source(Tier3Source {
            source_id: "b".into(),
            tool_name: "scip-typescript".into(),
            tool_version: "0.3.0".into(),
            project_root: "file:///b".into(),
            imported_at_ms: 2,
        });
        db.register_tier3_source(Tier3Source {
            source_id: "a".into(),
            tool_name: "scip-rust".into(),
            tool_version: "0.3.0".into(),
            project_root: "file:///a".into(),
            imported_at_ms: 1,
        });
        let got = db.tier3_sources();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].source_id, "a");
        assert_eq!(got[1].source_id, "b");
    }

    /// Re-registering the same `source_id` must overwrite the prior
    /// record in place, refreshing `imported_at_ms`. This is the
    /// mechanism clients rely on to mark a fresh import.
    #[test]
    fn tier3_reregistration_overwrites_in_place() {
        use crate::query_graph::types::Tier3Source;
        let mut db = LipDatabase::new();
        db.register_tier3_source(Tier3Source {
            source_id: "same".into(),
            tool_name: "scip-rust".into(),
            tool_version: "0.3.0".into(),
            project_root: "file:///r".into(),
            imported_at_ms: 1,
        });
        db.register_tier3_source(Tier3Source {
            source_id: "same".into(),
            tool_name: "scip-rust".into(),
            tool_version: "0.4.0".into(),
            project_root: "file:///r".into(),
            imported_at_ms: 99,
        });
        let got = db.tier3_sources();
        assert_eq!(got.len(), 1, "re-registration must not grow the list");
        assert_eq!(got[0].tool_version, "0.4.0");
        assert_eq!(got[0].imported_at_ms, 99);
    }

    // ── symbol_embeddings / nearest_symbol_by_vector ──────────────────────

    #[test]
    fn set_get_symbol_embedding_roundtrip() {
        let mut db = LipDatabase::new();
        let uri = "lip://local/src/main.rs#foo";
        let vec = vec![1.0_f32, 0.0, 0.0];
        db.set_symbol_embedding(uri, vec.clone(), "test-model");
        assert_eq!(db.get_symbol_embedding(uri), Some(&vec));
        assert!(db
            .get_symbol_embedding("lip://local/src/main.rs#missing")
            .is_none());
    }

    #[test]
    fn nearest_symbol_by_vector_orders_by_cosine() {
        let mut db = LipDatabase::new();
        // Three orthogonal unit vectors; query aligns with "foo".
        db.set_symbol_embedding("lip://local/f.rs#foo", vec![1.0, 0.0, 0.0], "test-model");
        db.set_symbol_embedding("lip://local/f.rs#bar", vec![0.0, 1.0, 0.0], "test-model");
        db.set_symbol_embedding("lip://local/f.rs#baz", vec![0.0, 0.0, 1.0], "test-model");

        let query = vec![1.0_f32, 0.0, 0.0];
        let results = db.nearest_symbol_by_vector(&query, 3, None, None);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].uri, "lip://local/f.rs#foo");
        assert!(
            (results[0].score - 1.0).abs() < 1e-5,
            "score should be ~1.0"
        );
        assert!(results[1].score < results[0].score);
    }

    #[test]
    fn nearest_symbol_by_vector_excludes_self() {
        let mut db = LipDatabase::new();
        db.set_symbol_embedding("lip://local/f.rs#foo", vec![1.0, 0.0], "test-model");
        db.set_symbol_embedding("lip://local/f.rs#bar", vec![0.9, 0.1], "test-model");

        let query = vec![1.0_f32, 0.0];
        let results = db.nearest_symbol_by_vector(&query, 5, Some("lip://local/f.rs#foo"), None);
        assert!(
            !results.iter().any(|r| r.uri == "lip://local/f.rs#foo"),
            "excluded URI must not appear in results"
        );
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn nearest_symbol_by_vector_empty_store_returns_empty() {
        let db = LipDatabase::new();
        let results = db.nearest_symbol_by_vector(&[1.0, 0.0], 5, None, None);
        assert!(results.is_empty());
    }

    #[test]
    fn nearest_symbol_by_vector_filters_by_model() {
        let mut db = LipDatabase::new();
        // Two symbols with near-identical vectors but different embedding
        // models. A query pinned to model-a must not match the model-b symbol
        // even though the raw cosine score would be high.
        db.set_symbol_embedding("lip://local/f.rs#alpha", vec![1.0, 0.0], "model-a");
        db.set_symbol_embedding("lip://local/f.rs#beta", vec![1.0, 0.0], "model-b");

        let query = vec![1.0_f32, 0.0];

        let all = db.nearest_symbol_by_vector(&query, 5, None, None);
        assert_eq!(all.len(), 2, "without filter both symbols rank");

        let pinned = db.nearest_symbol_by_vector(&query, 5, None, Some("model-a"));
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].uri, "lip://local/f.rs#alpha");
    }

    /// Pins the `QueryExpansion` handler contract: when the embedding
    /// service returns model X for the query, the subsequent ranking
    /// must be restricted to symbols embedded with model X.
    ///
    /// This test mirrors the exact call the session handler makes
    /// (see `session.rs::ClientMessage::QueryExpansion`). A regression
    /// that passes `None` for the model filter — which would silently
    /// re-introduce cross-model cosine scoring — would cause
    /// `cross-model-vector` to appear in the expansion terms and fail
    /// this assertion.
    #[test]
    fn query_expansion_terms_rejects_cross_model_scoring() {
        let mut db = LipDatabase::new();

        // Two symbols in different models, both aligned with the query
        // vector. Naive (unfiltered) cosine would rank both highly.
        let f_uri = "file:///src/f.rs".to_owned();
        db.upsert_file(
            f_uri.clone(),
            "fn matching_model() {}\nfn cross_model_vector() {}".into(),
            "rust".into(),
        );
        db.set_symbol_embedding("lip://local/f.rs#matching_model", vec![1.0, 0.0], "model-a");
        db.set_symbol_embedding(
            "lip://local/f.rs#cross_model_vector",
            vec![1.0, 0.0],
            "model-b",
        );

        let query_vec = vec![1.0_f32, 0.0];

        // The embedding service would have returned "model-a" for the
        // query. Handler passes that through.
        let terms = db.query_expansion_terms(&query_vec, "model-a", 5);

        assert!(
            terms.iter().any(|t| t.contains("matching_model")),
            "same-model term must appear: got {terms:?}"
        );
        assert!(
            !terms.iter().any(|t| t.contains("cross_model_vector")),
            "cross-model term must NOT appear — indicates the filter was \
             bypassed: got {terms:?}"
        );
    }

    // ── outliers ──────────────────────────────────────────────────────────

    #[test]
    fn outliers_returns_lowest_mean_similarity_first() {
        let mut db = LipDatabase::new();
        // Three tightly clustered files and one outlier in an orthogonal direction.
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0, 0.0], "test-model");
        db.set_file_embedding("file:///b.rs", vec![0.9, 0.1, 0.0], "test-model");
        db.set_file_embedding("file:///c.rs", vec![0.95, 0.05, 0.0], "test-model");
        db.set_file_embedding("file:///outlier.rs", vec![0.0, 0.0, 1.0], "test-model");

        let uris = vec![
            "file:///a.rs".into(),
            "file:///b.rs".into(),
            "file:///c.rs".into(),
            "file:///outlier.rs".into(),
        ];
        let results = db.outliers(&uris, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uri, "file:///outlier.rs");
    }

    #[test]
    fn outliers_empty_input_returns_empty() {
        let db = LipDatabase::new();
        let results = db.outliers(&[], 5);
        assert!(results.is_empty());
    }

    // ── similarity_matrix ─────────────────────────────────────────────────

    #[test]
    fn similarity_matrix_diagonal_is_one() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0], "test-model");
        db.set_file_embedding("file:///b.rs", vec![0.0, 1.0], "test-model");

        let uris = vec!["file:///a.rs".into(), "file:///b.rs".into()];
        let (result_uris, matrix) = db.similarity_matrix(&uris);
        assert_eq!(result_uris.len(), 2);
        assert!((matrix[0][0] - 1.0).abs() < 1e-5);
        assert!((matrix[1][1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similarity_matrix_symmetric_and_orthogonal() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0], "test-model");
        db.set_file_embedding("file:///b.rs", vec![0.0, 1.0], "test-model");

        let uris = vec!["file:///a.rs".into(), "file:///b.rs".into()];
        let (_, matrix) = db.similarity_matrix(&uris);
        assert!((matrix[0][1] - 0.0).abs() < 1e-5);
        assert!((matrix[1][0] - 0.0).abs() < 1e-5);
    }

    #[test]
    fn similarity_matrix_excludes_uris_without_embeddings() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0], "test-model");
        // file:///missing.rs has no embedding.

        let uris = vec!["file:///a.rs".into(), "file:///missing.rs".into()];
        let (result_uris, matrix) = db.similarity_matrix(&uris);
        assert_eq!(result_uris.len(), 1);
        assert_eq!(matrix.len(), 1);
        assert_eq!(matrix[0].len(), 1);
    }

    // ── coverage ──────────────────────────────────────────────────────────

    #[test]
    fn coverage_counts_indexed_and_embedded() {
        let mut db = LipDatabase::new();
        db.upsert_file(
            "file:///project/src/a.rs".into(),
            "fn a() {}".into(),
            "rust".into(),
        );
        db.upsert_file(
            "file:///project/src/b.rs".into(),
            "fn b() {}".into(),
            "rust".into(),
        );
        db.set_file_embedding("file:///project/src/a.rs", vec![1.0, 0.0], "test-model");
        // b.rs is indexed but has no embedding.

        let (total, embedded, dirs) = db.coverage("/project/src");
        assert_eq!(total, 2);
        assert_eq!(embedded, 1);
        assert!(!dirs.is_empty());
    }

    // ── novelty_scores ────────────────────────────────────────────────────

    #[test]
    fn novelty_scores_high_for_orthogonal_file() {
        let mut db = LipDatabase::new();
        // Existing repo: two similar auth files.
        db.set_file_embedding("file:///src/auth.rs", vec![1.0, 0.0, 0.0], "test-model");
        db.set_file_embedding(
            "file:///src/auth_helper.rs",
            vec![0.9, 0.1, 0.0],
            "test-model",
        );
        // New file: completely different direction.
        db.set_file_embedding("file:///src/billing.rs", vec![0.0, 1.0, 0.0], "test-model");

        let (mean, items) = db.novelty_scores(&["file:///src/billing.rs".to_owned()]);
        assert_eq!(items.len(), 1);
        // Nearest neighbour is auth_helper (closest at ~0.1 similarity), so novelty ≈ 0.9+.
        assert!(
            mean > 0.8,
            "billing.rs should have high novelty; got {mean}"
        );
    }

    #[test]
    fn novelty_scores_low_for_similar_file() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///src/auth.rs", vec![1.0, 0.0], "test-model");
        db.set_file_embedding("file:///src/auth2.rs", vec![0.99, 0.01], "test-model");

        // auth2 is the new file; auth is the existing repo.
        let (mean, items) = db.novelty_scores(&["file:///src/auth2.rs".to_owned()]);
        assert_eq!(items.len(), 1);
        assert!(
            mean < 0.1,
            "auth2.rs is very similar to auth.rs; got {mean}"
        );
        assert_eq!(
            items[0].nearest_existing.as_deref(),
            Some("file:///src/auth.rs")
        );
    }

    #[test]
    fn novelty_scores_excludes_input_set_from_neighbour_search() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///src/a.rs", vec![1.0, 0.0], "test-model");
        db.set_file_embedding("file:///src/b.rs", vec![1.0, 0.0], "test-model"); // identical direction
        db.set_file_embedding("file:///other/c.rs", vec![0.0, 1.0], "test-model");

        // Both a.rs and b.rs are in the input set; nearest_existing should be c.rs.
        let (_, items) =
            db.novelty_scores(&["file:///src/a.rs".to_owned(), "file:///src/b.rs".to_owned()]);
        for item in &items {
            assert_ne!(
                item.nearest_existing.as_deref(),
                Some("file:///src/a.rs"),
                "should not match within input set"
            );
            assert_ne!(
                item.nearest_existing.as_deref(),
                Some("file:///src/b.rs"),
                "should not match within input set"
            );
        }
    }

    // ── nearest_by_vector filter / min_score ─────────────────────────────

    #[test]
    fn nearest_by_vector_filter_restricts_by_filename() {
        let mut db = LipDatabase::new();
        db.file_embeddings
            .insert("file:///src/auth.rs".to_owned(), vec![1.0, 0.0]);
        db.file_embeddings
            .insert("file:///src/auth_test.go".to_owned(), vec![1.0, 0.0]);

        // Only *_test.go files.
        let hits = db.nearest_by_vector(&[1.0, 0.0], 10, None, Some("*_test.go"), None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].uri.ends_with("auth_test.go"));
    }

    #[test]
    fn nearest_by_vector_filter_restricts_by_path() {
        let mut db = LipDatabase::new();
        db.file_embeddings.insert(
            "file:///project/internal/auth.rs".to_owned(),
            vec![1.0, 0.0],
        );
        db.file_embeddings
            .insert("file:///project/cmd/main.rs".to_owned(), vec![1.0, 0.0]);

        // Only files under internal/.
        let hits = db.nearest_by_vector(&[1.0, 0.0], 10, None, Some("/project/internal/**"), None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].uri.contains("internal/auth.rs"));
    }

    #[test]
    fn nearest_by_vector_min_score_gates_results() {
        let mut db = LipDatabase::new();
        // Orthogonal: cosine similarity = 0.0
        db.file_embeddings
            .insert("file:///a.rs".to_owned(), vec![1.0, 0.0]);
        db.file_embeddings
            .insert("file:///b.rs".to_owned(), vec![0.0, 1.0]);

        // Query along [1,0] — a.rs scores 1.0, b.rs scores 0.0.
        let hits = db.nearest_by_vector(&[1.0, 0.0], 10, None, None, Some(0.5));
        assert_eq!(hits.len(), 1, "b.rs should be filtered out");
        assert_eq!(hits[0].uri, "file:///a.rs");
    }

    // ── centroid ──────────────────────────────────────────────────────────

    #[test]
    fn centroid_of_two_files_is_component_wise_mean() {
        let mut db = LipDatabase::new();
        let uri_a = "file:///a.rs".to_owned();
        let uri_b = "file:///b.rs".to_owned();
        db.file_embeddings.insert(uri_a.clone(), vec![1.0, 0.0]);
        db.file_embeddings.insert(uri_b.clone(), vec![0.0, 1.0]);

        let (vec, included) = db.centroid(&[uri_a, uri_b]);
        assert_eq!(included, 2);
        assert_eq!(vec.len(), 2);
        assert!((vec[0] - 0.5).abs() < 1e-5, "expected 0.5 got {}", vec[0]);
        assert!((vec[1] - 0.5).abs() < 1e-5, "expected 0.5 got {}", vec[1]);
    }

    #[test]
    fn centroid_empty_input_returns_empty_vector() {
        let db = LipDatabase::new();
        let (vec, included) = db.centroid(&[]);
        assert_eq!(included, 0);
        assert!(vec.is_empty());
    }

    #[test]
    fn centroid_excludes_uris_without_embeddings() {
        let mut db = LipDatabase::new();
        let uri_a = "file:///a.rs".to_owned();
        db.file_embeddings.insert(uri_a.clone(), vec![1.0, 0.0]);

        let (vec, included) = db.centroid(&[uri_a, "file:///no_embed.rs".to_owned()]);
        assert_eq!(included, 1);
        assert!((vec[0] - 1.0).abs() < 1e-5);
    }

    // ── file_embeddings_in_root ───────────────────────────────────────────

    #[test]
    fn file_embeddings_in_root_filters_by_prefix() {
        let mut db = LipDatabase::new();
        let in_root = "file:///project/src/a.rs".to_owned();
        let out_root = "file:///other/b.rs".to_owned();
        db.file_embeddings.insert(in_root.clone(), vec![1.0]);
        db.file_embeddings.insert(out_root, vec![0.0]);

        let results = db.file_embeddings_in_root("/project/src");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, in_root);
    }

    #[test]
    fn file_embeddings_in_root_missing_indexed_at_returns_zero() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/auth.rs".to_owned();
        db.file_embeddings.insert(uri.clone(), vec![1.0]);
        // No file_indexed_at entry.

        let results = db.file_embeddings_in_root("/project");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].1, 0,
            "no indexed_at should yield ts=0 (conservative stale)"
        );
    }

    // ── coverage_root_prefix_filters_correctly ────────────────────────────

    #[test]
    fn coverage_root_prefix_filters_correctly() {
        let mut db = LipDatabase::new();
        db.upsert_file(
            "file:///project/src/a.rs".into(),
            "fn a() {}".into(),
            "rust".into(),
        );
        db.upsert_file(
            "file:///other/b.rs".into(),
            "fn b() {}".into(),
            "rust".into(),
        );

        let (total, _, _) = db.coverage("/project/src");
        assert_eq!(total, 1, "should only count files under /project/src");
    }

    // ── Precomputed upsert (SCIP import path) ────────────────────────────

    #[test]
    fn precomputed_symbols_appear_in_search() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/lib.rs".to_owned();
        let sym_uri = "lip://local/lib.rs#MyStruct".to_owned();
        let symbols = vec![OwnedSymbolInfo {
            uri: sym_uri.clone(),
            display_name: "MyStruct".into(),
            kind: SymbolKind::Class,
            documentation: None,
            signature: None,
            confidence_score: 90,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
            ..Default::default()
        }];
        let occurrences = vec![OwnedOccurrence {
            symbol_uri: sym_uri.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 8,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        }];

        db.upsert_file_precomputed(
            uri.clone(),
            "rust".into(),
            "hash123".into(),
            symbols,
            occurrences,
            vec![],
        );

        let syms = db.file_symbols(&uri);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].display_name, "MyStruct");

        let results = db.workspace_symbols("MyStruct", 10);
        assert_eq!(
            results.len(),
            1,
            "pre-computed symbol must appear in workspace search"
        );

        assert!(
            db.symbol_definition_location(&sym_uri).is_some(),
            "pre-computed definition must be resolvable"
        );
    }

    #[test]
    fn precomputed_upsert_is_idempotent() {
        let mut db = LipDatabase::new();
        let uri = "file:///project/lib.rs".to_owned();
        let sym = OwnedSymbolInfo {
            uri: "lip://local/lib.rs#Foo".into(),
            display_name: "Foo".into(),
            kind: SymbolKind::Function,
            documentation: None,
            signature: None,
            confidence_score: 90,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
            ..Default::default()
        };
        let occ = OwnedOccurrence {
            symbol_uri: "lip://local/lib.rs#Foo".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };

        db.upsert_file_precomputed(
            uri.clone(),
            "rust".into(),
            "hash1".into(),
            vec![sym.clone()],
            vec![occ.clone()],
            vec![],
        );
        db.upsert_file_precomputed(
            uri.clone(),
            "rust".into(),
            "hash1".into(),
            vec![sym],
            vec![occ],
            vec![],
        );

        let results = db.workspace_symbols("Foo", 10);
        assert_eq!(results.len(), 1, "re-upsert must not duplicate symbols");
    }

    // ── Symbol-level invalidation ────────────────────────────────────────

    #[test]
    fn invalidated_files_for_returns_consumers() {
        // File A defines `fn foo()`, File B references `foo`.
        // Changing `foo` must invalidate B.
        let mut db = LipDatabase::new();

        // File A: defines foo
        let uri_a = "lip://local/a.rs".to_owned();
        let sym_foo = OwnedSymbolInfo::new("lip://local/a.rs#foo", "foo");
        let occ_def = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        db.upsert_file_precomputed(
            uri_a.clone(),
            "rust".into(),
            "h1".into(),
            vec![sym_foo],
            vec![occ_def],
            vec![],
        );

        // File B: references foo (defined in A → external)
        let uri_b = "lip://local/b.rs".to_owned();
        let occ_ref = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 80,
            role: Role::Reference,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        db.upsert_file_precomputed(
            uri_b.clone(),
            "rust".into(),
            "h2".into(),
            vec![],
            vec![occ_ref],
            vec![],
        );

        let invalidated = db.invalidated_files_for(&["lip://local/a.rs#foo".into()]);
        assert_eq!(invalidated, vec![uri_b]);
    }

    #[test]
    fn invalidated_files_for_unreferenced_symbol() {
        // File C defines `fn bar()`, no one references it.
        // Changing `bar` invalidates nothing.
        let mut db = LipDatabase::new();

        let uri_c = "lip://local/c.rs".to_owned();
        let sym_bar = OwnedSymbolInfo::new("lip://local/c.rs#bar", "bar");
        let occ_def = OwnedOccurrence {
            symbol_uri: "lip://local/c.rs#bar".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        db.upsert_file_precomputed(
            uri_c.clone(),
            "rust".into(),
            "h1".into(),
            vec![sym_bar],
            vec![occ_def],
            vec![],
        );

        let invalidated = db.invalidated_files_for(&["lip://local/c.rs#bar".into()]);
        assert!(
            invalidated.is_empty(),
            "unreferenced symbol should invalidate nothing"
        );
    }

    #[test]
    fn remove_file_clears_consumed_names() {
        // After removing a file, its consumed-names entries must be gone,
        // so it no longer appears in invalidation results.
        let mut db = LipDatabase::new();

        // File A: defines foo
        let uri_a = "lip://local/a.rs".to_owned();
        let sym_foo = OwnedSymbolInfo::new("lip://local/a.rs#foo", "foo");
        let occ_def = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        db.upsert_file_precomputed(
            uri_a.clone(),
            "rust".into(),
            "h1".into(),
            vec![sym_foo],
            vec![occ_def],
            vec![],
        );

        // File B: references foo
        let uri_b = "lip://local/b.rs".to_owned();
        let occ_ref = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
            },
            confidence_score: 80,
            role: Role::Reference,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        db.upsert_file_precomputed(
            uri_b.clone(),
            "rust".into(),
            "h2".into(),
            vec![],
            vec![occ_ref],
            vec![],
        );

        // Sanity: B is invalidated before removal
        assert_eq!(
            db.invalidated_files_for(&["lip://local/a.rs#foo".into()]),
            vec![uri_b.clone()],
        );

        // Remove B — its consumed-names entry should be cleaned up
        db.remove_file(&uri_b);

        let invalidated = db.invalidated_files_for(&["lip://local/a.rs#foo".into()]);
        assert!(
            invalidated.is_empty(),
            "removed file must not appear in invalidation results"
        );
    }

    // v2.3.4 — ImpactItem.module_id surfaces on blast-radius results when
    // the file was imported via SCIP with a parseable package descriptor.
    #[test]
    fn blast_radius_surfaces_module_id_from_scip_descriptor() {
        use crate::schema::{OwnedRange, OwnedSymbolInfo, SymbolKind};

        let caller_uri = "lip://local//abs/caller.rs".to_owned();
        let target_uri = "lip://local//abs/target.rs".to_owned();
        // SCIP-descriptor-form symbols: the package component ("cargo my-crate")
        // is what `resolve_module_id` tier-2 parses.
        let sym_caller = "scip-rs cargo my-crate 0.1.0 caller.rs#caller().".to_owned();
        let sym_target = "scip-rs cargo my-crate 0.1.0 target.rs#target().".to_owned();

        let mk_sym = |uri: &str| OwnedSymbolInfo {
            uri: uri.to_owned(),
            kind: SymbolKind::Function,
            display_name: uri
                .rsplit('#')
                .next()
                .unwrap_or("")
                .trim_end_matches("().")
                .to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        };
        let defn_occ = |sym: &str| OwnedOccurrence {
            symbol_uri: sym.to_owned(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        let edge = OwnedGraphEdge {
            from_uri: sym_caller.clone(),
            to_uri: sym_target.clone(),
            kind: EdgeKind::Calls,
            at_range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
        };

        let mut db = LipDatabase::new();
        db.upsert_file_precomputed(
            caller_uri.clone(),
            "rust".to_owned(),
            "c1".to_owned(),
            vec![mk_sym(&sym_caller)],
            vec![defn_occ(&sym_caller)],
            vec![edge],
        );
        db.upsert_file_precomputed(
            target_uri.clone(),
            "rust".to_owned(),
            "t1".to_owned(),
            vec![mk_sym(&sym_target)],
            vec![defn_occ(&sym_target)],
            vec![],
        );

        let result = db.blast_radius_for(&sym_target);
        let direct = result
            .direct_items
            .iter()
            .find(|i| i.file_uri == caller_uri)
            .expect("caller should appear as direct impact");
        assert_eq!(
            direct.module_id.as_deref(),
            Some("cargo/my-crate"),
            "module_id must be derived from the SCIP package descriptor; got {:?}",
            direct.module_id
        );
    }

    // v2.3.4 — when no SCIP or slice metadata is present, the manifest walk
    // fills module_id from Cargo.toml for tier-1-indexed Rust files.
    #[test]
    fn blast_radius_surfaces_module_id_from_cargo_toml_walk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-local-crate\"\n",
        )
        .unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let caller_path = src_dir.join("caller.rs");
        let target_path = src_dir.join("target.rs");
        std::fs::write(&caller_path, "fn caller() { target(); }\n").unwrap();
        std::fs::write(&target_path, "pub fn target() {}\n").unwrap();
        let caller_uri = format!(
            "lip://local/{}",
            caller_path.to_string_lossy().trim_start_matches('/')
        );
        let target_uri = format!(
            "lip://local/{}",
            target_path.to_string_lossy().trim_start_matches('/')
        );
        let caller_uri = format!("lip://local//{}", &caller_uri["lip://local/".len()..]);
        let target_uri = format!("lip://local//{}", &target_uri["lip://local/".len()..]);

        let mut db = LipDatabase::new();
        db.upsert_file(
            target_uri.clone(),
            std::fs::read_to_string(&target_path).unwrap(),
            "rust".to_owned(),
        );
        db.upsert_file(
            caller_uri.clone(),
            std::fs::read_to_string(&caller_path).unwrap(),
            "rust".to_owned(),
        );

        // Look up target's symbol — the tier-1 extractor emits `#target`.
        let sym_target = format!("{target_uri}#target");
        let result = db.blast_radius_for(&sym_target);
        let all_items: Vec<_> = result
            .direct_items
            .iter()
            .chain(result.transitive_items.iter())
            .collect();
        assert!(
            !all_items.is_empty(),
            "tier-1 blast radius should include caller.rs"
        );
        // Every item in this test comes from a file under the crate root,
        // so all module_ids must resolve to "my-local-crate".
        for item in &all_items {
            assert_eq!(
                item.module_id.as_deref(),
                Some("my-local-crate"),
                "manifest-walk should fill module_id for tier-1-indexed Rust files; got {:?}",
                item
            );
        }
    }

    // v2.3.4 — the forward twin (QueryOutgoingImpact) also fills module_id,
    // confirming the lookup is symmetric with blast radius.
    #[test]
    fn outgoing_impact_surfaces_module_id() {
        use crate::schema::{OwnedGraphEdge, OwnedRange, OwnedSymbolInfo, SymbolKind};

        let root_uri = "lip://local//abs/root.rs".to_owned();
        let leaf_uri = "lip://local//abs/leaf.rs".to_owned();
        // SCIP descriptors differ per file so we can verify the lookup is
        // per-file, not per-query.
        let sym_root = "scip-rs cargo pkg-a 0.1.0 root.rs#root().".to_owned();
        let sym_leaf = "scip-rs cargo pkg-b 0.1.0 leaf.rs#leaf().".to_owned();

        let mk_sym = |uri: &str, name: &str| OwnedSymbolInfo {
            uri: uri.to_owned(),
            kind: SymbolKind::Function,
            display_name: name.to_owned(),
            confidence_score: 90,
            is_exported: true,
            ..Default::default()
        };
        let defn_occ = |sym: &str| OwnedOccurrence {
            symbol_uri: sym.to_owned(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
            confidence_score: 90,
            role: Role::Definition,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        let edge = OwnedGraphEdge {
            from_uri: sym_root.clone(),
            to_uri: sym_leaf.clone(),
            kind: EdgeKind::Calls,
            at_range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
        };

        let mut db = LipDatabase::new();
        db.upsert_file_precomputed(
            root_uri.clone(),
            "rust".to_owned(),
            "r1".to_owned(),
            vec![mk_sym(&sym_root, "root")],
            vec![defn_occ(&sym_root)],
            vec![edge],
        );
        db.upsert_file_precomputed(
            leaf_uri.clone(),
            "rust".to_owned(),
            "l1".to_owned(),
            vec![mk_sym(&sym_leaf, "leaf")],
            vec![defn_occ(&sym_leaf)],
            vec![],
        );

        let result = db
            .outgoing_impact_for(&sym_root, Some(4), None)
            .expect("root resolves");
        let leaf_item = result
            .static_result
            .direct_items
            .iter()
            .find(|i| i.file_uri == leaf_uri)
            .expect("leaf is a direct callee");
        assert_eq!(
            leaf_item.module_id.as_deref(),
            Some("cargo/pkg-b"),
            "outgoing_impact must carry the callee's module_id; got {:?}",
            leaf_item.module_id
        );
    }
}
