use std::path::PathBuf;

use clap::Args;
use prost::Message;

use lip::schema::{OwnedDocument, OwnedSymbolInfo, Role, SymbolKind};

// Generated from src/proto/scip.proto by prost-build.
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
            version:     scip::ProtocolVersion::UnspecifiedProtocolVersion as i32,
            tool_info:   Some(scip::ToolInfo {
                name:      args.tool_name,
                version:   args.tool_version,
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
    let symbols: Vec<scip::SymbolInformation> = doc
        .symbols
        .iter()
        .map(|s| convert_symbol_info(s))
        .collect();

    let occurrences: Vec<scip::Occurrence> = doc
        .occurrences
        .iter()
        .map(|o| convert_occurrence(o))
        .collect();

    // Note: scip::Document has no `text` field in the generated proto; source
    // text is not part of the SCIP wire format at this schema version.
    let _ = doc.source_text; // present in LIP, absent in SCIP
    scip::Document {
        language:      doc.language,
        relative_path: uri_to_relative_path(&doc.uri),
        occurrences,
        symbols,
        ..Default::default()
    }
}

fn convert_symbol_info(sym: &OwnedSymbolInfo) -> scip::SymbolInformation {
    let relationships: Vec<scip::Relationship> = sym
        .relationships
        .iter()
        .map(|r| scip::Relationship {
            symbol:             lip_uri_to_scip_symbol(&r.target_uri),
            is_reference:       r.is_reference,
            is_implementation:  r.is_implementation,
            is_type_definition: r.is_type_definition,
            is_override:        r.is_override,
        })
        .collect();

    scip::SymbolInformation {
        symbol:        lip_uri_to_scip_symbol(&sym.uri),
        display_name:  sym.display_name.clone(),
        documentation: sym
            .documentation
            .as_deref()
            .map(|d| vec![d.to_owned()])
            .unwrap_or_default(),
        kind:          lip_kind_to_scip(sym.kind) as i32,
        relationships,
        ..Default::default()
    }
}

fn convert_occurrence(occ: &lip::schema::OwnedOccurrence) -> scip::Occurrence {
    let role_bits = if occ.role == Role::Definition {
        scip::SymbolRole::Definition as i32
    } else {
        scip::SymbolRole::UnspecifiedSymbolRole as i32
    };

    let range = vec![
        occ.range.start_line,
        occ.range.start_char,
        occ.range.end_line,
        occ.range.end_char,
    ];

    scip::Occurrence {
        range,
        symbol:               lip_uri_to_scip_symbol(&occ.symbol_uri),
        symbol_roles:         role_bits,
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
        SymbolKind::Class       => scip::Kind::KClass,
        SymbolKind::Interface   => scip::Kind::KInterface,
        SymbolKind::Method      => scip::Kind::KMethod,
        SymbolKind::Function    => scip::Kind::KFunction,
        SymbolKind::Field       => scip::Kind::KField,
        SymbolKind::Variable    => scip::Kind::KVariable,
        SymbolKind::Namespace   => scip::Kind::KNamespace,
        SymbolKind::Enum        => scip::Kind::KEnum,
        SymbolKind::EnumMember  => scip::Kind::KEnumMember,
        SymbolKind::Constructor => scip::Kind::KConstructor,
        SymbolKind::TypeAlias   => scip::Kind::KTypeAlias,
        SymbolKind::TypeParameter => scip::Kind::KTypeParameter,
        SymbolKind::Macro       => scip::Kind::KMacro,
        SymbolKind::Parameter   => scip::Kind::KVariable, // no SCIP equivalent
        SymbolKind::Unknown     => scip::Kind::KUnspecifiedKind,
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
        let lip_uri  = format!("lip://scip/{}", scip_sym.replace(' ', "/"));
        assert_eq!(lip_uri_to_scip_symbol(&lip_uri), scip_sym);
    }

    #[test]
    fn lip_uri_roundtrip_structured_format() {
        // New 5-field structured format from updated import.rs.
        let scip_sym = "scip-typescript npm react 18.2.0 React#Component.";
        let lip_uri  = "lip://scip-typescript/npm/react@18.2.0/React#Component.";
        assert_eq!(lip_uri_to_scip_symbol(lip_uri), scip_sym);
    }

    #[test]
    fn kind_roundtrip_for_common_kinds() {
        use lip::schema::SymbolKind;
        for (lip, scip) in [
            (SymbolKind::Class,    scip::Kind::KClass),
            (SymbolKind::Function, scip::Kind::KFunction),
            (SymbolKind::Enum,     scip::Kind::KEnum),
            (SymbolKind::Unknown,  scip::Kind::KUnspecifiedKind),
        ] {
            assert_eq!(lip_kind_to_scip(lip), scip);
        }
    }
}
