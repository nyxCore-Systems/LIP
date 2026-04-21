//! Signature normalization for ABI-hash-stable comparisons.
//!
//! `normalize_signature` produces a canonical form of a symbol's signature
//! so that churn in parameter names or whitespace does not flip the hash.
//! The output is idempotent: `normalize(normalize(s)) == normalize(s)`.
//!
//! Strategy:
//! - Trim and collapse internal whitespace runs to a single space.
//! - Drop trailing documentation tails (`// …`, `# …` line comments).
//! - For languages that use `name: Type` params (Rust, TypeScript, Python,
//!   Kotlin, Swift), replace the parameter name with `_` at paren depth 1.
//! - Languages that use different orderings (Go: `name Type`, C/C++/Dart:
//!   `Type name`) get whitespace-only normalization for now.

use crate::indexer::language::Language;

/// Produce a canonical, param-name-agnostic form of `raw`.
pub fn normalize_signature(raw: &str, lang: Language) -> String {
    let trimmed = strip_doc_tail(raw);
    let collapsed = collapse_whitespace(trimmed);
    if uses_colon_params(lang) {
        strip_colon_param_names(&collapsed)
    } else {
        collapsed
    }
}

fn uses_colon_params(lang: Language) -> bool {
    matches!(
        lang,
        Language::Rust
            | Language::TypeScript
            | Language::Python
            | Language::Kotlin
            | Language::Swift
    )
}

/// Drop the first `//`-line comment or `#`-comment tail, if present.
///
/// We only look at top-level (non-string) context for the line comment.
/// A simple heuristic: split at the first `\n`, then look for the comment
/// marker on that first line after any closing brace of the signature.
fn strip_doc_tail(s: &str) -> &str {
    // Take only the first line — signatures are single-line after LSP hover
    // code-block extraction, so any trailing documentation will be on a
    // separate line anyway.
    let first_line = s.split_once('\n').map(|(a, _)| a).unwrap_or(s);

    // Remove inline `//` comment if present.
    let without_slash = match first_line.find("//") {
        Some(idx) => &first_line[..idx],
        None => first_line,
    };

    // Remove inline `#` comment (Python) — conservative: only when
    // preceded by whitespace, so `#[derive(...)]` attributes are preserved.
    if let Some(idx) = find_hash_comment(without_slash) {
        &without_slash[..idx]
    } else {
        without_slash
    }
}

fn find_hash_comment(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            return Some(i);
        }
    }
    None
}

/// Trim and collapse runs of ASCII whitespace to a single space.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_ascii_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(c);
        }
    }
    out
}

/// Replace `ident:` with `_:` for parameters at paren depth 1
/// (and angle depth 0). Handles optional `?` markers (TS) and
/// `*` / `**` prefixes (Python varargs).
fn strip_colon_param_names(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut paren: i32 = 0;
    let mut angle: i32 = 0;
    let mut expect_param = false;

    while i < bytes.len() {
        let c = bytes[i];

        if expect_param {
            // Skip leading whitespace inside the param slot.
            if c == b' ' {
                out.push(' ');
                i += 1;
                continue;
            }
            if let Some((prefix_end, ident_end, colon_pos)) = try_match_named_param(bytes, i) {
                // Emit any `*` / `**` prefix verbatim, then `_`, then any
                // `?` / whitespace between the identifier and the colon.
                out.push_str(slice_str(bytes, i, prefix_end));
                out.push('_');
                out.push_str(slice_str(bytes, ident_end, colon_pos));
                i = colon_pos;
                expect_param = false;
                continue;
            }
            // Not a `name: type` pattern — emit chars normally and clear flag.
            expect_param = false;
        }

        match c {
            b'(' => {
                paren += 1;
                if paren == 1 && angle == 0 {
                    expect_param = true;
                }
            }
            b')' => {
                paren -= 1;
            }
            b'<' => {
                angle += 1;
            }
            b'>' => {
                angle -= 1;
            }
            b',' if paren == 1 && angle == 0 => {
                expect_param = true;
            }
            _ => {}
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Try to match `[*]{1,2} ident (\s* \?)? \s* :` starting at `start`.
/// Returns `(prefix_end, ident_end, colon_pos)` on success, else `None`.
fn try_match_named_param(bytes: &[u8], start: usize) -> Option<(usize, usize, usize)> {
    let mut j = start;
    // Optional `*` / `**` prefix (Python varargs).
    while j < bytes.len() && bytes[j] == b'*' && (j - start) < 2 {
        j += 1;
    }
    let prefix_end = j;
    // Identifier: first char must be [A-Za-z_], then [A-Za-z0-9_].
    if j >= bytes.len() || !is_ident_start(bytes[j]) {
        return None;
    }
    j += 1;
    while j < bytes.len() && is_ident_continue(bytes[j]) {
        j += 1;
    }
    let ident_end = j;
    // Optional `?` (TS optional param marker).
    if j < bytes.len() && bytes[j] == b'?' {
        j += 1;
    }
    // Optional whitespace.
    while j < bytes.len() && bytes[j] == b' ' {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b':' {
        Some((prefix_end, ident_end, j))
    } else {
        None
    }
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn slice_str(bytes: &[u8], a: usize, b: usize) -> &str {
    // Safe: callers only pass indices at ASCII boundaries.
    std::str::from_utf8(&bytes[a..b]).unwrap_or("")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_strips_param_names() {
        let got = normalize_signature("fn foo(x: i32, y: &str) -> bool", Language::Rust);
        assert_eq!(got, "fn foo(_: i32, _: &str) -> bool");
    }

    #[test]
    fn rust_preserves_self_variants() {
        // &self / &mut self have no colon, so nothing to replace.
        let got = normalize_signature("fn bar(&self, x: i32)", Language::Rust);
        assert_eq!(got, "fn bar(&self, _: i32)");
    }

    #[test]
    fn rust_generic_params_untouched() {
        // Angle-bracket generics must not be treated as params.
        let got = normalize_signature("fn baz<T: Clone>(x: Vec<T>) -> T", Language::Rust);
        assert_eq!(got, "fn baz<T: Clone>(_: Vec<T>) -> T");
    }

    #[test]
    fn typescript_optional_marker_preserved() {
        let got = normalize_signature("foo(x?: number, y: string): boolean", Language::TypeScript);
        assert_eq!(got, "foo(_?: number, _: string): boolean");
    }

    #[test]
    fn python_defaults_and_varargs() {
        let got = normalize_signature("def f(*args: int, **kwargs: str) -> None", Language::Python);
        assert_eq!(got, "def f(*_: int, **_: str) -> None");
    }

    #[test]
    fn kotlin_strips_names() {
        let got = normalize_signature("fun foo(x: Int, y: String): Boolean", Language::Kotlin);
        assert_eq!(got, "fun foo(_: Int, _: String): Boolean");
    }

    #[test]
    fn swift_strips_names() {
        let got = normalize_signature("func foo(x: Int, y: String) -> Bool", Language::Swift);
        assert_eq!(got, "func foo(_: Int, _: String) -> Bool");
    }

    #[test]
    fn go_whitespace_only() {
        // Go uses `name Type`; we don't (yet) strip names, just collapse WS.
        let got = normalize_signature("func  foo(x int,  y string) bool", Language::Go);
        assert_eq!(got, "func foo(x int, y string) bool");
    }

    #[test]
    fn c_whitespace_only() {
        let got = normalize_signature("int foo(const char *s,   int n)", Language::C);
        assert_eq!(got, "int foo(const char *s, int n)");
    }

    #[test]
    fn dart_whitespace_only() {
        let got = normalize_signature("bool foo(int x,  String y)", Language::Dart);
        assert_eq!(got, "bool foo(int x, String y)");
    }

    #[test]
    fn whitespace_collapse() {
        let got = normalize_signature("fn   foo(x:   i32)  ->   bool", Language::Rust);
        assert_eq!(got, "fn foo(_: i32) -> bool");
    }

    #[test]
    fn doc_tail_slash_comment_dropped() {
        let got = normalize_signature("fn foo(x: i32) -> bool // frobnicate", Language::Rust);
        assert_eq!(got, "fn foo(_: i32) -> bool");
    }

    #[test]
    fn doc_tail_hash_comment_dropped_python() {
        let got = normalize_signature("def f(x: int) -> None  # legacy", Language::Python);
        assert_eq!(got, "def f(_: int) -> None");
    }

    #[test]
    fn rust_attribute_preserved() {
        // `#[derive(...)]` uses `#` but no preceding whitespace, so it survives.
        let got = normalize_signature("#[inline] fn foo(x: i32)", Language::Rust);
        assert_eq!(got, "#[inline] fn foo(_: i32)");
    }

    #[test]
    fn idempotent_rust() {
        let once = normalize_signature("fn foo(xs: Vec<i32>, y: &str) -> T", Language::Rust);
        let twice = normalize_signature(&once, Language::Rust);
        assert_eq!(once, twice);
    }

    #[test]
    fn idempotent_typescript() {
        let once = normalize_signature("foo(x?: number): Promise<T>", Language::TypeScript);
        let twice = normalize_signature(&once, Language::TypeScript);
        assert_eq!(once, twice);
    }

    #[test]
    fn idempotent_go() {
        let once = normalize_signature("func foo(x int, y string) bool", Language::Go);
        let twice = normalize_signature(&once, Language::Go);
        assert_eq!(once, twice);
    }

    #[test]
    fn param_name_change_normalizes_equal() {
        let a = normalize_signature("fn foo(x: i32, y: i32) -> i32", Language::Rust);
        let b = normalize_signature("fn foo(a: i32, b: i32) -> i32", Language::Rust);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_and_no_params() {
        assert_eq!(normalize_signature("", Language::Rust), "");
        assert_eq!(
            normalize_signature("fn foo() -> bool", Language::Rust),
            "fn foo() -> bool"
        );
    }

    #[test]
    fn nested_parens_in_fn_type_arg() {
        // Callback params inside generic arg: outer paren strip only
        // at depth 1; inner should still get name stripped.
        let got = normalize_signature(
            "fn foo(cb: fn(x: i32) -> bool) -> ()",
            Language::Rust,
        );
        // Inner `x:` is at paren depth 2, so NOT stripped by the current
        // depth-1-only rule. Record this limitation as the expected output.
        assert_eq!(got, "fn foo(_: fn(x: i32) -> bool) -> ()");
    }
}
