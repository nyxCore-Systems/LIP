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

/// Fine-grained classification of a reference occurrence (v2.3).
///
/// Populated when `role != Definition`. `Role` marks whether an occurrence is
/// a definition/reference/read/write; `ReferenceKind` adds the *reason* the
/// reference exists — call site vs. type position vs. inheritance clause. CKB
/// uses it to distinguish "X is called from Y" from "X is the return type of
/// Y" without re-parsing.
///
/// Tier-1 can classify Call/Read/Write from the tree-sitter parent node;
/// Type/Implements/Extends require Tier-2 type information to distinguish
/// reliably. Leave as `Unknown` when the extractor cannot decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    #[default]
    Unknown,
    Call,
    Read,
    Write,
    Type,
    Implements,
    Extends,
}

impl ReferenceKind {
    pub fn is_unknown(&self) -> bool {
        matches!(self, ReferenceKind::Unknown)
    }
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

/// Symbol visibility (spec §v2.3 Feature #1).
///
/// LIP owns inference — see `schema::visibility::infer`. Derived from language
/// rules + modifiers at ingest time and carried on `OwnedSymbolInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Private,
    Internal,
    Protected,
}

/// Which extraction tier produced a symbol record (spec §v2.3 Feature #1).
///
/// Telemetry: NOT included in `OwnedSymbolInfo::PartialEq` so that a Tier-1 →
/// Tier-2 upgrade with no structural change does not invalidate the salsa
/// early-cutoff. Clients tier-gate confidence at query time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTier {
    #[default]
    Tier1,
    Tier1p5,
    Tier2,
    Tier3Scip,
}

/// Provenance of the `modifiers` field on SCIP-imported symbols.
///
/// `Proto` = the vendored SCIP `SymbolInformation.modifiers` field was present.
/// `PrefixParse` = fell back to parsing the signature prefix (older `.scip`
/// blobs predate upstream field 7). CKB discounts confidence on `PrefixParse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifiersSource {
    Proto,
    PrefixParse,
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

/// How fully the indexer resolved a symbol (spec §v2.3 Feature #1).
///
/// `score` in [0.0, 1.0]. `reason` is a short stable tag (e.g. `"tier1_syntactic"`,
/// `"lsp_verified"`, `"scip_precomputed"`, `"scip_unresolved_local"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completeness {
    pub score: f32,
    pub reason: String,
}

/// Heap-allocated SymbolInfo.
///
/// `runtime_p99_ms` and `call_rate_per_s` are advisory telemetry fields
/// (spec §8.3). They are excluded from `PartialEq`/`Eq`/`Hash` so that
/// salsa's early-cutoff can fire purely on the structural intelligence fields.
///
/// v2.3 adds rich metadata (spec §v2.3 Feature #1). Eq split per decision C.5:
/// `modifiers`, `visibility`, `container_name`, `signature_normalized` are
/// structural (a flip is an ABI change) and participate in Eq. `completeness`,
/// `visibility_confidence`, `extraction_tier`, `modifiers_source` are telemetry
/// and are excluded — a Tier-1 → Tier-2 upgrade with no structural change must
/// not invalidate the salsa early-cutoff.
///
/// `Default` is derived so construction sites can use `..Default::default()`
/// for telemetry/v2.3 fields. Note: the derived default has `confidence_score
/// = 0`; prefer `OwnedSymbolInfo::new` when you want the `30` baseline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// True when the symbol is part of the public/exported API surface.
    /// Set by the Tier 1 extractor using language-specific visibility rules;
    /// used by `file_api_surface()` for stable ABI hash computation.
    pub is_exported: bool,

    // ── v2.3 rich metadata — structural (in Eq) ──────────────────────────────
    /// Whitespace- and param-name-stripped signature, for API-compat comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_normalized: Option<String>,
    /// Raw modifier list from SCIP or tree-sitter (`public`, `async`, `static`,
    /// `deprecated`, `export`, `test`, …). Empty = extractor ran and saw none.
    /// Use `extraction_tier` to tell "none" from "not yet extracted".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    /// Canonical visibility, inferred by `schema::visibility::infer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    /// Enclosing class / namespace / module name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,

    // ── v2.3 rich metadata — telemetry (NOT in Eq) ───────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completeness: Option<Completeness>,
    /// Which tier produced this record. Defaults to `Tier1` on deserialize of
    /// v2.2 payloads.
    #[serde(default)]
    pub extraction_tier: ExtractionTier,
    /// Only set on SCIP-imported symbols; `None` on Tier-1/Tier-2 native paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifiers_source: Option<ModifiersSource>,
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
            && self.is_exported == other.is_exported
            // v2.3 structural fields — ABI-bearing, participate in early-cutoff
            && self.signature_normalized == other.signature_normalized
            && self.modifiers == other.modifiers
            && self.visibility == other.visibility
            && self.container_name == other.container_name
        // Excluded telemetry: runtime_p99_ms, call_rate_per_s,
        // visibility_confidence, completeness, extraction_tier, modifiers_source
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
            is_exported: false,
            signature_normalized: None,
            modifiers: vec![],
            visibility: None,
            container_name: None,
            visibility_confidence: None,
            completeness: None,
            extraction_tier: ExtractionTier::Tier1,
            modifiers_source: None,
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

    // ── v2.3 reference classification (additive) ─────────────────────────────
    /// Fine-grained reason this reference exists (call site vs. type position
    /// vs. inheritance clause). Defaults to `Unknown` on v2.2 payloads and
    /// when the extractor cannot classify. Skipped on wire when `Unknown` so
    /// v2.2 clients see the exact same JSON they used to.
    #[serde(default, skip_serializing_if = "ReferenceKind::is_unknown")]
    pub kind: ReferenceKind,
    /// True when the enclosing file or function is recognised as a test
    /// (file path under a test dir, `#[test]` attribute, `@Test` annotation,
    /// etc.). Lets CKB down-rank test-only references in production queries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_test: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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

    // ── v2.3 reference classification ─────────────────────────────────────────

    #[test]
    fn v22_occurrence_json_deserializes_with_default_ref_fields() {
        // A v2.2 client sends no `kind` or `is_test`; v2.3 code must accept
        // the payload and fill defaults (Unknown / false).
        let json = r#"{
            "symbol_uri": "lip://local/a.rs#foo",
            "range": {"start_line":0,"start_char":0,"end_line":0,"end_char":3},
            "confidence_score": 80,
            "role": "reference",
            "override_doc": null
        }"#;
        let occ: OwnedOccurrence = serde_json::from_str(json).expect("v2.2 payload");
        assert_eq!(occ.kind, ReferenceKind::Unknown);
        assert!(!occ.is_test);
    }

    #[test]
    fn v23_unknown_and_is_test_false_skipped_on_wire() {
        // Defaults must not bloat the wire — v2.2 clients should see identical
        // JSON to what they produce.
        let occ = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange::default(),
            confidence_score: 50,
            role: Role::Reference,
            override_doc: None,
            kind: ReferenceKind::Unknown,
            is_test: false,
        };
        let json = serde_json::to_string(&occ).unwrap();
        assert!(
            !json.contains("\"kind\""),
            "kind:unknown must be skipped: {json}"
        );
        assert!(
            !json.contains("\"is_test\""),
            "is_test:false must be skipped: {json}"
        );
    }

    #[test]
    fn v23_non_default_ref_fields_roundtrip() {
        let occ = OwnedOccurrence {
            symbol_uri: "lip://local/a.rs#foo".into(),
            range: OwnedRange::default(),
            confidence_score: 50,
            role: Role::Reference,
            override_doc: None,
            kind: ReferenceKind::Call,
            is_test: true,
        };
        let json = serde_json::to_string(&occ).unwrap();
        assert!(json.contains("\"kind\":\"call\""), "got {json}");
        assert!(json.contains("\"is_test\":true"), "got {json}");
        let back: OwnedOccurrence = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, ReferenceKind::Call);
        assert!(back.is_test);
    }
}
