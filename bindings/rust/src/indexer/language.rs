use std::path::Path;

/// Language recognized by the Tier 1 tree-sitter indexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Dart,
    Unknown,
}

impl Language {
    /// Detect language from the file URI/path and/or a language hint string.
    pub fn detect(uri: &str, hint: &str) -> Self {
        // Honour an explicit hint first.
        match hint.to_lowercase().as_str() {
            "rust"       => return Language::Rust,
            "typescript" | "ts" => return Language::TypeScript,
            "python"     | "py" => return Language::Python,
            "dart"       => return Language::Dart,
            _ => {}
        }

        // Fall back to file extension.
        let ext = Path::new(uri)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            "rs"            => Language::Rust,
            "ts" | "tsx"    => Language::TypeScript,
            "py"            => Language::Python,
            "dart"          => Language::Dart,
            _               => Language::Unknown,
        }
    }

    /// Returns the tree-sitter grammar for this language, if supported.
    pub fn tree_sitter_grammar(self) -> Option<tree_sitter::Language> {
        match self {
            Language::Rust       => Some(tree_sitter_rust::language()),
            Language::TypeScript => Some(tree_sitter_typescript::language_typescript()),
            Language::Python     => Some(tree_sitter_python::language()),
            Language::Dart       => Some(tree_sitter_dart::language()),
            Language::Unknown    => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust       => "rust",
            Language::TypeScript => "typescript",
            Language::Python     => "python",
            Language::Dart       => "dart",
            Language::Unknown    => "unknown",
        }
    }
}
