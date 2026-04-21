use std::path::PathBuf;

use clap::Args;
use prost::Message;

use lip::schema::{OwnedDocument, OwnedSymbolInfo, ReferenceKind, Role, SymbolKind};

// Generated from src/proto/scip.proto by prost-build.
#[allow(clippy::all)]
mod scip {
    include!(concat!(env!("OUT_DIR"), "/scip.rs"));
}

/// Export a LIP EventStream JSON file as a SCIP `.scip` index.
#[derive(Args)]
pub struct ExportArgs {
    /// Path to the LIP EventStream JSON file to export.
    #[arg(long = "to-scip")]
    pub scip_file: PathBuf,

    /// Path to the EventStream JSON to read (default: stdin).
    #[arg(long)]
    pub input: Option<PathBuf>,

    /// Tool name embedded in the SCIP metadata (default: "lip-cli").
    #[arg(long, default_value = "lip-cli")]
    pub tool_name: String,

    /// Tool version embedded in the SCIP metadata.
    #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
    pub tool_version: String,
}

pub async fn run(args: ExportArgs) -> anyhow::Result<()> {
    // Read the LIP EventStream JSON.
    let json: String = match args.input {
        Some(ref path) => std::fs::read_to_string(path)?,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };

    let stream: lip::schema::OwnedEventStream = serde_json::from_str(&json)?;

    let mut documents: Vec<scip::Document> = vec![];

    for delta in stream.deltas {
        if let Some(doc) = delta.document {
            documents.push(convert_document(doc));
        }
    }

    eprintln!(
        "exporting {} documents to {}",
        documents.len(),
        args.scip_file.display()
    );

    let index = scip::Index {
        metadata: Some(scip::Metadata {
            version: scip::ProtocolVersion::UnspecifiedProtocolVersion as i32,
            tool_info: Some(scip::ToolInfo {
                name: args.tool_name,
                version: args.tool_version,
                arguments: vec![],
            }),
            project_root: String::new(),
            text_document_encoding: scip::TextEncoding::Utf8 as i32,
        }),
        documents,
        external_symbols: vec![],
    };

    let mut buf = vec![];
    index.encode(&mut buf)?;
    std::fs::write(&args.scip_file, &buf)?;
    eprintln!("wrote {} bytes to {}", buf.len(), args.scip_file.display());

    Ok(())
}

// ─── Conversion helpers ───────────────────────────────────────────────────────

fn convert_document(doc: OwnedDocument) -> scip::Document {
    let symbols: Vec<scip::SymbolInformation> =
        doc.symbols.iter().map(convert_symbol_info).collect();

    let occurrences: Vec<scip::Occurrence> =
        doc.occurrences.iter().map(convert_occurrence).collect();

    // SCIP has no representation for source text or CPG edges; both are
    // LIP-only. A SCIP round-trip (import → export) is therefore lossy for
    // call-graph / blast-radius data.
    let _ = doc.source_text;
    let _ = doc.edges;
    scip::Document {
        language: doc.language,
        relative_path: uri_to_relative_path(&doc.uri),
        occurrences,
        symbols,
    }
}

fn convert_symbol_info(sym: &OwnedSymbolInfo) -> scip::SymbolInformation {
    let relationships: Vec<scip::Relationship> = sym
        .relationships
        .iter()
        .map(|r| scip::Relationship {
            symbol: lip_uri_to_scip_symbol(&r.target_uri),
            is_reference: r.is_reference,
            is_implementation: r.is_implementation,
            is_type_definition: r.is_type_definition,
            is_definition: r.is_override,
        })
        .collect();

    scip::SymbolInformation {
        symbol: lip_uri_to_scip_symbol(&sym.uri),
        display_name: sym.display_name.clone(),
        documentation: sym
            .documentation
            .as_deref()
            .map(|d| vec![d.to_owned()])
            .unwrap_or_default(),
        kind: lip_kind_to_scip(sym.kind) as i32,
        relationships,
        enclosing_symbol: sym.container_name.clone().unwrap_or_default(),
    }
}

fn convert_occurrence(occ: &lip::schema::OwnedOccurrence) -> scip::Occurrence {
    let mut role_bits = if occ.role == Role::Definition {
        scip::SymbolRole::Definition as i32
    } else {
        scip::SymbolRole::UnspecifiedSymbolRole as i32
    };
    // Preserve LIP ReferenceKind on export via SCIP symbol_roles bits (spec §10.2).
    match occ.kind {
        ReferenceKind::Write => role_bits |= scip::SymbolRole::WriteAccess as i32,
        ReferenceKind::Read => role_bits |= scip::SymbolRole::ReadAccess as i32,
        // Call/Type/Implements/Extends have no SCIP Occurrence-role equivalent; they
        // round-trip via other channels (call edges, Relationships, type info).
        ReferenceKind::Call
        | ReferenceKind::Type
        | ReferenceKind::Implements
        | ReferenceKind::Extends
        | ReferenceKind::Unknown => {}
    }
    if occ.is_test {
        role_bits |= scip::SymbolRole::Test as i32;
    }

    let range = vec![
        occ.range.start_line,
        occ.range.start_char,
        occ.range.end_line,
        occ.range.end_char,
    ];

    scip::Occurrence {
        range,
        symbol: lip_uri_to_scip_symbol(&occ.symbol_uri),
        symbol_roles: role_bits,
        override_documentation: occ
            .override_doc
            .as_deref()
            .map(|d| vec![d.to_owned()])
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// Convert a LIP URI back to a SCIP symbol string.
///
/// Handles three cases:
/// 1. New structured format from import.rs 5-field parser:
///    `lip://<scheme>/<manager>/<package>@<version>/<descriptor>` → 5-field SCIP string
/// 2. Legacy opaque format: `lip://scip/<scip-sym-with-slashes>` → un-slash SCIP string
/// 3. Native LIP local URIs: returned as-is (not a SCIP symbol)
fn lip_uri_to_scip_symbol(uri: &str) -> String {
    // Legacy opaque format produced by the old import.rs.
    if let Some(rest) = uri.strip_prefix("lip://scip/") {
        return rest.replace('/', " ");
    }
    // Structured format: lip://<scheme>/<manager>/<package>@<version>/<descriptor>
    // Does not start with "local/" or "scip/".
    if let Some(rest) = uri.strip_prefix("lip://") {
        if !rest.starts_with("local/") {
            // Split into [scheme, manager, package@version, descriptor...]
            let parts: Vec<&str> = rest.splitn(4, '/').collect();
            if parts.len() == 4 {
                let (scheme, manager, pkg_ver, descriptor_path) =
                    (parts[0], parts[1], parts[2], parts[3]);
                if let Some((package, version)) = pkg_ver.split_once('@') {
                    // Descriptor was stored with spaces replaced by '/'; restore them.
                    let descriptor = descriptor_path.replace('/', " ");
                    return format!("{scheme} {manager} {package} {version} {descriptor}");
                }
            }
        }
    }
    uri.to_owned()
}

/// Extract a relative path from a `file:///…` URI.
fn uri_to_relative_path(uri: &str) -> String {
    uri.strip_prefix("file:///")
        .or_else(|| uri.strip_prefix("file://"))
        .unwrap_or(uri)
        .to_owned()
}

fn lip_kind_to_scip(kind: SymbolKind) -> scip::Kind {
    match kind {
        SymbolKind::Class => scip::Kind::KClass,
        SymbolKind::Interface => scip::Kind::KInterface,
        SymbolKind::Method => scip::Kind::KMethod,
        SymbolKind::Function => scip::Kind::KFunction,
        SymbolKind::Field => scip::Kind::KField,
        SymbolKind::Variable => scip::Kind::KVariable,
        SymbolKind::Namespace => scip::Kind::KNamespace,
        SymbolKind::Enum => scip::Kind::KEnum,
        SymbolKind::EnumMember => scip::Kind::KEnumMember,
        SymbolKind::Constructor => scip::Kind::KConstructor,
        SymbolKind::TypeAlias => scip::Kind::KTypeAlias,
        SymbolKind::TypeParameter => scip::Kind::KTypeParameter,
        SymbolKind::Macro => scip::Kind::KMacro,
        SymbolKind::Parameter => scip::Kind::KVariable, // no SCIP equivalent
        SymbolKind::Unknown => scip::Kind::KUnspecifiedKind,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_relative_path_strips_prefix() {
        assert_eq!(uri_to_relative_path("file:///src/main.rs"), "src/main.rs");
        assert_eq!(uri_to_relative_path("file://src/main.rs"), "src/main.rs");
        assert_eq!(uri_to_relative_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn lip_uri_roundtrip_legacy_scip_prefix() {
        // Old import.rs format (lip://scip/ prefix) should still round-trip.
        let scip_sym = "scip-typescript npm react 18.2.0 React#Component.";
        let lip_uri = format!("lip://scip/{}", scip_sym.replace(' ', "/"));
        assert_eq!(lip_uri_to_scip_symbol(&lip_uri), scip_sym);
    }

    #[test]
    fn lip_uri_roundtrip_structured_format() {
        // New 5-field structured format from updated import.rs.
        let scip_sym = "scip-typescript npm react 18.2.0 React#Component.";
        let lip_uri = "lip://scip-typescript/npm/react@18.2.0/React#Component.";
        assert_eq!(lip_uri_to_scip_symbol(lip_uri), scip_sym);
    }

    #[test]
    fn kind_roundtrip_for_common_kinds() {
        use lip::schema::SymbolKind;
        for (lip, scip) in [
            (SymbolKind::Class, scip::Kind::KClass),
            (SymbolKind::Function, scip::Kind::KFunction),
            (SymbolKind::Enum, scip::Kind::KEnum),
            (SymbolKind::Unknown, scip::Kind::KUnspecifiedKind),
        ] {
            assert_eq!(lip_kind_to_scip(lip), scip);
        }
    }

    fn occ_with(kind: ReferenceKind, is_test: bool, role: Role) -> lip::schema::OwnedOccurrence {
        lip::schema::OwnedOccurrence {
            symbol_uri: "lip://local/x".to_owned(),
            range: lip::schema::OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
            confidence_score: 20,
            role,
            override_doc: None,
            kind,
            is_test,
        }
    }

    #[test]
    fn ref_kind_read_exports_read_access_bit() {
        let out = convert_occurrence(&occ_with(ReferenceKind::Read, false, Role::Reference));
        assert!(out.symbol_roles & scip::SymbolRole::ReadAccess as i32 != 0);
        assert!(out.symbol_roles & scip::SymbolRole::WriteAccess as i32 == 0);
        assert!(out.symbol_roles & scip::SymbolRole::Test as i32 == 0);
    }

    #[test]
    fn ref_kind_write_exports_write_access_bit() {
        let out = convert_occurrence(&occ_with(ReferenceKind::Write, false, Role::Reference));
        assert!(out.symbol_roles & scip::SymbolRole::WriteAccess as i32 != 0);
        assert!(out.symbol_roles & scip::SymbolRole::ReadAccess as i32 == 0);
    }

    #[test]
    fn is_test_exports_test_bit() {
        let out = convert_occurrence(&occ_with(ReferenceKind::Read, true, Role::Reference));
        assert!(out.symbol_roles & scip::SymbolRole::Test as i32 != 0);
    }

    #[test]
    fn ref_kind_call_does_not_set_access_bits() {
        // Call has no SCIP Occurrence-role equivalent; bits stay cleared.
        let out = convert_occurrence(&occ_with(ReferenceKind::Call, false, Role::Reference));
        assert!(out.symbol_roles & scip::SymbolRole::ReadAccess as i32 == 0);
        assert!(out.symbol_roles & scip::SymbolRole::WriteAccess as i32 == 0);
    }

    #[test]
    fn definition_with_write_kind_still_sets_definition() {
        // Defensive: if an upstream caller somehow marks a def with a Write kind,
        // both bits end up set. We don't actively strip kind on defs during export;
        // the pair remains semantically consistent because SCIP permits combined bits.
        let out = convert_occurrence(&occ_with(ReferenceKind::Write, false, Role::Definition));
        assert!(out.symbol_roles & scip::SymbolRole::Definition as i32 != 0);
    }
}
