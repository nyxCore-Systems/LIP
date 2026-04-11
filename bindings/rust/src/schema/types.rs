use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Compute SHA-256 of bytes and return lowercase hex string.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ─── Symbol URI ──────────────────────────────────────────────────────────────

/// Validated, opaque LIP symbol URI.
/// Grammar: `lip://scope/package@version/path#descriptor`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LipUri(String);

impl LipUri {
    /// Parse and validate a LIP URI string.
    ///
    /// Returns `Err` if the string does not start with `lip://`, contains a
    /// null byte, or contains the path-traversal sequence `..`.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Self::validate(s)?;
        Ok(LipUri(s.to_owned()))
    }

    /// Wrap a string as a `LipUri` without validation.
    ///
    /// Use only when the value is already known to be valid (e.g. round-tripped
    /// from a trusted source). Prefer [`LipUri::parse`] at system boundaries.
    pub fn new_unchecked(s: impl Into<String>) -> Self {
        LipUri(s.into())
    }

    /// Return the URI as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(s: &str) -> anyhow::Result<()> {
        if !s.starts_with("lip://") {
            anyhow::bail!("LIP URI must start with 'lip://': {s}");
        }
        if s.contains('\0') {
            anyhow::bail!("LIP URI contains null byte");
        }
        if s.contains("..") {
            anyhow::bail!("LIP URI contains path traversal sequence '..'");
        }
        Ok(())
    }

    /// The scope component, e.g. `"npm"` in `lip://npm/react@18.0.0/…`.
    pub fn scope(&self) -> Option<&str> {
        self.0.strip_prefix("lip://")?.split('/').next()
    }

    /// The package name without version, e.g. `"react"`.
    pub fn package(&self) -> Option<&str> {
        let rest = self.0.strip_prefix("lip://")?;
        rest.split('/').nth(1)?.split('@').next()
    }

    /// The semver string after `@`, e.g. `"18.0.0"`.
    pub fn version(&self) -> Option<&str> {
        let rest = self.0.strip_prefix("lip://")?;
        rest.split('/').nth(1)?.split_once('@').map(|x| x.1)
    }

    /// The file path component before `#`, e.g. `"src/index.js"`.
    pub fn path(&self) -> Option<&str> {
        let rest = self.0.strip_prefix("lip://")?;
        let third = rest.splitn(3, '/').nth(2)?;
        Some(third.split('#').next().unwrap_or(third))
    }

    /// The descriptor after `#`, e.g. `"createElement"`. `None` if absent.
    pub fn descriptor(&self) -> Option<&str> {
        self.0.split_once('#').map(|x| x.1)
    }
}

impl std::fmt::Display for LipUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ─── Enumerations ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Upsert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Definition,
    Reference,
    Implementation,
    TypeBinding,
    ReadAccess,
    WriteAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    #[default]
    Unknown,
    Namespace,
    Class,
    Interface,
    Method,
    Field,
    Variable,
    Function,
    TypeParameter,
    Parameter,
    Macro,
    Enum,
    EnumMember,
    Constructor,
    TypeAlias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IndexingState {
    #[default]
    Cold,
    WarmPartial,
    WarmFull,
}

// ─── Owned heap types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct OwnedRange {
    pub start_line: i32,
    pub start_char: i32,
    pub end_line: i32,
    pub end_char: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedRelationship {
    pub target_uri: String,
    pub is_implementation: bool,
    pub is_reference: bool,
    pub is_type_definition: bool,
    pub is_override: bool,
}

/// Heap-allocated SymbolInfo.
///
/// `runtime_p99_ms` and `call_rate_per_s` are advisory telemetry fields
/// (spec §8.3). They are excluded from `PartialEq`/`Eq`/`Hash` so that
/// salsa's early-cutoff can fire purely on the structural intelligence fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedSymbolInfo {
    pub uri: String,
    pub display_name: String,
    pub kind: SymbolKind,
    pub documentation: Option<String>,
    pub signature: Option<String>,
    pub confidence_score: u8,
    pub relationships: Vec<OwnedRelationship>,
    // Telemetry — excluded from Eq; see below.
    pub runtime_p99_ms: Option<f32>,
    pub call_rate_per_s: Option<f32>,
    pub taint_labels: Vec<String>,
    pub blast_radius: u32,
}

impl PartialEq for OwnedSymbolInfo {
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri
            && self.display_name == other.display_name
            && self.kind == other.kind
            && self.documentation == other.documentation
            && self.signature == other.signature
            && self.confidence_score == other.confidence_score
            && self.relationships == other.relationships
            && self.taint_labels == other.taint_labels
            && self.blast_radius == other.blast_radius
        // runtime_p99_ms / call_rate_per_s intentionally omitted
    }
}
impl Eq for OwnedSymbolInfo {}

impl std::hash::Hash for OwnedSymbolInfo {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.uri.hash(state);
        self.confidence_score.hash(state);
        self.kind.hash(state);
    }
}

impl OwnedSymbolInfo {
    pub fn new(uri: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            display_name: display_name.into(),
            kind: SymbolKind::Unknown,
            documentation: None,
            signature: None,
            confidence_score: 30,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedOccurrence {
    pub symbol_uri: String,
    pub range: OwnedRange,
    pub confidence_score: u8,
    pub role: Role,
    pub override_doc: Option<String>,
}

/// Edge kind in the Code Property Graph (spec §4.1, §8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    #[default]
    Calls,
    DataFlows,
    ControlFlows,
    Instantiates,
    Inherits,
    Imports,
}

/// A typed directed edge in the Code Property Graph.
///
/// Absent in Tier 1 documents (tree-sitter cannot derive data-flow edges).
/// Populated by Tier 2 compiler verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedGraphEdge {
    pub from_uri: String,
    pub to_uri: String,
    pub kind: EdgeKind,
    /// Source range where the edge originates (call site, assignment, etc.).
    pub at_range: OwnedRange,
}

/// A persistent annotation attached to a symbol URI (spec §9.4).
///
/// Written by humans or AI agents. Survives context resets, editor restarts,
/// and CI runs. The key namespace determines how the annotation is interpreted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedAnnotationEntry {
    pub symbol_uri: String,
    /// Namespaced key, e.g. `"lip:fragile"`, `"agent:note"`, `"team:owner"`.
    pub key: String,
    /// Markdown string or JSON blob.
    pub value: String,
    /// `"human:<email>"` or `"agent:<model-id>"`.
    pub author_id: String,
    pub confidence: u8,
    pub timestamp_ms: i64,
    /// Unix ms timestamp after which this entry may be garbage-collected.
    /// `0` means permanent.
    pub expires_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedDocument {
    pub uri: String,
    pub content_hash: String,
    pub language: String,
    pub occurrences: Vec<OwnedOccurrence>,
    pub symbols: Vec<OwnedSymbolInfo>,
    pub merkle_path: String,
    /// CPG edges originating from this file.
    /// Empty in Tier 1 documents; populated by Tier 2 verification.
    pub edges: Vec<OwnedGraphEdge>,
    /// Raw UTF-8 source bytes.
    /// Present when the client sends a file update so the daemon can drive
    /// the Tier 1 indexer. `None` in registry slices and SCIP imports where
    /// symbols are already pre-computed.
    pub source_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedDependencySlice {
    pub manager: String,
    pub package_name: String,
    pub version: String,
    pub package_hash: String,
    pub content_hash: String,
    pub symbols: Vec<OwnedSymbolInfo>,
    pub slice_url: String,
    pub built_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedDelta {
    pub action: Action,
    pub commit_hash: String,
    pub document: Option<OwnedDocument>,
    pub symbol: Option<OwnedSymbolInfo>,
    pub slice: Option<OwnedDependencySlice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedEventStream {
    pub deltas: Vec<OwnedDelta>,
    pub schema_version: u16,
    pub emitter_id: String,
    pub timestamp_ms: i64,
}

impl OwnedEventStream {
    pub fn new(emitter_id: impl Into<String>, deltas: Vec<OwnedDelta>) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            deltas,
            schema_version: 1,
            emitter_id: emitter_id.into(),
            timestamp_ms,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── LipUri validation ─────────────────────────────────────────────────

    #[test]
    fn uri_valid_full() {
        let uri = LipUri::parse("lip://npm/react@18.2.0/src/index.js#createElement").unwrap();
        assert_eq!(uri.scope(), Some("npm"));
        assert_eq!(uri.package(), Some("react"));
        assert_eq!(uri.version(), Some("18.2.0"));
        assert_eq!(uri.path(), Some("src/index.js"));
        assert_eq!(uri.descriptor(), Some("createElement"));
    }

    #[test]
    fn uri_valid_no_descriptor() {
        let uri = LipUri::parse("lip://cargo/serde@1.0.0/src/lib.rs").unwrap();
        assert_eq!(uri.descriptor(), None);
        assert_eq!(uri.path(), Some("src/lib.rs"));
    }

    #[test]
    fn uri_rejects_wrong_scheme() {
        assert!(LipUri::parse("https://npm/react@18.0.0/index.js").is_err());
        assert!(LipUri::parse("lsp://npm/react@18.0.0/index.js").is_err());
        assert!(LipUri::parse("/absolute/path").is_err());
    }

    #[test]
    fn uri_rejects_null_byte() {
        assert!(LipUri::parse("lip://npm/pkg@1.0.0/\0evil").is_err());
    }

    #[test]
    fn uri_rejects_path_traversal() {
        assert!(LipUri::parse("lip://npm/pkg@1.0.0/../etc/passwd").is_err());
    }

    #[test]
    fn uri_roundtrip_display() {
        let s = "lip://cargo/tokio@1.0.0/src/runtime.rs#spawn";
        assert_eq!(LipUri::parse(s).unwrap().as_str(), s);
        assert_eq!(LipUri::parse(s).unwrap().to_string(), s);
    }

    #[test]
    fn uri_equality_and_hash() {
        use std::collections::HashSet;
        let a = LipUri::parse("lip://npm/pkg@1.0.0/a.js#foo").unwrap();
        let b = LipUri::parse("lip://npm/pkg@1.0.0/a.js#foo").unwrap();
        let c = LipUri::parse("lip://npm/pkg@1.0.0/a.js#bar").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        let set: HashSet<_> = [a, b, c].into_iter().collect();
        assert_eq!(set.len(), 2);
    }

    // ── OwnedSymbolInfo Eq excludes telemetry ──────────────────────────────

    #[test]
    fn symbol_eq_ignores_telemetry_fields() {
        let mut a = OwnedSymbolInfo::new("lip://s/p@1/f.rs#foo", "foo");
        let mut b = OwnedSymbolInfo::new("lip://s/p@1/f.rs#foo", "foo");
        a.runtime_p99_ms = Some(1.0);
        b.runtime_p99_ms = Some(999.0);
        a.call_rate_per_s = Some(0.1);
        b.call_rate_per_s = Some(50000.0);
        assert_eq!(a, b);
    }

    #[test]
    fn symbol_eq_detects_structural_difference() {
        let a = OwnedSymbolInfo::new("lip://s/p@1/f.rs#foo", "foo");
        let mut b = OwnedSymbolInfo::new("lip://s/p@1/f.rs#foo", "foo");
        b.kind = SymbolKind::Function;
        assert_ne!(a, b);
    }

    // ── sha256_hex ────────────────────────────────────────────────────────

    #[test]
    fn sha256_hex_known_value() {
        // echo -n "" | sha256sum → e3b0c44…
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex(b"hello"), sha256_hex(b"hello"));
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }
}
