//! Module-identifier resolution for `ImpactItem.module_id` (v2.3.4).
//!
//! Resolves a stable "which module does this file belong to" string that CKB's
//! risk classifier uses to weight cross-module blast. Three tiers, tried in
//! order, first hit wins:
//!
//! 1. **Third-party slice URI** — `lip://<manager>/<package>@<version>/...`
//!    → `"<manager>/<package>"`. Covers mounted dependency slices.
//! 2. **SCIP symbol descriptor** — the `<manager> <name>` pair from any SCIP
//!    symbol's package descriptor. Covers every file imported via
//!    `upsert_file_precomputed` with real SCIP metadata.
//! 3. **Manifest walk** — upward walk from the file's directory looking for a
//!    language-appropriate manifest (Cargo.toml, go.mod, package.json,
//!    pyproject.toml / setup.py, pubspec.yaml). Covers tier-1-only local
//!    files.
//!
//! Unsupported languages (C/C++/Kotlin/Swift/Java) return `None` from the
//! manifest walk; they still get a value from tiers 1 or 2 if applicable.
//! Everything is best-effort and never blocks indexing: parse errors and I/O
//! failures return `None` rather than propagating.

use std::path::{Path, PathBuf};

use crate::schema::OwnedSymbolInfo;

/// Maximum upward hops during manifest resolution. Twelve is enough for any
/// realistic monorepo (file depth from manifest rarely exceeds 6–8).
const MANIFEST_WALK_DEPTH_CAP: usize = 12;

/// Resolve the module identifier for a file, trying three tiers in order.
///
/// `scip_symbols` may be empty for tier-1-only files; the second tier is
/// skipped in that case.
pub(crate) fn resolve_module_id(
    uri: &str,
    language: &str,
    scip_symbols: &[OwnedSymbolInfo],
) -> Option<String> {
    if let Some(id) = from_slice_uri(uri) {
        return Some(id);
    }
    for sym in scip_symbols {
        if let Some(id) = from_scip_symbol(&sym.uri) {
            return Some(id);
        }
    }
    if let Some(path) = crate::daemon::watcher::uri_to_path(uri) {
        return from_manifest_walk(&path, language);
    }
    None
}

/// Tier 1: slice URIs carry the manager and package in the URI itself.
///
/// `lip://cargo/serde@1.0.0/...` → `"cargo/serde"`.
/// `lip://local/...` is NOT a slice — returns `None`.
fn from_slice_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("lip://")?;
    if rest.starts_with("local/") || rest == "local" {
        return None;
    }
    let first_slash = rest.find('/')?;
    let manager = &rest[..first_slash];
    let after_manager = &rest[first_slash + 1..];
    // Package extends to the first `@` (version marker) or `/` (path) — whichever comes first.
    let pkg_end = after_manager
        .find(|c: char| c == '@' || c == '/')
        .unwrap_or(after_manager.len());
    let package = &after_manager[..pkg_end];
    if manager.is_empty() || package.is_empty() {
        return None;
    }
    Some(format!("{manager}/{package}"))
}

/// Tier 2: SCIP symbols are space-separated `<scheme> <manager> <name> <version> <descriptors...>`.
///
/// Returns `"<manager>/<name>"` when all four header tokens are present and
/// non-sentinel. SCIP's `local <id>` short-form has only two tokens and
/// returns `None`, as do empty-sentinel packages (`. . .`).
fn from_scip_symbol(symbol: &str) -> Option<String> {
    let mut parts = symbol.split_whitespace();
    let _scheme = parts.next()?;
    let manager = parts.next()?;
    let name = parts.next()?;
    let _version = parts.next()?;
    if manager == "." || name == "." || manager.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{manager}/{name}"))
}

/// Tier 3: walk upward from the file's directory looking for a manifest
/// recognised for `language`. Returns the manifest's "name" value on first
/// hit.
fn from_manifest_walk(file_path: &Path, language: &str) -> Option<String> {
    let parsers = manifests_for_language(language);
    if parsers.is_empty() {
        return None;
    }
    let mut current: PathBuf = file_path.parent()?.to_path_buf();
    for _ in 0..MANIFEST_WALK_DEPTH_CAP {
        for (filename, parser) in &parsers {
            let candidate = current.join(filename);
            if candidate.is_file() {
                if let Ok(text) = std::fs::read_to_string(&candidate) {
                    if let Some(name) = parser(&text) {
                        return Some(name);
                    }
                }
            }
        }
        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }
    None
}

type ManifestParser = fn(&str) -> Option<String>;

fn manifests_for_language(language: &str) -> Vec<(&'static str, ManifestParser)> {
    match language.to_ascii_lowercase().as_str() {
        "rust" => vec![("Cargo.toml", parse_cargo_toml)],
        "go" => vec![("go.mod", parse_go_mod)],
        "typescript" | "javascript" | "tsx" | "jsx" | "typescriptreact" | "javascriptreact" => {
            vec![("package.json", parse_package_json)]
        }
        "python" => vec![
            ("pyproject.toml", parse_pyproject_toml),
            ("setup.py", parse_setup_py),
        ],
        "dart" => vec![("pubspec.yaml", parse_pubspec_yaml)],
        _ => vec![],
    }
}

/// `[package]` section, `name = "foo"` line. Stops at the first subsequent
/// section header so workspace roots whose `[workspace] members = [...]`
/// precedes `[package]` still parse.
fn parse_cargo_toml(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let t = strip_toml_comment(line).trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_package = t == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = t.strip_prefix("name") {
            if let Some(name) = parse_assignment_value(rest) {
                return Some(name);
            }
        }
    }
    None
}

/// `module github.com/foo/bar` — first `module` directive wins.
fn parse_go_mod(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = strip_go_comment(line).trim();
        if let Some(rest) = t.strip_prefix("module") {
            let rest = rest.trim_start();
            if rest.is_empty() {
                continue;
            }
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// JSON "name": "foo" at top level. Conservative: bails if the value is not
/// a double-quoted string. Doesn't attempt full JSON parsing — package.json
/// may contain comments in workspaces that ship `package.json5`, but the
/// common case is well-formed JSON.
fn parse_package_json(text: &str) -> Option<String> {
    let mut idx = 0;
    let bytes = text.as_bytes();
    while idx < bytes.len() {
        if let Some(k) = find_json_key(&text[idx..], "name") {
            let after = &text[idx + k..];
            if let Some(value) = parse_json_string_value(after) {
                if !value.is_empty() {
                    return Some(value);
                }
            }
            idx += k + 1;
        } else {
            break;
        }
    }
    None
}

/// Look for `"name": "..."` — returns the offset *past* the key, at the colon
/// or beyond.
fn find_json_key(text: &str, key: &str) -> Option<usize> {
    let pattern = format!("\"{key}\"");
    let mut search_start = 0;
    while let Some(pos) = text[search_start..].find(&pattern) {
        let abs = search_start + pos;
        // Ensure the next non-whitespace char is `:` (so this is a key, not a value).
        let after = &text[abs + pattern.len()..];
        let trimmed = after.trim_start();
        if trimmed.starts_with(':') {
            return Some(abs + pattern.len());
        }
        search_start = abs + pattern.len();
    }
    None
}

fn parse_json_string_value(after_key: &str) -> Option<String> {
    let colon_idx = after_key.find(':')?;
    let after_colon = &after_key[colon_idx + 1..];
    let trimmed = after_colon.trim_start();
    if !trimmed.starts_with('"') {
        return None;
    }
    let body = &trimmed[1..];
    // No escape handling — package names never contain backslashes or quotes.
    let end = body.find('"')?;
    Some(body[..end].to_owned())
}

/// `[project]` or `[tool.poetry]` section, `name = "foo"` line.
fn parse_pyproject_toml(text: &str) -> Option<String> {
    let mut section = String::new();
    for line in text.lines() {
        let t = strip_toml_comment(line).trim();
        if t.starts_with('[') && t.ends_with(']') {
            section = t[1..t.len() - 1].to_owned();
            continue;
        }
        if section != "project" && section != "tool.poetry" {
            continue;
        }
        if let Some(rest) = t.strip_prefix("name") {
            if let Some(name) = parse_assignment_value(rest) {
                return Some(name);
            }
        }
    }
    None
}

/// `setup(name="foo", ...)` — only tolerates the single common form.
fn parse_setup_py(text: &str) -> Option<String> {
    let mut idx = 0;
    while let Some(pos) = text[idx..].find("name") {
        let abs = idx + pos;
        let after = &text[abs + 4..];
        let trimmed = after.trim_start();
        if !trimmed.starts_with('=') {
            idx = abs + 4;
            continue;
        }
        let after_eq = trimmed[1..].trim_start();
        let quote = after_eq.chars().next()?;
        if quote != '"' && quote != '\'' {
            idx = abs + 4;
            continue;
        }
        let body = &after_eq[1..];
        if let Some(end) = body.find(quote) {
            let name = &body[..end];
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
        idx = abs + 4;
    }
    None
}

/// Top-level `name: foo` line. Pub files are shallow YAML; full-YAML parsing
/// is overkill.
fn parse_pubspec_yaml(text: &str) -> Option<String> {
    for line in text.lines() {
        // Ignore indented lines (nested keys).
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let t = strip_yaml_comment(line).trim_end();
        if let Some(rest) = t.strip_prefix("name:") {
            let name = rest.trim().trim_matches('"').trim_matches('\'');
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

// ── small helpers ──────────────────────────────────────────────────────────

fn strip_toml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn strip_go_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn strip_yaml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Parse the RHS of a TOML `name = "foo"` assignment. `rest` is the slice
/// starting right after the `name` keyword.
fn parse_assignment_value(rest: &str) -> Option<String> {
    let r = rest.trim_start();
    let r = r.strip_prefix('=')?;
    let r = r.trim();
    // Strip surrounding single or double quotes, if any.
    let unquoted = r
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| r.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(r);
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_owned())
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(uri: &str) -> OwnedSymbolInfo {
        OwnedSymbolInfo {
            uri: uri.to_owned(),
            display_name: String::new(),
            kind: crate::schema::SymbolKind::Function,
            documentation: None,
            signature: None,
            confidence_score: 0,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
            ..Default::default()
        }
    }

    #[test]
    fn slice_uri_returns_manager_slash_package() {
        assert_eq!(
            from_slice_uri("lip://cargo/serde@1.0.0/src/lib.rs#Deserialize"),
            Some("cargo/serde".to_owned())
        );
        assert_eq!(
            from_slice_uri("lip://npm/react@18.2.0/index.js"),
            Some("npm/react".to_owned())
        );
        assert_eq!(
            from_slice_uri("lip://gomod/github.com/foo/bar@v1.0/pkg/foo.go"),
            Some("gomod/github.com".to_owned())
        );
    }

    #[test]
    fn slice_uri_rejects_local_scheme() {
        assert_eq!(from_slice_uri("lip://local/src/main.rs"), None);
        assert_eq!(from_slice_uri("lip://local//Users/a/proj/src/main.rs"), None);
    }

    #[test]
    fn scip_symbol_extracts_manager_and_name() {
        assert_eq!(
            from_scip_symbol("scip-go gomod github.com/foo/bar v1.0 internal/query/SearchSymbols()."),
            Some("gomod/github.com/foo/bar".to_owned())
        );
        assert_eq!(
            from_scip_symbol("scip-typescript npm react 18.2.0 src/`App.tsx`/App#"),
            Some("npm/react".to_owned())
        );
    }

    #[test]
    fn scip_symbol_rejects_local_and_sentinels() {
        assert_eq!(from_scip_symbol("local 42"), None);
        assert_eq!(from_scip_symbol("scip-go . . . foo/bar."), None);
        assert_eq!(from_scip_symbol(""), None);
    }

    #[test]
    fn parse_cargo_toml_extracts_crate_name() {
        let toml = r#"
[package]
name = "my-crate"
version = "0.1.0"
"#;
        assert_eq!(parse_cargo_toml(toml), Some("my-crate".to_owned()));
    }

    #[test]
    fn parse_cargo_toml_ignores_dependency_names() {
        // `name` under `[dependencies]` must not be confused with the package name.
        let toml = r#"
[dependencies]
name = "should-not-match"

[package]
name = "real-crate"
"#;
        assert_eq!(parse_cargo_toml(toml), Some("real-crate".to_owned()));
    }

    #[test]
    fn parse_cargo_toml_workspace_only_returns_none() {
        let toml = r#"
[workspace]
members = ["a", "b"]
"#;
        assert_eq!(parse_cargo_toml(toml), None);
    }

    #[test]
    fn parse_go_mod_extracts_module_path() {
        let gomod = "module github.com/foo/bar\n\ngo 1.21\n";
        assert_eq!(
            parse_go_mod(gomod),
            Some("github.com/foo/bar".to_owned())
        );
    }

    #[test]
    fn parse_package_json_extracts_name() {
        let json = r#"{
  "name": "@scope/pkg",
  "version": "1.0.0"
}"#;
        assert_eq!(parse_package_json(json), Some("@scope/pkg".to_owned()));
    }

    #[test]
    fn parse_package_json_ignores_name_inside_values() {
        // "name" appearing inside a description value must not hijack the parse.
        let json = r#"{
  "description": "a pkg whose name is special",
  "name": "real-name"
}"#;
        assert_eq!(parse_package_json(json), Some("real-name".to_owned()));
    }

    #[test]
    fn parse_pyproject_toml_project_section() {
        let toml = r#"
[project]
name = "my-py-pkg"
version = "0.1"
"#;
        assert_eq!(parse_pyproject_toml(toml), Some("my-py-pkg".to_owned()));
    }

    #[test]
    fn parse_pyproject_toml_poetry_section() {
        let toml = r#"
[tool.poetry]
name = "poetry-pkg"
"#;
        assert_eq!(parse_pyproject_toml(toml), Some("poetry-pkg".to_owned()));
    }

    #[test]
    fn parse_setup_py_double_and_single_quotes() {
        assert_eq!(
            parse_setup_py("setup(name=\"pkg-a\", version=\"1.0\")"),
            Some("pkg-a".to_owned())
        );
        assert_eq!(
            parse_setup_py("setup(name='pkg-b')"),
            Some("pkg-b".to_owned())
        );
    }

    #[test]
    fn parse_pubspec_yaml_name() {
        let yaml = "name: my_dart_pkg\nversion: 0.1.0\n";
        assert_eq!(parse_pubspec_yaml(yaml), Some("my_dart_pkg".to_owned()));
    }

    #[test]
    fn parse_pubspec_yaml_ignores_nested_name() {
        let yaml = "dependencies:\n  name: not-this\nname: root_pkg\n";
        assert_eq!(parse_pubspec_yaml(yaml), Some("root_pkg".to_owned()));
    }

    #[test]
    fn resolve_prefers_slice_uri_over_scip() {
        let scip_sym = sym("scip-go gomod github.com/foo/bar v1.0 pkg/Baz().");
        let id = resolve_module_id(
            "lip://cargo/serde@1.0.0/src/lib.rs",
            "rust",
            &[scip_sym],
        );
        assert_eq!(id, Some("cargo/serde".to_owned()));
    }

    #[test]
    fn resolve_falls_back_to_scip_when_no_slice() {
        let scip_sym = sym("scip-go gomod github.com/foo/bar v1.0 pkg/Baz().");
        let id = resolve_module_id("lip://local/pkg/foo.go", "go", &[scip_sym]);
        assert_eq!(id, Some("gomod/github.com/foo/bar".to_owned()));
    }

    #[test]
    fn resolve_walks_manifest_for_tier1_rust_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("Cargo.toml");
        std::fs::write(&cargo, "[package]\nname = \"walker-crate\"\n").unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let main = src_dir.join("main.rs");
        std::fs::write(&main, "fn main() {}\n").unwrap();

        let uri = format!("lip://local/{}", main.display());
        let id = resolve_module_id(&uri, "rust", &[]);
        assert_eq!(id, Some("walker-crate".to_owned()));
    }

    #[test]
    fn resolve_returns_none_when_unsupported_language_and_no_scip() {
        let id = resolve_module_id("lip://local/foo.c", "c", &[]);
        assert_eq!(id, None);
    }
}
