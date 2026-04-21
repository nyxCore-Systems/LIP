use tower_lsp::lsp_types::{
    Hover, HoverContents, Location, MarkupContent, MarkupKind, Position, Range, SymbolInformation,
    SymbolKind as LspSymbolKind, Url,
};

use crate::schema::{OwnedOccurrence, OwnedRange, OwnedSymbolInfo, SymbolKind};

// ─── Range / Position ────────────────────────────────────────────────────────

pub fn lip_range_to_lsp(r: &OwnedRange) -> Range {
    Range {
        start: Position::new(r.start_line.max(0) as u32, r.start_char.max(0) as u32),
        end: Position::new(r.end_line.max(0) as u32, r.end_char.max(0) as u32),
    }
}

pub fn lsp_position_to_lip(pos: &Position) -> OwnedRange {
    OwnedRange {
        start_line: pos.line as i32,
        start_char: pos.character as i32,
        end_line: pos.line as i32,
        end_char: pos.character as i32,
    }
}

// ─── SymbolKind ──────────────────────────────────────────────────────────────

pub fn lip_kind_to_lsp(kind: SymbolKind) -> LspSymbolKind {
    match kind {
        SymbolKind::Namespace => LspSymbolKind::NAMESPACE,
        SymbolKind::Class => LspSymbolKind::CLASS,
        SymbolKind::Interface => LspSymbolKind::INTERFACE,
        SymbolKind::Method => LspSymbolKind::METHOD,
        SymbolKind::Field => LspSymbolKind::FIELD,
        SymbolKind::Variable => LspSymbolKind::VARIABLE,
        SymbolKind::Function => LspSymbolKind::FUNCTION,
        SymbolKind::TypeParameter => LspSymbolKind::TYPE_PARAMETER,
        SymbolKind::Parameter => LspSymbolKind::VARIABLE,
        SymbolKind::Macro => LspSymbolKind::FUNCTION,
        SymbolKind::Enum => LspSymbolKind::ENUM,
        SymbolKind::EnumMember => LspSymbolKind::ENUM_MEMBER,
        SymbolKind::Constructor => LspSymbolKind::CONSTRUCTOR,
        SymbolKind::TypeAlias => LspSymbolKind::TYPE_PARAMETER,
        SymbolKind::Unknown => LspSymbolKind::NULL,
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
            kind: MarkupKind::Markdown,
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
        name: sym.display_name.clone(),
        kind: lip_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        location,
        container_name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::OwnedSymbolInfo;

    fn make_sym(name: &str, kind: SymbolKind, confidence: u8) -> OwnedSymbolInfo {
        OwnedSymbolInfo {
            uri: format!("lip://local/t#{name}"),
            display_name: name.to_owned(),
            kind,
            confidence_score: confidence,
            ..OwnedSymbolInfo::new("", "")
        }
    }

    // ── range / position ──────────────────────────────────────────────────────

    #[test]
    fn range_round_trip() {
        let lip = OwnedRange {
            start_line: 5,
            start_char: 10,
            end_line: 5,
            end_char: 20,
        };
        let lsp = lip_range_to_lsp(&lip);
        assert_eq!(lsp.start.line, 5);
        assert_eq!(lsp.start.character, 10);
        assert_eq!(lsp.end.line, 5);
        assert_eq!(lsp.end.character, 20);
    }

    #[test]
    fn negative_range_clamped_to_zero() {
        let lip = OwnedRange {
            start_line: -1,
            start_char: -3,
            end_line: 0,
            end_char: 5,
        };
        let lsp = lip_range_to_lsp(&lip);
        assert_eq!(lsp.start.line, 0);
        assert_eq!(lsp.start.character, 0);
    }

    #[test]
    fn position_to_lip_is_zero_length_range() {
        let pos = Position::new(7, 3);
        let r = lsp_position_to_lip(&pos);
        assert_eq!(r.start_line, 7);
        assert_eq!(r.start_char, 3);
        assert_eq!(r.end_line, 7);
        assert_eq!(r.end_char, 3);
    }

    // ── SymbolKind mapping ────────────────────────────────────────────────────

    #[test]
    fn kind_mapping_covers_all_variants() {
        // Every variant must map to something other than NULL (which signals Unknown).
        let non_unknown = [
            SymbolKind::Namespace,
            SymbolKind::Class,
            SymbolKind::Interface,
            SymbolKind::Method,
            SymbolKind::Field,
            SymbolKind::Variable,
            SymbolKind::Function,
            SymbolKind::TypeParameter,
            SymbolKind::Enum,
            SymbolKind::EnumMember,
            SymbolKind::Constructor,
            SymbolKind::TypeAlias,
            SymbolKind::Macro,
        ];
        for kind in non_unknown {
            assert_ne!(
                lip_kind_to_lsp(kind),
                LspSymbolKind::NULL,
                "{kind:?} should not map to NULL"
            );
        }
        // Unknown explicitly maps to NULL
        assert_eq!(lip_kind_to_lsp(SymbolKind::Unknown), LspSymbolKind::NULL);
    }

    // ── hover ─────────────────────────────────────────────────────────────────

    #[test]
    fn hover_includes_signature_fenced() {
        let mut sym = make_sym("foo", SymbolKind::Function, 90);
        sym.signature = Some("pub fn foo(x: i32) -> bool".to_owned());
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        assert!(
            mc.value.contains("```"),
            "signature should be in a code fence"
        );
        assert!(mc.value.contains("pub fn foo"));
    }

    #[test]
    fn hover_tier1_adds_verifying_note() {
        let sym = make_sym("pending", SymbolKind::Function, 30);
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        assert!(
            mc.value.contains("LIP verifying"),
            "Tier 1 hover should include the verifying indicator"
        );
    }

    #[test]
    fn hover_tier2_omits_verifying_note() {
        let sym = make_sym("verified", SymbolKind::Function, 90);
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        assert!(
            !mc.value.contains("LIP verifying"),
            "Tier 2 hover must not show the verifying indicator"
        );
    }

    #[test]
    fn hover_taint_labels_included() {
        let mut sym = make_sym("sink", SymbolKind::Function, 90);
        sym.taint_labels = vec!["PII".to_owned(), "EXTERNAL_INPUT".to_owned()];
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        assert!(mc.value.contains("PII"));
        assert!(mc.value.contains("EXTERNAL_INPUT"));
    }

    #[test]
    fn hover_includes_documentation() {
        let mut sym = make_sym("bar", SymbolKind::Function, 90);
        sym.documentation = Some("Does the bar thing.".to_owned());
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        assert!(mc.value.contains("Does the bar thing."));
    }

    #[test]
    fn hover_empty_symbol_produces_empty_string() {
        let sym = make_sym("empty", SymbolKind::Unknown, 90);
        let hover = symbol_to_hover(&sym);
        let HoverContents::Markup(mc) = hover.contents else {
            panic!()
        };
        // Unknown confidence=90: no verifying note, no sig, no doc → empty
        assert!(mc.value.is_empty());
    }

    // ── location helpers ──────────────────────────────────────────────────────

    #[test]
    fn location_from_valid_file_uri() {
        let range = OwnedRange {
            start_line: 1,
            start_char: 0,
            end_line: 1,
            end_char: 10,
        };
        let loc = location_from_uri_range("file:///src/main.rs", &range);
        assert!(loc.is_some(), "valid file URI should produce a Location");
        let loc = loc.unwrap();
        assert_eq!(loc.range.start.line, 1);
        assert_eq!(loc.range.end.character, 10);
    }

    #[test]
    fn location_from_invalid_uri_returns_none() {
        let range = OwnedRange {
            start_line: 0,
            start_char: 0,
            end_line: 0,
            end_char: 0,
        };
        let loc = location_from_uri_range("not a valid uri !!!", &range);
        assert!(loc.is_none());
    }

    // ── symbol_to_lsp_symbol_info ─────────────────────────────────────────────

    #[test]
    fn symbol_info_name_and_kind() {
        let sym = make_sym("MyService", SymbolKind::Class, 90);
        let info = symbol_to_lsp_symbol_info(&sym, "file:///src/svc.rs").unwrap();
        assert_eq!(info.name, "MyService");
        assert_eq!(info.kind, LspSymbolKind::CLASS);
    }

    #[test]
    fn symbol_info_invalid_uri_returns_none() {
        let sym = make_sym("x", SymbolKind::Variable, 30);
        assert!(symbol_to_lsp_symbol_info(&sym, "::invalid::").is_none());
    }

    #[test]
    fn occurrences_to_locations_filters_invalid() {
        let valid = OwnedOccurrence {
            symbol_uri: "lip://local/t#x".to_owned(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 1,
            },
            confidence_score: 20,
            role: crate::schema::Role::Reference,
            override_doc: None,
            kind: crate::schema::ReferenceKind::Unknown,
            is_test: false,
        };
        let locs = occurrences_to_locations(&[valid], "file:///src/a.rs");
        assert_eq!(locs.len(), 1);

        let locs_bad = occurrences_to_locations(&[], "::bad::");
        assert!(locs_bad.is_empty());
    }
}
