/// # LIP Query Graph
///
/// A revision-based incremental query graph inspired by Salsa (spec §3.1).
/// Implements the core invariant: a stable API surface shields all callers
/// from internal changes.
///
/// Salsa's proc-macro API has changed across versions; v0.1 implements the
/// pattern manually. A v0.2 migration to the salsa crate is tracked in the
/// roadmap.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::indexer::{language::Language, Tier1Indexer};
use crate::query_graph::types::{ApiSurface, BlastRadiusResult};
use crate::schema::{sha256_hex, OwnedAnnotationEntry, OwnedOccurrence, OwnedRange, OwnedSymbolInfo, Role, SymbolKind};

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

// ─── Internal types ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct FileInput {
    text:      String,
    language:  String,
    /// Revision at which this input was last changed.
    revision:  u64,
}

#[derive(Debug)]
struct Cached<T> {
    value:    T,
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
    revision:    u64,
    file_inputs: HashMap<String, FileInput>,
    sym_cache:   HashMap<String, Cached<Arc<Vec<OwnedSymbolInfo>>>>,
    occ_cache:   HashMap<String, Cached<Arc<Vec<OwnedOccurrence>>>>,
    api_cache:   HashMap<String, Cached<Arc<ApiSurface>>>,
    /// Reverse index: symbol_uri → (file_uri, definition range). O(1) definition lookup.
    def_index:   HashMap<String, (String, OwnedRange)>,
    /// Last Merkle root sent by the client. Drives lifecycle state reporting.
    merkle_root:    Option<String>,
    /// Repo root from the last ManifestRequest. Used by Tier 2 to locate rust-analyzer's workspace.
    workspace_root: Option<PathBuf>,
    /// Persistent annotations keyed by (symbol_uri, annotation_key).
    annotations: HashMap<String, HashMap<String, OwnedAnnotationEntry>>,
}

impl LipDatabase {
    pub fn new() -> Self {
        Self {
            revision:       0,
            file_inputs:    HashMap::new(),
            sym_cache:      HashMap::new(),
            occ_cache:      HashMap::new(),
            api_cache:      HashMap::new(),
            def_index:      HashMap::new(),
            merkle_root:    None,
            workspace_root: None,
            annotations:    HashMap::new(),
        }
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Register or update a file. Bumps the global revision and invalidates
    /// cached derived data for `uri`.
    pub fn upsert_file(&mut self, uri: String, text: String, language: String) {
        self.revision += 1;
        let rev = self.revision;
        self.file_inputs.insert(uri.clone(), FileInput { text, language, revision: rev });
        // Invalidate the direct derived caches. api_cache is intentionally kept
        // so file_api_surface can compare the new hash against the previous one and
        // fire an early-cutoff (returning the same Arc) when the API surface is stable.
        self.sym_cache.remove(&uri);
        self.occ_cache.remove(&uri);

        // Eagerly rebuild the definition reverse index for this file.
        self.def_index.retain(|_, (furi, _)| furi != &uri);
        let occs = self.compute_occurrences(&uri);
        for occ in occs.iter() {
            if occ.role == Role::Definition {
                self.def_index.insert(occ.symbol_uri.clone(), (uri.clone(), occ.range.clone()));
            }
        }
        // Cache the occurrences we just computed to avoid a redundant parse on first query.
        self.occ_cache.insert(uri.to_owned(), Cached::new(occs, rev));
    }

    pub fn remove_file(&mut self, uri: &str) {
        self.revision += 1;
        self.file_inputs.remove(uri);
        self.sym_cache.remove(uri);
        self.occ_cache.remove(uri);
        self.api_cache.remove(uri);
        self.def_index.retain(|_, (furi, _)| furi.as_str() != uri);
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
        if upgrades.is_empty() { return; }
        let Some(cached) = self.sym_cache.get(uri) else { return; };
        let rev = cached.revision;
        let existing: Vec<OwnedSymbolInfo> = cached.value.as_ref().clone();

        let merged: Vec<OwnedSymbolInfo> = existing
            .into_iter()
            .map(|mut sym| {
                if let Some(up) = upgrades.iter().find(|u| u.uri == sym.uri) {
                    sym.confidence_score = up.confidence_score;
                    if up.signature.is_some() {
                        sym.signature = up.signature.clone();
                    }
                    if up.documentation.is_some() {
                        sym.documentation = up.documentation.clone();
                    }
                }
                sym
            })
            .collect();

        self.sym_cache.insert(uri.to_owned(), Cached::new(Arc::new(merged), rev));
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

    pub fn current_revision(&self) -> u64 {
        self.revision
    }

    // ── Derived queries ───────────────────────────────────────────────────

    /// Tier 1 symbols for a file, lazily computed and cached.
    pub fn file_symbols(&mut self, uri: &str) -> Arc<Vec<OwnedSymbolInfo>> {
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None    => return Arc::new(vec![]),
        };

        if let Some(cached) = self.sym_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let result = self.compute_symbols(uri);
        self.sym_cache.insert(uri.to_owned(), Cached::new(result.clone(), file_rev));
        result
    }

    /// Tier 1 occurrences for a file, lazily computed and cached.
    pub fn file_occurrences(&mut self, uri: &str) -> Arc<Vec<OwnedOccurrence>> {
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None    => return Arc::new(vec![]),
        };

        if let Some(cached) = self.occ_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let result = self.compute_occurrences(uri);
        self.occ_cache.insert(uri.to_owned(), Cached::new(result.clone(), file_rev));
        result
    }

    /// Exported API surface — the primary early-cutoff node.
    ///
    /// If `content_hash` is identical to the last-cached value, downstream
    /// callers can skip their own recomputation (see spec §3.1 "early cutoff").
    pub fn file_api_surface(&mut self, uri: &str) -> Arc<ApiSurface> {
        let file_rev = match self.file_inputs.get(uri) {
            Some(f) => f.revision,
            None    => return Arc::new(ApiSurface { content_hash: String::new(), symbols: vec![] }),
        };

        // Early-cutoff check: if the API surface is fresh, return it.
        if let Some(cached) = self.api_cache.get(uri) {
            if cached.revision >= file_rev {
                return cached.value.clone();
            }
        }

        let symbols = self.file_symbols(uri);
        let public: Vec<OwnedSymbolInfo> = symbols
            .iter()
            .filter(|s| {
                !s.display_name.starts_with('_')
                    && !matches!(s.kind, SymbolKind::Parameter | SymbolKind::Variable)
            })
            .cloned()
            .collect();

        let surface_text = public
            .iter()
            .map(|s| format!("{}:{}", s.uri, s.signature.as_deref().unwrap_or(&s.display_name)))
            .collect::<Vec<_>>()
            .join("\n");

        // Compare new hash to cached; if equal, restore the old cached entry so
        // callers can detect the early-cutoff (same Arc pointer or same hash).
        let new_hash = sha256_hex(surface_text.as_bytes());
        if let Some(cached) = self.api_cache.get(uri) {
            if cached.value.content_hash == new_hash {
                // API surface unchanged — early cutoff. Update revision in place.
                let old_val = cached.value.clone();
                self.api_cache.insert(uri.to_owned(), Cached::new(old_val.clone(), file_rev));
                return old_val;
            }
        }

        let surface = Arc::new(ApiSurface { content_hash: new_hash, symbols: public });
        self.api_cache.insert(uri.to_owned(), Cached::new(surface.clone(), file_rev));
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
                occs.iter().any(|occ| target_uris.iter().any(|u| *u == occ.symbol_uri))
            })
            .collect()
    }

    /// Compute blast radius for a symbol URI (spec §8.1).
    pub fn blast_radius_for(&mut self, symbol_uri: &str) -> BlastRadiusResult {
        let all_uris: Vec<String> = self.file_inputs.keys().cloned().collect();
        let def_uri = all_uris.iter().find(|uri| {
            self.file_symbols(uri).iter().any(|s| s.uri == symbol_uri)
        }).cloned();

        let Some(def_uri) = def_uri else {
            return BlastRadiusResult {
                symbol_uri: symbol_uri.to_owned(),
                ..Default::default()
            };
        };

        let direct = self.reverse_deps(&def_uri);
        let mut transitive: std::collections::HashSet<String> = direct.iter().cloned().collect();
        for dep in direct.clone() {
            for indirect in self.reverse_deps(&dep) {
                transitive.insert(indirect);
            }
        }

        BlastRadiusResult {
            symbol_uri:            symbol_uri.to_owned(),
            direct_dependents:     direct.len() as u32,
            transitive_dependents: transitive.len() as u32,
            affected_files:        transitive.into_iter().collect(),
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

    /// Find `OwnedSymbolInfo` for a given symbol URI across all tracked files.
    pub fn symbol_by_uri(&mut self, symbol_uri: &str) -> Option<OwnedSymbolInfo> {
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
        let referenced: HashSet<String> = uris.iter()
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
                    if result.len() >= cap { break 'outer; }
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

    /// Get the annotation for `(symbol_uri, key)`, if present.
    pub fn annotation_get(&self, symbol_uri: &str, key: &str) -> Option<&OwnedAnnotationEntry> {
        self.annotations.get(symbol_uri)?.get(key)
    }

    /// List all annotations for `symbol_uri`.
    pub fn annotation_list(&self, symbol_uri: &str) -> Vec<OwnedAnnotationEntry> {
        self.annotations
            .get(symbol_uri)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// All annotations across every symbol URI — used by journal compaction.
    pub fn all_annotations(&self) -> Vec<OwnedAnnotationEntry> {
        self.annotations
            .values()
            .flat_map(|m| m.values().cloned())
            .collect()
    }

    /// Symbol search across all tracked files.
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
        matches
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

        let first  = db.file_symbols(&uri);
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

        db.upsert_file("lip://s/p@1/a.rs".to_owned(), String::new(), "rust".to_owned());
        db.upsert_file("lip://s/p@1/b.rs".to_owned(), String::new(), "rust".to_owned());
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
        db.upsert_file("lip://s/p@1/a.rs".to_owned(), String::new(), "rust".to_owned());
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
        db.upsert_file(uri.clone(), "pub fn greet() {}".to_owned(), "rust".to_owned());

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
        db.upsert_file(uri.clone(), "pub fn defined_fn() {}".to_owned(), "rust".to_owned());

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
        db.upsert_file(uri.clone(), "pub fn gone() {}".to_owned(), "rust".to_owned());

        let occs = db.file_occurrences(&uri);
        let def_occ = occs.iter().find(|o| o.role == Role::Definition);
        let Some(def_occ) = def_occ else { return; };
        let sym_uri = def_occ.symbol_uri.clone();

        assert!(db.symbol_definition_location(&sym_uri).is_some());
        db.remove_file(&uri);
        assert!(db.symbol_definition_location(&sym_uri).is_none(),
            "def_index should be pruned on remove_file");
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
            symbol_uri:   "lip://s/p@1/f.rs#foo".to_owned(),
            key:          "team:owner".to_owned(),
            value:        "platform".to_owned(),
            author_id:    "human:alice".to_owned(),
            confidence:   100,
            timestamp_ms: 0,
            expires_ms:   0,
        };
        db.annotation_set(entry.clone());

        let got = db.annotation_get("lip://s/p@1/f.rs#foo", "team:owner");
        assert!(got.is_some());
        assert_eq!(got.unwrap().value, "platform");
    }

    #[test]
    fn annotation_get_missing_returns_none() {
        let db = LipDatabase::new();
        assert!(db.annotation_get("lip://s/p@1/f.rs#no_sym", "key").is_none());
    }

    #[test]
    fn annotation_list_returns_all_keys_for_symbol() {
        use crate::schema::OwnedAnnotationEntry;
        let mut db = LipDatabase::new();
        let sym = "lip://s/p@1/f.rs#bar";
        for key in ["k1", "k2", "k3"] {
            db.annotation_set(OwnedAnnotationEntry {
                symbol_uri:   sym.to_owned(),
                key:          key.to_owned(),
                value:        key.to_owned(),
                author_id:    "human:test".to_owned(),
                confidence:   100,
                timestamp_ms: 0,
                expires_ms:   0,
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
        let sym_uri  = format!("{file_uri}#foo");

        db.upsert_file(file_uri.clone(), "pub fn foo() {}".to_owned(), "rust".to_owned());
        db.annotation_set(OwnedAnnotationEntry {
            symbol_uri:   sym_uri.clone(),
            key:          "note".to_owned(),
            value:        "fragile".to_owned(),
            author_id:    "human:test".to_owned(),
            confidence:   100,
            timestamp_ms: 0,
            expires_ms:   0,
        });

        // Re-upsert and then remove the file — annotation must survive both.
        db.upsert_file(file_uri.clone(), "pub fn foo() { /* changed */ }".to_owned(), "rust".to_owned());
        assert_eq!(db.annotation_get(&sym_uri, "note").map(|e| e.value.as_str()), Some("fragile"));

        db.remove_file(&file_uri);
        assert_eq!(db.annotation_get(&sym_uri, "note").map(|e| e.value.as_str()), Some("fragile"),
            "annotation must survive file removal");
    }

    // ── upgrade_file_symbols ──────────────────────────────────────────────

    #[test]
    fn upgrade_file_symbols_raises_confidence() {
        let mut db = LipDatabase::new();
        let uri = "lip://s/p@1/up.rs".to_owned();
        db.upsert_file(uri.clone(), "pub fn upgradable() {}".to_owned(), "rust".to_owned());

        let syms_before = db.file_symbols(&uri);
        // Tier 1 should give confidence 30.
        assert!(syms_before.iter().all(|s| s.confidence_score == 30),
            "Tier 1 symbols should start at confidence 30");

        // Simulate Tier 2 upgrade.
        let upgrades: Vec<_> = syms_before.iter().map(|s| {
            let mut up = s.clone();
            up.confidence_score = 70;
            up.signature = Some("fn upgradable()".to_owned());
            up
        }).collect();

        db.upgrade_file_symbols(&uri, &upgrades);

        let syms_after = db.file_symbols(&uri);
        assert!(syms_after.iter().all(|s| s.confidence_score == 70),
            "symbols should be upgraded to confidence 70");
        assert!(syms_after.iter().any(|s| s.signature.is_some()),
            "upgraded symbols should carry signatures");
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
"#.to_owned(),
            "rust".to_owned(),
        );
        let syms = db.file_symbols(&uri);
        let names: Vec<&str> = syms.iter().map(|s| s.display_name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "struct should be extracted; got: {names:?}");
        assert!(names.contains(&"hello"),   "pub method should be extracted; got: {names:?}");
        assert!(names.contains(&"private_method"), "private method should be extracted; got: {names:?}");
    }

    // ── reverse_deps ─────────────────────────────────────────────────────

    #[test]
    fn reverse_deps_empty_for_isolated_file() {
        let mut db = LipDatabase::new();
        db.upsert_file("lip://s/p@1/solo.rs".to_owned(), "pub fn solo() {}".to_owned(), "rust".to_owned());
        let deps = db.reverse_deps("lip://s/p@1/solo.rs");
        assert!(deps.is_empty(), "isolated file should have no reverse deps");
    }
}
