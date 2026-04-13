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
        Self {
            parser: Parser::new(),
        }
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
    pub fn index_file(&mut self, uri: &str, source: &str, language: Language) -> OwnedDocument {
        let content_hash = sha256_hex(source.as_bytes());
        let symbols = self.symbols_for_source(uri, source, language);
        let occurrences = self.occurrences_for_source(uri, source, language);
        let edges = self.edges_for_source(uri, source, language);

        OwnedDocument {
            uri: uri.to_owned(),
            content_hash,
            language: language.as_str().to_owned(),
            occurrences,
            symbols,
            merkle_path: uri.to_owned(),
            edges,
            source_text: Some(source.to_owned()),
        }
    }
}

impl Default for Tier1Indexer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EdgeKind, Role, SymbolKind};

    // ── helpers ───────────────────────────────────────────────────────────────

    fn sym(source: &str, lang: Language) -> Vec<OwnedSymbolInfo> {
        Tier1Indexer::new().symbols_for_source("file:///t", source, lang)
    }

    fn edges(source: &str, lang: Language) -> Vec<OwnedGraphEdge> {
        Tier1Indexer::new().edges_for_source("file:///t", source, lang)
    }

    fn occs(source: &str, lang: Language) -> Vec<OwnedOccurrence> {
        Tier1Indexer::new().occurrences_for_source("file:///t", source, lang)
    }

    fn names(syms: &[OwnedSymbolInfo]) -> Vec<&str> {
        syms.iter().map(|s| s.display_name.as_str()).collect()
    }

    fn find<'a>(syms: &'a [OwnedSymbolInfo], name: &str) -> &'a OwnedSymbolInfo {
        syms.iter()
            .find(|s| s.display_name == name)
            .unwrap_or_else(|| panic!("symbol '{name}' not found in {:?}", names(syms)))
    }

    // ── empty input ───────────────────────────────────────────────────────────

    #[test]
    fn empty_source_returns_empty() {
        assert!(sym("", Language::Rust).is_empty());
        assert!(sym("", Language::TypeScript).is_empty());
        assert!(sym("", Language::Python).is_empty());
        assert!(sym("", Language::Dart).is_empty());
    }

    #[test]
    fn unknown_language_returns_empty() {
        assert!(sym("pub fn foo() {}", Language::Unknown).is_empty());
    }

    // ── URI format ────────────────────────────────────────────────────────────

    #[test]
    fn uri_strips_file_scheme() {
        let syms = sym("pub fn greet() {}", Language::Rust);
        assert_eq!(syms.len(), 1);
        assert!(
            !syms[0].uri.contains("file://"),
            "lip URI must not re-embed the file:// scheme: {}",
            syms[0].uri
        );
        assert!(
            syms[0].uri.starts_with("lip://local/"),
            "unexpected URI: {}",
            syms[0].uri
        );
    }

    // ── Rust ──────────────────────────────────────────────────────────────────

    #[test]
    fn rust_fn_extracted() {
        let syms = sym("fn foo() {}", Language::Rust);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].display_name, "foo");
        assert_eq!(syms[0].kind, SymbolKind::Function);
        assert_eq!(syms[0].confidence_score, 30);
    }

    #[test]
    fn rust_pub_fn_is_exported() {
        let syms = sym("pub fn bar() {}", Language::Rust);
        assert!(find(&syms, "bar").is_exported, "pub fn should be exported");
    }

    #[test]
    fn rust_private_fn_not_exported() {
        let syms = sym("fn hidden() {}", Language::Rust);
        assert!(
            !find(&syms, "hidden").is_exported,
            "private fn should not be exported"
        );
    }

    #[test]
    fn rust_struct_kind() {
        let syms = sym("pub struct Point { x: i32 }", Language::Rust);
        let s = find(&syms, "Point");
        assert_eq!(s.kind, SymbolKind::Class);
        assert!(s.is_exported);
    }

    #[test]
    fn rust_enum_kind() {
        let syms = sym("enum Color { Red, Green }", Language::Rust);
        assert_eq!(find(&syms, "Color").kind, SymbolKind::Enum);
    }

    #[test]
    fn rust_trait_is_interface() {
        let syms = sym("pub trait Render {}", Language::Rust);
        let s = find(&syms, "Render");
        assert_eq!(s.kind, SymbolKind::Interface);
        assert!(s.is_exported);
    }

    #[test]
    fn rust_type_alias() {
        let syms = sym("type Meters = f64;", Language::Rust);
        assert_eq!(find(&syms, "Meters").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn rust_mod() {
        let syms = sym("pub mod utils {}", Language::Rust);
        let s = find(&syms, "utils");
        assert_eq!(s.kind, SymbolKind::Namespace);
        assert!(s.is_exported);
    }

    #[test]
    fn rust_macro_definition() {
        let syms = sym("macro_rules! vec_of { () => {} }", Language::Rust);
        assert_eq!(find(&syms, "vec_of").kind, SymbolKind::Macro);
    }

    #[test]
    fn rust_multiple_items() {
        let src = "pub fn alpha() {} struct Beta {} pub trait Gamma {}";
        let syms = sym(src, Language::Rust);
        let ns = names(&syms);
        assert!(ns.contains(&"alpha"));
        assert!(ns.contains(&"Beta"));
        assert!(ns.contains(&"Gamma"));
        assert_eq!(syms.len(), 3);
    }

    #[test]
    fn rust_cpg_call_edge() {
        let src = "fn caller() { callee(); } fn callee() {}";
        let es = edges(src, Language::Rust);
        assert!(
            es.iter().any(|e| {
                e.from_uri.contains("#caller")
                    && e.to_uri.contains("#callee")
                    && e.kind == EdgeKind::Calls
            }),
            "expected caller→callee edge, got: {es:?}"
        );
    }

    #[test]
    fn rust_call_edge_field_method() {
        // method call via field expression: self.helper()
        let src = "fn process(&self) { self.helper(); } fn helper(&self) {}";
        let es = edges(src, Language::Rust);
        assert!(
            es.iter().any(|e| e.to_uri.contains("#helper")),
            "field method call should produce a Calls edge"
        );
    }

    #[test]
    fn rust_occurrence_definition_role() {
        let occs_list = occs("pub fn defined() {}", Language::Rust);
        let def = occs_list
            .iter()
            .find(|o| o.symbol_uri.contains("#defined"))
            .unwrap();
        assert_eq!(def.role, Role::Definition);
    }

    #[test]
    fn rust_occurrence_reference_role() {
        let src = "fn a() { b(); } fn b() {}";
        let occs_list = occs(src, Language::Rust);
        // `b` appears as a reference inside `a()`'s body
        let refs: Vec<_> = occs_list
            .iter()
            .filter(|o| o.symbol_uri.contains("#b") && o.role == Role::Reference)
            .collect();
        assert!(
            !refs.is_empty(),
            "b used in a() should be a Reference occurrence"
        );
    }

    // ── TypeScript ────────────────────────────────────────────────────────────

    #[test]
    fn ts_function_declaration() {
        let syms = sym("function greet() {}", Language::TypeScript);
        let s = find(&syms, "greet");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
        assert!(!s.is_exported);
    }

    #[test]
    fn ts_exported_function() {
        let syms = sym("export function send() {}", Language::TypeScript);
        assert!(
            find(&syms, "send").is_exported,
            "export function should be exported"
        );
    }

    #[test]
    fn ts_class_declaration() {
        let syms = sym("class Service {}", Language::TypeScript);
        assert_eq!(find(&syms, "Service").kind, SymbolKind::Class);
    }

    #[test]
    fn ts_interface_declaration() {
        let syms = sym("interface Repo {}", Language::TypeScript);
        assert_eq!(find(&syms, "Repo").kind, SymbolKind::Interface);
    }

    #[test]
    fn ts_type_alias() {
        let syms = sym("type Handler = () => void;", Language::TypeScript);
        assert_eq!(find(&syms, "Handler").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn ts_enum_declaration() {
        let syms = sym("enum Status { Ok, Err }", Language::TypeScript);
        assert_eq!(find(&syms, "Status").kind, SymbolKind::Enum);
    }

    #[test]
    fn ts_const_variable() {
        let syms = sym("const MAX = 100;", Language::TypeScript);
        let s = find(&syms, "MAX");
        assert_eq!(s.kind, SymbolKind::Variable);
        assert_eq!(s.confidence_score, 25);
    }

    #[test]
    fn ts_exported_const() {
        let syms = sym("export const HOST = 'x';", Language::TypeScript);
        assert!(
            find(&syms, "HOST").is_exported,
            "exported const should be exported"
        );
    }

    #[test]
    fn ts_cpg_call_edge() {
        let src = "function dispatch() { handle(); } function handle() {}";
        let es = edges(src, Language::TypeScript);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#dispatch") && e.to_uri.contains("#handle")),
            "expected dispatch→handle edge"
        );
    }

    #[test]
    fn ts_method_call_member_expression() {
        let src = "function run() { db.query(); }";
        let es = edges(src, Language::TypeScript);
        assert!(
            es.iter().any(|e| e.to_uri.contains("#query")),
            "member expression call should produce an edge"
        );
    }

    // ── Python ────────────────────────────────────────────────────────────────

    #[test]
    fn py_function_definition() {
        let syms = sym("def compute():\n    pass", Language::Python);
        let s = find(&syms, "compute");
        assert_eq!(s.kind, SymbolKind::Function);
        assert!(s.is_exported);
    }

    #[test]
    fn py_class_definition() {
        let syms = sym("class Engine:\n    pass", Language::Python);
        let s = find(&syms, "Engine");
        assert_eq!(s.kind, SymbolKind::Class);
        assert!(s.is_exported);
    }

    #[test]
    fn py_private_underscore_not_exported() {
        let syms = sym("def _internal():\n    pass", Language::Python);
        assert!(
            !find(&syms, "_internal").is_exported,
            "underscore-prefixed name should not be exported"
        );
    }

    #[test]
    fn py_decorated_function() {
        let src = "@staticmethod\ndef helper():\n    pass";
        let syms = sym(src, Language::Python);
        assert!(
            names(&syms).contains(&"helper"),
            "decorated function should still be extracted"
        );
    }

    #[test]
    fn py_cpg_call_edge() {
        let src = "def main():\n    setup()\ndef setup():\n    pass";
        let es = edges(src, Language::Python);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#main") && e.to_uri.contains("#setup")),
            "expected main→setup call edge"
        );
    }

    #[test]
    fn py_attribute_call_edge() {
        let src = "def run():\n    self.flush()";
        let es = edges(src, Language::Python);
        assert!(
            es.iter().any(|e| e.to_uri.contains("#flush")),
            "attribute call should produce an edge"
        );
    }

    // ── Dart ──────────────────────────────────────────────────────────────────

    #[test]
    fn dart_function_declaration() {
        let syms = sym("void greet() {}", Language::Dart);
        let s = find(&syms, "greet");
        assert_eq!(s.kind, SymbolKind::Function);
        assert!(s.is_exported);
    }

    #[test]
    fn dart_class_declaration() {
        let syms = sym("class Widget {}", Language::Dart);
        assert_eq!(find(&syms, "Widget").kind, SymbolKind::Class);
    }

    #[test]
    fn dart_private_underscore() {
        let syms = sym("void _helper() {}", Language::Dart);
        assert!(
            !find(&syms, "_helper").is_exported,
            "underscore-prefixed Dart name should not be exported"
        );
    }

    #[test]
    fn dart_mixin_is_class_kind() {
        let syms = sym("mixin Scrollable {}", Language::Dart);
        assert_eq!(find(&syms, "Scrollable").kind, SymbolKind::Class);
    }

    // ── index_file integration ────────────────────────────────────────────────

    #[test]
    fn index_file_populates_all_fields() {
        let doc = Tier1Indexer::new().index_file(
            "file:///src/lib.rs",
            "pub fn init() {} struct State {}",
            Language::Rust,
        );
        assert!(!doc.symbols.is_empty(), "symbols must be populated");
        assert!(!doc.occurrences.is_empty(), "occurrences must be populated");
        assert!(!doc.content_hash.is_empty(), "content hash must be set");
        assert_eq!(doc.language, "rust");
    }

    #[test]
    fn index_file_content_hash_stable() {
        let src = "fn stable() {}";
        let a = Tier1Indexer::new().index_file("file:///a.rs", src, Language::Rust);
        let b = Tier1Indexer::new().index_file("file:///a.rs", src, Language::Rust);
        assert_eq!(
            a.content_hash, b.content_hash,
            "same source must produce same hash"
        );
    }

    #[test]
    fn index_file_hash_differs_on_content_change() {
        let a = Tier1Indexer::new().index_file("file:///a.rs", "fn a() {}", Language::Rust);
        let b = Tier1Indexer::new().index_file("file:///a.rs", "fn b() {}", Language::Rust);
        assert_ne!(a.content_hash, b.content_hash);
    }
}
