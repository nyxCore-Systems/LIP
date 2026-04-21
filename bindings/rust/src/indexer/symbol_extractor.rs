use tree_sitter::{Node, Tree};

use crate::schema::{
    normalize_signature, visibility, EdgeKind, ExtractionTier, OwnedGraphEdge, OwnedOccurrence,
    OwnedRange, OwnedSymbolInfo, ReferenceKind, Role, SymbolKind,
};

use super::language::Language;

/// Walks a tree-sitter parse tree and extracts LIP symbols and occurrences.
/// Produces Tier 1 results: confidence_score in the 1–50 range.
pub struct SymbolExtractor<'a> {
    source: &'a [u8],
    language: Language,
    file_uri: &'a str,
}

impl<'a> SymbolExtractor<'a> {
    pub fn new(source: &'a [u8], language: Language, file_uri: &'a str) -> Self {
        Self {
            source,
            language,
            file_uri,
        }
    }

    pub fn extract_symbols(&self, tree: &Tree) -> Vec<OwnedSymbolInfo> {
        let mut symbols = Vec::new();
        self.walk_symbols(tree.root_node(), &mut symbols);
        symbols
    }

    pub fn extract_occurrences(&self, tree: &Tree) -> Vec<OwnedOccurrence> {
        let mut occs = Vec::new();
        self.walk_occurrences(tree.root_node(), &mut occs);
        occs
    }

    /// Extract CPG `Calls` edges from function call sites.
    ///
    /// Only emits edges when both caller and callee names are identifiable;
    /// anonymous closures and complex receiver expressions are skipped.
    pub fn extract_edges(&self, tree: &Tree) -> Vec<OwnedGraphEdge> {
        let mut edges = Vec::new();
        self.walk_calls(tree.root_node(), None, &mut edges);
        edges
    }

    fn walk_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        match self.language {
            Language::Rust => self.rust_calls(node, caller, edges),
            Language::TypeScript => self.ts_calls(node, caller, edges),
            Language::Python => self.py_calls(node, caller, edges),
            Language::Dart => self.dart_calls(node, caller, edges),
            Language::C => self.c_calls(node, caller, edges),
            Language::Cpp => self.cpp_calls(node, caller, edges),
            Language::Go => self.go_calls(node, caller, edges),
            Language::JavaScript | Language::JavaScriptReact => self.ts_calls(node, caller, edges),
            Language::Kotlin => self.kotlin_calls(node, caller, edges),
            Language::Swift => self.swift_calls(node, caller, edges),
            Language::Unknown => {}
        }
    }

    fn node_text(&self, node: &Node) -> &str {
        std::str::from_utf8(&self.source[node.start_byte()..node.end_byte()]).unwrap_or("")
    }

    fn node_range(node: &Node) -> OwnedRange {
        let start = node.start_position();
        let end = node.end_position();
        OwnedRange {
            start_line: start.row as i32,
            start_char: start.column as i32,
            end_line: end.row as i32,
            end_char: end.column as i32,
        }
    }

    fn lip_uri(&self, name: &str) -> String {
        // Strip the file:// scheme so we don't produce lip://local/file:///abs/path#Name.
        let path = self
            .file_uri
            .strip_prefix("file://")
            .unwrap_or(self.file_uri);
        format!("lip://local/{path}#{name}")
    }

    /// Heuristic: is the current file a test file?
    ///
    /// Looks at path segments and filename patterns common across ecosystems.
    /// Conservative — matches cleanly-named test files; misses configurable
    /// test dirs (e.g. Python `conftest.py`) and inline `#[cfg(test)]` modules.
    /// Tier-2 can refine per-file with compiler-level knowledge.
    fn is_test_file(&self) -> bool {
        let u = self.file_uri;
        u.contains("/tests/")
            || u.contains("/test/")
            || u.contains("/__tests__/")
            || u.contains("/spec/")
            || u.contains(".test.")
            || u.contains(".spec.")
            || u.contains("_test.")
            || u.ends_with("Test.java")
            || u.ends_with("Test.kt")
            || u.ends_with("Tests.swift")
    }

    /// Classify a reference occurrence based on its tree-sitter parent context.
    ///
    /// Returns `Call` when the identifier is the callee of a call expression,
    /// `Write` when it is the LHS of an assignment, otherwise `Read`. Type /
    /// Implements / Extends classification requires Tier-2 type info and is
    /// left to the LSP backends. Returns `Unknown` when the node has no
    /// parent (a bare module).
    fn classify_ref_kind(&self, node: &Node) -> ReferenceKind {
        let Some(parent) = node.parent() else {
            return ReferenceKind::Unknown;
        };
        let pk = parent.kind();

        // Call site: the identifier is the function/method being invoked.
        let is_call_parent = matches!(
            (self.language, pk),
            (Language::Rust, "call_expression" | "macro_invocation")
                | (
                    Language::TypeScript
                        | Language::JavaScript
                        | Language::JavaScriptReact,
                    "call_expression" | "new_expression"
                )
                | (Language::Python, "call")
                | (Language::Go, "call_expression")
                | (Language::C | Language::Cpp, "call_expression")
                | (
                    Language::Dart,
                    "method_invocation" | "function_expression_invocation"
                )
                | (Language::Kotlin, "call_expression")
                | (Language::Swift, "call_expression")
        );
        if is_call_parent {
            // When the call has a receiver (`obj.method()`), only the method
            // identifier is the callee — the receiver is still a Read.
            let callee_field = parent
                .child_by_field_name("function")
                .or_else(|| parent.child_by_field_name("method"));
            let is_callee = callee_field
                .map(|f| {
                    f.id() == node.id()
                        || f.child_by_field_name("property")
                            .map(|p| p.id() == node.id())
                            .unwrap_or(false)
                        || f.child_by_field_name("field")
                            .map(|p| p.id() == node.id())
                            .unwrap_or(false)
                })
                .unwrap_or(true);
            if is_callee {
                return ReferenceKind::Call;
            }
        }

        // Assignment LHS → Write. Covers Python/TS/JS `=`, augmented variants,
        // and Rust assignment_expression.
        let is_assign_lhs = matches!(
            pk,
            "assignment_expression"
                | "assignment"
                | "augmented_assignment_expression"
                | "augmented_assignment"
                | "compound_assignment_expr"
        ) && parent
            .child_by_field_name("left")
            .map(|c| c.id() == node.id())
            .unwrap_or(false);
        if is_assign_lhs {
            return ReferenceKind::Write;
        }

        ReferenceKind::Read
    }

    /// Build a Tier-1 occurrence with v2.3 classification fields populated.
    fn make_occurrence(&self, node: &Node, name: &str, role: Role) -> OwnedOccurrence {
        let kind = if matches!(role, Role::Reference) {
            self.classify_ref_kind(node)
        } else {
            ReferenceKind::Unknown
        };
        OwnedOccurrence {
            symbol_uri: self.lip_uri(name),
            range: Self::node_range(node),
            confidence_score: 20,
            role,
            override_doc: None,
            kind,
            is_test: self.is_test_file(),
        }
    }

    // ── Rust ─────────────────────────────────────────────────────────────────

    fn walk_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        match self.language {
            Language::Rust => self.rust_symbols(node, out),
            Language::TypeScript => self.ts_symbols(node, out),
            Language::Python => self.py_symbols(node, out),
            Language::Dart => self.dart_symbols(node, out),
            Language::C => self.c_symbols(node, out),
            Language::Cpp => self.cpp_symbols(node, out),
            Language::Go => self.go_symbols(node, out),
            Language::JavaScript | Language::JavaScriptReact => self.ts_symbols(node, out),
            Language::Kotlin => self.kotlin_symbols(node, out),
            Language::Swift => self.swift_symbols(node, out),
            Language::Unknown => {}
        }
    }

    fn walk_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        match self.language {
            Language::Rust => self.rust_occurrences(node, out),
            Language::TypeScript => self.ts_occurrences(node, out),
            Language::Python => self.py_occurrences(node, out),
            Language::Dart => self.dart_occurrences(node, out),
            Language::C => self.c_occurrences(node, out),
            Language::Cpp => self.cpp_occurrences(node, out),
            Language::Go => self.go_occurrences(node, out),
            Language::JavaScript | Language::JavaScriptReact => self.ts_occurrences(node, out),
            Language::Kotlin => self.kotlin_occurrences(node, out),
            Language::Swift => self.swift_occurrences(node, out),
            Language::Unknown => {}
        }
    }

    // ─── Rust ────────────────────────────────────────────────────────────────

    fn rust_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let (kind, name_field) = match node.kind() {
            "function_item" => (SymbolKind::Function, "name"),
            "struct_item" => (SymbolKind::Class, "name"),
            "enum_item" => (SymbolKind::Enum, "name"),
            "trait_item" => (SymbolKind::Interface, "name"),
            "type_item" => (SymbolKind::TypeAlias, "name"),
            "mod_item" => (SymbolKind::Namespace, "name"),
            "macro_definition" => (SymbolKind::Macro, "name"),
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.rust_symbols(child, out);
                    }
                }
                return;
            }
        };

        if let Some(name_node) = node.child_by_field_name(name_field) {
            let name = self.node_text(&name_node);
            if !name.is_empty() {
                let modifiers = self.rust_modifiers(&node);
                let is_exported =
                    modifiers.iter().any(|m| m == "pub" || m.starts_with("pub("));
                let (vis, vc) = visibility::infer(name, &modifiers, self.language);
                let container = self.rust_container(&node);
                let signature = self.rust_signature(&node);
                let signature_normalized = signature
                    .as_deref()
                    .map(|s| normalize_signature(s, self.language));

                out.push(OwnedSymbolInfo {
                    uri: self.lip_uri(name),
                    display_name: name.to_owned(),
                    kind,
                    confidence_score: 30,
                    is_exported,
                    modifiers,
                    visibility: Some(vis),
                    visibility_confidence: Some(vc as f32 / 100.0),
                    container_name: container,
                    signature,
                    signature_normalized,
                    extraction_tier: ExtractionTier::Tier1,
                    ..OwnedSymbolInfo::new("", "")
                });
            }
        }

        // Recurse into child items (e.g. impl blocks, mod contents).
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.rust_symbols(child, out);
            }
        }
    }

    /// Collect Rust modifier keywords adjacent to a definition node.
    ///
    /// Picks up `visibility_modifier` children (`pub`, `pub(crate)`, …) plus
    /// `function_modifiers` tokens (`async`, `unsafe`, `const`, `extern`).
    fn rust_modifiers(&self, node: &Node) -> Vec<String> {
        let mut mods = Vec::new();
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else { continue };
            match child.kind() {
                "visibility_modifier" => {
                    let text = self.node_text(&child).trim().to_owned();
                    if !text.is_empty() {
                        mods.push(text);
                    }
                }
                "function_modifiers" => {
                    for j in 0..child.child_count() {
                        if let Some(m) = child.child(j) {
                            let t = self.node_text(&m).trim();
                            if !t.is_empty() {
                                mods.push(t.to_owned());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        mods
    }

    /// Walk up from a definition node to find the enclosing container name.
    ///
    /// Recognizes `impl`/`trait`/`struct`/`enum`/`mod` ancestors; returns the
    /// first container found, or `None` at top level.
    fn rust_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            match n.kind() {
                "impl_item" => {
                    if let Some(ty) = n.child_by_field_name("type") {
                        return Some(self.node_text(&ty).trim().to_owned());
                    }
                }
                "trait_item" | "struct_item" | "enum_item" | "mod_item" | "union_item" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        return Some(self.node_text(&name).trim().to_owned());
                    }
                }
                _ => {}
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a Rust function (everything before the body block).
    /// Returns `None` for non-function items.
    fn rust_signature(&self, node: &Node) -> Option<String> {
        if node.kind() != "function_item" {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    fn rust_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                // Identifiers that are the `name` field of a declaration node are
                // definitions; all other identifier uses are references.
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_item"
                            | "struct_item"
                            | "enum_item"
                            | "trait_item"
                            | "type_item"
                            | "mod_item"
                            | "macro_definition"
                            | "field_declaration"
                            | "variant"
                    );
                    let is_name = parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.rust_occurrences(child, out);
            }
        }
    }

    // ─── TypeScript ──────────────────────────────────────────────────────────

    fn ts_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let (kind, name_field) = match node.kind() {
            "function_declaration" => (SymbolKind::Function, "name"),
            "method_definition" => (SymbolKind::Method, "name"),
            "class_declaration" => (SymbolKind::Class, "name"),
            "interface_declaration" => (SymbolKind::Interface, "name"),
            "type_alias_declaration" => (SymbolKind::TypeAlias, "name"),
            "enum_declaration" => (SymbolKind::Enum, "name"),
            "lexical_declaration" => {
                // May contain const/let variable declarators.
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.ts_symbols(child, out);
                    }
                }
                return;
            }
            "variable_declarator" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node);
                    if !name.is_empty() {
                        // Exported if the containing lexical_declaration's parent is export_statement.
                        let is_exported = node
                            .parent()
                            .and_then(|p| p.parent())
                            .map(|gp| gp.kind() == "export_statement")
                            .unwrap_or(false);
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(name),
                            display_name: name.to_owned(),
                            kind: SymbolKind::Variable,
                            confidence_score: 25,
                            is_exported,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
                return;
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.ts_symbols(child, out);
                    }
                }
                return;
            }
        };

        if let Some(name_node) = node.child_by_field_name(name_field) {
            let name = self.node_text(&name_node);
            if !name.is_empty() {
                // Exported if the declaration's parent is an export_statement.
                let is_exported = node
                    .parent()
                    .map(|p| p.kind() == "export_statement")
                    .unwrap_or(false);
                let mut modifiers = self.ts_modifiers(&node);
                if is_exported {
                    modifiers.push("export".to_owned());
                }
                let (vis, vc) = visibility::infer(name, &modifiers, self.language);
                let container = self.ts_container(&node);
                let signature = self.ts_signature(&node);
                let signature_normalized = signature
                    .as_deref()
                    .map(|s| normalize_signature(s, self.language));

                out.push(OwnedSymbolInfo {
                    uri: self.lip_uri(name),
                    display_name: name.to_owned(),
                    kind,
                    confidence_score: 30,
                    is_exported,
                    modifiers,
                    visibility: Some(vis),
                    visibility_confidence: Some(vc as f32 / 100.0),
                    container_name: container,
                    signature,
                    signature_normalized,
                    extraction_tier: ExtractionTier::Tier1,
                    ..OwnedSymbolInfo::new("", "")
                });
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.ts_symbols(child, out);
            }
        }
    }

    /// Collect TypeScript/JavaScript modifier keywords from a declaration node.
    fn ts_modifiers(&self, node: &Node) -> Vec<String> {
        const KEYWORDS: &[&str] = &[
            "async",
            "static",
            "readonly",
            "abstract",
            "override",
            "public",
            "private",
            "protected",
            "declare",
        ];
        let mut mods = Vec::new();
        // Direct children may carry keyword tokens or an `accessibility_modifier`.
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else { continue };
            if KEYWORDS.contains(&child.kind()) {
                mods.push(child.kind().to_owned());
            }
            if child.kind() == "accessibility_modifier" {
                let text = self.node_text(&child).trim().to_owned();
                if !text.is_empty() {
                    mods.push(text);
                }
            }
        }
        mods
    }

    /// Walk up for the enclosing class/interface container.
    fn ts_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            match n.kind() {
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "abstract_class_declaration" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        return Some(self.node_text(&name).trim().to_owned());
                    }
                }
                _ => {}
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a TS/JS function or method (text before body).
    fn ts_signature(&self, node: &Node) -> Option<String> {
        if !matches!(node.kind(), "function_declaration" | "method_definition") {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    fn ts_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_declaration"
                            | "method_definition"
                            | "class_declaration"
                            | "interface_declaration"
                            | "type_alias_declaration"
                            | "enum_declaration"
                            | "variable_declarator"
                    );
                    let is_name = parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.ts_occurrences(child, out);
            }
        }
    }

    // ─── Python ──────────────────────────────────────────────────────────────

    fn py_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let (kind, name_field) = match node.kind() {
            "function_definition" => (SymbolKind::Function, "name"),
            "class_definition" => (SymbolKind::Class, "name"),
            "decorated_definition" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.py_symbols(child, out);
                    }
                }
                return;
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.py_symbols(child, out);
                    }
                }
                return;
            }
        };

        if let Some(name_node) = node.child_by_field_name(name_field) {
            let name = self.node_text(&name_node);
            if !name.is_empty() {
                // Python convention: names starting with _ are private.
                let is_exported = !name.starts_with('_');
                let modifiers: Vec<String> = Vec::new();
                let (vis, vc) = visibility::infer(name, &modifiers, self.language);
                let container = self.py_container(&node);
                let signature = self.py_signature(&node);
                let signature_normalized = signature
                    .as_deref()
                    .map(|s| normalize_signature(s, self.language));

                out.push(OwnedSymbolInfo {
                    uri: self.lip_uri(name),
                    display_name: name.to_owned(),
                    kind,
                    confidence_score: 30,
                    is_exported,
                    modifiers,
                    visibility: Some(vis),
                    visibility_confidence: Some(vc as f32 / 100.0),
                    container_name: container,
                    signature,
                    signature_normalized,
                    extraction_tier: ExtractionTier::Tier1,
                    ..OwnedSymbolInfo::new("", "")
                });
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.py_symbols(child, out);
            }
        }
    }

    /// Walk up for the enclosing Python class container.
    fn py_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            if n.kind() == "class_definition" {
                if let Some(name) = n.child_by_field_name("name") {
                    return Some(self.node_text(&name).trim().to_owned());
                }
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a Python function (text before body).
    fn py_signature(&self, node: &Node) -> Option<String> {
        if node.kind() != "function_definition" {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        // Strip the trailing `:` that separates signature from body.
        let trimmed = text.trim().trim_end_matches(':').trim_end();
        Some(trimmed.to_owned())
    }

    fn py_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if node.kind() == "identifier" {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl =
                        matches!(parent.kind(), "function_definition" | "class_definition");
                    let is_name = parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.py_occurrences(child, out);
            }
        }
    }

    // ─── Dart ────────────────────────────────────────────────────────────────

    fn dart_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        // tree-sitter-dart v0.0.4 grammar notes:
        //   - Top-level functions use `lambda_expression` (not `function_declaration`).
        //     The name lives in a `function_signature` child (field "parameters") under
        //     the field "name" of that signature node.
        //   - Classes use `class_definition` (not `class_declaration`), field "name" works.
        //   - `mixin_declaration` exists but its identifier child has no named field.
        let push = |name: &str, kind: SymbolKind, decl: &Node, out: &mut Vec<OwnedSymbolInfo>| {
            if name.is_empty() {
                return;
            }
            let modifiers = self.dart_modifiers(decl);
            let (vis, vc) = visibility::infer(name, &modifiers, self.language);
            let container = self.dart_container(decl);
            let signature = self.dart_signature(decl);
            let signature_normalized = signature
                .as_deref()
                .map(|s| normalize_signature(s, self.language));

            out.push(OwnedSymbolInfo {
                uri: self.lip_uri(name),
                display_name: name.to_owned(),
                kind,
                confidence_score: 30,
                is_exported: !name.starts_with('_'),
                modifiers,
                visibility: Some(vis),
                visibility_confidence: Some(vc as f32 / 100.0),
                container_name: container,
                signature,
                signature_normalized,
                extraction_tier: ExtractionTier::Tier1,
                ..OwnedSymbolInfo::new("", "")
            });
        };

        match node.kind() {
            "lambda_expression" => {
                // lambda_expression → function_signature (field "parameters") → identifier (field "name")
                if let Some(name) = node
                    .child_by_field_name("parameters")
                    .filter(|c| c.kind() == "function_signature")
                    .and_then(|sig| sig.child_by_field_name("name"))
                    .map(|n| self.node_text(&n).to_owned())
                {
                    push(&name, SymbolKind::Function, &node, out);
                }
            }
            "class_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    push(self.node_text(&name_node), SymbolKind::Class, &node, out);
                }
            }
            "mixin_declaration" => {
                // Identifier has no named field; take the first identifier named child.
                let name = (0..node.named_child_count())
                    .filter_map(|i| node.named_child(i))
                    .find(|c| c.kind() == "identifier")
                    .map(|c| self.node_text(&c).to_owned())
                    .unwrap_or_default();
                push(&name, SymbolKind::Class, &node, out);
            }
            "method_declaration"
            | "constructor_declaration"
            | "getter_signature"
            | "setter_signature" => {
                let kind = if node.kind() == "constructor_declaration" {
                    SymbolKind::Constructor
                } else {
                    SymbolKind::Method
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    push(self.node_text(&name_node), kind, &node, out);
                }
            }
            "extension_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    push(
                        self.node_text(&name_node),
                        SymbolKind::Namespace,
                        &node,
                        out,
                    );
                }
            }
            _ => {}
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.dart_symbols(child, out);
            }
        }
    }

    /// Collect Dart modifier keywords from a declaration node's direct children.
    fn dart_modifiers(&self, node: &Node) -> Vec<String> {
        const KEYWORDS: &[&str] = &[
            "static", "abstract", "final", "const", "external", "factory", "late", "covariant",
        ];
        collect_matching_keywords(*node, KEYWORDS)
    }

    /// Walk up for the enclosing Dart class/mixin/extension container.
    fn dart_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            match n.kind() {
                "class_definition" | "extension_declaration" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        return Some(self.node_text(&name).trim().to_owned());
                    }
                }
                "mixin_declaration" => {
                    let name = (0..n.named_child_count())
                        .filter_map(|i| n.named_child(i))
                        .find(|c| c.kind() == "identifier")
                        .map(|c| self.node_text(&c).trim().to_owned());
                    if name.as_deref().map_or(false, |s| !s.is_empty()) {
                        return name;
                    }
                }
                _ => {}
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a Dart function or method (text before body).
    fn dart_signature(&self, node: &Node) -> Option<String> {
        let body = node
            .child_by_field_name("body")
            .or_else(|| node.child_by_field_name("function_body"))?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    fn dart_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if node.kind() == "identifier" {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    // Nodes where field "name" points to the declaration identifier.
                    let is_named_decl = matches!(
                        parent.kind(),
                        "function_signature"   // identifier "name" inside lambda_expression
                            | "method_declaration"
                            | "class_definition"
                            | "constructor_declaration"
                            | "getter_signature"
                            | "setter_signature"
                            | "extension_declaration"
                            | "variable_declarator"
                    ) && parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);

                    // mixin_declaration: identifier has no named field; treat the first
                    // identifier named child as the definition.
                    let is_mixin_name = parent.kind() == "mixin_declaration"
                        && (0..parent.named_child_count())
                            .filter_map(|i| parent.named_child(i))
                            .find(|c| c.kind() == "identifier")
                            .map(|c| c.id() == node.id())
                            .unwrap_or(false);

                    if is_named_decl || is_mixin_name {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.dart_occurrences(child, out);
            }
        }
    }

    fn dart_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = match node.kind() {
            "lambda_expression" => node
                .child_by_field_name("parameters")
                .filter(|c| c.kind() == "function_signature")
                .and_then(|sig| sig.child_by_field_name("name"))
                .map(|n| self.node_text(&n).to_owned()),
            "method_declaration" | "constructor_declaration" => node
                .child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned()),
            _ => None,
        };
        let effective = new_caller.or_else(|| caller.clone());

        // Dart: function invocations use `invocation_expression`; the function
        // being called is the `function` field (an identifier or member access).
        if node.kind() == "invocation_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    // selector_expression: `obj.method(...)` — use the rhs
                    "selector_expression" => func_node
                        .child_by_field_name("selector")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.dart_calls(child, effective.clone(), edges);
            }
        }
    }

    // ─── CPG call edge walkers ────────────────────────────────────────────

    fn rust_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        // Track the enclosing function name so we can label the caller side.
        let new_caller: Option<String> = if node.kind() == "function_item" {
            node.child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned())
        } else {
            None
        };
        let effective = new_caller.clone().or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "field_expression" => func_node
                        .child_by_field_name("field")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    "scoped_identifier" => func_node
                        .child_by_field_name("name")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.rust_calls(child, effective.clone(), edges);
            }
        }
    }

    fn ts_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = match node.kind() {
            "function_declaration" | "method_definition" => node
                .child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned()),
            _ => None,
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "member_expression" => func_node
                        .child_by_field_name("property")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.ts_calls(child, effective.clone(), edges);
            }
        }
    }

    fn py_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = if node.kind() == "function_definition" {
            node.child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned())
        } else {
            None
        };
        let effective = new_caller.or_else(|| caller.clone());

        // Python: node kind is "call", function field is the callee expression.
        if node.kind() == "call" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "attribute" => func_node
                        .child_by_field_name("attribute")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.py_calls(child, effective.clone(), edges);
            }
        }
    }

    // ─── C ───────────────────────────────────────────────────────────────────

    /// Recursively resolve an identifier name from a C/C++ declarator node.
    ///
    /// In tree-sitter-c/cpp, function names are buried inside nested declarator
    /// nodes: `function_declarator → pointer_declarator → identifier`.
    fn c_declarator_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "identifier" | "field_identifier" => {
                let name = self.node_text(&node);
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_owned())
                }
            }
            "function_declarator"
            | "pointer_declarator"
            | "array_declarator"
            | "abstract_function_declarator"
            | "parenthesized_declarator"
            | "reference_declarator" => {
                // Try the "declarator" field first; if absent (e.g. C++ member
                // functions have the field_identifier as a direct child without
                // a named field), fall back to scanning named children.
                if let Some(child) = node.child_by_field_name("declarator") {
                    return self.c_declarator_name(child);
                }
                for i in 0..node.named_child_count() {
                    let Some(c) = node.named_child(i) else { continue };
                    if matches!(
                        c.kind(),
                        "identifier"
                            | "field_identifier"
                            | "function_declarator"
                            | "pointer_declarator"
                            | "reference_declarator"
                            | "parenthesized_declarator"
                    ) {
                        if let Some(n) = self.c_declarator_name(c) {
                            return Some(n);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn c_function_name(&self, node: &Node) -> Option<String> {
        node.child_by_field_name("declarator")
            .and_then(|decl| self.c_declarator_name(decl))
    }

    /// Returns `true` if the node has a `static` storage-class specifier child.
    fn c_has_static_storage(&self, node: &Node) -> bool {
        (0..node.child_count()).any(|i| {
            node.child(i)
                .map(|c| c.kind() == "storage_class_specifier" && self.node_text(&c) == "static")
                .unwrap_or(false)
        })
    }

    fn c_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        match node.kind() {
            "function_definition" => {
                if let Some(name) = self.c_function_name(&node) {
                    let is_exported = !self.c_has_static_storage(&node);
                    let modifiers = self.c_modifiers(&node);
                    let (vis, vc) = visibility::infer(&name, &modifiers, self.language);
                    let signature = self.c_signature(&node);
                    let signature_normalized = signature
                        .as_deref()
                        .map(|s| normalize_signature(s, self.language));
                    out.push(OwnedSymbolInfo {
                        uri: self.lip_uri(&name),
                        display_name: name,
                        kind: SymbolKind::Function,
                        confidence_score: 30,
                        is_exported,
                        modifiers,
                        visibility: Some(vis),
                        visibility_confidence: Some(vc as f32 / 100.0),
                        signature,
                        signature_normalized,
                        extraction_tier: ExtractionTier::Tier1,
                        ..OwnedSymbolInfo::new("", "")
                    });
                }
            }
            "struct_specifier" | "union_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Class,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "enum_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Enum,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "type_definition" => {
                if let Some(decl_node) = node.child_by_field_name("declarator") {
                    if decl_node.kind() == "type_identifier" {
                        let name = self.node_text(&decl_node).to_owned();
                        if !name.is_empty() {
                            out.push(OwnedSymbolInfo {
                                uri: self.lip_uri(&name),
                                display_name: name,
                                kind: SymbolKind::TypeAlias,
                                confidence_score: 30,
                                is_exported: true,
                                visibility: Some(crate::schema::Visibility::Public),
                                visibility_confidence: Some(0.5),
                                extraction_tier: ExtractionTier::Tier1,
                                ..OwnedSymbolInfo::new("", "")
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.c_symbols(child, out);
            }
        }
    }

    /// Collect C/C++ modifier keywords from a function_definition's direct children.
    fn c_modifiers(&self, node: &Node) -> Vec<String> {
        const KEYWORDS: &[&str] = &[
            "static",
            "extern",
            "inline",
            "const",
            "virtual",
            "override",
            "final",
            "explicit",
            "constexpr",
        ];
        let mut mods = Vec::new();
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else { continue };
            match child.kind() {
                "storage_class_specifier" | "type_qualifier" | "function_specifier"
                | "virtual_function_specifier" | "explicit_function_specifier" => {
                    let t = self.node_text(&child).trim().to_owned();
                    if !t.is_empty() {
                        mods.push(t);
                    }
                }
                k if KEYWORDS.contains(&k) => mods.push(k.to_owned()),
                _ => {}
            }
        }
        mods
    }

    /// Declaration head for a C/C++ function (text before body block).
    fn c_signature(&self, node: &Node) -> Option<String> {
        if node.kind() != "function_definition" {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    /// Walk up for the enclosing C++ class/struct/namespace container.
    fn cpp_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            match n.kind() {
                "class_specifier"
                | "struct_specifier"
                | "union_specifier"
                | "namespace_definition" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        return Some(self.node_text(&name).trim().to_owned());
                    }
                }
                _ => {}
            }
            cur = n.parent();
        }
        None
    }

    fn c_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_def = matches!(
                        parent.kind(),
                        "function_definition"
                            | "function_declarator"
                            | "struct_specifier"
                            | "union_specifier"
                            | "enum_specifier"
                            | "type_definition"
                    ) && (parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false)
                        || parent
                            .child_by_field_name("declarator")
                            .map(|n| n.id() == node.id())
                            .unwrap_or(false));
                    if is_def {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.c_occurrences(child, out);
            }
        }
    }

    fn c_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = if node.kind() == "function_definition" {
            self.c_function_name(&node)
        } else {
            None
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "field_expression" => func_node
                        .child_by_field_name("field")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.c_calls(child, effective.clone(), edges);
            }
        }
    }

    // ─── C++ ─────────────────────────────────────────────────────────────────

    fn cpp_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        match node.kind() {
            // Shared with C
            "function_definition" => {
                if let Some(name) = self.c_function_name(&node) {
                    let is_exported = !self.c_has_static_storage(&node);
                    let modifiers = self.c_modifiers(&node);
                    let (vis, vc) = visibility::infer(&name, &modifiers, self.language);
                    let container = self.cpp_container(&node);
                    let signature = self.c_signature(&node);
                    let signature_normalized = signature
                        .as_deref()
                        .map(|s| normalize_signature(s, self.language));
                    // Inside a class body, treat as Method.
                    let kind = if container.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    out.push(OwnedSymbolInfo {
                        uri: self.lip_uri(&name),
                        display_name: name,
                        kind,
                        confidence_score: 30,
                        is_exported,
                        modifiers,
                        visibility: Some(vis),
                        visibility_confidence: Some(vc as f32 / 100.0),
                        container_name: container,
                        signature,
                        signature_normalized,
                        extraction_tier: ExtractionTier::Tier1,
                        ..OwnedSymbolInfo::new("", "")
                    });
                }
            }
            "struct_specifier" | "union_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Class,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "enum_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Enum,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "type_definition" => {
                if let Some(decl_node) = node.child_by_field_name("declarator") {
                    if decl_node.kind() == "type_identifier" {
                        let name = self.node_text(&decl_node).to_owned();
                        if !name.is_empty() {
                            out.push(OwnedSymbolInfo {
                                uri: self.lip_uri(&name),
                                display_name: name,
                                kind: SymbolKind::TypeAlias,
                                confidence_score: 30,
                                is_exported: true,
                                visibility: Some(crate::schema::Visibility::Public),
                                visibility_confidence: Some(0.5),
                                extraction_tier: ExtractionTier::Tier1,
                                ..OwnedSymbolInfo::new("", "")
                            });
                        }
                    }
                }
            }
            // C++ only
            "class_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Class,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "namespace_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name,
                            kind: SymbolKind::Namespace,
                            confidence_score: 30,
                            is_exported: true,
                            visibility: Some(crate::schema::Visibility::Public),
                            visibility_confidence: Some(0.5),
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.cpp_symbols(child, out);
            }
        }
    }

    fn cpp_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_def = matches!(
                        parent.kind(),
                        "function_definition"
                            | "function_declarator"
                            | "struct_specifier"
                            | "union_specifier"
                            | "enum_specifier"
                            | "type_definition"
                            | "class_specifier"
                            | "namespace_definition"
                    ) && (parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false)
                        || parent
                            .child_by_field_name("declarator")
                            .map(|n| n.id() == node.id())
                            .unwrap_or(false));
                    if is_def {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.cpp_occurrences(child, out);
            }
        }
    }

    fn cpp_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = if node.kind() == "function_definition" {
            self.c_function_name(&node)
        } else {
            None
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "field_expression" => func_node
                        .child_by_field_name("field")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    "qualified_identifier" => func_node
                        .child_by_field_name("name")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.cpp_calls(child, effective.clone(), edges);
            }
        }
    }

    // ─── Go ──────────────────────────────────────────────────────────────────

    fn go_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        match node.kind() {
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        let (vis, vc) = visibility::infer(&name, &[], self.language);
                        let signature = self.go_signature(&node);
                        let signature_normalized = signature
                            .as_deref()
                            .map(|s| normalize_signature(s, self.language));
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name.clone(),
                            kind: SymbolKind::Function,
                            confidence_score: 30,
                            is_exported: go_is_exported(&name),
                            visibility: Some(vis),
                            visibility_confidence: Some(vc as f32 / 100.0),
                            signature,
                            signature_normalized,
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_owned();
                    if !name.is_empty() {
                        let (vis, vc) = visibility::infer(&name, &[], self.language);
                        let container = self.go_receiver_type(&node);
                        let signature = self.go_signature(&node);
                        let signature_normalized = signature
                            .as_deref()
                            .map(|s| normalize_signature(s, self.language));
                        out.push(OwnedSymbolInfo {
                            uri: self.lip_uri(&name),
                            display_name: name.clone(),
                            kind: SymbolKind::Method,
                            confidence_score: 30,
                            is_exported: go_is_exported(&name),
                            visibility: Some(vis),
                            visibility_confidence: Some(vc as f32 / 100.0),
                            container_name: container,
                            signature,
                            signature_normalized,
                            extraction_tier: ExtractionTier::Tier1,
                            ..OwnedSymbolInfo::new("", "")
                        });
                    }
                }
            }
            "type_declaration" => {
                // type_declaration contains one or more type_spec children.
                for i in 0..node.named_child_count() {
                    if let Some(spec) = node.named_child(i) {
                        if spec.kind() == "type_spec" {
                            if let Some(name_node) = spec.child_by_field_name("name") {
                                let name = self.node_text(&name_node).to_owned();
                                if !name.is_empty() {
                                    let kind = spec
                                        .child_by_field_name("type")
                                        .map(|t| match t.kind() {
                                            "struct_type" => SymbolKind::Class,
                                            "interface_type" => SymbolKind::Interface,
                                            _ => SymbolKind::TypeAlias,
                                        })
                                        .unwrap_or(SymbolKind::TypeAlias);
                                    let (vis, vc) =
                                        visibility::infer(&name, &[], self.language);
                                    out.push(OwnedSymbolInfo {
                                        uri: self.lip_uri(&name),
                                        display_name: name.clone(),
                                        kind,
                                        confidence_score: 30,
                                        is_exported: go_is_exported(&name),
                                        visibility: Some(vis),
                                        visibility_confidence: Some(vc as f32 / 100.0),
                                        extraction_tier: ExtractionTier::Tier1,
                                        ..OwnedSymbolInfo::new("", "")
                                    });
                                }
                            }
                        }
                    }
                }
                return; // children already walked above
            }
            "const_declaration" => {
                for i in 0..node.named_child_count() {
                    if let Some(spec) = node.named_child(i) {
                        if spec.kind() == "const_spec" {
                            if let Some(name_node) = spec.child_by_field_name("name") {
                                let name = self.node_text(&name_node).to_owned();
                                if !name.is_empty() {
                                    let (vis, vc) =
                                        visibility::infer(&name, &[], self.language);
                                    out.push(OwnedSymbolInfo {
                                        uri: self.lip_uri(&name),
                                        display_name: name.clone(),
                                        kind: SymbolKind::Variable,
                                        confidence_score: 25,
                                        is_exported: go_is_exported(&name),
                                        visibility: Some(vis),
                                        visibility_confidence: Some(vc as f32 / 100.0),
                                        extraction_tier: ExtractionTier::Tier1,
                                        ..OwnedSymbolInfo::new("", "")
                                    });
                                }
                            }
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.go_symbols(child, out);
            }
        }
    }

    /// Declaration head for a Go function/method (text before body block).
    fn go_signature(&self, node: &Node) -> Option<String> {
        if !matches!(node.kind(), "function_declaration" | "method_declaration") {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    /// Receiver-type name for a Go method declaration, e.g. `Foo` in
    /// `func (f *Foo) Bar() {}`.
    fn go_receiver_type(&self, node: &Node) -> Option<String> {
        let recv = node.child_by_field_name("receiver")?;
        // `receiver` is a parameter_list; the first parameter_declaration has a
        // `type` field that may be `type_identifier` or `pointer_type → type_identifier`.
        for i in 0..recv.named_child_count() {
            let p = recv.named_child(i)?;
            if p.kind() != "parameter_declaration" {
                continue;
            }
            let ty = p.child_by_field_name("type")?;
            let ident = match ty.kind() {
                "type_identifier" => Some(ty),
                "pointer_type" => ty
                    .named_child(0)
                    .filter(|c| c.kind() == "type_identifier"),
                _ => None,
            };
            return ident.map(|n| self.node_text(&n).trim().to_owned());
        }
        None
    }

    fn go_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_def = matches!(
                        parent.kind(),
                        "function_declaration"
                            | "method_declaration"
                            | "type_spec"
                            | "const_spec"
                            | "var_spec"
                    ) && parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_def {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.go_occurrences(child, out);
            }
        }
    }

    fn go_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller: Option<String> = match node.kind() {
            "function_declaration" | "method_declaration" => node
                .child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned()),
            _ => None,
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee: &str = match func_node.kind() {
                    "identifier" => self.node_text(&func_node),
                    "selector_expression" => func_node
                        .child_by_field_name("field")
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.go_calls(child, effective.clone(), edges);
            }
        }
    }
}

/// Go exports by capitalization: an identifier is exported if its first character
/// is an uppercase Unicode letter.
fn go_is_exported(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// Kotlin and Swift implementations are in the `impl SymbolExtractor` block below
// (appended here to avoid splitting the struct).
//
// They are plain functions outside the impl block so we don't have to re-open
// it — instead they are free helpers called from the methods.
// Actually: the methods belong on the impl, so we re-open it:

impl<'a> SymbolExtractor<'a> {
    // ─── Kotlin ───────────────────────────────────────────────────────────────

    fn kotlin_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let (kind, recurse) = match node.kind() {
            "function_declaration" => (Some(SymbolKind::Function), true),
            "class_declaration" => {
                // Distinguish class vs interface: interfaces have an anonymous
                // "interface" keyword child (direct child, before the name).
                // Only scan direct children to avoid false positives inside the body.
                let is_interface = (0..node.child_count()).any(|i| {
                    node.child(i)
                        .map(|c| c.kind() == "interface")
                        .unwrap_or(false)
                });
                (
                    Some(if is_interface {
                        SymbolKind::Interface
                    } else {
                        SymbolKind::Class
                    }),
                    true,
                )
            }
            "object_declaration" => (Some(SymbolKind::Class), true),
            _ => (None, true),
        };

        if let Some(k) = kind {
            if let Some(name_node) = kotlin_first_name_child(node) {
                let name = self.node_text(&name_node).to_owned();
                if !name.is_empty() {
                    let is_exported = kotlin_is_exported(node);
                    let modifiers = kotlin_modifiers(node);
                    let (vis, vc) = visibility::infer(&name, &modifiers, self.language);
                    let container = self.kotlin_container(&node);
                    let signature = self.kotlin_signature(&node);
                    let signature_normalized = signature
                        .as_deref()
                        .map(|s| normalize_signature(s, self.language));
                    out.push(OwnedSymbolInfo {
                        uri: self.lip_uri(&name),
                        display_name: name,
                        kind: k,
                        confidence_score: 30,
                        is_exported,
                        modifiers,
                        visibility: Some(vis),
                        visibility_confidence: Some(vc as f32 / 100.0),
                        container_name: container,
                        signature,
                        signature_normalized,
                        extraction_tier: ExtractionTier::Tier1,
                        ..OwnedSymbolInfo::new("", "")
                    });
                }
            }
        }

        if recurse {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    self.kotlin_symbols(child, out);
                }
            }
        }
    }

    /// Walk up for the enclosing Kotlin class/object container.
    fn kotlin_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            if matches!(n.kind(), "class_declaration" | "object_declaration") {
                if let Some(name) = kotlin_first_name_child(n) {
                    return Some(self.node_text(&name).trim().to_owned());
                }
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a Kotlin function (text before body block).
    fn kotlin_signature(&self, node: &Node) -> Option<String> {
        if node.kind() != "function_declaration" {
            return None;
        }
        // Kotlin bodies come via field "body" (block or expression).
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        // Strip trailing `=` that introduces an expression body.
        let trimmed = text.trim().trim_end_matches('=').trim_end();
        Some(trimmed.to_owned())
    }

    fn kotlin_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "simple_identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_def = matches!(
                        parent.kind(),
                        "function_declaration"
                            | "class_declaration"
                            | "object_declaration"
                            | "property_declaration"
                    ) && kotlin_first_name_child(parent)
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_def {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.kotlin_occurrences(child, out);
            }
        }
    }

    fn kotlin_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller = match node.kind() {
            "function_declaration" => {
                kotlin_first_name_child(node).map(|n| self.node_text(&n).to_owned())
            }
            _ => None,
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child(0) {
                let callee: &str = match func_node.kind() {
                    "simple_identifier" => self.node_text(&func_node),
                    "navigation_expression" => func_node
                        .child_by_field_name("name")
                        .or_else(|| {
                            // last simple_identifier child
                            (0..func_node.named_child_count())
                                .rev()
                                .find_map(|i| func_node.named_child(i))
                                .filter(|n| n.kind() == "simple_identifier")
                        })
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.kotlin_calls(child, effective.clone(), edges);
            }
        }
    }

    // ─── Swift ────────────────────────────────────────────────────────────────

    fn swift_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let kind: Option<SymbolKind> = match node.kind() {
            "function_declaration" => Some(SymbolKind::Function),
            "class_declaration" => {
                // declaration_kind field: "class" | "struct" | "actor" | "enum" | "extension"
                let dk = node
                    .child_by_field_name("declaration_kind")
                    .map(|n| self.node_text(&n).to_owned())
                    .unwrap_or_default();
                match dk.as_str() {
                    "enum" => Some(SymbolKind::Enum),
                    "extension" => Some(SymbolKind::Namespace),
                    _ => Some(SymbolKind::Class), // class | struct | actor
                }
            }
            "protocol_declaration" => Some(SymbolKind::Interface),
            "typealias_declaration" => Some(SymbolKind::TypeAlias),
            _ => None,
        };

        if let Some(k) = kind {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = self.node_text(&name_node).to_owned();
                if !name.is_empty() {
                    let is_exported = swift_is_exported(node);
                    let modifiers = swift_modifiers(node);
                    let (vis, vc) = visibility::infer(&name, &modifiers, self.language);
                    let container = self.swift_container(&node);
                    let signature = self.swift_signature(&node);
                    let signature_normalized = signature
                        .as_deref()
                        .map(|s| normalize_signature(s, self.language));
                    out.push(OwnedSymbolInfo {
                        uri: self.lip_uri(&name),
                        display_name: name,
                        kind: k,
                        confidence_score: 30,
                        is_exported,
                        modifiers,
                        visibility: Some(vis),
                        visibility_confidence: Some(vc as f32 / 100.0),
                        container_name: container,
                        signature,
                        signature_normalized,
                        extraction_tier: ExtractionTier::Tier1,
                        ..OwnedSymbolInfo::new("", "")
                    });
                }
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.swift_symbols(child, out);
            }
        }
    }

    /// Walk up for the enclosing Swift class/protocol/extension container.
    fn swift_container(&self, node: &Node) -> Option<String> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            if matches!(
                n.kind(),
                "class_declaration" | "protocol_declaration" | "extension_declaration"
            ) {
                if let Some(name) = n.child_by_field_name("name") {
                    return Some(self.node_text(&name).trim().to_owned());
                }
            }
            cur = n.parent();
        }
        None
    }

    /// Declaration head for a Swift function (text before body block).
    fn swift_signature(&self, node: &Node) -> Option<String> {
        if node.kind() != "function_declaration" {
            return None;
        }
        let body = node.child_by_field_name("body")?;
        let text =
            std::str::from_utf8(&self.source[node.start_byte()..body.start_byte()]).ok()?;
        Some(text.trim().to_owned())
    }

    fn swift_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "simple_identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_def = matches!(
                        parent.kind(),
                        "function_declaration"
                            | "class_declaration"
                            | "protocol_declaration"
                            | "typealias_declaration"
                    ) && parent
                        .child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_def {
                        Role::Definition
                    } else {
                        Role::Reference
                    }
                });
                out.push(self.make_occurrence(&node, name, role));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.swift_occurrences(child, out);
            }
        }
    }

    fn swift_calls(&self, node: Node, caller: Option<String>, edges: &mut Vec<OwnedGraphEdge>) {
        let new_caller = match node.kind() {
            "function_declaration" => node
                .child_by_field_name("name")
                .map(|n| self.node_text(&n).to_owned()),
            _ => None,
        };
        let effective = new_caller.or_else(|| caller.clone());

        if node.kind() == "call_expression" {
            // Swift: call_expression → function field contains the callee
            let callee_node = node
                .child_by_field_name("function")
                .or_else(|| node.child(0));
            if let Some(func_node) = callee_node {
                let callee: &str = match func_node.kind() {
                    "simple_identifier" => self.node_text(&func_node),
                    "navigation_expression" => func_node
                        .child_by_field_name("name")
                        .or_else(|| {
                            (0..func_node.named_child_count())
                                .rev()
                                .find_map(|i| func_node.named_child(i))
                                .filter(|n| n.kind() == "simple_identifier")
                        })
                        .map(|n| self.node_text(&n))
                        .unwrap_or(""),
                    _ => "",
                };
                if !callee.is_empty() {
                    if let Some(ref c) = effective {
                        if !c.is_empty() {
                            edges.push(OwnedGraphEdge {
                                from_uri: self.lip_uri(c),
                                to_uri: self.lip_uri(callee),
                                kind: EdgeKind::Calls,
                                at_range: Self::node_range(&node),
                            });
                        }
                    }
                }
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.swift_calls(child, effective.clone(), edges);
            }
        }
    }
}

// ─── Kotlin helpers ───────────────────────────────────────────────────────────

/// Return the first `simple_identifier` or `type_identifier` named child —
/// Kotlin's grammar does not use named `field()` wrappers for declaration names.
fn kotlin_first_name_child(node: Node) -> Option<Node> {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if matches!(child.kind(), "simple_identifier" | "type_identifier") {
                return Some(child);
            }
        }
    }
    None
}

/// Collect every node in `node`'s subtree whose `kind()` matches one of
/// `keywords`. Duplicates are preserved — callers that want unique values
/// must deduplicate. Tree-sitter anonymous nodes use their source text as
/// their kind, so matching on kind is reliable without reading source bytes.
fn collect_matching_keywords(node: Node, keywords: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    walk_collect_keywords(node, keywords, &mut out);
    out
}

fn walk_collect_keywords(node: Node, keywords: &[&str], out: &mut Vec<String>) {
    if keywords.contains(&node.kind()) {
        out.push(node.kind().to_owned());
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_collect_keywords(child, keywords, out);
        }
    }
}

/// Returns true if any node in the subtree has a kind matching one of the given keywords.
///
/// Tree-sitter anonymous nodes (keywords) use their source text as their kind, so
/// checking `node.kind() == "private"` is reliable without needing source bytes.
fn subtree_has_keyword(node: Node, keywords: &[&str]) -> bool {
    if keywords.contains(&node.kind()) {
        return true;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if subtree_has_keyword(child, keywords) {
                return true;
            }
        }
    }
    false
}

/// A Kotlin declaration is exported unless it explicitly carries a `private`
/// or `protected` visibility modifier.
fn kotlin_is_exported(node: Node) -> bool {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if child.kind() == "modifiers" && subtree_has_keyword(child, &["private", "protected"])
            {
                return false;
            }
        }
    }
    true
}

/// Collect Kotlin modifier keywords from the `modifiers` child node.
fn kotlin_modifiers(node: Node) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "private", "protected", "internal", "public", "abstract", "final", "open", "override",
        "suspend", "inline", "external", "data", "sealed", "enum", "companion", "lateinit",
        "const", "operator", "infix", "tailrec",
    ];
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if child.kind() == "modifiers" {
                return collect_matching_keywords(child, KEYWORDS);
            }
        }
    }
    Vec::new()
}

/// A Swift declaration is exported unless it has a `private` or `fileprivate`
/// access modifier.
fn swift_is_exported(node: Node) -> bool {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if matches!(child.kind(), "modifiers" | "modifier")
                && subtree_has_keyword(child, &["private", "fileprivate"])
            {
                return false;
            }
        }
    }
    true
}

/// Collect Swift modifier keywords from any `modifiers`/`modifier` child.
fn swift_modifiers(node: Node) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "private",
        "fileprivate",
        "internal",
        "public",
        "open",
        "static",
        "final",
        "override",
        "mutating",
        "nonmutating",
        "class",
        "required",
        "convenience",
        "lazy",
        "weak",
        "unowned",
    ];
    let mut out = Vec::new();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if matches!(child.kind(), "modifiers" | "modifier") {
                out.extend(collect_matching_keywords(child, KEYWORDS));
            }
        }
    }
    out
}
