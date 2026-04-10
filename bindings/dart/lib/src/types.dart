import 'enums.dart';

/// LIP Symbol URI (spec §5).
class LipUri {
  final String _value;

  const LipUri._(this._value);

  /// Parse and validate a LIP URI string.
  factory LipUri.parse(String s) {
    if (!s.startsWith('lip://')) {
      throw FormatException('LIP URI must start with lip://', s);
    }
    if (s.contains('\x00')) {
      throw FormatException('LIP URI contains null byte', s);
    }
    if (s.contains('..')) {
      throw FormatException("LIP URI contains path traversal '..'", s);
    }
    return LipUri._(s);
  }

  /// Unchecked constructor for use when the value is already validated.
  factory LipUri.unchecked(String s) => LipUri._(s);

  String get scope {
    final rest = _value.substring('lip://'.length);
    return rest.split('/').first;
  }

  String? get package {
    final parts = _value.substring('lip://'.length).split('/');
    if (parts.length < 2) return null;
    return parts[1].split('@').first;
  }

  String? get version {
    final parts = _value.substring('lip://'.length).split('/');
    if (parts.length < 2) return null;
    final pkgVer = parts[1].split('@');
    return pkgVer.length > 1 ? pkgVer[1] : null;
  }

  String? get path {
    final parts = _value.substring('lip://'.length).split('/');
    if (parts.length < 3) return null;
    return parts.sublist(2).join('/').split('#').first;
  }

  String? get descriptor {
    final hash = _value.split('#');
    return hash.length > 1 ? hash[1] : null;
  }

  @override
  String toString() => _value;

  @override
  bool operator ==(Object other) =>
      other is LipUri && _value == other._value;

  @override
  int get hashCode => _value.hashCode;
}

// ─── Value types ─────────────────────────────────────────────────────────────

class LipRange {
  final int startLine;
  final int startChar;
  final int endLine;
  final int endChar;

  const LipRange({
    required this.startLine,
    required this.startChar,
    required this.endLine,
    required this.endChar,
  });

  factory LipRange.fromJson(Map<String, dynamic> j) => LipRange(
        startLine: j['start_line'] as int? ?? 0,
        startChar: j['start_char'] as int? ?? 0,
        endLine:   j['end_line']   as int? ?? 0,
        endChar:   j['end_char']   as int? ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'start_line': startLine,
        'start_char': startChar,
        'end_line':   endLine,
        'end_char':   endChar,
      };
}

class Relationship {
  final String targetUri;
  final bool isImplementation;
  final bool isReference;
  final bool isTypeDefinition;
  final bool isOverride;

  const Relationship({
    required this.targetUri,
    this.isImplementation  = false,
    this.isReference       = false,
    this.isTypeDefinition  = false,
    this.isOverride        = false,
  });

  factory Relationship.fromJson(Map<String, dynamic> j) => Relationship(
        targetUri:        j['target_uri'] as String,
        isImplementation: j['is_implementation'] as bool? ?? false,
        isReference:      j['is_reference']       as bool? ?? false,
        isTypeDefinition: j['is_type_definition'] as bool? ?? false,
        isOverride:       j['is_override']         as bool? ?? false,
      );

  Map<String, dynamic> toJson() => {
        'target_uri':         targetUri,
        'is_implementation':  isImplementation,
        'is_reference':       isReference,
        'is_type_definition': isTypeDefinition,
        'is_override':        isOverride,
      };
}

class SymbolInfo {
  final String uri;
  final String displayName;
  final SymbolKind kind;
  final String? documentation;
  final String? signature;
  final int confidenceScore;
  final List<Relationship> relationships;
  final double? runtimeP99Ms;
  final double? callRatePerS;
  final List<String> taintLabels;
  final int blastRadius;

  const SymbolInfo({
    required this.uri,
    required this.displayName,
    this.kind            = SymbolKind.unknown,
    this.documentation,
    this.signature,
    this.confidenceScore = 30,
    this.relationships   = const [],
    this.runtimeP99Ms,
    this.callRatePerS,
    this.taintLabels     = const [],
    this.blastRadius     = 0,
  });

  factory SymbolInfo.fromJson(Map<String, dynamic> j) => SymbolInfo(
        uri:             j['uri'] as String,
        displayName:     j['display_name'] as String,
        kind:            symbolKindFromJson(j['kind'] as String? ?? 'unknown'),
        documentation:   j['documentation'] as String?,
        signature:       j['signature'] as String?,
        confidenceScore: j['confidence_score'] as int? ?? 30,
        relationships:   (j['relationships'] as List<dynamic>?)
                             ?.map((e) => Relationship.fromJson(e as Map<String, dynamic>))
                             .toList() ?? [],
        runtimeP99Ms:    (j['runtime_p99_ms'] as num?)?.toDouble(),
        callRatePerS:    (j['call_rate_per_s'] as num?)?.toDouble(),
        taintLabels:     (j['taint_labels'] as List<dynamic>?)
                             ?.cast<String>() ?? [],
        blastRadius:     j['blast_radius'] as int? ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'uri':              uri,
        'display_name':     displayName,
        'kind':             kind.name,
        if (documentation != null) 'documentation': documentation,
        if (signature != null)     'signature':      signature,
        'confidence_score': confidenceScore,
        if (relationships.isNotEmpty)
          'relationships': relationships.map((r) => r.toJson()).toList(),
        if (runtimeP99Ms != null)  'runtime_p99_ms':  runtimeP99Ms,
        if (callRatePerS != null)  'call_rate_per_s': callRatePerS,
        if (taintLabels.isNotEmpty) 'taint_labels':   taintLabels,
        'blast_radius': blastRadius,
      };
}

class Occurrence {
  final String symbolUri;
  final LipRange range;
  final int confidenceScore;
  final Role role;
  final String? overrideDoc;

  const Occurrence({
    required this.symbolUri,
    required this.range,
    this.confidenceScore = 20,
    this.role            = Role.reference,
    this.overrideDoc,
  });

  factory Occurrence.fromJson(Map<String, dynamic> j) => Occurrence(
        symbolUri:       j['symbol_uri'] as String,
        range:           LipRange.fromJson(j['range'] as Map<String, dynamic>),
        confidenceScore: j['confidence_score'] as int? ?? 20,
        role:            roleFromJson(j['role'] as String? ?? 'reference'),
        overrideDoc:     j['override_doc'] as String?,
      );

  Map<String, dynamic> toJson() => {
        'symbol_uri':       symbolUri,
        'range':            range.toJson(),
        'confidence_score': confidenceScore,
        'role':             roleToJson(role),
        if (overrideDoc != null) 'override_doc': overrideDoc,
      };
}
