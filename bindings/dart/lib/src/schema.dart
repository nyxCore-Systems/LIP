import 'enums.dart';
import 'types.dart';

// ─── Document ────────────────────────────────────────────────────────────────

class Document {
  final String uri;
  final String contentHash;
  final String language;
  final List<Occurrence> occurrences;
  final List<SymbolInfo> symbols;
  final String merklePath;

  const Document({
    required this.uri,
    required this.contentHash,
    required this.language,
    this.occurrences = const [],
    this.symbols     = const [],
    this.merklePath  = '',
  });

  factory Document.fromJson(Map<String, dynamic> j) => Document(
        uri:          j['uri'] as String,
        contentHash:  j['content_hash'] as String,
        language:     j['language'] as String,
        occurrences:  (j['occurrences'] as List<dynamic>?)
                          ?.map((e) => Occurrence.fromJson(e as Map<String, dynamic>))
                          .toList() ?? [],
        symbols:      (j['symbols'] as List<dynamic>?)
                          ?.map((e) => SymbolInfo.fromJson(e as Map<String, dynamic>))
                          .toList() ?? [],
        merklePath:   j['merkle_path'] as String? ?? '',
      );

  Map<String, dynamic> toJson() => {
        'uri':          uri,
        'content_hash': contentHash,
        'language':     language,
        'occurrences':  occurrences.map((o) => o.toJson()).toList(),
        'symbols':      symbols.map((s) => s.toJson()).toList(),
        'merkle_path':  merklePath,
      };
}

// ─── DependencySlice ─────────────────────────────────────────────────────────

class DependencySlice {
  final String manager;
  final String packageName;
  final String version;
  final String packageHash;
  final String contentHash;
  final List<SymbolInfo> symbols;
  final String sliceUrl;
  final int builtAtMs;

  const DependencySlice({
    required this.manager,
    required this.packageName,
    required this.version,
    required this.packageHash,
    required this.contentHash,
    this.symbols   = const [],
    this.sliceUrl  = '',
    this.builtAtMs = 0,
  });

  factory DependencySlice.fromJson(Map<String, dynamic> j) => DependencySlice(
        manager:     j['manager']      as String,
        packageName: j['package_name'] as String,
        version:     j['version']      as String,
        packageHash: j['package_hash'] as String,
        contentHash: j['content_hash'] as String,
        symbols:     (j['symbols'] as List<dynamic>?)
                         ?.map((e) => SymbolInfo.fromJson(e as Map<String, dynamic>))
                         .toList() ?? [],
        sliceUrl:    j['slice_url']    as String? ?? '',
        builtAtMs:   j['built_at_ms']  as int? ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'manager':      manager,
        'package_name': packageName,
        'version':      version,
        'package_hash': packageHash,
        'content_hash': contentHash,
        'symbols':      symbols.map((s) => s.toJson()).toList(),
        'slice_url':    sliceUrl,
        'built_at_ms':  builtAtMs,
      };
}

// ─── Delta / EventStream ─────────────────────────────────────────────────────

class Delta {
  final Action action;
  final String commitHash;
  final Document? document;
  final SymbolInfo? symbol;
  final DependencySlice? slice;

  const Delta({
    required this.action,
    this.commitHash = '',
    this.document,
    this.symbol,
    this.slice,
  });

  factory Delta.fromJson(Map<String, dynamic> j) => Delta(
        action:     actionFromJson(j['action'] as String? ?? 'upsert'),
        commitHash: j['commit_hash'] as String? ?? '',
        document:   j['document'] != null
                        ? Document.fromJson(j['document'] as Map<String, dynamic>)
                        : null,
        symbol:     j['symbol'] != null
                        ? SymbolInfo.fromJson(j['symbol'] as Map<String, dynamic>)
                        : null,
        slice:      j['slice'] != null
                        ? DependencySlice.fromJson(j['slice'] as Map<String, dynamic>)
                        : null,
      );

  Map<String, dynamic> toJson() => {
        'action':      actionToJson(action),
        'commit_hash': commitHash,
        if (document != null) 'document': document!.toJson(),
        if (symbol   != null) 'symbol':   symbol!.toJson(),
        if (slice    != null) 'slice':    slice!.toJson(),
      };
}

class EventStream {
  final List<Delta> deltas;
  final int schemaVersion;
  final String emitterId;
  final int timestampMs;

  const EventStream({
    required this.deltas,
    this.schemaVersion = 1,
    this.emitterId     = '',
    this.timestampMs   = 0,
  });

  factory EventStream.fromJson(Map<String, dynamic> j) => EventStream(
        deltas:        (j['deltas'] as List<dynamic>)
                           .map((e) => Delta.fromJson(e as Map<String, dynamic>))
                           .toList(),
        schemaVersion: j['schema_version'] as int? ?? 1,
        emitterId:     j['emitter_id']     as String? ?? '',
        timestampMs:   j['timestamp_ms']   as int? ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'deltas':         deltas.map((d) => d.toJson()).toList(),
        'schema_version': schemaVersion,
        'emitter_id':     emitterId,
        'timestamp_ms':   timestampMs,
      };
}

// ─── Manifest ────────────────────────────────────────────────────────────────

class ManifestRequest {
  final String repoRoot;
  final String merkleRoot;
  final String depTreeHash;
  final String lipVersion;

  const ManifestRequest({
    required this.repoRoot,
    this.merkleRoot  = '',
    this.depTreeHash = '',
    this.lipVersion  = '0.1.0',
  });

  Map<String, dynamic> toJson() => {
        'type':          'manifest',
        'repo_root':     repoRoot,
        'merkle_root':   merkleRoot,
        'dep_tree_hash': depTreeHash,
        'lip_version':   lipVersion,
      };
}

class ManifestResponse {
  final String cachedMerkleRoot;
  final List<String> missingSlices;
  final IndexingState indexingState;

  const ManifestResponse({
    this.cachedMerkleRoot = '',
    this.missingSlices    = const [],
    this.indexingState    = IndexingState.cold,
  });

  factory ManifestResponse.fromJson(Map<String, dynamic> j) => ManifestResponse(
        cachedMerkleRoot: j['cached_merkle_root'] as String? ?? '',
        missingSlices:    (j['missing_slices'] as List<dynamic>?)?.cast<String>() ?? [],
        indexingState:    indexingStateFromJson(j['indexing_state'] as String? ?? 'cold'),
      );
}

// ─── BlastRadiusResult ───────────────────────────────────────────────────────

class BlastRadiusResult {
  final String symbolUri;
  final int directDependents;
  final int transitiveDependents;
  final List<String> affectedFiles;

  const BlastRadiusResult({
    required this.symbolUri,
    this.directDependents     = 0,
    this.transitiveDependents = 0,
    this.affectedFiles        = const [],
  });

  factory BlastRadiusResult.fromJson(Map<String, dynamic> j) => BlastRadiusResult(
        symbolUri:            j['symbol_uri']             as String? ?? '',
        directDependents:     j['direct_dependents']      as int? ?? 0,
        transitiveDependents: j['transitive_dependents']  as int? ?? 0,
        affectedFiles:        (j['affected_files'] as List<dynamic>?)?.cast<String>() ?? [],
      );
}
