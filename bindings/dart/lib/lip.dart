/// LIP — Linked Incremental Protocol: Dart bindings.
///
/// First-class Dart/Flutter support (spec §5.2 — `pub` scope).
///
/// ```dart
/// import 'package:lip/lip.dart';
///
/// final client = LipClient(socketPath: '/tmp/lip-daemon.sock');
/// await client.connect();
/// final sym = await client.definition('file:///lib/main.dart', 10, 5);
/// ```
library lip;

export 'src/client.dart' show LipClient;
export 'src/enums.dart';
export 'src/schema.dart';
export 'src/types.dart';
