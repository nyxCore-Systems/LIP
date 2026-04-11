use std::path::PathBuf;

use clap::Args;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};
use lip::schema::{
    sha256_hex, Action, OwnedDelta, OwnedDocument, OwnedEventStream, OwnedOccurrence, OwnedRange,
    OwnedSymbolInfo, Role, SymbolKind,
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
            .map(|sym| convert_symbol_info(&sym, confidence))
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
            anyhow::anyhow!(
                "cannot connect to daemon at {}: {e}",
                socket_path.display()
            )
        })?;
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
            if let ServerMessage::DeltaAck { accepted: false, error, .. } = &resp {
                anyhow::bail!("daemon rejected delta {seq}: {}", error.as_deref().unwrap_or("?"));
            }
        }
        eprintln!("pushed {total} deltas to daemon at {}", socket_path.display());
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

// ─── Conversion helpers ───────────────────────────────────────────────────────

fn convert_document(doc: scip::Document, confidence: u8) -> OwnedDelta {
    let uri = format!("file:///{}", doc.relative_path.trim_start_matches('/'));
    let content_hash = sha256_hex(doc.relative_path.as_bytes());

    let symbols: Vec<OwnedSymbolInfo> = doc
        .symbols
        .iter()
        .map(|s| convert_symbol_info(s, confidence))
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

fn convert_symbol_info(sym: &scip::SymbolInformation, confidence: u8) -> OwnedSymbolInfo {
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
    let doc = sym.documentation.join("\n\n");

    // SCIP private symbols begin with "local "; everything else is exported.
    let is_exported = !sym.symbol.starts_with("local ");
    OwnedSymbolInfo {
        uri: scip_symbol_to_lip_uri(&sym.symbol),
        display_name: display,
        kind,
        documentation: if doc.is_empty() { None } else { Some(doc) },
        signature: None,
        confidence_score: confidence,
        relationships: vec![],
        runtime_p99_ms: None,
        call_rate_per_s: None,
        taint_labels: vec![],
        blast_radius: 0,
        is_exported,
    }
}

fn convert_occurrence(occ: &scip::Occurrence) -> Option<OwnedOccurrence> {
    let range = parse_scip_range(&occ.range)?;
    let role = if occ.symbol_roles & (scip::SymbolRole::Definition as i32) != 0 {
        Role::Definition
    } else {
        Role::Reference
    };

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
}
