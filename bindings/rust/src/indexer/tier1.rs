use tree_sitter::Parser;

use crate::schema::{sha256_hex, OwnedDocument, OwnedGraphEdge, OwnedOccurrence, OwnedSymbolInfo};

use super::{language::Language, symbol_extractor::SymbolExtractor};

/// Tier 1 indexer: tree-sitter based, < 10 ms per file, confidence 1–50.
///
/// `Parser` is `!Sync`, so `Tier1Indexer` must be used from a single thread
/// or wrapped in a `Mutex`. The daemon spawns indexing tasks via
/// `tokio::task::spawn_blocking`.
pub struct Tier1Indexer {
    parser: Parser,
}

impl Tier1Indexer {
    pub fn new() -> Self {
        Self { parser: Parser::new() }
    }

    /// Extract symbols from source text.
    pub fn symbols_for_source(
        &mut self,
        uri: &str,
        source: &str,
        language: Language,
    ) -> Vec<OwnedSymbolInfo> {
        let Some(grammar) = language.tree_sitter_grammar() else {
            return vec![];
        };
        if self.parser.set_language(&grammar).is_err() {
            return vec![];
        }
        let Some(tree) = self.parser.parse(source, None) else {
            return vec![];
        };
        let extractor = SymbolExtractor::new(source.as_bytes(), language, uri);
        extractor.extract_symbols(&tree)
    }

    /// Extract CPG call edges from source text.
    pub fn edges_for_source(
        &mut self,
        uri: &str,
        source: &str,
        language: Language,
    ) -> Vec<OwnedGraphEdge> {
        let Some(grammar) = language.tree_sitter_grammar() else {
            return vec![];
        };
        if self.parser.set_language(&grammar).is_err() {
            return vec![];
        }
        let Some(tree) = self.parser.parse(source, None) else {
            return vec![];
        };
        let extractor = SymbolExtractor::new(source.as_bytes(), language, uri);
        extractor.extract_edges(&tree)
    }

    /// Extract occurrences (all identifier uses) from source text.
    pub fn occurrences_for_source(
        &mut self,
        uri: &str,
        source: &str,
        language: Language,
    ) -> Vec<OwnedOccurrence> {
        let Some(grammar) = language.tree_sitter_grammar() else {
            return vec![];
        };
        if self.parser.set_language(&grammar).is_err() {
            return vec![];
        }
        let Some(tree) = self.parser.parse(source, None) else {
            return vec![];
        };
        let extractor = SymbolExtractor::new(source.as_bytes(), language, uri);
        extractor.extract_occurrences(&tree)
    }

    /// Index a full file and produce an `OwnedDocument`.
    pub fn index_file(
        &mut self,
        uri: &str,
        source: &str,
        language: Language,
    ) -> OwnedDocument {
        let content_hash = sha256_hex(source.as_bytes());
        let symbols     = self.symbols_for_source(uri, source, language);
        let occurrences = self.occurrences_for_source(uri, source, language);
        let edges       = self.edges_for_source(uri, source, language);

        OwnedDocument {
            uri:          uri.to_owned(),
            content_hash,
            language:     language.as_str().to_owned(),
            occurrences,
            symbols,
            merkle_path:  uri.to_owned(),
            edges,
            source_text:  Some(source.to_owned()),
        }
    }
}

impl Default for Tier1Indexer {
    fn default() -> Self {
        Self::new()
    }
}
