//! Visibility inference from modifier keywords and name conventions.
//!
//! `infer(name, modifiers, lang)` returns `(Visibility, confidence)` where
//! confidence is a 0-100 score:
//! - `100` — explicit modifier keyword determined visibility (e.g. `pub`,
//!   `private`, `export`).
//! - `80`  — naming convention determined visibility (Go capital-letter
//!   export, Python/Dart `_` prefix, language default).
//! - `50`  — fallback when neither modifiers nor conventions are decisive.
//!
//! This helper is the single source of truth so Tier 1, Tier 1.5 and SCIP
//! import paths produce consistent visibility values.

use crate::indexer::language::Language;
use crate::schema::Visibility;

/// Infer a symbol's visibility and a 0-100 confidence score.
pub fn infer(name: &str, modifiers: &[String], lang: Language) -> (Visibility, u8) {
    match lang {
        Language::Rust => infer_rust(modifiers),
        Language::TypeScript | Language::JavaScript | Language::JavaScriptReact => {
            infer_ts_js(modifiers)
        }
        Language::Python => infer_python(name),
        Language::Dart => infer_dart(name),
        Language::Go => infer_go(name),
        Language::Kotlin => infer_kotlin(modifiers),
        Language::Swift => infer_swift(modifiers),
        Language::C | Language::Cpp => infer_cpp(modifiers),
        Language::Unknown => (Visibility::Public, 50),
    }
}

fn has_mod(modifiers: &[String], keyword: &str) -> bool {
    modifiers.iter().any(|m| m == keyword)
}

fn infer_rust(modifiers: &[String]) -> (Visibility, u8) {
    // `pub` → Public; `pub(crate)`, `pub(super)`, `pub(in …)` → Internal.
    if has_mod(modifiers, "pub") {
        return (Visibility::Public, 100);
    }
    if modifiers.iter().any(|m| m.starts_with("pub(")) {
        return (Visibility::Internal, 100);
    }
    (Visibility::Private, 50)
}

fn infer_ts_js(modifiers: &[String]) -> (Visibility, u8) {
    if has_mod(modifiers, "private") {
        return (Visibility::Private, 100);
    }
    if has_mod(modifiers, "protected") {
        return (Visibility::Protected, 100);
    }
    if has_mod(modifiers, "export") || has_mod(modifiers, "public") {
        return (Visibility::Public, 100);
    }
    // No explicit keyword: a top-level symbol without `export` is module-private
    // but a class member without a modifier is public. Without more context we
    // return `Internal` at low confidence.
    (Visibility::Internal, 50)
}

fn infer_python(name: &str) -> (Visibility, u8) {
    // PEP 8: dunder (`__x__`) is public by convention; `__x` is
    // name-mangled (private to class); `_x` is module-private.
    if name.starts_with("__") && name.ends_with("__") && name.len() > 4 {
        return (Visibility::Public, 80);
    }
    if name.starts_with('_') {
        return (Visibility::Private, 80);
    }
    (Visibility::Public, 80)
}

fn infer_dart(name: &str) -> (Visibility, u8) {
    // Dart: `_`-prefixed identifiers are library-private.
    if name.starts_with('_') {
        return (Visibility::Private, 80);
    }
    (Visibility::Public, 80)
}

fn infer_go(name: &str) -> (Visibility, u8) {
    // Go export rule: first rune uppercase → exported.
    match name.chars().next() {
        Some(c) if c.is_ascii_uppercase() => (Visibility::Public, 80),
        Some(_) => (Visibility::Private, 80),
        None => (Visibility::Public, 50),
    }
}

fn infer_kotlin(modifiers: &[String]) -> (Visibility, u8) {
    if has_mod(modifiers, "private") {
        return (Visibility::Private, 100);
    }
    if has_mod(modifiers, "protected") {
        return (Visibility::Protected, 100);
    }
    if has_mod(modifiers, "internal") {
        return (Visibility::Internal, 100);
    }
    if has_mod(modifiers, "public") {
        return (Visibility::Public, 100);
    }
    (Visibility::Public, 80) // Kotlin default is public
}

fn infer_swift(modifiers: &[String]) -> (Visibility, u8) {
    if has_mod(modifiers, "private") || has_mod(modifiers, "fileprivate") {
        return (Visibility::Private, 100);
    }
    if has_mod(modifiers, "public") || has_mod(modifiers, "open") {
        return (Visibility::Public, 100);
    }
    if has_mod(modifiers, "internal") {
        return (Visibility::Internal, 100);
    }
    (Visibility::Internal, 80) // Swift default is internal
}

fn infer_cpp(modifiers: &[String]) -> (Visibility, u8) {
    if has_mod(modifiers, "private") {
        return (Visibility::Private, 100);
    }
    if has_mod(modifiers, "protected") {
        return (Visibility::Protected, 100);
    }
    if has_mod(modifiers, "public") {
        return (Visibility::Public, 100);
    }
    // C / free C++ functions are public; class-body defaults (private for
    // `class`, public for `struct`) need call-site context we don't have here.
    (Visibility::Public, 50)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ms: &[&str]) -> Vec<String> {
        ms.iter().map(|s| (*s).to_owned()).collect()
    }

    // Rust
    #[test]
    fn rust_pub_is_public() {
        assert_eq!(
            infer("foo", &mods(&["pub"]), Language::Rust),
            (Visibility::Public, 100)
        );
    }

    #[test]
    fn rust_pub_crate_is_internal() {
        assert_eq!(
            infer("foo", &mods(&["pub(crate)"]), Language::Rust),
            (Visibility::Internal, 100)
        );
    }

    #[test]
    fn rust_pub_super_is_internal() {
        assert_eq!(
            infer("foo", &mods(&["pub(super)"]), Language::Rust),
            (Visibility::Internal, 100)
        );
    }

    #[test]
    fn rust_no_modifier_is_private() {
        assert_eq!(infer("foo", &[], Language::Rust), (Visibility::Private, 50));
    }

    // TypeScript / JavaScript
    #[test]
    fn ts_export_is_public() {
        assert_eq!(
            infer("foo", &mods(&["export"]), Language::TypeScript),
            (Visibility::Public, 100)
        );
    }

    #[test]
    fn ts_private_keyword_is_private() {
        assert_eq!(
            infer("foo", &mods(&["private"]), Language::TypeScript),
            (Visibility::Private, 100)
        );
    }

    #[test]
    fn ts_protected_is_protected() {
        assert_eq!(
            infer("foo", &mods(&["protected"]), Language::TypeScript),
            (Visibility::Protected, 100)
        );
    }

    #[test]
    fn ts_no_modifier_is_internal_lowconf() {
        assert_eq!(
            infer("foo", &[], Language::TypeScript),
            (Visibility::Internal, 50)
        );
    }

    // Python
    #[test]
    fn python_underscore_is_private() {
        assert_eq!(
            infer("_helper", &[], Language::Python),
            (Visibility::Private, 80)
        );
    }

    #[test]
    fn python_double_underscore_is_private() {
        assert_eq!(
            infer("__mangled", &[], Language::Python),
            (Visibility::Private, 80)
        );
    }

    #[test]
    fn python_dunder_is_public() {
        assert_eq!(
            infer("__init__", &[], Language::Python),
            (Visibility::Public, 80)
        );
    }

    #[test]
    fn python_plain_name_is_public() {
        assert_eq!(
            infer("foo", &[], Language::Python),
            (Visibility::Public, 80)
        );
    }

    // Dart
    #[test]
    fn dart_underscore_is_private() {
        assert_eq!(
            infer("_internal", &[], Language::Dart),
            (Visibility::Private, 80)
        );
    }

    #[test]
    fn dart_plain_is_public() {
        assert_eq!(
            infer("public", &[], Language::Dart),
            (Visibility::Public, 80)
        );
    }

    // Go
    #[test]
    fn go_capital_is_public() {
        assert_eq!(infer("Foo", &[], Language::Go), (Visibility::Public, 80));
    }

    #[test]
    fn go_lowercase_is_private() {
        assert_eq!(infer("foo", &[], Language::Go), (Visibility::Private, 80));
    }

    // Kotlin
    #[test]
    fn kotlin_private_keyword() {
        assert_eq!(
            infer("foo", &mods(&["private"]), Language::Kotlin),
            (Visibility::Private, 100)
        );
    }

    #[test]
    fn kotlin_internal_keyword() {
        assert_eq!(
            infer("foo", &mods(&["internal"]), Language::Kotlin),
            (Visibility::Internal, 100)
        );
    }

    #[test]
    fn kotlin_default_is_public() {
        assert_eq!(
            infer("foo", &[], Language::Kotlin),
            (Visibility::Public, 80)
        );
    }

    // Swift
    #[test]
    fn swift_fileprivate_is_private() {
        assert_eq!(
            infer("foo", &mods(&["fileprivate"]), Language::Swift),
            (Visibility::Private, 100)
        );
    }

    #[test]
    fn swift_open_is_public() {
        assert_eq!(
            infer("foo", &mods(&["open"]), Language::Swift),
            (Visibility::Public, 100)
        );
    }

    #[test]
    fn swift_default_is_internal() {
        assert_eq!(
            infer("foo", &[], Language::Swift),
            (Visibility::Internal, 80)
        );
    }

    // C / C++
    #[test]
    fn cpp_explicit_private() {
        assert_eq!(
            infer("foo", &mods(&["private"]), Language::Cpp),
            (Visibility::Private, 100)
        );
    }

    #[test]
    fn c_no_modifier_is_public_lowconf() {
        assert_eq!(infer("foo", &[], Language::C), (Visibility::Public, 50));
    }

    // Unknown
    #[test]
    fn unknown_language_fallback() {
        assert_eq!(
            infer("foo", &[], Language::Unknown),
            (Visibility::Public, 50)
        );
    }
}
