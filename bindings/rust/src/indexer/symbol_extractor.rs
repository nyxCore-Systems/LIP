use tree_sitter::{Node, Tree};

use crate::schema::{EdgeKind, OwnedGraphEdge, OwnedOccurrence, OwnedRange, OwnedSymbolInfo, Role, SymbolKind};

use super::language::Language;

/// Walks a tree-sitter parse tree and extracts LIP symbols and occurrences.
/// Produces Tier 1 results: confidence_score in the 1–50 range.
pub struct SymbolExtractor<'a> {
    source:   &'a [u8],
    language: Language,
    file_uri: &'a str,
}

impl<'a> SymbolExtractor<'a> {
    pub fn new(source: &'a [u8], language: Language, file_uri: &'a str) -> Self {
        Self { source, language, file_uri }
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
            Language::Rust       => self.rust_calls(node, caller, edges),
            Language::TypeScript => self.ts_calls(node, caller, edges),
            Language::Python     => self.py_calls(node, caller, edges),
            Language::Dart       => self.dart_calls(node, caller, edges),
            Language::Unknown    => {}
        }
    }

    fn node_text(&self, node: &Node) -> &str {
        std::str::from_utf8(&self.source[node.start_byte()..node.end_byte()])
            .unwrap_or("")
    }

    fn node_range(node: &Node) -> OwnedRange {
        let start = node.start_position();
        let end   = node.end_position();
        OwnedRange {
            start_line: start.row as i32,
            start_char: start.column as i32,
            end_line:   end.row as i32,
            end_char:   end.column as i32,
        }
    }

    fn lip_uri(&self, name: &str) -> String {
        // Strip the file:// scheme so we don't produce lip://local/file:///abs/path#Name.
        let path = self.file_uri
            .strip_prefix("file://")
            .unwrap_or(self.file_uri);
        format!("lip://local/{path}#{name}")
    }

    // ── Rust ─────────────────────────────────────────────────────────────────

    fn walk_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        match self.language {
            Language::Rust       => self.rust_symbols(node, out),
            Language::TypeScript => self.ts_symbols(node, out),
            Language::Python     => self.py_symbols(node, out),
            Language::Dart       => self.dart_symbols(node, out),
            Language::Unknown    => {}
        }
    }

    fn walk_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        match self.language {
            Language::Rust       => self.rust_occurrences(node, out),
            Language::TypeScript => self.ts_occurrences(node, out),
            Language::Python     => self.py_occurrences(node, out),
            Language::Dart       => self.dart_occurrences(node, out),
            Language::Unknown    => {}
        }
    }

    // ─── Rust ────────────────────────────────────────────────────────────────

    fn rust_symbols(&self, node: Node, out: &mut Vec<OwnedSymbolInfo>) {
        let (kind, name_field) = match node.kind() {
            "function_item"    => (SymbolKind::Function, "name"),
            "struct_item"      => (SymbolKind::Class,    "name"),
            "enum_item"        => (SymbolKind::Enum,     "name"),
            "trait_item"       => (SymbolKind::Interface,"name"),
            "type_item"        => (SymbolKind::TypeAlias,"name"),
            "mod_item"         => (SymbolKind::Namespace,"name"),
            "macro_definition" => (SymbolKind::Macro,    "name"),
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
                out.push(OwnedSymbolInfo {
                    uri:              self.lip_uri(name),
                    display_name:     name.to_owned(),
                    kind,
                    confidence_score: 30,
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

    fn rust_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                // Identifiers that are the `name` field of a declaration node are
                // definitions; all other identifier uses are references.
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_item"    | "struct_item"   | "enum_item"  |
                        "trait_item"       | "type_item"     | "mod_item"   |
                        "macro_definition" | "field_declaration" | "variant"
                    );
                    let is_name = parent.child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name { Role::Definition } else { Role::Reference }
                });
                out.push(OwnedOccurrence {
                    symbol_uri:       self.lip_uri(name),
                    range:            Self::node_range(&node),
                    confidence_score: 20,
                    role,
                    override_doc:     None,
                });
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
            "function_declaration"         => (SymbolKind::Function,  "name"),
            "method_definition"            => (SymbolKind::Method,    "name"),
            "class_declaration"            => (SymbolKind::Class,     "name"),
            "interface_declaration"        => (SymbolKind::Interface, "name"),
            "type_alias_declaration"       => (SymbolKind::TypeAlias, "name"),
            "enum_declaration"             => (SymbolKind::Enum,      "name"),
            "lexical_declaration"          => {
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
                        out.push(OwnedSymbolInfo {
                            uri:              self.lip_uri(name),
                            display_name:     name.to_owned(),
                            kind:             SymbolKind::Variable,
                            confidence_score: 25,
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
                out.push(OwnedSymbolInfo {
                    uri:              self.lip_uri(name),
                    display_name:     name.to_owned(),
                    kind,
                    confidence_score: 30,
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

    fn ts_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_declaration" | "method_definition"   | "class_declaration"   |
                        "interface_declaration"| "type_alias_declaration" | "enum_declaration"  |
                        "variable_declarator"
                    );
                    let is_name = parent.child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name { Role::Definition } else { Role::Reference }
                });
                out.push(OwnedOccurrence {
                    symbol_uri:       self.lip_uri(name),
                    range:            Self::node_range(&node),
                    confidence_score: 20,
                    role,
                    override_doc:     None,
                });
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
            "function_definition"   => (SymbolKind::Function,  "name"),
            "class_definition"      => (SymbolKind::Class,     "name"),
            "decorated_definition"  => {
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
                out.push(OwnedSymbolInfo {
                    uri:              self.lip_uri(name),
                    display_name:     name.to_owned(),
                    kind,
                    confidence_score: 30,
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

    fn py_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if node.kind() == "identifier" {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_definition" | "class_definition"
                    );
                    let is_name = parent.child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name { Role::Definition } else { Role::Reference }
                });
                out.push(OwnedOccurrence {
                    symbol_uri:       self.lip_uri(name),
                    range:            Self::node_range(&node),
                    confidence_score: 20,
                    role,
                    override_doc:     None,
                });
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
        let (kind, name_field) = match node.kind() {
            "function_declaration"  => (SymbolKind::Function,    "name"),
            "method_declaration"    => (SymbolKind::Method,      "name"),
            "class_declaration"     => (SymbolKind::Class,       "name"),
            "constructor_declaration" => (SymbolKind::Constructor, "name"),
            "getter_signature"      => (SymbolKind::Method,      "name"),
            "setter_signature"      => (SymbolKind::Method,      "name"),
            "mixin_declaration"     => (SymbolKind::Class,       "name"),
            "extension_declaration" => (SymbolKind::Namespace,   "name"),
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.dart_symbols(child, out);
                    }
                }
                return;
            }
        };

        if let Some(name_node) = node.child_by_field_name(name_field) {
            let name = self.node_text(&name_node);
            if !name.is_empty() {
                out.push(OwnedSymbolInfo {
                    uri:              self.lip_uri(name),
                    display_name:     name.to_owned(),
                    kind,
                    confidence_score: 30,
                    ..OwnedSymbolInfo::new("", "")
                });
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.dart_symbols(child, out);
            }
        }
    }

    fn dart_occurrences(&self, node: Node, out: &mut Vec<OwnedOccurrence>) {
        if node.kind() == "identifier" {
            let name = self.node_text(&node);
            if !name.is_empty() {
                let role = node.parent().map_or(Role::Reference, |parent| {
                    let is_decl = matches!(
                        parent.kind(),
                        "function_declaration"
                        | "method_declaration"
                        | "class_declaration"
                        | "constructor_declaration"
                        | "getter_signature"
                        | "setter_signature"
                        | "mixin_declaration"
                        | "extension_declaration"
                        | "variable_declarator"
                    );
                    let is_name = parent.child_by_field_name("name")
                        .map(|n| n.id() == node.id())
                        .unwrap_or(false);
                    if is_decl && is_name { Role::Definition } else { Role::Reference }
                });
                out.push(OwnedOccurrence {
                    symbol_uri:       self.lip_uri(name),
                    range:            Self::node_range(&node),
                    confidence_score: 20,
                    role,
                    override_doc:     None,
                });
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
            "function_declaration" | "method_declaration" | "constructor_declaration" => {
                node.child_by_field_name("name")
                    .map(|n| self.node_text(&n).to_owned())
            }
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
                                to_uri:   self.lip_uri(callee),
                                kind:     EdgeKind::Calls,
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
                                to_uri:   self.lip_uri(callee),
                                kind:     EdgeKind::Calls,
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
                                to_uri:   self.lip_uri(callee),
                                kind:     EdgeKind::Calls,
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
                                to_uri:   self.lip_uri(callee),
                                kind:     EdgeKind::Calls,
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
}
