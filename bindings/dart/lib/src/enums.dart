/// LIP enumeration types — mirrors lip.fbs (spec §4.1).

enum Action { upsert, delete }

enum Role {
  definition,
  reference,
  implementation,
  typeBinding,
  readAccess,
  writeAccess,
}

enum SymbolKind {
  unknown,
  namespace,
  klass,   // 'class' is a reserved keyword in Dart
  interface,
  method,
  field,
  variable,
  function,
  typeParameter,
  parameter,
  macro,
  enumKind,   // 'enum' is reserved
  enumMember,
  constructor,
  typeAlias,
}

enum IndexingState { cold, warmPartial, warmFull }

// ─── JSON round-trip helpers ──────────────────────────────────────────────────

Action actionFromJson(String s) =>
    s == 'delete' ? Action.delete : Action.upsert;

String actionToJson(Action a) => a == Action.delete ? 'delete' : 'upsert';

Role roleFromJson(String s) {
  return const {
    'definition':     Role.definition,
    'reference':      Role.reference,
    'implementation': Role.implementation,
    'type_binding':   Role.typeBinding,
    'read_access':    Role.readAccess,
    'write_access':   Role.writeAccess,
  }[s] ?? Role.reference;
}

String roleToJson(Role r) {
  return const {
    Role.definition:     'definition',
    Role.reference:      'reference',
    Role.implementation: 'implementation',
    Role.typeBinding:    'type_binding',
    Role.readAccess:     'read_access',
    Role.writeAccess:    'write_access',
  }[r]!;
}

SymbolKind symbolKindFromJson(String s) {
  return const {
    'unknown':        SymbolKind.unknown,
    'namespace':      SymbolKind.namespace,
    'class':          SymbolKind.klass,
    'interface':      SymbolKind.interface,
    'method':         SymbolKind.method,
    'field':          SymbolKind.field,
    'variable':       SymbolKind.variable,
    'function':       SymbolKind.function,
    'type_parameter': SymbolKind.typeParameter,
    'parameter':      SymbolKind.parameter,
    'macro':          SymbolKind.macro,
    'enum':           SymbolKind.enumKind,
    'enum_member':    SymbolKind.enumMember,
    'constructor':    SymbolKind.constructor,
    'type_alias':     SymbolKind.typeAlias,
  }[s] ?? SymbolKind.unknown;
}

IndexingState indexingStateFromJson(String s) {
  return const {
    'cold':         IndexingState.cold,
    'warm_partial': IndexingState.warmPartial,
    'warm_full':    IndexingState.warmFull,
  }[s] ?? IndexingState.cold;
}
