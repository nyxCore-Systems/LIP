import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'enums.dart';
import 'schema.dart';
import 'types.dart';

/// LIP daemon client for Dart/Flutter projects (spec §7.1).
///
/// Connects to the LIP daemon over a Unix domain socket and exchanges
/// JSON-framed messages (4-byte big-endian length prefix + UTF-8 JSON body).
///
/// v0.1 uses JSON framing; v0.2 upgrades to FlatBuffers + mmap (spec §7.1).
class LipClient {
  final String socketPath;

  Socket? _socket;
  bool _connected = false;

  /// Buffered incoming bytes — used to reassemble length-prefixed frames.
  final List<int> _buffer = [];
  int? _pendingLength;

  /// In-flight response completers, keyed by request ID.
  final Map<int, Completer<Map<String, dynamic>>> _pending = {};
  int _nextId = 0;

  LipClient({required this.socketPath});

  // ─── Connection ───────────────────────────────────────────────────────────

  Future<void> connect() async {
    final addr = InternetAddress(socketPath, type: InternetAddressType.unix);
    _socket = await Socket.connect(addr, 0);
    _connected = true;
    _socket!.listen(
      _onData,
      onError: (_) => _connected = false,
      onDone:  ()  => _connected = false,
    );
  }

  Future<void> disconnect() async {
    await _socket?.close();
    _connected = false;
  }

  bool get isConnected => _connected;

  // ─── Handshake ────────────────────────────────────────────────────────────

  Future<ManifestResponse> handshake(ManifestRequest req) async {
    final resp = await _rpc(req.toJson());
    final inner = resp['manifest_response'] as Map<String, dynamic>? ?? resp;
    return ManifestResponse.fromJson(inner);
  }

  // ─── Queries ──────────────────────────────────────────────────────────────

  Future<SymbolInfo?> definition(String uri, int line, int col) async {
    final resp = await _rpc({
      'type': 'query_definition',
      'uri':  uri,
      'line': line,
      'col':  col,
    });
    final sym = resp['symbol'];
    return sym == null
        ? null
        : SymbolInfo.fromJson(sym as Map<String, dynamic>);
  }

  Future<List<Occurrence>> references(
    String symbolUri, {
    int limit = 50,
  }) async {
    final resp = await _rpc({
      'type':       'query_references',
      'symbol_uri': symbolUri,
      'limit':      limit,
    });
    final list = resp['occurrences'] as List<dynamic>? ?? [];
    return list
        .map((e) => Occurrence.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  Future<SymbolInfo?> hover(String uri, int line, int col) async {
    final resp = await _rpc({
      'type': 'query_hover',
      'uri':  uri,
      'line': line,
      'col':  col,
    });
    final sym = resp['symbol'];
    return sym == null
        ? null
        : SymbolInfo.fromJson(sym as Map<String, dynamic>);
  }

  Future<BlastRadiusResult> blastRadius(String symbolUri) async {
    final resp = await _rpc({
      'type':       'query_blast_radius',
      'symbol_uri': symbolUri,
    });
    return BlastRadiusResult.fromJson(resp);
  }

  Future<List<SymbolInfo>> workspaceSymbols(
    String query, {
    int limit = 100,
  }) async {
    final resp = await _rpc({
      'type':  'query_workspace_symbols',
      'query': query,
      'limit': limit,
    });
    final list = resp['symbols'] as List<dynamic>? ?? [];
    return list
        .map((e) => SymbolInfo.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  // ─── File notifications ───────────────────────────────────────────────────

  Future<void> notifyFileChanged(
    String uri,
    String language, {
    Action action = Action.upsert,
  }) async {
    await _send({
      'type':   'delta',
      'action': actionToJson(action),
      'document': {
        'uri':          uri,
        'content_hash': '',
        'language':     language,
        'occurrences':  <dynamic>[],
        'symbols':      <dynamic>[],
        'merkle_path':  uri,
      },
    });
  }

  // ─── Internal ─────────────────────────────────────────────────────────────

  Future<Map<String, dynamic>> _rpc(Map<String, dynamic> msg) async {
    if (!_connected) await connect();
    final id = _nextId++;
    final completer = Completer<Map<String, dynamic>>();
    _pending[id] = completer;
    msg['_id'] = id;
    await _send(msg);
    return completer.future.timeout(const Duration(seconds: 10));
  }

  Future<void> _send(Map<String, dynamic> msg) async {
    final body   = utf8.encode(jsonEncode(msg));
    final header = Uint8List(4)
      ..buffer.asByteData().setUint32(0, body.length, Endian.big);
    _socket!
      ..add(header)
      ..add(body);
    await _socket!.flush();
  }

  void _onData(List<int> data) {
    _buffer.addAll(data);

    while (true) {
      if (_pendingLength == null) {
        if (_buffer.length < 4) return;
        _pendingLength = Uint8List.fromList(_buffer.sublist(0, 4))
            .buffer
            .asByteData()
            .getUint32(0, Endian.big);
        _buffer.removeRange(0, 4);
      }

      final needed = _pendingLength!;
      if (_buffer.length < needed) return;

      final body = _buffer.sublist(0, needed);
      _buffer.removeRange(0, needed);
      _pendingLength = null;

      try {
        final resp = jsonDecode(utf8.decode(body)) as Map<String, dynamic>;
        final id   = resp['_id'] as int?;
        if (id != null) {
          _pending.remove(id)?.complete(resp);
        }
      } catch (_) {}
    }
  }
}
