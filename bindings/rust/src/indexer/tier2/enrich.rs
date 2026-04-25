//! Shared v2.3 structural-metadata enrichment for Tier-2 LSP backends.
//!
//! Every backend calls [`enrich_v23`] once per symbol before pushing it into the
//! result set. The helper:
//!
//! 1. Tags the record with `extraction_tier = Tier2`.
//! 2. Extracts modifier keywords from the LSP-provided signature prefix.
//! 3. Resolves canonical `visibility` + `visibility_confidence` via
//!    [`crate::schema::visibility::infer`] — the same oracle Tier 1 uses.
//! 4. Records `container_name` from LSP `SymbolInformation.containerName`.
//! 5. Normalises the signature via [`crate::schema::normalize_signature`].
//!
//! `modifiers_source` stays `None` — that field is reserved for SCIP imports
//! (spec §v2.3 C.5). Tier-2 native paths are self-evidently LSP-verified.

use crate::indexer::language::Language;
use crate::schema::{normalize_signature, visibility, ExtractionTier, OwnedSymbolInfo};

/// Populate the v2.3 structural-metadata fields on `sym` in place.
///
/// `signature` is the raw hover text (may be `None` when the server returned no
/// hover). `container` is the LSP `containerName` for the symbol, if any.
pub fn enrich_v23(
    sym: &mut OwnedSymbolInfo,
    signature: Option<&str>,
    container: Option<String>,
    lang: Language,
) {
    sym.extraction_tier = ExtractionTier::Tier2;

    let modifiers = signature
        .map(|s| extract_modifiers(s, lang))
        .unwrap_or_default();

    let (vis, confidence) = visibility::infer(&sym.display_name, &modifiers, lang);
    sym.visibility = Some(vis);
    sym.visibility_confidence = Some(confidence as f32 / 100.0);
    sym.modifiers = modifiers;

    if let Some(name) = container.filter(|n| !n.is_empty()) {
        sym.container_name = Some(name);
    }

    if let Some(sig) = signature {
        sym.signature_normalized = Some(normalize_signature(sig, lang));
    }
}

/// Extract modifier keywords appearing at the start of a hover signature.
///
/// Scans leading whitespace-separated tokens that match the language's known
/// modifier vocabulary and stops at the first token that is not a modifier.
/// Order is preserved. Rust `pub(crate)` / `pub(super)` style visibility tokens
/// are emitted verbatim so [`visibility::infer`] can recognise them.
pub fn extract_modifiers(signature: &str, lang: Language) -> Vec<String> {
    let keywords = modifier_keywords(lang);
    let mut out = Vec::new();
    let mut rest = signature.trim_start();

    loop {
        // Rust `pub(...)` — consume as one token even though it has parens.
        if matches!(lang, Language::Rust) && rest.starts_with("pub(") {
            if let Some(close) = rest.find(')') {
                out.push(rest[..=close].to_owned());
                rest = rest[close + 1..].trim_start();
                continue;
            }
        }

        let end = rest
            .find(|c: char| c.is_whitespace() || c == '(' || c == '<' || c == ':')
            .unwrap_or(rest.len());
        if end == 0 {
            break;
        }
        let tok = &rest[..end];
        if keywords.contains(&tok) {
            out.push(tok.to_owned());
            rest = rest[end..].trim_start();
        } else {
            break;
        }
    }
    out
}

fn modifier_keywords(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &[
            "pub", "const", "async", "unsafe", "extern", "static", "mut", "default", "move",
        ],
        Language::TypeScript | Language::JavaScript | Language::JavaScriptReact => &[
            "export",
            "default",
            "async",
            "static",
            "readonly",
            "public",
            "private",
            "protected",
            "abstract",
            "declare",
            "override",
            "const",
            "let",
            "var",
        ],
        Language::Python => &["async", "def"],
        Language::Dart => &[
            "static",
            "abstract",
            "final",
            "const",
            "external",
            "factory",
            "late",
            "covariant",
            "async",
        ],
        Language::Go => &["func"],
        Language::Kotlin => &[
            "private",
            "protected",
            "internal",
            "public",
            "abstract",
            "final",
            "open",
            "override",
            "suspend",
            "inline",
            "external",
            "data",
            "sealed",
            "enum",
            "companion",
            "lateinit",
            "const",
            "operator",
            "infix",
            "tailrec",
        ],
        Language::Swift => &[
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
            "dynamic",
        ],
        Language::C | Language::Cpp => &[
            "static",
            "extern",
            "const",
            "virtual",
            "override",
            "explicit",
            "inline",
            "constexpr",
            "private",
            "protected",
            "public",
            "friend",
            "mutable",
            "volatile",
            "register",
            "typedef",
        ],
        Language::Unknown => &[],
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SymbolKind, Visibility};

    fn sym(name: &str) -> OwnedSymbolInfo {
        let mut s = OwnedSymbolInfo::new("lip://local/f#x", name);
        s.kind = SymbolKind::Function;
        s
    }

    #[test]
    fn rust_extracts_pub_async() {
        let mods = extract_modifiers("pub async fn foo(x: i32) -> Bar", Language::Rust);
        assert_eq!(mods, vec!["pub", "async"]);
    }

    #[test]
    fn rust_extracts_pub_crate_verbatim() {
        let mods = extract_modifiers("pub(crate) fn foo()", Language::Rust);
        assert_eq!(mods, vec!["pub(crate)"]);
    }

    #[test]
    fn rust_extracts_pub_in_path() {
        let mods = extract_modifiers("pub(in crate::x) fn foo()", Language::Rust);
        assert_eq!(mods, vec!["pub(in crate::x)"]);
    }

    #[test]
    fn rust_no_modifiers() {
        assert!(extract_modifiers("fn foo()", Language::Rust).is_empty());
    }

    #[test]
    fn ts_export_async() {
        let mods = extract_modifiers(
            "export async function foo(): Promise<T>",
            Language::TypeScript,
        );
        assert_eq!(mods, vec!["export", "async"]);
    }

    #[test]
    fn kotlin_private_suspend() {
        let mods = extract_modifiers("private suspend fun foo(): Int", Language::Kotlin);
        assert_eq!(mods, vec!["private", "suspend"]);
    }

    #[test]
    fn swift_public_final() {
        let mods = extract_modifiers("public final func foo() -> Int", Language::Swift);
        assert_eq!(mods, vec!["public", "final"]);
    }

    #[test]
    fn cpp_static_inline() {
        let mods = extract_modifiers("static inline int foo()", Language::Cpp);
        assert_eq!(mods, vec!["static", "inline"]);
    }

    #[test]
    fn enrich_sets_tier_and_visibility() {
        let mut s = sym("foo");
        enrich_v23(&mut s, Some("pub fn foo() -> i32"), None, Language::Rust);
        assert_eq!(s.extraction_tier, ExtractionTier::Tier2);
        assert_eq!(s.visibility, Some(Visibility::Public));
        assert_eq!(s.visibility_confidence, Some(1.0));
        assert_eq!(s.modifiers, vec!["pub".to_owned()]);
        assert_eq!(
            s.signature_normalized.as_deref(),
            Some("pub fn foo() -> i32")
        );
    }

    #[test]
    fn enrich_without_signature_still_runs_inference() {
        // No hover → no modifiers, but visibility still inferred from name (Go rule).
        let mut s = sym("Foo");
        enrich_v23(&mut s, None, None, Language::Go);
        assert_eq!(s.extraction_tier, ExtractionTier::Tier2);
        assert_eq!(s.visibility, Some(Visibility::Public));
        assert_eq!(s.signature_normalized, None);
        assert!(s.modifiers.is_empty());
    }

    #[test]
    fn enrich_records_container() {
        let mut s = sym("method");
        enrich_v23(
            &mut s,
            Some("public int run()"),
            Some("Svc".to_owned()),
            Language::Cpp,
        );
        assert_eq!(s.container_name.as_deref(), Some("Svc"));
    }

    #[test]
    fn enrich_skips_empty_container() {
        let mut s = sym("foo");
        enrich_v23(
            &mut s,
            Some("fn foo()"),
            Some(String::new()),
            Language::Rust,
        );
        assert!(s.container_name.is_none());
    }

    #[test]
    fn enrich_python_name_convention() {
        let mut s = sym("_helper");
        enrich_v23(
            &mut s,
            Some("def _helper() -> None"),
            None,
            Language::Python,
        );
        assert_eq!(s.visibility, Some(Visibility::Private));
    }

    #[test]
    fn enrich_ts_no_modifier_is_low_conf_internal() {
        let mut s = sym("foo");
        enrich_v23(
            &mut s,
            Some("function foo(): void"),
            None,
            Language::TypeScript,
        );
        assert_eq!(s.visibility, Some(Visibility::Internal));
        assert_eq!(s.visibility_confidence, Some(0.5));
    }
}
