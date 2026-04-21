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
        assert!(sym("", Language::C).is_empty());
        assert!(sym("", Language::Cpp).is_empty());
        assert!(sym("", Language::Go).is_empty());
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

    // ── Rust: v2.3 structural metadata ────────────────────────────────────────

    #[test]
    fn rust_pub_fn_visibility_public() {
        use crate::schema::{ExtractionTier, Visibility};
        let syms = sym("pub fn bar(x: i32) {}", Language::Rust);
        let s = find(&syms, "bar");
        assert_eq!(s.visibility, Some(Visibility::Public));
        assert!(s.modifiers.iter().any(|m| m == "pub"));
        assert_eq!(s.extraction_tier, ExtractionTier::Tier1);
        // Confidence from explicit keyword → 1.0.
        assert_eq!(s.visibility_confidence, Some(1.0));
    }

    #[test]
    fn rust_pub_crate_fn_visibility_internal() {
        use crate::schema::Visibility;
        let syms = sym("pub(crate) fn helper() {}", Language::Rust);
        let s = find(&syms, "helper");
        assert_eq!(s.visibility, Some(Visibility::Internal));
        assert!(s.modifiers.iter().any(|m| m.starts_with("pub(")));
    }

    #[test]
    fn rust_private_fn_visibility_private() {
        use crate::schema::Visibility;
        let syms = sym("fn hidden() {}", Language::Rust);
        let s = find(&syms, "hidden");
        assert_eq!(s.visibility, Some(Visibility::Private));
        // No modifier keyword → 0.5 confidence.
        assert_eq!(s.visibility_confidence, Some(0.5));
    }

    #[test]
    fn rust_async_unsafe_modifiers_collected() {
        let src = "pub async unsafe fn io() {}";
        let syms = sym(src, Language::Rust);
        let s = find(&syms, "io");
        assert!(s.modifiers.iter().any(|m| m == "pub"));
        assert!(s.modifiers.iter().any(|m| m == "async"));
        assert!(s.modifiers.iter().any(|m| m == "unsafe"));
    }

    #[test]
    fn rust_container_name_from_impl() {
        let src = "impl Foo { pub fn bar(&self) {} }";
        let syms = sym(src, Language::Rust);
        let s = find(&syms, "bar");
        assert_eq!(s.container_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn rust_container_name_from_trait() {
        // Default method has a body and parses as `function_item`, so it is
        // extracted. Abstract trait methods (`function_signature_item`) are
        // not extracted today — orthogonal gap.
        let src = "pub trait Render { fn draw(&self) {} }";
        let syms = sym(src, Language::Rust);
        let s = find(&syms, "draw");
        assert_eq!(s.container_name.as_deref(), Some("Render"));
    }

    #[test]
    fn rust_no_container_at_top_level() {
        let syms = sym("pub fn top() {}", Language::Rust);
        assert_eq!(find(&syms, "top").container_name, None);
    }

    #[test]
    fn rust_signature_and_normalized() {
        let syms = sym("pub fn add(x: i32, y: i32) -> i32 { x + y }", Language::Rust);
        let s = find(&syms, "add");
        assert_eq!(
            s.signature.as_deref(),
            Some("pub fn add(x: i32, y: i32) -> i32")
        );
        assert_eq!(
            s.signature_normalized.as_deref(),
            Some("pub fn add(_: i32, _: i32) -> i32")
        );
    }

    #[test]
    fn rust_non_function_has_no_signature() {
        let syms = sym("pub struct Point { x: i32 }", Language::Rust);
        let s = find(&syms, "Point");
        assert_eq!(s.signature, None);
        assert_eq!(s.signature_normalized, None);
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

    // ── C ─────────────────────────────────────────────────────────────────────

    #[test]
    fn c_function_extracted() {
        let syms = sym("void do_thing(int x) {}", Language::C);
        let s = find(&syms, "do_thing");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
        assert!(s.is_exported, "top-level C function should be exported");
    }

    #[test]
    fn c_static_function_not_exported() {
        let syms = sym("static void internal(void) {}", Language::C);
        assert!(
            !find(&syms, "internal").is_exported,
            "static function should not be exported"
        );
    }

    #[test]
    fn c_struct_extracted() {
        let syms = sym("struct Point { int x; int y; };", Language::C);
        let s = find(&syms, "Point");
        assert_eq!(s.kind, SymbolKind::Class);
    }

    #[test]
    fn c_enum_extracted() {
        let syms = sym("enum Color { Red, Green, Blue };", Language::C);
        assert_eq!(find(&syms, "Color").kind, SymbolKind::Enum);
    }

    #[test]
    fn c_typedef_extracted() {
        let syms = sym("typedef unsigned int uint32;", Language::C);
        assert_eq!(find(&syms, "uint32").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn c_cpg_call_edge() {
        let src = "void caller(void) { callee(); } void callee(void) {}";
        let es = edges(src, Language::C);
        assert!(
            es.iter()
                .any(|e| { e.from_uri.contains("#caller") && e.to_uri.contains("#callee") }),
            "expected caller→callee edge, got: {es:?}"
        );
    }

    // ── C++ ───────────────────────────────────────────────────────────────────

    #[test]
    fn cpp_function_extracted() {
        let syms = sym("void greet() {}", Language::Cpp);
        let s = find(&syms, "greet");
        assert_eq!(s.kind, SymbolKind::Function);
        assert!(s.is_exported);
    }

    #[test]
    fn cpp_class_extracted() {
        let syms = sym("class Engine {};", Language::Cpp);
        assert_eq!(find(&syms, "Engine").kind, SymbolKind::Class);
    }

    #[test]
    fn cpp_namespace_extracted() {
        let syms = sym("namespace utils {}", Language::Cpp);
        assert_eq!(find(&syms, "utils").kind, SymbolKind::Namespace);
    }

    #[test]
    fn cpp_struct_extracted() {
        let syms = sym("struct Vec2 { float x; float y; };", Language::Cpp);
        assert_eq!(find(&syms, "Vec2").kind, SymbolKind::Class);
    }

    #[test]
    fn cpp_cpg_call_edge() {
        let src = "void dispatch() { handle(); } void handle() {}";
        let es = edges(src, Language::Cpp);
        assert!(
            es.iter()
                .any(|e| { e.from_uri.contains("#dispatch") && e.to_uri.contains("#handle") }),
            "expected dispatch→handle edge"
        );
    }

    #[test]
    fn empty_source_returns_empty_c_cpp() {
        assert!(sym("", Language::C).is_empty());
        assert!(sym("", Language::Cpp).is_empty());
    }

    // ── Go ────────────────────────────────────────────────────────────────────

    #[test]
    fn go_function_extracted() {
        let syms = sym("package p\nfunc DoThing() {}", Language::Go);
        let s = find(&syms, "DoThing");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
        assert!(s.is_exported, "uppercase Go function should be exported");
    }

    #[test]
    fn go_unexported_function() {
        let syms = sym("package p\nfunc internal() {}", Language::Go);
        assert!(
            !find(&syms, "internal").is_exported,
            "lowercase Go function should not be exported"
        );
    }

    #[test]
    fn go_struct_extracted() {
        let syms = sym("package p\ntype Point struct { X, Y int }", Language::Go);
        assert_eq!(find(&syms, "Point").kind, SymbolKind::Class);
    }

    #[test]
    fn go_interface_extracted() {
        let syms = sym("package p\ntype Reader interface { Read() }", Language::Go);
        assert_eq!(find(&syms, "Reader").kind, SymbolKind::Interface);
    }

    #[test]
    fn go_type_alias_extracted() {
        let syms = sym("package p\ntype MyInt int", Language::Go);
        assert_eq!(find(&syms, "MyInt").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn go_const_extracted() {
        let syms = sym("package p\nconst MaxSize = 100", Language::Go);
        let s = find(&syms, "MaxSize");
        assert_eq!(s.kind, SymbolKind::Variable);
        assert_eq!(s.confidence_score, 25);
        assert!(s.is_exported);
    }

    #[test]
    fn go_cpg_call_edge() {
        let src = "package p\nfunc Caller() { Callee() }\nfunc Callee() {}";
        let es = edges(src, Language::Go);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#Caller") && e.to_uri.contains("#Callee")),
            "expected Caller→Callee edge, got: {es:?}"
        );
    }

    #[test]
    fn go_method_call_selector() {
        let src = "package p\nfunc Run() { db.Query() }";
        let es = edges(src, Language::Go);
        assert!(
            es.iter().any(|e| e.to_uri.contains("#Query")),
            "selector method call should produce an edge"
        );
    }

    #[test]
    fn empty_source_returns_empty_go() {
        assert!(sym("", Language::Go).is_empty());
    }

    // ── JavaScript / JSX ─────────────────────────────────────────────────────

    #[test]
    fn js_function_declaration_extracted() {
        let syms = sym(
            "function greet(name) { return name; }",
            Language::JavaScript,
        );
        let s = find(&syms, "greet");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
    }

    #[test]
    fn js_class_extracted() {
        let syms = sym("class EventEmitter {}", Language::JavaScript);
        assert_eq!(find(&syms, "EventEmitter").kind, SymbolKind::Class);
    }

    #[test]
    fn jsx_component_extracted() {
        let syms = sym(
            "function Button(props) { return null; }",
            Language::JavaScriptReact,
        );
        assert_eq!(find(&syms, "Button").kind, SymbolKind::Function);
    }

    #[test]
    fn js_cpg_call_edge() {
        let src = "function caller() { callee(); } function callee() {}";
        let es = edges(src, Language::JavaScript);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#caller") && e.to_uri.contains("#callee")),
            "expected caller→callee edge, got: {es:?}"
        );
    }

    #[test]
    fn empty_source_returns_empty_js() {
        assert!(sym("", Language::JavaScript).is_empty());
        assert!(sym("", Language::JavaScriptReact).is_empty());
    }

    // ── Kotlin ────────────────────────────────────────────────────────────────

    #[test]
    fn kotlin_function_extracted() {
        let syms = sym("fun doThing(x: Int): String = \"\"", Language::Kotlin);
        let s = find(&syms, "doThing");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
    }

    #[test]
    fn kotlin_class_extracted() {
        let syms = sym("class MyService {}", Language::Kotlin);
        assert_eq!(find(&syms, "MyService").kind, SymbolKind::Class);
    }

    #[test]
    fn kotlin_interface_extracted() {
        let syms = sym("interface Runnable { fun run() }", Language::Kotlin);
        assert_eq!(find(&syms, "Runnable").kind, SymbolKind::Interface);
    }

    #[test]
    fn kotlin_cpg_call_edge() {
        let src = "fun caller() { callee() }\nfun callee() {}";
        let es = edges(src, Language::Kotlin);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#caller") && e.to_uri.contains("#callee")),
            "expected caller→callee edge, got: {es:?}"
        );
    }

    #[test]
    fn empty_source_returns_empty_kotlin() {
        assert!(sym("", Language::Kotlin).is_empty());
    }

    // ── Swift ─────────────────────────────────────────────────────────────────

    #[test]
    fn swift_function_extracted() {
        let syms = sym(
            "func greet(name: String) -> String { name }",
            Language::Swift,
        );
        let s = find(&syms, "greet");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.confidence_score, 30);
    }

    #[test]
    fn swift_class_extracted() {
        let syms = sym("class ViewController {}", Language::Swift);
        assert_eq!(find(&syms, "ViewController").kind, SymbolKind::Class);
    }

    #[test]
    fn swift_struct_extracted() {
        let syms = sym("struct Point { var x: Int; var y: Int }", Language::Swift);
        assert_eq!(find(&syms, "Point").kind, SymbolKind::Class);
    }

    #[test]
    fn swift_protocol_extracted() {
        let syms = sym("protocol Drawable { func draw() }", Language::Swift);
        assert_eq!(find(&syms, "Drawable").kind, SymbolKind::Interface);
    }

    #[test]
    fn swift_enum_extracted() {
        let syms = sym("enum Direction { case north, south }", Language::Swift);
        assert_eq!(find(&syms, "Direction").kind, SymbolKind::Enum);
    }

    #[test]
    fn swift_cpg_call_edge() {
        let src = "func caller() { callee() }\nfunc callee() {}";
        let es = edges(src, Language::Swift);
        assert!(
            es.iter()
                .any(|e| e.from_uri.contains("#caller") && e.to_uri.contains("#callee")),
            "expected caller→callee edge, got: {es:?}"
        );
    }

    #[test]
    fn empty_source_returns_empty_swift() {
        assert!(sym("", Language::Swift).is_empty());
    }

    // ── v2.3 structural metadata: smoke tests per language ───────────────────

    #[test]
    fn ts_method_visibility_and_container() {
        use crate::schema::{ExtractionTier, Visibility};
        let src = "class Svc { private handle(x: number): boolean { return true; } }";
        let syms = sym(src, Language::TypeScript);
        let s = find(&syms, "handle");
        assert_eq!(s.visibility, Some(Visibility::Private));
        assert_eq!(s.container_name.as_deref(), Some("Svc"));
        assert!(s.modifiers.iter().any(|m| m == "private"));
        assert_eq!(s.extraction_tier, ExtractionTier::Tier1);
        assert_eq!(
            s.signature_normalized.as_deref(),
            Some("private handle(_: number): boolean")
        );
    }

    #[test]
    fn ts_exported_function_modifier() {
        use crate::schema::Visibility;
        let syms = sym("export function send(x: number): void {}", Language::TypeScript);
        let s = find(&syms, "send");
        assert!(s.modifiers.iter().any(|m| m == "export"));
        assert_eq!(s.visibility, Some(Visibility::Public));
    }

    #[test]
    fn py_method_container_and_visibility() {
        use crate::schema::{ExtractionTier, Visibility};
        let src = "class C:\n    def _private(self, x: int) -> None:\n        pass\n";
        let syms = sym(src, Language::Python);
        let s = find(&syms, "_private");
        assert_eq!(s.visibility, Some(Visibility::Private));
        assert_eq!(s.container_name.as_deref(), Some("C"));
        assert_eq!(s.extraction_tier, ExtractionTier::Tier1);
        // `self` has no `:` and is left as-is; only the typed param is normalized.
        assert_eq!(
            s.signature_normalized.as_deref(),
            Some("def _private(self, _: int) -> None")
        );
    }

    #[test]
    fn go_func_visibility_from_name() {
        use crate::schema::Visibility;
        let syms = sym("package p\nfunc Exported(x int) bool { return true }", Language::Go);
        let s = find(&syms, "Exported");
        assert_eq!(s.visibility, Some(Visibility::Public));
        assert_eq!(
            s.signature.as_deref(),
            Some("func Exported(x int) bool")
        );
    }

    #[test]
    fn go_method_receiver_as_container() {
        let src = "package p\nfunc (f *Foo) Bar() {}";
        let syms = sym(src, Language::Go);
        let s = find(&syms, "Bar");
        assert_eq!(s.container_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn dart_private_underscore_visibility_top_level() {
        // Note: Dart class-body methods (`class_member_definition` →
        // `method_signature`) are a pre-existing extractor gap — we only
        // match `method_declaration` today. Validate the underscore-private
        // convention on a top-level function instead.
        use crate::schema::Visibility;
        let src = "void _priv(int x) {}";
        let syms = sym(src, Language::Dart);
        let s = find(&syms, "_priv");
        assert_eq!(s.visibility, Some(Visibility::Private));
        assert!(!s.is_exported);
    }

    #[test]
    fn c_static_modifier_and_signature() {
        use crate::schema::Visibility;
        let syms = sym("static int helper(int n) { return n; }", Language::C);
        let s = find(&syms, "helper");
        assert!(s.modifiers.iter().any(|m| m == "static"));
        // Signature at minimum covers the visible declarator; normalized form is whitespace-collapsed.
        assert!(s.signature.as_deref().unwrap_or("").contains("helper"));
        assert_eq!(s.visibility, Some(Visibility::Public));
    }

    #[test]
    fn cpp_method_container_in_class() {
        use crate::schema::SymbolKind;
        let src = "class Svc { public: int run() { return 0; } };";
        let syms = sym(src, Language::Cpp);
        let s = find(&syms, "run");
        assert_eq!(s.container_name.as_deref(), Some("Svc"));
        assert_eq!(s.kind, SymbolKind::Method);
    }

    #[test]
    fn kotlin_private_modifier_and_visibility() {
        use crate::schema::Visibility;
        let src = "class Svc { private fun hidden(x: Int): Boolean = true }";
        let syms = sym(src, Language::Kotlin);
        let s = find(&syms, "hidden");
        assert!(s.modifiers.iter().any(|m| m == "private"));
        assert_eq!(s.visibility, Some(Visibility::Private));
        assert_eq!(s.container_name.as_deref(), Some("Svc"));
    }

    #[test]
    fn swift_fileprivate_modifier_and_visibility() {
        use crate::schema::Visibility;
        let src = "class Svc {\n    fileprivate func hidden() {}\n}";
        let syms = sym(src, Language::Swift);
        let s = find(&syms, "hidden");
        assert!(s.modifiers.iter().any(|m| m == "fileprivate"));
        assert_eq!(s.visibility, Some(Visibility::Private));
        assert_eq!(s.container_name.as_deref(), Some("Svc"));
    }

    // ── v2.3 reference classification (Call/Read/Write + is_test) ────────────

    fn occs_at(uri: &str, source: &str, lang: Language) -> Vec<OwnedOccurrence> {
        Tier1Indexer::new().occurrences_for_source(uri, source, lang)
    }

    #[test]
    fn ref_kind_call_rust() {
        use crate::schema::ReferenceKind;
        let occs_list = occs("fn a() { b(); } fn b() {}", Language::Rust);
        let call = occs_list
            .iter()
            .find(|o| o.symbol_uri.contains("#b") && o.role == Role::Reference)
            .expect("b() should be a reference");
        assert_eq!(call.kind, ReferenceKind::Call);
    }

    #[test]
    fn ref_kind_call_typescript_method_property() {
        use crate::schema::ReferenceKind;
        // `obj.method()` — the property identifier is the callee.
        let src = "function demo(obj: any) { obj.method(); }";
        let occs_list = occs(src, Language::TypeScript);
        let callee = occs_list
            .iter()
            .find(|o| o.symbol_uri.contains("#method"));
        if let Some(c) = callee {
            assert_eq!(
                c.kind,
                ReferenceKind::Call,
                "obj.method() callee must be classified as Call, got {:?}",
                c.kind
            );
        }
    }

    #[test]
    fn ref_kind_read_rust_local_variable_use() {
        use crate::schema::ReferenceKind;
        let src = "fn f(x: i32) -> i32 { x + 1 }";
        let occs_list = occs(src, Language::Rust);
        // `x` on the RHS of the expression is a Read.
        let read = occs_list
            .iter()
            .find(|o| o.symbol_uri.contains("#x") && o.role == Role::Reference);
        if let Some(r) = read {
            assert_eq!(r.kind, ReferenceKind::Read);
        }
    }

    #[test]
    fn ref_kind_write_python_assignment() {
        use crate::schema::ReferenceKind;
        let src = "x = 1\nx = 2\n";
        let occs_list = occs(src, Language::Python);
        // At least one Write occurrence for `x`.
        let has_write = occs_list
            .iter()
            .any(|o| o.symbol_uri.contains("#x") && o.kind == ReferenceKind::Write);
        assert!(
            has_write,
            "expected at least one Write occurrence on `x = ...`; got {:?}",
            occs_list
                .iter()
                .filter(|o| o.symbol_uri.contains("#x"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn is_test_file_detects_common_paths() {
        let cases = [
            ("file:///proj/tests/foo.rs", true),
            ("file:///proj/src/foo_test.go", true),
            ("file:///proj/src/foo.test.ts", true),
            ("file:///proj/src/foo.spec.js", true),
            ("file:///proj/__tests__/foo.ts", true),
            ("file:///proj/src/MyServiceTest.java", true),
            ("file:///proj/src/lib.rs", false),
            ("file:///proj/src/foo.rs", false),
        ];
        for (uri, expected) in cases {
            let occs_list = occs_at(uri, "fn foo() {}", Language::Rust);
            let any = occs_list.first();
            if let Some(o) = any {
                assert_eq!(
                    o.is_test, expected,
                    "wrong is_test for uri {uri}: got {}",
                    o.is_test
                );
            }
        }
    }

    #[test]
    fn definition_role_leaves_kind_unknown() {
        use crate::schema::ReferenceKind;
        let occs_list = occs("pub fn defined() {}", Language::Rust);
        let def = occs_list
            .iter()
            .find(|o| o.symbol_uri.contains("#defined") && o.role == Role::Definition)
            .expect("definition occurrence");
        assert_eq!(
            def.kind,
            ReferenceKind::Unknown,
            "definitions must leave kind as Unknown — ReferenceKind only classifies references"
        );
    }
}
