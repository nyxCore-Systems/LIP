use tower_lsp::lsp_types::{
    Hover, HoverContents, Location, MarkupContent, MarkupKind, Position, Range,
    SymbolInformation, SymbolKind as LspSymbolKind, Url,
};

use crate::schema::{OwnedOccurrence, OwnedRange, OwnedSymbolInfo, SymbolKind};

// ─── Range / Position ────────────────────────────────────────────────────────

pub fn lip_range_to_lsp(r: &OwnedRange) -> Range {
    Range {
        start: Position::new(r.start_line.max(0) as u32, r.start_char.max(0) as u32),
        end:   Position::new(r.end_line.max(0) as u32, r.end_char.max(0) as u32),
    }
}

pub fn lsp_position_to_lip(pos: &Position) -> OwnedRange {
    OwnedRange {
        start_line: pos.line as i32,
        start_char: pos.character as i32,
        end_line:   pos.line as i32,
        end_char:   pos.character as i32,
    }
}

// ─── SymbolKind ──────────────────────────────────────────────────────────────

pub fn lip_kind_to_lsp(kind: SymbolKind) -> LspSymbolKind {
    match kind {
        SymbolKind::Namespace     => LspSymbolKind::NAMESPACE,
        SymbolKind::Class         => LspSymbolKind::CLASS,
        SymbolKind::Interface     => LspSymbolKind::INTERFACE,
        SymbolKind::Method        => LspSymbolKind::METHOD,
        SymbolKind::Field         => LspSymbolKind::FIELD,
        SymbolKind::Variable      => LspSymbolKind::VARIABLE,
        SymbolKind::Function      => LspSymbolKind::FUNCTION,
        SymbolKind::TypeParameter => LspSymbolKind::TYPE_PARAMETER,
        SymbolKind::Parameter     => LspSymbolKind::VARIABLE,
        SymbolKind::Macro         => LspSymbolKind::FUNCTION,
        SymbolKind::Enum          => LspSymbolKind::ENUM,
        SymbolKind::EnumMember    => LspSymbolKind::ENUM_MEMBER,
        SymbolKind::Constructor   => LspSymbolKind::CONSTRUCTOR,
        SymbolKind::TypeAlias     => LspSymbolKind::TYPE_PARAMETER,
        SymbolKind::Unknown       => LspSymbolKind::NULL,
    }
}

// ─── Location ────────────────────────────────────────────────────────────────

/// Build an LSP `Location` from a file URI and a LIP range.
pub fn location_from_uri_range(file_uri: &str, range: &OwnedRange) -> Option<Location> {
    let url: Url = file_uri.parse().ok()?;
    Some(Location::new(url, lip_range_to_lsp(range)))
}

/// Build a zero-range location from a symbol and a file URI.
///
/// Used as a fallback for workspace/symbol results where no occurrence-level
/// range is available yet. Callers that have a real range should use
/// [`location_from_uri_range`] directly.
pub fn symbol_to_location(_sym: &OwnedSymbolInfo, file_uri: &str) -> Option<Location> {
    let url: Url = file_uri.parse().ok()?;
    Some(Location::new(url, Range::default()))
}

pub fn occurrence_to_location(occ: &OwnedOccurrence, file_uri: &str) -> Option<Location> {
    let url: Url = file_uri.parse().ok()?;
    Some(Location::new(url, lip_range_to_lsp(&occ.range)))
}

pub fn occurrences_to_locations(occs: &[OwnedOccurrence], file_uri: &str) -> Vec<Location> {
    occs.iter()
        .filter_map(|o| occurrence_to_location(o, file_uri))
        .collect()
}

// ─── Hover ───────────────────────────────────────────────────────────────────

pub fn symbol_to_hover(sym: &OwnedSymbolInfo) -> Hover {
    let mut parts = vec![];
    if let Some(sig) = &sym.signature {
        parts.push(format!("```\n{sig}\n```"));
    }
    if let Some(doc) = &sym.documentation {
        parts.push(doc.clone());
    }
    if sym.confidence_score <= 50 {
        parts.push("_LIP verifying…_".to_owned());
    }
    if !sym.taint_labels.is_empty() {
        parts.push(format!("**Taint:** {}", sym.taint_labels.join(", ")));
    }
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind:  MarkupKind::Markdown,
            value: parts.join("\n\n"),
        }),
        range: None,
    }
}

// ─── SymbolInformation ───────────────────────────────────────────────────────

pub fn symbol_to_lsp_symbol_info(
    sym: &OwnedSymbolInfo,
    file_uri: &str,
) -> Option<SymbolInformation> {
    let location = symbol_to_location(sym, file_uri)?;
    #[allow(deprecated)]
    Some(SymbolInformation {
        name:           sym.display_name.clone(),
        kind:           lip_kind_to_lsp(sym.kind),
        tags:           None,
        deprecated:     None,
        location,
        container_name: None,
    })
}
