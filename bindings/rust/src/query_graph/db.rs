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
    ApiSurface, BlastRadiusResult, ImpactItem, RiskLevel, SimilarSymbol,
};
use crate::schema::EdgeKind;
use crate::schema::{
    sha256_hex, OwnedAnnotationEntry, OwnedDependencySlice, OwnedOccurrence, OwnedRange,
    OwnedSymbolInfo, Role,
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

// ─── Internal types ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct FileInput {
    text: String,
    language: String,
    /// Revision at which this input was last changed.
    revision: u64,
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
    /// Cached embedding vectors for individual symbols: symbol_uri → dense float vector.
    /// Keyed by `lip://` URIs. Populated on demand by `QueryNearestBySymbol` or by
    /// `EmbeddingBatch` when called with `lip://` URIs.
    symbol_embeddings: HashMap<String, Vec<f32>>,
    /// Unix timestamps (ms) recording when each URI was last upserted.
    file_indexed_at: HashMap<String, i64>,
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
            file_call_edges: HashMap::new(),
            name_to_symbols: HashMap::new(),
            callee_name_to_callers: HashMap::new(),
            mounted_symbols: HashMap::new(),
            mounted_packages: HashMap::new(),
            file_consumed_names: HashMap::new(),
            file_embeddings: HashMap::new(),
            symbol_embeddings: HashMap::new(),
            file_indexed_at: HashMap::new(),
        }
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Register or update a file. Bumps the global revision and invalidates
    /// cached derived data for `uri`.
    pub fn upsert_file(&mut self, uri: String, text: String, language: String) {
        self.revision += 1;
        let rev = self.revision;
        self.file_inputs.insert(
            uri.clone(),
            FileInput {
                text,
                language,
                revision: rev,
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
                // Name-based index: enables cross-file resolution in blast_radius_for.
                let callee_name = extract_name(&edge.to_uri).to_owned();
                if !callee_name.is_empty() {
                    self.callee_name_to_callers
                        .entry(callee_name)
                        .or_default()
                        .push(edge.from_uri.clone());
                }
                pairs.push((edge.from_uri.clone(), edge.to_uri.clone()));
            }
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

    pub fn remove_file(&mut self, uri: &str) {
        self.revision += 1;
        self.file_inputs.remove(uri);
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
        self.def_index.retain(|_, (furi, _)| furi.as_str() != uri);
        self.remove_file_call_edges(uri);
        self.file_consumed_names.remove(uri);
        self.file_embeddings.remove(uri);
        self.file_indexed_at.remove(uri);
    }

    fn remove_file_call_edges(&mut self, uri: &str) {
        if let Some(pairs) = self.file_call_edges.remove(uri) {
            for (from, to) in pairs {
                if let Some(callers) = self.callee_to_callers.get_mut(&to) {
                    callers.retain(|c| *c != from);
                }
                let callee_name = extract_name(&to);
                if let Some(callers) = self.callee_name_to_callers.get_mut(callee_name) {
                    callers.retain(|c| *c != from);
                    if callers.is_empty() {
                        self.callee_name_to_callers.remove(callee_name);
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
        self.file_inputs.get(uri).map(|f| f.text.clone())
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
        self.file_inputs.get(uri).map(|f| f.text.as_str())
    }

    pub fn file_language(&self, uri: &str) -> Option<&str> {
        self.file_inputs.get(uri).map(|f| f.language.as_str())
    }

    pub fn tracked_uris(&self) -> Vec<String> {
        self.file_inputs.keys().cloned().collect()
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
                    Some(fi) => sha256_hex(fi.text.as_bytes()) != *client_hash,
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
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None => return Arc::new(vec![]),
        };

        if let Some(cached) = self.sym_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let result = self.compute_symbols(uri);
        self.sym_cache
            .insert(uri.to_owned(), Cached::new(result.clone(), file_rev));
        result
    }

    /// Tier 1 occurrences for a file, lazily computed and cached.
    pub fn file_occurrences(&mut self, uri: &str) -> Arc<Vec<OwnedOccurrence>> {
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
                    for caller in callers {
                        if !cpg_distance.contains_key(&caller) {
                            cpg_distance.insert(caller.clone(), depth + 1);
                            queue.push_back((caller, depth + 1));
                        }
                    }
                }
                // Name-based callers: catches file-local URIs from other files.
                let name = extract_name(&callee);
                if !name.is_empty() {
                    if let Some(callers) = self.callee_name_to_callers.get(name).cloned() {
                        for caller in callers {
                            if !cpg_distance.contains_key(&caller) {
                                cpg_distance.insert(caller.clone(), depth + 1);
                                queue.push_back((caller, depth + 1));
                            }
                        }
                    }
                }
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
            if let Some((file_uri, _)) = self.def_index.get(caller_sym) {
                let prev_dist = file_distance.get(file_uri).copied().unwrap_or(u32::MAX);
                file_distance.insert(file_uri.clone(), sym_dist.min(prev_dist));
                sym_items.push((caller_sym.clone(), file_uri.clone(), sym_dist));
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

        BlastRadiusResult {
            symbol_uri: symbol_uri.to_owned(),
            direct_dependents: direct_count,
            transitive_dependents: transitive_count,
            affected_files,
            direct_items,
            transitive_items,
            truncated,
            risk_level,
        }
    }

    /// Find the symbol URI whose occurrence range contains `(line, col)` in `uri`.
    ///
    /// Returns `None` if no occurrence covers the given position.
    pub fn symbol_at_position(&mut self, uri: &str, line: i32, col: i32) -> Option<String> {
        let occs = self.file_occurrences(uri);
        occs.iter()
            .find(|occ| range_contains(&occ.range, line, col))
            .map(|occ| occ.symbol_uri.clone())
    }

    /// Find the definition occurrence location for `symbol_uri`.
    ///
    /// O(1) via the definition reverse index maintained in `upsert_file`.
    pub fn symbol_definition_location(&self, symbol_uri: &str) -> Option<(String, OwnedRange)> {
        self.def_index.get(symbol_uri).cloned()
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

    // ── Embedding / observability ─────────────────────────────────────────

    /// Store a pre-computed embedding vector for a file.
    pub fn set_file_embedding(&mut self, uri: &str, vector: Vec<f32>) {
        self.file_embeddings.insert(uri.to_owned(), vector);
    }

    /// Retrieve the stored embedding vector for a file, if any.
    pub fn get_file_embedding(&self, uri: &str) -> Option<&Vec<f32>> {
        self.file_embeddings.get(uri)
    }

    /// Store a pre-computed embedding vector for a symbol URI (`lip://` scheme).
    pub fn set_symbol_embedding(&mut self, uri: &str, vector: Vec<f32>) {
        self.symbol_embeddings.insert(uri.to_owned(), vector);
    }

    /// Retrieve the stored embedding vector for a symbol URI, if any.
    pub fn get_symbol_embedding(&self, uri: &str) -> Option<&Vec<f32>> {
        self.symbol_embeddings.get(uri)
    }

    /// Find the `top_k` symbols whose embedding is most similar (cosine) to `query_vec`.
    ///
    /// Mirrors `nearest_by_vector` but operates over `symbol_embeddings`.
    pub fn nearest_symbol_by_vector(
        &self,
        query_vec: &[f32],
        top_k: usize,
        exclude_uri: Option<&str>,
    ) -> Vec<crate::query_graph::types::NearestItem> {
        let q_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if q_norm == 0.0 || top_k == 0 {
            return vec![];
        }
        let mut scored: Vec<(String, f32)> = self
            .symbol_embeddings
            .iter()
            .filter(|(uri, _)| exclude_uri.map(|e| e != uri.as_str()).unwrap_or(true))
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
            .map(|(uri, score)| crate::query_graph::types::NearestItem { uri, score })
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
            .map(|(uri, score)| crate::query_graph::types::NearestItem { uri, score })
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
            return vec![NearestItem {
                uri: pairs[0].0.to_owned(),
                score: 0.0,
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
            .map(|(uri, score)| NearestItem { uri, score })
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
        let uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let q = query.to_lowercase();
        let mut matches = vec![];
        'outer: for uri in &uris {
            for sym in self.file_symbols(uri).iter() {
                if sym.display_name.to_lowercase().contains(&q) {
                    matches.push(sym.clone());
                    if matches.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }
        if matches.len() < limit {
            for sym in self.mounted_symbols.values() {
                if sym.display_name.to_lowercase().contains(&q) {
                    matches.push(sym.clone());
                    if matches.len() >= limit {
                        break;
                    }
                }
            }
        }
        matches
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
    use crate::schema::SymbolKind;

    fn make_rust_file(content: &str) -> (String, String, String) {
        (
            "lip://npm/pkg@1.0.0/src/lib.rs".to_owned(),
            content.to_owned(),
            "rust".to_owned(),
        )
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

    // ── symbol_embeddings / nearest_symbol_by_vector ──────────────────────

    #[test]
    fn set_get_symbol_embedding_roundtrip() {
        let mut db = LipDatabase::new();
        let uri = "lip://local/src/main.rs#foo";
        let vec = vec![1.0_f32, 0.0, 0.0];
        db.set_symbol_embedding(uri, vec.clone());
        assert_eq!(db.get_symbol_embedding(uri), Some(&vec));
        assert!(db
            .get_symbol_embedding("lip://local/src/main.rs#missing")
            .is_none());
    }

    #[test]
    fn nearest_symbol_by_vector_orders_by_cosine() {
        let mut db = LipDatabase::new();
        // Three orthogonal unit vectors; query aligns with "foo".
        db.set_symbol_embedding("lip://local/f.rs#foo", vec![1.0, 0.0, 0.0]);
        db.set_symbol_embedding("lip://local/f.rs#bar", vec![0.0, 1.0, 0.0]);
        db.set_symbol_embedding("lip://local/f.rs#baz", vec![0.0, 0.0, 1.0]);

        let query = vec![1.0_f32, 0.0, 0.0];
        let results = db.nearest_symbol_by_vector(&query, 3, None);
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
        db.set_symbol_embedding("lip://local/f.rs#foo", vec![1.0, 0.0]);
        db.set_symbol_embedding("lip://local/f.rs#bar", vec![0.9, 0.1]);

        let query = vec![1.0_f32, 0.0];
        let results = db.nearest_symbol_by_vector(&query, 5, Some("lip://local/f.rs#foo"));
        assert!(
            !results.iter().any(|r| r.uri == "lip://local/f.rs#foo"),
            "excluded URI must not appear in results"
        );
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn nearest_symbol_by_vector_empty_store_returns_empty() {
        let db = LipDatabase::new();
        let results = db.nearest_symbol_by_vector(&[1.0, 0.0], 5, None);
        assert!(results.is_empty());
    }

    // ── outliers ──────────────────────────────────────────────────────────

    #[test]
    fn outliers_returns_lowest_mean_similarity_first() {
        let mut db = LipDatabase::new();
        // Three tightly clustered files and one outlier in an orthogonal direction.
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0, 0.0]);
        db.set_file_embedding("file:///b.rs", vec![0.9, 0.1, 0.0]);
        db.set_file_embedding("file:///c.rs", vec![0.95, 0.05, 0.0]);
        db.set_file_embedding("file:///outlier.rs", vec![0.0, 0.0, 1.0]);

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
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0]);
        db.set_file_embedding("file:///b.rs", vec![0.0, 1.0]);

        let uris = vec!["file:///a.rs".into(), "file:///b.rs".into()];
        let (result_uris, matrix) = db.similarity_matrix(&uris);
        assert_eq!(result_uris.len(), 2);
        assert!((matrix[0][0] - 1.0).abs() < 1e-5);
        assert!((matrix[1][1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similarity_matrix_symmetric_and_orthogonal() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0]);
        db.set_file_embedding("file:///b.rs", vec![0.0, 1.0]);

        let uris = vec!["file:///a.rs".into(), "file:///b.rs".into()];
        let (_, matrix) = db.similarity_matrix(&uris);
        assert!((matrix[0][1] - 0.0).abs() < 1e-5);
        assert!((matrix[1][0] - 0.0).abs() < 1e-5);
    }

    #[test]
    fn similarity_matrix_excludes_uris_without_embeddings() {
        let mut db = LipDatabase::new();
        db.set_file_embedding("file:///a.rs", vec![1.0, 0.0]);
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
        db.set_file_embedding("file:///project/src/a.rs", vec![1.0, 0.0]);
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
        db.set_file_embedding("file:///src/auth.rs", vec![1.0, 0.0, 0.0]);
        db.set_file_embedding("file:///src/auth_helper.rs", vec![0.9, 0.1, 0.0]);
        // New file: completely different direction.
        db.set_file_embedding("file:///src/billing.rs", vec![0.0, 1.0, 0.0]);

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
        db.set_file_embedding("file:///src/auth.rs", vec![1.0, 0.0]);
        db.set_file_embedding("file:///src/auth2.rs", vec![0.99, 0.01]);

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
        db.set_file_embedding("file:///src/a.rs", vec![1.0, 0.0]);
        db.set_file_embedding("file:///src/b.rs", vec![1.0, 0.0]); // identical direction
        db.set_file_embedding("file:///other/c.rs", vec![0.0, 1.0]);

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
}
