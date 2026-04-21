use std::path::PathBuf;

use clap::Args;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::indexer::language::Language;
use lip::query_graph::{ClientMessage, ServerMessage, Tier3Source};
use lip::schema::{
    normalize_signature, sha256_hex, visibility, Action, ExtractionTier, ModifiersSource,
    OwnedDelta, OwnedDocument, OwnedEventStream, OwnedOccurrence, OwnedRange, OwnedSymbolInfo,
    ReferenceKind, Role, SymbolKind,
};

use crate::output;

// Generated from src/proto/scip.proto by prost-build.
#[allow(clippy::all)]
mod scip {
    include!(concat!(env!("OUT_DIR"), "/scip.rs"));
}

/// Import a SCIP `.scip` index file and emit a LIP EventStream.
///
/// With `--push-to-daemon`, each document delta is streamed directly to a running
/// LIP daemon — enabling nightly CI to push compiler-accurate symbols into the
/// live graph without a daemon restart.
#[derive(Args)]
pub struct ImportArgs {
    /// Path to the `.scip` file to import.
    #[arg(long = "from-scip")]
    pub scip_file: PathBuf,

    /// Write the resulting EventStream JSON to this file (default: stdout when
    /// not using --push-to-daemon).
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Push deltas directly to a running LIP daemon via its Unix socket.
    /// When set, each document in the SCIP index is streamed as a `Delta` message.
    #[arg(long)]
    pub push_to_daemon: Option<PathBuf>,

    /// Confidence score to assign to imported symbols (1–100).
    /// Default: 90 (compiler-verified, not locally re-checked).
    #[arg(long, default_value_t = 90)]
    pub confidence: u8,

    /// Skip Tier 3 provenance registration on the daemon.
    ///
    /// By default `--push-to-daemon` sends a `RegisterTier3Source`
    /// message before streaming deltas so `QueryIndexStatus` reports
    /// who produced the imported symbols and when. Use this flag for
    /// ephemeral or test imports whose provenance should not pollute
    /// a long-lived daemon's status output. No effect on the default
    /// EventStream-JSON output path.
    #[arg(long)]
    pub no_provenance: bool,
}

pub async fn run(args: ImportArgs) -> anyhow::Result<()> {
    let bytes = std::fs::read(&args.scip_file)?;
    let index = scip::Index::decode(bytes.as_slice()).map_err(|e| {
        anyhow::anyhow!(
            "failed to decode SCIP index from {}: {e}",
            args.scip_file.display()
        )
    })?;

    eprintln!(
        "importing {} documents from {}",
        index.documents.len(),
        args.scip_file.display()
    );

    // Capture Tier 3 provenance before consuming `index.documents`.
    // `project_root` is a file:// URL identifying the source tree the
    // producer indexed; clients can later resolve it to a working tree
    // to compare HEAD against `imported_at_ms` for staleness.
    //
    // Skipped when `--no-provenance` is set — ephemeral/test imports
    // opt out of registering so they do not pollute a long-lived
    // daemon's `tier3_sources` list.
    let tier3_source = if args.no_provenance {
        None
    } else {
        Some(build_tier3_source(&index, &args.scip_file))
    };

    let confidence = args.confidence;
    let mut deltas: Vec<OwnedDelta> = index
        .documents
        .into_iter()
        .map(|d| convert_document(d, confidence))
        .collect();

    // Also import external symbols as a synthetic document.
    if !index.external_symbols.is_empty() {
        let symbols: Vec<OwnedSymbolInfo> = index
            .external_symbols
            .into_iter()
            .map(|sym| convert_symbol_info(&sym, confidence, Language::Unknown))
            .collect();

        let doc = OwnedDocument {
            uri: "scip://external".to_owned(),
            content_hash: sha256_hex(b"external"),
            language: String::new(),
            occurrences: vec![],
            symbols,
            merkle_path: String::new(),
            edges: vec![],
            source_text: None,
        };
        deltas.push(OwnedDelta {
            action: Action::Upsert,
            commit_hash: String::new(),
            document: Some(doc),
            symbol: None,
            slice: None,
        });
    }

    // ── CI batch push: stream deltas directly to a running daemon ──────────────
    if let Some(socket_path) = args.push_to_daemon {
        let mut stream = UnixStream::connect(&socket_path).await.map_err(|e| {
            anyhow::anyhow!("cannot connect to daemon at {}: {e}", socket_path.display())
        })?;

        // Register provenance before streaming deltas so the daemon can
        // timestamp the import and expose the record via `QueryIndexStatus`.
        // Older daemons that predate `register_tier3_source` will reply
        // with `UnknownMessage`; we tolerate that and proceed — the deltas
        // still land, the provenance is just unavailable.
        if let Some(source) = tier3_source {
            let reg_msg = ClientMessage::RegisterTier3Source { source };
            let reg_body = serde_json::to_vec(&reg_msg)?;
            stream
                .write_all(&(reg_body.len() as u32).to_be_bytes())
                .await?;
            stream.write_all(&reg_body).await?;
            let mut reg_len = [0u8; 4];
            stream.read_exact(&mut reg_len).await?;
            let reg_resp_len = u32::from_be_bytes(reg_len) as usize;
            let mut reg_resp_bytes = vec![0u8; reg_resp_len];
            stream.read_exact(&mut reg_resp_bytes).await?;
            // We do not fail on UnknownMessage — that only means the daemon
            // is pre-v2.1. We do surface a genuine DeltaAck rejection.
            if let Ok(ServerMessage::DeltaAck {
                accepted: false,
                error,
                ..
            }) = serde_json::from_slice::<ServerMessage>(&reg_resp_bytes)
            {
                eprintln!(
                    "warning: daemon rejected tier3 provenance registration: {}",
                    error.as_deref().unwrap_or("?")
                );
            }
        } else {
            eprintln!("provenance registration skipped (--no-provenance)");
        }

        let total = deltas.len();
        for (seq, delta) in deltas.into_iter().enumerate() {
            let Some(doc) = delta.document else { continue };
            let msg = ClientMessage::Delta {
                seq: seq as u64,
                action: delta.action,
                document: doc,
            };
            let body = serde_json::to_vec(&msg)?;
            stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
            stream.write_all(&body).await?;

            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let resp_len = u32::from_be_bytes(len_buf) as usize;
            let mut resp_bytes = vec![0u8; resp_len];
            stream.read_exact(&mut resp_bytes).await?;
            let resp: ServerMessage = serde_json::from_slice(&resp_bytes)?;
            if let ServerMessage::DeltaAck {
                accepted: false,
                error,
                ..
            } = &resp
            {
                anyhow::bail!(
                    "daemon rejected delta {seq}: {}",
                    error.as_deref().unwrap_or("?")
                );
            }
        }
        eprintln!(
            "pushed {total} deltas to daemon at {}",
            socket_path.display()
        );
        return Ok(());
    }

    // ── Default: emit EventStream JSON ────────────────────────────────────────
    let stream = OwnedEventStream::new(
        concat!("lip-cli/", env!("CARGO_PKG_VERSION"), " import-scip"),
        deltas,
    );

    if let Some(out_path) = args.output {
        let json = serde_json::to_string_pretty(&stream)?;
        std::fs::write(&out_path, json)?;
        eprintln!("wrote EventStream to {}", out_path.display());
    } else {
        output::print_json(&stream)?;
    }

    Ok(())
}

/// Build a Tier 3 provenance record from a SCIP index.
///
/// `source_id` is derived from producer name + `project_root` (or the
/// .scip filename when metadata is absent), so re-imports of the same
/// source refresh the record in place rather than growing the list.
fn build_tier3_source(index: &scip::Index, scip_path: &std::path::Path) -> Tier3Source {
    let (tool_name, tool_version, project_root) = match index.metadata.as_ref() {
        Some(md) => {
            let (tn, tv) = md
                .tool_info
                .as_ref()
                .map(|ti| (ti.name.clone(), ti.version.clone()))
                .unwrap_or_default();
            (tn, tv, md.project_root.clone())
        }
        None => (String::new(), String::new(), String::new()),
    };

    let fingerprint = if project_root.is_empty() {
        scip_path.display().to_string()
    } else {
        project_root.clone()
    };
    let source_id = sha256_hex(format!("{tool_name}:{fingerprint}").as_bytes());

    let imported_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Tier3Source {
        source_id,
        tool_name,
        tool_version,
        project_root,
        imported_at_ms,
    }
}

// ─── Conversion helpers ───────────────────────────────────────────────────────

fn convert_document(doc: scip::Document, confidence: u8) -> OwnedDelta {
    let uri = format!("file:///{}", doc.relative_path.trim_start_matches('/'));
    let content_hash = sha256_hex(doc.relative_path.as_bytes());
    let lang = scip_language_to_lip(&doc.language);

    let symbols: Vec<OwnedSymbolInfo> = doc
        .symbols
        .iter()
        .map(|s| convert_symbol_info(s, confidence, lang))
        .collect();

    let occurrences: Vec<OwnedOccurrence> = doc
        .occurrences
        .iter()
        .filter_map(convert_occurrence)
        .collect();

    let lip_doc = OwnedDocument {
        uri: uri.clone(),
        content_hash: content_hash.clone(),
        language: doc.language.clone(),
        occurrences,
        symbols,
        merkle_path: uri,
        edges: vec![],
        source_text: None, // SCIP imports have pre-computed symbols; no raw text
    };

    OwnedDelta {
        action: Action::Upsert,
        // All imported symbols start at Tier 2 confidence (compiler-verified in SCIP)
        // pending daemon re-verification, per spec §10.2.
        commit_hash: content_hash,
        document: Some(lip_doc),
        symbol: None,
        slice: None,
    }
}

fn convert_symbol_info(
    sym: &scip::SymbolInformation,
    confidence: u8,
    lang: Language,
) -> OwnedSymbolInfo {
    let display = if sym.display_name.is_empty() {
        // Fall back to the last descriptor segment of the symbol string.
        sym.symbol
            .rsplit('#')
            .next()
            .unwrap_or(&sym.symbol)
            .to_owned()
    } else {
        sym.display_name.clone()
    };

    let kind = scip_kind_to_lip(sym.kind);

    // SCIP indexers (scip-rust, scip-typescript, scip-java, …) place the
    // rendered type signature as documentation[0] and prose doc comments as
    // subsequent entries. Extract the signature and doc separately.
    let (signature, documentation) = scip_extract_sig_and_doc(&sym.documentation);

    // SCIP private symbols begin with "local "; everything else is exported.
    let is_exported = !sym.symbol.starts_with("local ");

    // v2.3 structural metadata. Upstream SCIP carries `enclosing_symbol` but no
    // canonical modifier field, so we always reconstruct modifiers by parsing
    // the signature prefix and tag the source accordingly. CKB discounts
    // confidence on `PrefixParse` when merging imports with Tier-1 results.
    let modifiers = signature
        .as_deref()
        .map(|s| parse_modifiers_from_signature(s, lang))
        .unwrap_or_default();

    let (vis, vis_confidence) = if matches!(lang, Language::Unknown) {
        // Without a language we cannot apply name/keyword rules; skip inference.
        (None, None)
    } else {
        let (v, c) = visibility::infer(&display, &modifiers, lang);
        (Some(v), Some(c as f32 / 100.0))
    };

    let signature_normalized = signature
        .as_deref()
        .map(|s| normalize_signature(s, lang));

    let container_name = Some(sym.enclosing_symbol.clone())
        .filter(|s| !s.is_empty())
        .map(|enc| scip_enclosing_to_display(&enc));

    OwnedSymbolInfo {
        uri: scip_symbol_to_lip_uri(&sym.symbol),
        display_name: display,
        kind,
        documentation,
        signature,
        confidence_score: confidence,
        relationships: vec![],
        runtime_p99_ms: None,
        call_rate_per_s: None,
        taint_labels: vec![],
        blast_radius: 0,
        is_exported,
        signature_normalized,
        modifiers,
        visibility: vis,
        container_name,
        visibility_confidence: vis_confidence,
        extraction_tier: ExtractionTier::Tier3Scip,
        modifiers_source: Some(ModifiersSource::PrefixParse),
        ..Default::default()
    }
}

/// Map a SCIP `Document.language` string to a LIP [`Language`].
///
/// Returns [`Language::Unknown`] when the SCIP index omits the language field or
/// uses a value we do not handle. Matches the casing upstream producers emit
/// (e.g. scip-typescript emits `"TypeScript"`).
fn scip_language_to_lip(lang: &str) -> Language {
    match lang.to_ascii_lowercase().as_str() {
        "rust" => Language::Rust,
        "typescript" | "tsx" => Language::TypeScript,
        "javascript" | "javascriptreact" | "jsx" => Language::JavaScript,
        "python" => Language::Python,
        "dart" => Language::Dart,
        "go" => Language::Go,
        "kotlin" => Language::Kotlin,
        "swift" => Language::Swift,
        "c" => Language::C,
        "cpp" | "c++" | "cxx" => Language::Cpp,
        _ => Language::Unknown,
    }
}

/// Parse leading modifier keywords from a signature prefix (SCIP-native docs).
fn parse_modifiers_from_signature(signature: &str, lang: Language) -> Vec<String> {
    let keywords: &[&str] = match lang {
        Language::Rust => &["pub", "const", "async", "unsafe", "extern", "static", "mut"],
        Language::TypeScript
        | Language::JavaScript
        | Language::JavaScriptReact => &[
            "export", "default", "async", "static", "readonly", "public", "private", "protected",
            "abstract", "declare", "override",
        ],
        Language::Dart => &[
            "static", "abstract", "final", "const", "external", "factory", "late", "covariant",
        ],
        Language::Kotlin => &[
            "private", "protected", "internal", "public", "abstract", "final", "open", "override",
            "suspend", "inline", "external", "data", "sealed", "companion", "lateinit", "const",
            "operator", "infix", "tailrec",
        ],
        Language::Swift => &[
            "private", "fileprivate", "internal", "public", "open", "static", "final", "override",
            "mutating", "nonmutating", "required", "convenience", "lazy", "weak", "unowned",
            "dynamic",
        ],
        Language::C | Language::Cpp => &[
            "static", "extern", "const", "virtual", "override", "explicit", "inline", "constexpr",
            "private", "protected", "public", "friend", "mutable", "volatile",
        ],
        // Python / Go / Unknown: prefix-parse not meaningful; rely on name rules.
        _ => &[],
    };

    let mut out = Vec::new();
    let mut rest = signature.trim_start();
    loop {
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

/// Convert a SCIP `enclosing_symbol` string to a human-readable container name.
///
/// SCIP encodes enclosing symbols the same way as regular `symbol` strings
/// (e.g. `scip-typescript npm react 18.2.0 React#Component.`); the trailing
/// descriptor is what a human calls the class/namespace.
fn scip_enclosing_to_display(enc: &str) -> String {
    // splitn(5): keep the descriptor intact even when it contains spaces.
    let parts: Vec<&str> = enc.splitn(5, ' ').collect();
    if parts.len() == 5 {
        let desc = parts[4];
        let cleaned = desc.trim_end_matches(['#', '.', '/', '`', ' ']);
        let last = cleaned
            .rsplit(['#', '.', '/', '`'])
            .next()
            .unwrap_or(cleaned);
        if !last.is_empty() {
            return last.to_owned();
        }
    }
    enc.to_owned()
}

/// Split SCIP documentation entries into a `(signature, doc_comment)` pair.
///
/// SCIP indexers place the rendered type signature as the first entry of the
/// `documentation` array (e.g. `"pub fn foo(x: i32) -> Bar"`), followed by
/// prose doc comments. When there are two or more entries, the first is always
/// the signature. When there is exactly one entry, a lightweight heuristic
/// checks whether it looks like a declaration or a prose comment.
fn scip_extract_sig_and_doc(docs: &[String]) -> (Option<String>, Option<String>) {
    match docs {
        [] => (None, None),
        [only] => {
            if looks_like_signature(only) {
                (Some(only.clone()), None)
            } else {
                (None, Some(only.clone()))
            }
        }
        [sig, rest @ ..] => {
            let doc = rest.join("\n\n");
            (
                Some(sig.clone()),
                if doc.is_empty() { None } else { Some(doc) },
            )
        }
    }
}

/// Returns `true` when a single-entry SCIP documentation string is likely a
/// type signature rather than a prose doc comment.
///
/// Heuristic: the string starts with a language declaration keyword. This
/// covers every language supported by major SCIP indexers. Prose comments
/// never start with these keywords.
fn looks_like_signature(s: &str) -> bool {
    let trimmed = s.trim_start();
    // Common declaration-keyword prefixes across Rust, TypeScript, Python,
    // Java, Go, Dart, and Kotlin.
    const SIG_PREFIXES: &[&str] = &[
        "pub ",
        "pub(",
        "fn ",
        "async fn ",
        "pub fn ",
        "pub async fn ",
        "def ",
        "class ",
        "interface ",
        "type ",
        "export ",
        "func ",
        "abstract ",
        "struct ",
        "enum ",
        "const ",
        "var ",
        "let ",
        "static ",
        "final ",
        "override ",
        "object ",
        "impl ",
        "trait ",
        "module ",
        "namespace ",
    ];
    SIG_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn convert_occurrence(occ: &scip::Occurrence) -> Option<OwnedOccurrence> {
    let range = parse_scip_range(&occ.range)?;
    let roles = occ.symbol_roles;
    let role = if roles & (scip::SymbolRole::Definition as i32) != 0 {
        Role::Definition
    } else {
        Role::Reference
    };
    // SCIP symbol_roles bits map to LIP ReferenceKind (spec §10.2).
    // Write takes precedence over Read if both bits are set.
    let kind = if matches!(role, Role::Definition) {
        ReferenceKind::Unknown
    } else if roles & (scip::SymbolRole::WriteAccess as i32) != 0 {
        ReferenceKind::Write
    } else if roles & (scip::SymbolRole::ReadAccess as i32) != 0 {
        ReferenceKind::Read
    } else {
        ReferenceKind::Unknown
    };
    let is_test = roles & (scip::SymbolRole::Test as i32) != 0;

    Some(OwnedOccurrence {
        symbol_uri: scip_symbol_to_lip_uri(&occ.symbol),
        range,
        confidence_score: 90,
        role,
        override_doc: if occ.override_documentation.is_empty() {
            None
        } else {
            Some(occ.override_documentation.join("\n"))
        },
        kind,
        is_test,
    })
}

/// Convert a SCIP symbol string to a LIP URI (spec §5, §10.2).
///
/// SCIP symbol format: `<scheme> <manager> <package> <version> <descriptor>`
/// Example: `scip-typescript npm react 18.2.0 React#Component.`
///
/// Produces: `lip://<scheme>/<manager>/<package>@<version>/<descriptor>`
fn scip_symbol_to_lip_uri(symbol: &str) -> String {
    if symbol.is_empty() {
        return "lip://local/unknown#unknown".to_owned();
    }
    // splitn(5) ensures the descriptor (5th field) is kept intact even if it
    // contained spaces (e.g. Java qualified names with inner-class separators).
    let parts: Vec<&str> = symbol.splitn(5, ' ').collect();
    if parts.len() == 5 {
        let (scheme, manager, package, version, descriptor) =
            (parts[0], parts[1], parts[2], parts[3], parts[4]);
        // Descriptor spaces are rare but legal; use '/' as in-path separator.
        let desc_path = descriptor.replace(' ', "/");
        format!("lip://{scheme}/{manager}/{package}@{version}/{desc_path}")
    } else {
        // Non-standard or partial symbol — fall back to opaque encoding.
        let sanitised = symbol
            .replace(' ', "/")
            .replace("..", ".")
            .trim_start_matches('/')
            .to_owned();
        format!("lip://scip/{sanitised}")
    }
}

fn parse_scip_range(range: &[i32]) -> Option<OwnedRange> {
    match range.len() {
        3 => Some(OwnedRange {
            start_line: range[0],
            start_char: range[1],
            end_line: range[0],
            end_char: range[2],
        }),
        4 => Some(OwnedRange {
            start_line: range[0],
            start_char: range[1],
            end_line: range[2],
            end_char: range[3],
        }),
        _ => None,
    }
}

fn scip_kind_to_lip(kind: i32) -> SymbolKind {
    use scip::Kind;
    match Kind::try_from(kind).unwrap_or(Kind::KUnspecifiedKind) {
        Kind::KClass | Kind::KStruct | Kind::KObject => SymbolKind::Class,
        Kind::KInterface | Kind::KProtocol => SymbolKind::Interface,
        Kind::KFunction
        | Kind::KAbstractMethod
        | Kind::KMethod
        | Kind::KMethodAlias
        | Kind::KStaticMethod
        | Kind::KPureVirtualMethod => SymbolKind::Function,
        Kind::KEnum => SymbolKind::Enum,
        Kind::KEnumMember => SymbolKind::EnumMember,
        Kind::KVariable | Kind::KConstant | Kind::KStaticVariable | Kind::KField => {
            SymbolKind::Variable
        }
        Kind::KModule | Kind::KNamespace | Kind::KPackage | Kind::KPackageObject => {
            SymbolKind::Namespace
        }
        Kind::KTypeAlias | Kind::KTypeParameter => SymbolKind::TypeAlias,
        Kind::KConstructor => SymbolKind::Constructor,
        Kind::KMacro => SymbolKind::Macro,
        _ => SymbolKind::Unknown,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::scip::Kind;
    use super::*;

    fn kind(k: Kind) -> SymbolKind {
        scip_kind_to_lip(k as i32)
    }

    #[test]
    fn class_variants_map_to_class() {
        assert_eq!(kind(Kind::KClass), SymbolKind::Class);
        assert_eq!(kind(Kind::KStruct), SymbolKind::Class);
        assert_eq!(kind(Kind::KObject), SymbolKind::Class);
    }

    #[test]
    fn interface_variants_map_to_interface() {
        assert_eq!(kind(Kind::KInterface), SymbolKind::Interface);
        assert_eq!(kind(Kind::KProtocol), SymbolKind::Interface);
    }

    #[test]
    fn function_variants_map_to_function() {
        assert_eq!(kind(Kind::KFunction), SymbolKind::Function);
        assert_eq!(kind(Kind::KAbstractMethod), SymbolKind::Function);
        assert_eq!(kind(Kind::KMethod), SymbolKind::Function);
        assert_eq!(kind(Kind::KStaticMethod), SymbolKind::Function);
        assert_eq!(kind(Kind::KPureVirtualMethod), SymbolKind::Function);
    }

    #[test]
    fn enum_maps_correctly() {
        assert_eq!(kind(Kind::KEnum), SymbolKind::Enum);
        assert_eq!(kind(Kind::KEnumMember), SymbolKind::EnumMember);
    }

    #[test]
    fn variable_variants_map_to_variable() {
        assert_eq!(kind(Kind::KVariable), SymbolKind::Variable);
        assert_eq!(kind(Kind::KConstant), SymbolKind::Variable);
        assert_eq!(kind(Kind::KStaticVariable), SymbolKind::Variable);
        assert_eq!(kind(Kind::KField), SymbolKind::Variable);
    }

    #[test]
    fn namespace_variants_map_to_namespace() {
        assert_eq!(kind(Kind::KModule), SymbolKind::Namespace);
        assert_eq!(kind(Kind::KNamespace), SymbolKind::Namespace);
        assert_eq!(kind(Kind::KPackage), SymbolKind::Namespace);
        assert_eq!(kind(Kind::KPackageObject), SymbolKind::Namespace);
    }

    #[test]
    fn type_alias_variants() {
        assert_eq!(kind(Kind::KTypeAlias), SymbolKind::TypeAlias);
        assert_eq!(kind(Kind::KTypeParameter), SymbolKind::TypeAlias);
    }

    #[test]
    fn unspecified_and_unknown_map_to_unknown() {
        assert_eq!(kind(Kind::KUnspecifiedKind), SymbolKind::Unknown);
        // Out-of-range i32 should also fall through to Unknown.
        assert_eq!(scip_kind_to_lip(9999), SymbolKind::Unknown);
    }

    #[test]
    fn constructor_and_macro() {
        assert_eq!(kind(Kind::KConstructor), SymbolKind::Constructor);
        assert_eq!(kind(Kind::KMacro), SymbolKind::Macro);
    }

    #[test]
    fn scip_symbol_to_lip_uri_5field() {
        let uri = scip_symbol_to_lip_uri("scip-typescript npm react 18.2.0 React#Component.");
        assert_eq!(
            uri,
            "lip://scip-typescript/npm/react@18.2.0/React#Component."
        );
    }

    #[test]
    fn scip_symbol_to_lip_uri_fallback_for_short_symbol() {
        // Fewer than 5 fields → opaque fallback.
        let uri = scip_symbol_to_lip_uri("custom foo");
        assert!(uri.starts_with("lip://scip/"));
    }

    #[test]
    fn scip_symbol_empty_returns_unknown() {
        assert_eq!(scip_symbol_to_lip_uri(""), "lip://local/unknown#unknown");
    }

    // ── SCIP signature extraction ─────────────────────────────────────────────

    #[test]
    fn scip_signature_extracted_from_multi_doc() {
        // When SCIP provides 2+ documentation entries, doc[0] is the signature.
        let (sig, doc) = scip_extract_sig_and_doc(&[
            "pub fn verify_token(token: &str) -> Result<Claims>".to_owned(),
            "Verify a JWT token and return its decoded claims.".to_owned(),
            "Returns `Err` if the token is expired or has an invalid signature.".to_owned(),
        ]);
        assert_eq!(
            sig.as_deref(),
            Some("pub fn verify_token(token: &str) -> Result<Claims>"),
            "doc[0] should become the signature"
        );
        assert!(
            doc.as_deref().unwrap_or("").contains("Verify a JWT token"),
            "remaining entries should become the documentation"
        );
    }

    #[test]
    fn scip_single_doc_keyword_heuristic() {
        // A single entry starting with a declaration keyword → signature.
        let (sig, doc) = scip_extract_sig_and_doc(&["pub fn foo(x: i32) -> Bar".to_owned()]);
        assert_eq!(sig.as_deref(), Some("pub fn foo(x: i32) -> Bar"));
        assert!(doc.is_none(), "no doc comment expected");

        // A single prose entry → documentation, not signature.
        let (sig2, doc2) = scip_extract_sig_and_doc(&["A useful helper function.".to_owned()]);
        assert!(sig2.is_none(), "prose should not become a signature");
        assert_eq!(doc2.as_deref(), Some("A useful helper function."));

        // TypeScript export keyword is recognised.
        let (sig3, _) =
            scip_extract_sig_and_doc(&["export function greet(name: string): void".to_owned()]);
        assert!(
            sig3.is_some(),
            "export keyword should be recognised as signature"
        );

        // Empty slice → both None.
        let (sig4, doc4) = scip_extract_sig_and_doc(&[]);
        assert!(sig4.is_none());
        assert!(doc4.is_none());
    }

    // ── v2.3 SCIP importer enrichment ─────────────────────────────────────────

    use lip::schema::{ModifiersSource, Visibility};

    fn sym_with(lang: &str, proto_sym: scip::SymbolInformation) -> OwnedSymbolInfo {
        convert_symbol_info(&proto_sym, 90, scip_language_to_lip(lang))
    }

    #[test]
    fn scip_language_known_values_round_trip() {
        assert!(matches!(scip_language_to_lip("rust"), Language::Rust));
        assert!(matches!(
            scip_language_to_lip("TypeScript"),
            Language::TypeScript
        ));
        assert!(matches!(scip_language_to_lip("Python"), Language::Python));
        assert!(matches!(scip_language_to_lip(""), Language::Unknown));
    }

    #[test]
    fn scip_rust_import_populates_v23_fields() {
        let proto = scip::SymbolInformation {
            symbol: "rust-analyzer cargo my_crate 1.0 my_mod/foo().".to_owned(),
            display_name: "foo".to_owned(),
            documentation: vec!["pub async fn foo(x: i32) -> Bar".to_owned()],
            relationships: vec![],
            kind: scip::Kind::KFunction as i32,
            enclosing_symbol: String::new(),
        };
        let out = sym_with("rust", proto);
        assert_eq!(out.modifiers, vec!["pub".to_owned(), "async".to_owned()]);
        assert_eq!(out.visibility, Some(Visibility::Public));
        assert_eq!(out.visibility_confidence, Some(1.0));
        assert_eq!(out.extraction_tier, ExtractionTier::Tier3Scip);
        assert_eq!(out.modifiers_source, Some(ModifiersSource::PrefixParse));
        assert_eq!(
            out.signature_normalized.as_deref(),
            Some("pub async fn foo(_: i32) -> Bar")
        );
    }

    #[test]
    fn scip_enclosing_symbol_becomes_container() {
        let proto = scip::SymbolInformation {
            symbol: "scip-typescript npm react 18.2.0 src/App.ts`render().".to_owned(),
            display_name: "render".to_owned(),
            documentation: vec!["render(): void".to_owned()],
            relationships: vec![],
            kind: scip::Kind::KMethod as i32,
            enclosing_symbol:
                "scip-typescript npm react 18.2.0 src/App.ts`App#".to_owned(),
        };
        let out = sym_with("typescript", proto);
        // Last descriptor segment of the enclosing symbol becomes container name.
        assert_eq!(out.container_name.as_deref(), Some("App"));
    }

    #[test]
    fn scip_unknown_language_skips_inference() {
        let proto = scip::SymbolInformation {
            symbol: "x y z 1.0 Foo.".to_owned(),
            display_name: "Foo".to_owned(),
            documentation: vec![],
            relationships: vec![],
            kind: scip::Kind::KClass as i32,
            enclosing_symbol: String::new(),
        };
        let out = sym_with("", proto);
        // No signature and no language → no modifiers, no visibility inference.
        assert!(out.modifiers.is_empty());
        assert!(out.visibility.is_none());
        assert!(out.visibility_confidence.is_none());
        // Source is still marked as PrefixParse — absence of a proto modifier
        // field is the whole reason we tag it that way.
        assert_eq!(out.modifiers_source, Some(ModifiersSource::PrefixParse));
    }

    #[test]
    fn scip_python_uses_name_convention() {
        let proto = scip::SymbolInformation {
            symbol: "scip-python pypi mylib 1.0 _helper().".to_owned(),
            display_name: "_helper".to_owned(),
            documentation: vec!["def _helper(x: int) -> None".to_owned()],
            relationships: vec![],
            kind: scip::Kind::KFunction as i32,
            enclosing_symbol: String::new(),
        };
        let out = sym_with("python", proto);
        assert_eq!(out.visibility, Some(Visibility::Private));
        // Python modifier keywords are not meaningful in our prefix-parse set,
        // so the list stays empty — visibility comes from the name rule.
        assert!(out.modifiers.is_empty());
    }

    #[test]
    fn scip_pub_crate_is_internal() {
        let proto = scip::SymbolInformation {
            symbol: "rust-analyzer cargo crate 1.0 foo().".to_owned(),
            display_name: "foo".to_owned(),
            documentation: vec!["pub(crate) fn foo()".to_owned()],
            relationships: vec![],
            kind: scip::Kind::KFunction as i32,
            enclosing_symbol: String::new(),
        };
        let out = sym_with("rust", proto);
        assert_eq!(out.modifiers, vec!["pub(crate)".to_owned()]);
        assert_eq!(out.visibility, Some(Visibility::Internal));
    }

    /// Wire-format round-trip: encode a `scip::Index` containing
    /// `enclosing_symbol` (field 8, the only upstream-compatible v2.3 field we
    /// added to the proto) and verify it decodes back and flows into
    /// `container_name` on the LIP side. This guards against a future proto
    /// edit that drops or renumbers the field.
    #[test]
    fn scip_enclosing_symbol_survives_wire_round_trip() {
        let proto = scip::Index {
            metadata: Some(scip::Metadata {
                version: scip::ProtocolVersion::UnspecifiedProtocolVersion as i32,
                tool_info: Some(scip::ToolInfo {
                    name: "test".to_owned(),
                    version: "0.0".to_owned(),
                    arguments: vec![],
                }),
                project_root: String::new(),
                text_document_encoding: scip::TextEncoding::Utf8 as i32,
            }),
            documents: vec![scip::Document {
                language: "rust".to_owned(),
                relative_path: "src/mod.rs".to_owned(),
                occurrences: vec![],
                symbols: vec![scip::SymbolInformation {
                    symbol: "rust-analyzer cargo mycrate 1.0 Mod/bar().".to_owned(),
                    display_name: "bar".to_owned(),
                    documentation: vec!["pub fn bar()".to_owned()],
                    relationships: vec![],
                    kind: scip::Kind::KMethod as i32,
                    enclosing_symbol:
                        "rust-analyzer cargo mycrate 1.0 Mod/MyStruct#".to_owned(),
                }],
            }],
            external_symbols: vec![],
        };

        let mut buf = vec![];
        prost::Message::encode(&proto, &mut buf).expect("prost encode");
        let decoded = <scip::Index as prost::Message>::decode(&buf[..]).expect("prost decode");

        let doc = &decoded.documents[0];
        let sym = &doc.symbols[0];
        assert_eq!(
            sym.enclosing_symbol,
            "rust-analyzer cargo mycrate 1.0 Mod/MyStruct#",
            "field 8 enclosing_symbol lost across prost encode/decode"
        );

        let out = convert_symbol_info(sym, 90, scip_language_to_lip(&doc.language));
        assert_eq!(
            out.container_name.as_deref(),
            Some("MyStruct"),
            "decoded enclosing_symbol did not populate container_name"
        );
        assert_eq!(out.extraction_tier, ExtractionTier::Tier3Scip);
        assert_eq!(out.modifiers_source, Some(ModifiersSource::PrefixParse));
    }

    fn occ_with_roles(roles: i32) -> scip::Occurrence {
        scip::Occurrence {
            range: vec![1, 0, 1, 5],
            symbol: "rust-analyzer cargo c 1.0 m/x.".to_owned(),
            symbol_roles: roles,
            override_documentation: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn scip_read_access_maps_to_ref_kind_read() {
        let o = convert_occurrence(&occ_with_roles(scip::SymbolRole::ReadAccess as i32))
            .expect("occurrence converts");
        assert_eq!(o.role, Role::Reference);
        assert_eq!(o.kind, ReferenceKind::Read);
        assert!(!o.is_test);
    }

    #[test]
    fn scip_write_access_maps_to_ref_kind_write() {
        let o = convert_occurrence(&occ_with_roles(scip::SymbolRole::WriteAccess as i32))
            .expect("occurrence converts");
        assert_eq!(o.kind, ReferenceKind::Write);
    }

    #[test]
    fn scip_write_wins_over_read_when_both_set() {
        let bits =
            scip::SymbolRole::WriteAccess as i32 | scip::SymbolRole::ReadAccess as i32;
        let o = convert_occurrence(&occ_with_roles(bits)).expect("occurrence converts");
        assert_eq!(o.kind, ReferenceKind::Write);
    }

    #[test]
    fn scip_test_bit_sets_is_test() {
        let bits =
            scip::SymbolRole::Test as i32 | scip::SymbolRole::ReadAccess as i32;
        let o = convert_occurrence(&occ_with_roles(bits)).expect("occurrence converts");
        assert!(o.is_test);
        assert_eq!(o.kind, ReferenceKind::Read);
    }

    #[test]
    fn scip_definition_keeps_kind_unknown() {
        // SCIP does not set read/write on definitions; kind stays Unknown.
        let bits =
            scip::SymbolRole::Definition as i32 | scip::SymbolRole::WriteAccess as i32;
        let o = convert_occurrence(&occ_with_roles(bits)).expect("occurrence converts");
        assert_eq!(o.role, Role::Definition);
        assert_eq!(o.kind, ReferenceKind::Unknown);
    }

    #[test]
    fn scip_unspecified_role_leaves_kind_unknown() {
        let o = convert_occurrence(&occ_with_roles(0)).expect("occurrence converts");
        assert_eq!(o.role, Role::Reference);
        assert_eq!(o.kind, ReferenceKind::Unknown);
        assert!(!o.is_test);
    }
}
