# LIP — Linked Incremental Protocol

**Design Document & Specification v0.3** · Draft · Apache 2.0  
Authors: Lisa Küpper (TasteHub GmbH) · April 2026  
Repository: https://github.com/lip-protocol/lip

---

## Abstract

LIP (Linked Incremental Protocol) is a language-agnostic, open-source protocol for streaming, incremental, AI-native code intelligence.

**The single most important design constraint:**

> LIP MUST work on any repository, immediately, with zero external tools installed.  
> No compiler. No build system. No SCIP indexer. No language server.  
> A developer clones a repo and has working code intelligence within 30 seconds.

SCIP requires 30–90+ minutes on large repositories. LSP requires a running language server. LIP starts with Tree-sitter (built-in, ~5ms per file) and progressively upgrades to compiler-accurate results in the background — but those upgrades are always *optional enhancements*, never *prerequisites*.

### What changed in v0.3

All four changes in this version are targeted technical additions based on documented weaknesses in LSP and SCIP. No architectural rewrites.

| Addition | Section | Rationale |
|---|---|---|
| UTF-8 byte offsets on `Range` | §4.1 | LSP UTF-16 requires O(n) decoding; UTF-8 is O(1) |
| Delta acknowledgment (`DeltaAck`) | §6.5 | Eliminate fire-and-forget drift (Dart Analysis Protocol model) |
| Wire framing clarified | §7.1 | Explicit 4-byte length prefix, not HTTP-style `Content-Length` |
| `GraphEdge` + `EdgeKind` (CPG) | §4.1, §8.5 | Enable taint tracking and control-flow analysis |
| Annotation Overlay Layer | §4.1, §9.4 | Solve the Year-Zero Problem for AI agents |

---

## Table of Contents

1. [Core Design Constraints](#1-core-design-constraints)
2. [Comparison Matrix](#2-comparison-matrix)
3. [Architecture Overview](#3-architecture-overview)
4. [Wire Format — FlatBuffers Schema](#4-wire-format--flatbuffers-schema)
5. [Symbol URI Scheme](#5-symbol-uri-scheme)
6. [Protocol Lifecycle](#6-protocol-lifecycle)
7. [Transport & IPC](#7-transport--ipc)
8. [Intelligence Extensions](#8-intelligence-extensions)
9. [AI & Agent Integration](#9-ai--agent-integration)
10. [Compatibility Layer](#10-compatibility-layer)
11. [Security Considerations](#11-security-considerations)
12. [Governance & RFC Process](#12-governance--rfc-process)
13. [Roadmap](#13-roadmap)
14. [Appendix A — Why not UTF-16](#appendix-a--why-not-utf-16)
15. [Appendix B — Why not HTTP framing](#appendix-b--why-not-http-framing)
16. [Appendix C — Why not Protobuf](#appendix-c--why-not-protobuf)
17. [Appendix D — Prior Art & References](#appendix-d--prior-art--references)

---

## 1. Core Design Constraints

These constraints are immutable. Every design decision in LIP must satisfy all of them.

### 1.1 Zero-dependency cold start

LIP MUST provide useful code intelligence on any repository within 30 seconds of first run, with NO external dependencies:

- No compiler (`rustc`, `tsc`, `dart`, `javac`, ...)
- No build system (`cargo`, `npm install`, `pub get`, `gradle`, ...)
- No language server (`rust-analyzer`, `typescript-language-server`, `dartls`, ...)
- No SCIP indexer (`scip-typescript`, `scip-rust`, ...)
- No network access required for local repos

The only built-in analysis engine is **Tree-sitter**, compiled into the LIP daemon binary. Tree-sitter parses any supported language in < 10ms per file, with no external process and no configuration. This is **Tier 1** intelligence. It is always available.

### 1.2 Progressive enhancement, never blocking

Tier 2 (compiler-accurate) and Tier 3 (dependency slices) are background upgrades. No query may block waiting for them. Every query returns a Tier 1 result within 5ms of the daemon being in steady state.

### 1.3 Compiler optionality

If a language server is on PATH, LIP uses it for Tier 2 verification. If not, LIP works correctly without it. The protocol and schema are identical in both cases.

### 1.4 SCIP compatibility without SCIP dependency

LIP can import `.scip` files to bootstrap its cache. It can export `.scip` files for downstream tools. It never requires an SCIP indexer to be installed or run.

### 1.5 Language-agnostic core schema

The core FlatBuffers tables contain no language-specific fields. Language-specific concepts (Flutter widget trees, Dart mixins) use the generic `edge_label` field on `Relationship` and optional `SymbolKind` extensions.

### 1.6 Open governance

LIP is governed by an open RFC process. No single organization controls the spec. See §12.

---

## 2. Comparison Matrix

| Property | LSP 3.17 | SCIP | **LIP v0.3** |
|---|---|---|---|
| Wire format | JSON-RPC 2.0 | Protobuf 3 | **FlatBuffers (zero-copy)** |
| Framing | HTTP `Content-Length` | n/a (file) | **4-byte length prefix** |
| Character offsets | UTF-16 code units | UTF-16 code units | **UTF-8 byte offsets** |
| Scope | Open files only | Full repo snapshot | **Full repo + deps, streaming** |
| Indexing model | Volatile (in-memory) | Batch artifact (.scip) | **Persistent lazy query graph** |
| Change handling | Full re-parse per file | Full re-index O(N) | **Blast radius O(Δ + depth)** |
| Cold start (no tools) | Needs lang server | Needs indexer + build | **< 30s, zero tools required** |
| Compiler required | Yes | Yes | **No (optional Tier 2)** |
| Build system required | Often | Yes | **No** |
| SCIP indexer required | No | Yes | **No** |
| Dependency indexing | ✗ | ⚠ re-indexes every run | **✓ federated CAS slices** |
| Incremental sync | ⚠ fire-and-forget, drift | ✗ none | **✓ acknowledged deltas + Merkle** |
| Confidence levels | ✗ | Single (slow) | **✓ three tiers, progressive** |
| Data / control flow graph | ✗ | ✗ | **✓ CPG edges (Tier 2+)** |
| Taint tracking | ✗ | ✗ | **✓ CPG DataFlows traversal** |
| Persistent annotations | ✗ | ✗ | **✓ annotation overlay layer** |
| AI / agent ready | ⚠ limited | ⚠ read-only batch | **✓ streaming graph queries** |
| Cross-repo | ✗ | ✓ | **✓** |
| Team cache sharing | ✗ | ✗ | **✓ CAS registry** |
| Runtime telemetry | ✗ | ✗ | **✓ opt-in overlay** |
| Cold start latency | Seconds–minutes | 30–90 min | **< 30 s (shallow) + background** |

---

## 3. Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│  Consumers                                                       │
│  IDE plugin · AI agent · MCP client · CLI tool                  │
└────────────────────┬─────────────────────────────────────────────┘
                     │  Unix socket + mmap  (FlatBuffers, zero-copy)
                     │  HTTP/SSE            (MCP clients)
┌────────────────────▼─────────────────────────────────────────────┐
│  LIP Daemon                                                      │
│                                                                  │
│  ┌──────────────┐  ┌───────────────┐  ┌──────────────────────┐  │
│  │ Tier 1       │  │ Tier 2        │  │ Annotation Store     │  │
│  │ Tree-sitter  │  │ (optional)    │  │ symbol URI → notes   │  │
│  │ <10ms/file   │  │ lang server   │  │ persistent + shared  │  │
│  │ always on    │  │ background    │  └──────────────────────┘  │
│  └──────┬───────┘  └──────┬────────┘                           │
│         └────────┬─────────┘                                    │
│  ┌───────────────▼──────────────────────────────────────────┐   │
│  │  Salsa Query Graph (lazy, incremental, persisted)         │   │
│  │  · Symbol table    · Reverse dep graph (blast radius)     │   │
│  │  · GraphEdge/CPG   · Merkle tree (sync integrity)        │   │
│  └──────────────────────────┬───────────────────────────────┘   │
│                              │                                    │
│  ┌───────────────────────────▼───────────────────────────────┐   │
│  │  Dep Slice Registry Client                                 │   │
│  │  npm · cargo · pub · pip · go — pull once, cache forever   │   │
│  └────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### Indexing tiers

#### Tier 1 — Syntactic (always available, zero dependencies)

| Property | Value |
|---|---|
| Engine | Tree-sitter (compiled into daemon binary) |
| Per-file latency | < 10 ms |
| Full cold-start | < 30 s for 500k-file repo |
| Requires | Nothing |
| Confidence score | 1–40 |

Provides: go to definition (within file + import heuristic), find references, workspace symbol search, hover (kind + signature from AST), document outline, folding ranges, syntactic call graph, import/export surface, blast radius approximation.

Does **not** provide: cross-file type resolution, overload resolution, compiler diagnostics.

Built-in grammars: TypeScript, JavaScript, TSX, JSX, Rust, Python, Go, Dart, Java, Kotlin, C, C++, C#, Swift, Ruby, PHP, Scala, Elixir, Haskell, Lua, Zig, TOML, JSON, YAML, Markdown, and any language with a published Tree-sitter grammar.

#### Tier 2 — Semantic (optional, background)

| Property | Value |
|---|---|
| Engine | Language-specific analyzer (rust-analyzer, dartls, tsserver, ...) |
| Latency | 200ms–5min after Tier 1 |
| Requires | Language toolchain on PATH (optional) |
| Confidence score | 41–85 |
| UI signal | Dotted underline while upgrading |

Auto-detects available language servers. If none found, Tier 1 remains active — no error, no warning. Covers only the blast radius of changed files. Full repo is never re-analyzed.

#### Tier 3 — Global Anchor (optional, federated)

| Property | Value |
|---|---|
| Engine | Federated CAS registry pull |
| Latency | Instant after first download |
| Requires | Network (first pull only; works offline thereafter) |
| Confidence score | 86–100 |

External package symbols downloaded as pre-built, hash-verified `DependencySlice` blobs. Once cached, permanent until package version changes.

---

## 4. Wire Format — FlatBuffers Schema

LIP uses FlatBuffers for all payloads. Key property: zero-copy reads directly from an mmap'd buffer — the IDE reads a field from a 10MB symbol graph via one pointer offset without deserializing the rest. See Appendix C.

```flatbuffers
// lip.fbs — LIP FlatBuffers Schema v0.3
// License: Apache 2.0
//
// Changelog vs v0.1:
//   - Range: UTF-8 byte offsets specified explicitly (was implicit)
//   - Document: edges field added (GraphEdge, optional CPG)
//   - GraphEdge + EdgeKind: new, Code Property Graph support
//   - AnnotationEntry: new, Annotation Overlay Layer
//   - DeltaAck: new, acknowledgment for client-sent deltas
//   - SymbolKind: Extension, Mixin, Widget added (Dart/Flutter)

namespace lip;

// ─── Streaming push: intelligence updates ─────────────────────────────────────

table EventStream {
  deltas:         [Delta];
  schema_version: uint16 = 3;
  emitter_id:     string;
  timestamp_ms:   int64;
}

table Delta {
  action:      Action;
  commit_hash: string;
  document:    Document;
  symbol:      SymbolInfo;
  slice:       DependencySlice;
  annotation:  AnnotationEntry;  // new in v0.3
  // Sequence number for acknowledgment protocol
  seq:         uint64 = 0;
}

enum Action : byte { Upsert = 0, Delete = 1 }

// ─── Delta acknowledgment (new in v0.3) ───────────────────────────────────────
//
// Every Delta sent by the client receives a DeltaAck response.
// This eliminates the fire-and-forget drift problem in LSP.

table DeltaAck {
  seq:      uint64;    // echoes Delta.seq
  accepted: bool;
  error:    string;    // non-empty if accepted = false
}

// ─── Document ─────────────────────────────────────────────────────────────────

table Document {
  uri:          string;     // file:///absolute/path
  content_hash: string;     // SHA-256 of raw source bytes (UTF-8)
  language:     string;     // "dart", "rust", "typescript", ...
  occurrences:  [Occurrence];
  symbols:      [SymbolInfo];
  merkle_path:  string;
  // CPG edges originating from this file.
  // Populated by Tier 2 verification; absent (null) in Tier 1 documents.
  edges:        [GraphEdge];
}

// ─── Range ────────────────────────────────────────────────────────────────────
//
// All character offsets are UTF-8 byte offsets from the start of the line.
// This is a deliberate departure from LSP's UTF-16 code unit counting.
//
// Rationale: UTF-16 offsets require O(n) decoding to produce a byte pointer.
// UTF-8 byte offsets map directly to a pointer offset into the source buffer,
// making range slicing O(1). Every language runtime that LIP targets stores
// source files as UTF-8 bytes on disk; UTF-16 is not the natural unit for any
// of them. See Appendix A.

table Range {
  start_line: int32;   // 0-based
  start_char: int32;   // UTF-8 byte offset from start of line (inclusive)
  end_line:   int32;   // 0-based (inclusive)
  end_char:   int32;   // UTF-8 byte offset from start of line (exclusive)
}

// ─── Occurrence ───────────────────────────────────────────────────────────────

table Occurrence {
  symbol_uri:       string;
  range:            Range;
  confidence_score: uint8;    // 1-40=Tier1, 41-85=Tier2, 86-100=Tier3
  role:             Role;
  override_doc:     string;
}

enum Role : byte {
  Definition      = 0,
  Reference       = 1,
  Implementation  = 2,
  TypeBinding     = 3,
  ReadAccess      = 4,
  WriteAccess     = 5,
  Import          = 6,        // marks import/use declarations
  Export          = 7,        // marks export declarations
}

// ─── Symbol ───────────────────────────────────────────────────────────────────

table SymbolInfo {
  uri:               string;   // lip://scope/pkg@ver/path#descriptor
  display_name:      string;
  kind:              SymbolKind;
  documentation:     string;   // Markdown
  signature:         string;   // language-specific type signature
  confidence_score:  uint8;
  relationships:     [Relationship];
  // AI extension slots (zero-cost when unused via FlatBuffers defaults)
  runtime_p99_ms:    float32 = -1;
  call_rate_per_s:   float32 = -1;
  taint_labels:      [string];     // e.g. ["PII", "UNSAFE_IO", "EXTERNAL"]
  blast_radius:      uint32 = 0;
  token_estimate:    uint32 = 0;   // estimated LLM token count for context
}

enum SymbolKind : byte {
  Unknown       =  0,
  Namespace     =  1,
  Class         =  2,
  Interface     =  3,
  Method        =  4,
  Field         =  5,
  Variable      =  6,
  Function      =  7,
  TypeParameter =  8,
  Parameter     =  9,
  Macro         = 10,
  Enum          = 11,
  EnumMember    = 12,
  Constructor   = 13,
  TypeAlias     = 14,
  // Dart/Flutter extensions (other languages ignore these)
  Extension     = 15,
  Mixin         = 16,
  Widget        = 17,
}

table Relationship {
  target_uri:          string;
  is_implementation:   bool;
  is_reference:        bool;
  is_type_definition:  bool;
  is_override:         bool;
  is_widget_child:     bool;   // Flutter widget tree parent-child
  edge_label:          string; // Kythe-compat: arbitrary labeled edge
}

// ─── Code Property Graph edges (new in v0.3) ──────────────────────────────────
//
// LIP's graph is a superset of a Code Property Graph: it unifies the AST
// (symbol definitions), the call graph, data-flow edges, and control-flow
// edges in a single queryable structure.
//
// Tier 1 documents carry syntactic call edges (Calls, Imports) derived from
// Tree-sitter. Tier 2 verification adds DataFlows and ControlFlows edges
// derived from the compiler's IR.
//
// Taint tracking (§8.2) and blast-radius analysis (§8.1) share the same
// reachability query engine — they differ only in which EdgeKind is traversed.

table GraphEdge {
  from_uri:  string;
  to_uri:    string;
  kind:      EdgeKind;
  // Source location of the edge origin (e.g. the call site, the assignment).
  at_range:  Range;
}

enum EdgeKind : byte {
  Calls        = 0,  // function/method invocation
  DataFlows    = 1,  // value flows from `from` to `to` (assignment, return, arg)
  ControlFlows = 2,  // control may pass from `from` to `to` (branch, loop)
  Instantiates = 3,  // `from` constructs an instance of `to`
  Inherits     = 4,  // `from` extends / implements `to`
  Imports      = 5,  // `from` file imports symbol `to`
}

// ─── Annotation Overlay (new in v0.3) ─────────────────────────────────────────
//
// A persistent, human- or agent-authored note attached to a symbol URI.
//
// Annotation entries solve the "Year-Zero Problem" for AI agents: every
// session starts with no memory of past reasoning. By persisting annotations
// on the LIP daemon (and optionally syncing them to the team cache), both
// human developers and AI agents accumulate project knowledge that survives
// context resets, editor restarts, and CI runs.
//
// Annotations are stored in a per-daemon content-addressable KV store,
// queryable by symbol URI, author, or key prefix.

table AnnotationEntry {
  symbol_uri:   string;   // the symbol this note is attached to
  key:          string;   // namespaced key, e.g. "lip:fragile", "agent:note"
  value:        string;   // markdown string or JSON blob
  author_id:    string;   // "human:<email>" | "agent:<model-id>"
  confidence:   uint8;    // reuses the 1–100 confidence scale
  timestamp_ms: int64;
  // If set, this annotation expires and is garbage-collected after this time.
  // 0 = permanent.
  expires_ms:   int64 = 0;
}

// ─── Dependency Slice ─────────────────────────────────────────────────────────

table DependencySlice {
  manager:      string;   // "npm" | "cargo" | "pub" | "pip" | "go"
  package_name: string;
  version:      string;
  package_hash: string;   // SHA-256(manager + name + version + resolved_deps)
  content_hash: string;   // SHA-256 of slice blob (integrity check)
  symbols:      [SymbolInfo];
  slice_url:    string;
  built_at_ms:  int64;
  spdx_license: string;
}

root_type EventStream;
```

### Schema evolution rules

- New fields may be appended to any table with a default value.
- Fields may be deprecated but never removed or reordered.
- `schema_version` allows clients to reject incompatible versions gracefully.
- Within a major version (1.x): backward compatibility guaranteed.
- Between 0.x versions: best-effort.

---

## 5. Symbol URI Scheme

LIP uses human-readable symbol URIs throughout — no opaque numeric IDs. Inherited from SCIP (which inherited from SemanticDB).

### 5.1 Grammar

```
lip-uri    ::= "lip://" scope "/" package "@" version "/" path "#" descriptor
scope      ::= "npm" | "cargo" | "pub" | "pip" | "go" | "local" | "team" | "kythe"
package    ::= UTF-8, URL-encoded, no spaces
version    ::= semver | content-hash | "local"
path       ::= relative/path/within/package
descriptor ::= identifier
             | type "." method [ "(" params ")" ]
             | type "." field
```

Identifiers with non-alphanumeric characters are backtick-escaped (identical to SCIP): `` lip://npm/lodash@4.17.21/lodash#`_.chunk` ``

### 5.2 Examples

```
# npm
lip://npm/react@18.2.0/index#useState
lip://npm/react@18.2.0/index#Component.setState(object)

# cargo
lip://cargo/tokio@1.35.1/runtime#Runtime
lip://cargo/tokio@1.35.1/runtime#Runtime.spawn(Future)

# pub (Dart/Flutter — first-class scope)
lip://pub/flutter@3.19.0/widgets#StatefulWidget
lip://pub/flutter@3.19.0/widgets#StatefulWidget.createState()
lip://pub/flutter@3.19.0/material#Scaffold
lip://pub/riverpod@2.5.0/riverpod#StateNotifier
lip://pub/http@1.2.0/http#Client.get(Uri)

# pip
lip://pip/numpy@1.26.0/numpy/core#ndarray

# go
lip://go/github.com.gin-gonic.gin@v1.9.0/gin#Engine.GET

# local
lip://local/myproject/lib/src/auth.dart#AuthService
lip://local/myproject/lib/src/auth.dart#AuthService.verifyToken(String)

# team (private registry)
lip://team/internal-api@2.1.0/models#UserRecord

# kythe (migration compat)
lip://kythe/chromium/src/chrome/browser#BrowserView
```

---

## 6. Protocol Lifecycle

### Phase 0 — Daemon startup (< 1 second)

The LIP daemon starts as a background process. It:

1. Loads persisted Salsa query graph from `~/.local/share/lip/graphs/<repo-hash>/`
2. Reads `lip.toml` from repo root (uses defaults if absent)
3. Opens the Unix socket / named pipe
4. Begins accepting connections **immediately** — before indexing completes

No tools need to be installed for the daemon to start and serve Tier 1 queries.

### Phase 1 — Handshake (< 100ms)

```
Client → Daemon:  ManifestRequest {
  repo_root:      string,   // absolute path
  merkle_root:    string,   // SHA-256 root of tracked files
  dep_tree_hash:  string,   // SHA-256 of dep manifest
  lip_version:    string,   // e.g. "0.3.0"
}

Daemon → Client:  ManifestResponse {
  cached_merkle_root: string,
  missing_slices:     [string],
  indexing_state:     IndexingState,
  tier2_available:    bool,        // lang server detected?
}

enum IndexingState { Cold, WarmTier1, WarmTier2, WarmFull }
```

**Warm start** (graph matches `merkle_root`): intelligence available immediately.  
**Cold start**: daemon begins Tier 1 indexing and streams progress events.

### Phase 2 — Tier 1 indexing (< 30 seconds for most repos)

Tree-sitter runs over all tracked files concurrently. Progress streamed as `push/progress` events. Dependency slices for `missing_slices` fetched in parallel and mounted as Tier 3 anchors.

### Phase 3 — Tier 2 background verification (optional, continuous)

If a language server is detected, the daemon spawns it as a low-priority background subprocess. It verifies the blast radius of uncommitted changes first, then processes files in reverse-dependency order. Delta events upgrade Tier 1 → Tier 2 symbols silently.

### Phase 4 — Steady state

On every file save:

1. Client sends `Delta.Upsert { seq: N, document: { uri, content_hash, ... } }`
2. **Daemon sends `DeltaAck { seq: N, accepted: true }` immediately** — before analysis completes
3. Daemon diffs the content hash against the stored hash
4. If unchanged: no further messages
5. If changed: compute API surface diff
   - API surface unchanged → re-verify function bodies at low priority
   - API surface changed → walk reverse dep graph → re-verify affected files
6. Stream `Delta.Upsert` events to client as results arrive

**Typical latency:** simple edits 50–200ms, API surface change 200–800ms.

#### Delta acknowledgment

Every `Delta` sent by the client **must** receive a `DeltaAck` response:

```
Client → Daemon:  Delta { seq: uint64, ... }
Daemon → Client:  DeltaAck { seq: uint64, accepted: bool, error?: string }
```

The `seq` field is a monotonically increasing client-side counter. If `accepted = false`, the client must re-send the delta or re-synchronize via a new `ManifestRequest`.

**Rationale**: LSP `textDocument/didChange` notifications are fire-and-forget. Client and server state can silently diverge — a well-known source of stale or incorrect intelligence that is nearly impossible to reproduce. LIP's explicit acknowledgment prevents this: if a `DeltaAck` is not received within a configurable timeout, the client knows the delta was dropped and can recover deterministically.

This is modeled on the Dart Analysis Protocol, which acknowledges every notification and thereby eliminates a whole class of drift bugs.

### Phase 5 — Annotation sync (optional, background)

When a team registry is configured, the daemon periodically pushes and pulls `AnnotationEntry` records. Human-authored annotations (no `expires_ms`) are permanent. Agent-authored annotations with short TTLs (e.g. 24h) are pruned automatically.

### 6.6 Query API

All queries return within 5ms from the Salsa cache. If a result is not yet cached, a Tier 1 estimate is returned immediately and background verification is queued.

```
lip.query.definition(uri, position, tier?)  → SymbolInfo
lip.query.references(symbol_uri, limit?)    → [Occurrence]
lip.query.hover(uri, position)              → HoverResult
lip.query.blast_radius(symbol_uri)          → BlastRadiusResult
lip.query.subgraph(symbol_uri, depth, max_tokens?) → SymbolGraph
lip.query.taint(symbol_uri)                 → [TaintPath]
lip.query.workspace_symbols(query, limit?)  → [SymbolInfo]
lip.query.dead_symbols(uri?)                → [SymbolInfo]
lip.query.cpg(symbol_uri, edge_kinds, depth) → CpgResult
lip.query.workspace_summary()               → WorkspaceSummary

lip.annotation.set(symbol_uri, key, value, confidence, expires_ms?)
lip.annotation.get(symbol_uri, key?)        → [AnnotationEntry]
lip.annotation.list(key_prefix, limit?)     → [AnnotationEntry]

lip.stream.context(file_uri, cursor_position, max_tokens) → stream<SymbolInfo>
```

---

## 7. Transport & IPC

### 7.1 Wire framing

Messages are framed with a **4-byte big-endian length prefix** followed by the payload bytes:

```
┌──────────────────┬────────────────────────────────┐
│  length : u32 BE │  FlatBuffers or JSON payload   │
└──────────────────┴────────────────────────────────┘
```

This is deliberately simpler than LSP's HTTP-inspired `Content-Length: N\r\n\r\n` framing. There is no header parsing, no line scanning, no CRLF handling. A reader needs exactly two `read()` calls per message. See Appendix B.

### 7.2 IDE ↔ Daemon (local, high-frequency)

**Unix domain socket** (Linux/macOS) or **named pipe** (Windows) for the request-response channel. Socket path: `/tmp/lip-<uid>-<repo-hash>.sock`.

**Shared memory (mmap)** for large FlatBuffers payloads. The socket carries only the 4-byte length prefix plus a 16-byte `MmapHeader` (offset + region ID). The IDE reads the payload directly from the mapped region — zero copy.

```
┌──────────────┐   socket (4-byte header + MmapHeader)   ┌─────────────┐
│  IDE plugin  │ ◄────────────────────────────────────►  │ LIP daemon  │
│  (client)    │   mmap (FlatBuffers blob)                │  (server)   │
└──────────────┘ ◄────────────────────────────────────►  └─────────────┘
```

### 7.3 Daemon ↔ Registry (remote)

**gRPC streaming over TLS** for slice downloads. Slices are also valid as plain HTTPS blobs, enabling CDN delivery:

```
GET https://registry.lip-protocol.dev/slices/{content-hash}
Content-Type: application/x-lip-flatbuffers
```

### 7.4 Daemon ↔ CI (incremental push)

CI runners emit `EventStream` deltas per commit — not full `.scip` files. The daemon applies them incrementally.

---

## 8. Intelligence Extensions

### 8.1 Blast radius analysis

```
lip.query.blast_radius("lip://local/myapp/lib/payment.dart#PaymentService") → {
  direct_dependents:     12,
  transitive_dependents: 47,
  affected_files:        ["lib/checkout.dart", "lib/order.dart", ...],
  affected_services:     ["checkout-service", "billing-service"],
  highest_risk_symbols:  [SymbolInfo, ...],
}
```

Available at Tier 1 (approximate, from import graph) and Tier 2 (precise, from type-resolved call graph).

### 8.2 Taint tracking

Symbols annotated with `taint_labels` (e.g. `["PII"]`) propagate those labels through `DataFlows` edges in the CPG. A query returns all paths from a tainted source to an unsafe sink.

```
lip.query.taint("lip://local/myapp/lib/user.dart#User.email") → [
  {
    source: "User.email",
    path:   ["UserService.serialize", "LoggingMiddleware.write"],
    sink:   "Logger.info",
    risk:   "PII_TO_PLAINTEXT_LOG",
  }
]
```

Requires Tier 2 CPG edges for inter-procedural accuracy. Tier 1 provides approximate intra-file taint paths.

### 8.3 Code Property Graph queries

LIP's graph is a superset of a Code Property Graph: it unifies symbol definitions (AST), call edges, data-flow edges, and control-flow edges in a single queryable structure.

Tier 1 documents carry syntactic `Calls` and `Imports` edges derived from Tree-sitter. Tier 2 adds `DataFlows` and `ControlFlows` edges from the compiler's IR.

```
lip.query.cpg(
  symbol_uri: string,
  edge_kinds: [EdgeKind],   // Calls | DataFlows | ControlFlows | ...
  depth: int,               // hop limit
) → {
  nodes: [SymbolInfo],
  edges: [GraphEdge],
}
```

Taint tracking (§8.2) and blast-radius analysis (§8.1) are the same reachability query over different edge kinds — they share one engine.

**Why this matters**: Vulnerabilities arise from interactions across function, file, and service boundaries. A CPG lets LIP answer "does user-controlled input ever reach this SQL sink?" by traversing `DataFlows` edges, without requiring a separate SAST tool or a second index pass.

The CPG schema is compatible with [Joern](https://joern.io)'s Code Property Graph model.

### 8.4 Runtime telemetry overlay (opt-in)

When an OpenTelemetry or Datadog integration is configured, hover data includes:

```
runtime: {
  calls_per_second: 1243.5,
  p50_ms:           12.1,
  p99_ms:           187.4,
  error_rate_pct:   0.03,
}
```

Stored in `SymbolInfo.runtime_p99_ms` and `call_rate_per_s`. Always opt-in, never blocks symbol resolution.

### 8.5 Dead code detection

A symbol is dead if `blast_radius == 0`, it is not exported from the package, and (if telemetry is enabled) `call_rate_per_s <= 0`.

---

## 9. AI & Agent Integration

### 9.1 Semantic subgraph queries

```
lip.query.subgraph(
  symbol_uri:      "lip://local/myapp/lib/checkout.dart#CheckoutService",
  depth:           2,
  include_types:   true,
  include_callers: true,
  include_callees: true,
  max_tokens:      4000,
) → SymbolGraph {
  nodes:           [SymbolInfo],
  edges:           [Relationship],
  cpg_edges:       [GraphEdge],
  token_estimate:  3847,
  truncated:       false,
  truncation_hint: "",
}
```

`token_estimate` lets agents stay within context windows without materializing the full graph. `truncated` and `truncation_hint` explain what was excluded.

### 9.2 Streaming context for RAG

```
lip.stream.context(
  file_uri:        string,
  cursor_position: Range,
  max_tokens:      int,
) → stream<SymbolInfo>
```

Returns the most relevant symbols in order: direct definition at cursor → Tier 3 dep symbols → callers → callees → related types. Stops at `max_tokens`.

### 9.3 Change impact preview

```
lip.query.impact_preview(proposed_changes: [FileDiff]) → {
  affected_symbols:      [SymbolInfo],
  broken_call_sites:     [Occurrence],
  type_errors_predicted: int,
  blast_radius:          int,
  annotation_warnings:   [AnnotationEntry],  // "lip:fragile", "lip:owner" triggered
}
```

Agents receive annotation warnings before applying changes. A symbol annotated `lip:fragile` surfaces as a warning when the proposed change touches it.

### 9.4 Annotation Overlay Layer

AI coding agents restart from zero on every session. Developers accumulate project knowledge over years: that a caching layer is fragile, that a function must not be modified without coordinating with another team, that a particular API is being deprecated. None of this knowledge is currently standardized or machine-readable.

LIP provides an **Annotation Overlay Layer** — a persistent, content-addressed key-value store that attaches structured notes to symbol URIs. Both human developers and AI agents can read and write annotations. They survive context resets, editor restarts, and CI runs.

```
lip.annotation.set(
  symbol_uri: string,
  key:        string,   // namespaced: "lip:fragile", "agent:note", "team:owner"
  value:      string,   // markdown or JSON blob
  confidence: uint8,    // 1–100
  expires_ms: int64?,   // optional TTL; 0 = permanent
) → AnnotationEntry

lip.annotation.get(
  symbol_uri: string,
  key?:       string,   // omit to get all keys for this symbol
) → [AnnotationEntry]

lip.annotation.list(
  key_prefix: string,   // e.g. "agent:" to find all agent-authored notes
  limit?:     int,
) → [AnnotationEntry]
```

**Canonical key prefixes:**

| Prefix | Meaning |
|---|---|
| `lip:fragile` | Symbol is known to be fragile; treat changes with extra care |
| `lip:owner` | Team or person responsible for this symbol |
| `lip:deprecated` | Deprecated; migration target in `value` |
| `lip:taint` | Manually asserted taint label (supplements `taint_labels` in schema) |
| `agent:note` | Agent-authored reasoning note from a prior session |
| `agent:verified` | Agent has verified this symbol behaves as documented |
| `team:*` | Team-specific namespace; uninterpreted by the LIP daemon |

**Sync behavior**: Annotations are stored locally by the daemon and optionally pushed to the team LIP cache (§7.3) so they are visible to all developers and CI runs. Agent-authored annotations with short TTLs (e.g. 24h) are pruned automatically; human-authored annotations are permanent unless explicitly deleted.

**Why not just code comments?** Code comments are invisible to tools that don't parse the specific language, don't survive refactors that move code, and can't carry structured confidence scores or author attribution. Annotations are indexed by symbol URI, so they survive renames — the URI changes, but the rename event updates the annotation key automatically.

### 9.5 Workspace summary

```
lip.query.workspace_summary() → {
  total_symbols:      int,
  total_files:        int,
  current_tier:       IndexingState,
  functional_domains: [{ name, symbols, summary }],
  top_blast_radius:   [SymbolInfo],
  annotation_count:   int,
  dep_health: {
    total_deps:   int,
    slices_ready: int,
    missing:      int,
  }
}
```

An agent starting a new session calls this first to get a structured map of the codebase without reading any source files directly.

---

## 10. Compatibility Layer

### 10.1 LIP-to-LSP bridge

Translates LIP queries to standard LSP responses for editors without native LIP support.

| LSP request | LIP query |
|---|---|
| `textDocument/definition` | `lip.query.definition` |
| `textDocument/references` | `lip.query.references` |
| `textDocument/hover` | `lip.query.hover` |
| `workspace/symbol` | `lip.query.workspace_symbols` |
| `textDocument/publishDiagnostics` | streamed from blast radius analysis |

**Coordinate translation**: the bridge converts LIP's UTF-8 byte offsets to LSP's UTF-16 code unit offsets at the bridge layer, not in the daemon.

### 10.2 SCIP importer

```bash
lip import --from-scip ./index.scip
```

Converts SCIP Protobuf → LIP FlatBuffers, reconstructs the Salsa graph, assigns imported symbols `confidence_score: 80`. Does **not** require an SCIP indexer — only an existing `.scip` file produced by any means.

### 10.3 SCIP exporter

```bash
lip export --to-scip ./index.scip
```

Emits a standard `.scip` file from LIP's current graph state.

### 10.4 Kythe importer

```bash
lip import --from-kythe ./kythe-tables/
```

Converts Kythe VNames → LIP URIs, Kythe edges → `Relationship` with `edge_label`, Kythe anchors → `Occurrence` with `confidence_score: 75`.

### 10.5 MCP server

The daemon exposes an embedded MCP server on `127.0.0.1:3714`:

```
mcp://lip/symbol/{uri-encoded}             → SymbolInfo
mcp://lip/subgraph/{uri-encoded}/{depth}   → SymbolGraph
mcp://lip/annotations/{uri-encoded}        → [AnnotationEntry]
mcp://lip/blast-radius/{uri-encoded}       → BlastRadiusResult
```

MCP tools: `lip_semantic_search`, `lip_blast_radius`, `lip_impact_preview`, `lip_hover`

---

## 11. Security Considerations

### 11.1 Dependency slice integrity

Every `DependencySlice` carries a `content_hash` (SHA-256). The daemon verifies this before mounting. The registry additionally signs slice manifests with Ed25519. Clients verify signatures before trusting slices.

### 11.2 Symbol URI validation

URIs are validated against the grammar in §5.1. URIs containing path traversal sequences (`..`), null bytes, or non-UTF-8 content are rejected.

### 11.3 Shared memory safety

The mmap region is `MAP_PRIVATE` on the reader side. The daemon communicates region bounds via the socket header. The client validates offset and length before access — buffer overread is not possible.

### 11.4 Annotation confidentiality

Annotations default to local storage. Sensitive content (API keys, passwords, personal data) MUST NOT be written into annotations. The annotation store is not encrypted at rest in v0.3 — disk encryption is recommended for sensitive environments.

### 11.5 MCP server security

Binds to `127.0.0.1` by default. Never accepts remote connections without explicit configuration. Exposes read-only queries only; annotation writes require the Unix socket channel.

### 11.6 CPG taint label trust

Taint labels are advisory. They are propagated by the daemon but never enforced at the protocol level. Integration with CI blocking rules is out of scope for v0.3.

---

## 12. Governance & RFC Process

### 12.1 Philosophy

LIP is an open standard. No single organization controls it. The goal is a process similar to IETF RFCs: open discussion, documented decisions, community consensus.

### 12.2 RFC lifecycle

1. **Proposal**: Open a GitHub issue in `lip-protocol/lip` with prefix `[RFC]`. Describe the problem, proposed change, and alternatives considered.
2. **Discussion period**: 14 calendar days minimum. Anyone may comment.
3. **Revision**: Author updates based on feedback.
4. **Consensus call**: A maintainer calls for consensus. If no blocking objections from active implementers within 7 days, the RFC is accepted.
5. **Implementation**: Author submits a PR referencing the RFC number.
6. **Merge**: Reviewed and merged by a maintainer.

### 12.3 Active implementers

An "active implementer" is any person or organization that has contributed a merged PR to `lip-protocol/lip` in the last 12 months, OR has a registered implementation in `IMPLEMENTATIONS.md`. Active implementers have blocking objection rights.

### 12.4 Breaking changes

Changes to the FlatBuffers schema that remove or reorder fields, or changes to transport framing, require a major version bump, a 6-month deprecation period, and a migration guide.

---

## 13. Roadmap

### v0.1 — Foundation

- [x] FlatBuffers schema (initial)
- [x] Rust bindings (generated + utilities)
- [x] Dart bindings (CKB/Cartographer)
- [x] Daemon: Tree-sitter Tier 1 indexer
- [x] Daemon: Salsa query graph (in-memory, single session)
- [x] LIP-to-LSP bridge
- [x] CLI: `lip index`, `lip query`, `lip import --from-scip`

### v0.2 — Persistence & incremental

- [ ] Persisted query graph (survives restarts)
- [ ] Blast radius engine (reverse dep graph)
- [ ] Merkle sync protocol
- [ ] **Delta acknowledgment (`DeltaAck`) — eliminate fire-and-forget drift**
- [ ] Daemon: Tier 2 incremental compiler (TypeScript, Rust, Dart)
- [ ] Daemon: FlatBuffers + shared-memory IPC (mmap path)

### v0.3 — Federation

- [ ] Registry protocol (gRPC + HTTP blob)
- [ ] Reference registry server
- [ ] Dependency slice builder (npm, cargo, pub, pip, go)
- [ ] Team cache sharing (daemon ↔ registry sync)
- [ ] CLI: `lip export --to-scip`, `lip push`, `lip pull`

### v0.4 — Intelligence extensions

- [ ] **Code Property Graph edges (`GraphEdge`, `EdgeKind`) — Tier 2 populated**
- [ ] **CPG query API (`lip.query.cpg`)**
- [ ] **Taint tracking via CPG `DataFlows` traversal**
- [ ] Dead code detection
- [ ] Runtime telemetry overlay (OpenTelemetry integration)
- [ ] **Annotation Overlay Layer (`AnnotationEntry`, `lip.annotation.*`)**
- [ ] **Annotation sync to team cache**
- [ ] RFC process + public governance model

### v1.0 — Stable protocol

- [ ] Schema frozen (backward compat guaranteed)
- [ ] TypeScript, Go, Python bindings
- [ ] VS Code extension (LIP-native)
- [ ] Neovim plugin
- [ ] Public registry at `registry.lip-protocol.dev`
- [ ] Language support: TypeScript, Rust, Dart, Go, Python, Java, Kotlin, C, C++
- [ ] Formal governance body established

---

## Appendix A — Why not UTF-16

LSP uses UTF-16 code unit offsets for column positions. This is wrong for three independent reasons:

**Wrong for display**: Users see *grapheme clusters* as single characters. A flag emoji 🇦🇹 is 1 grapheme cluster, 2 Unicode code points, 4 UTF-16 code units, 8 UTF-8 bytes. Converting to display columns requires grapheme segmentation regardless of encoding. UTF-16 provides no advantage here.

**Wrong for strings**: Accessing a substring at a given column offset in a UTF-16 position requires O(N) scanning if the string is stored as UTF-8. UTF-8 byte offsets allow O(1) substring access when the string is stored as UTF-8 — the universal default for source files.

**Wrong for the ecosystem**: Every modern programming language stores strings as UTF-8 or UTF-32. UTF-16 is a legacy encoding from the Windows/Java era requiring explicit conversion at every boundary.

**LIP uses UTF-8 byte offsets.** Converting to display columns for presentation is the IDE's responsibility, using a grapheme segmentation library. This work is required regardless of encoding and belongs in the presentation layer.

---

## Appendix B — Why not HTTP framing

LSP uses:
```
Content-Length: 92\r\n
\r\n
{"jsonrpc":"2.0","method":"textDocument/definition",...}
```

This is not actual HTTP. It requires custom parsing code with no benefit over a 4-byte integer prefix. LIP uses:

```
[00 00 00 5C]   ← uint32 big-endian = 92
[payload, 92 bytes]
```

Reading this is `read_exact(4 bytes)` + `u32::from_be_bytes` + `read_exact(N bytes)` — available in every language's standard library as a unit. The HTTP framing is accidental complexity inherited from LSP's origin as a VS Code extension API. The Dart Analysis Protocol (which predates LSP) uses simple newline-delimited messages for the same reason.

---

## Appendix C — Why not Protobuf

SCIP uses Protobuf 3. Protobuf requires full deserialization to access any field: the entire message is decoded into allocated heap memory before a single field can be read. For high-frequency symbol queries (hover on every cursor move), this creates measurable GC pressure.

**LIP uses FlatBuffers.** FlatBuffers allows zero-copy reads from the raw buffer via table offsets. The IDE reads a single field from a 10MB symbol graph via one pointer offset and a bounds check — no allocation, no copy, no GC event.

The tradeoff: FlatBuffers serialization (write path) is slower. This is acceptable because the write path (daemon building the graph) is background work, while the read path (IDE querying symbols) is the latency-critical hot path. For the shared-memory transport, FlatBuffers is the only viable option — Protobuf cannot be mmap'd and read without deserialization.

---

## Appendix D — Prior Art & References

### Protocols & formats

- **LSP 3.17** — https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
- **SCIP** — https://github.com/scip-code/scip · https://sourcegraph.com/blog/announcing-scip
- **LSIF** — https://microsoft.github.io/language-server-protocol/specifications/lsif/
- **SemanticDB** — https://scalameta.org/docs/semanticdb/specification.html
- **Dart Analysis Protocol** — Predecessor to LSP; acknowledges every notification. Technically superior on concurrency and framing. https://htmlpreview.github.io/?https://github.com/dart-lang/sdk/blob/main/pkg/analysis_server/doc/api.html
- **Kythe** — https://kythe.io/docs/kythe-overview.html
- **MCP November 2025** — https://modelcontextprotocol.io/specification/2025-11-25
- **Joern / Code Property Graph** — Inter-procedural static analysis via unified AST + CFG + DFG. Original paper: Yamaguchi et al., IEEE S&P 2014. https://joern.io

### Incremental computation

- **Salsa** — https://github.com/salsa-rs/salsa
- **Durable Incrementality (rust-analyzer blog)** — https://rust-analyzer.github.io/blog/2023/07/24/durable-incrementality.html
- **rust-analyzer architecture** — https://rust-analyzer.github.io/book/contributing/architecture.html

### Critiques that directly informed this spec

- **matklad — LSP could have been better** (Oct 2023) — https://matklad.github.io/2023/10/12/lsp-could-have-been-better.html  
  Source for: UTF-8 over UTF-16, 4-byte framing over HTTP-style Content-Length, notification causality
- **michaelpj — LSP: the good, the bad, and the ugly** (Sep 2024) — https://www.michaelpj.com/blog/2024/09/03/lsp-good-bad-ugly.html  
  Source for: concurrency specification, open governance, dynamic registration analysis
- **sheeptechnologies RFC-001 — Remove SCIP dependency** (Jan 2026) — https://github.com/orgs/sheeptechnologies/discussions/4  
  Source for: SCIP build system coupling, file-incremental alternatives, Stack Graphs post-mortem

### Serialization

- **FlatBuffers** — https://flatbuffers.dev
- **Cap'n Proto vs FlatBuffers vs Protobuf** — https://capnproto.org/news/2014-06-17-capnproto-flatbuffers-sbe.html

---

*LIP Specification v0.3 · April 2026 · Apache 2.0*  
*Drafted by Lisa Küpper, TasteHub GmbH, Vienna*  
*Based on research into LSP, SCIP, Kythe, Salsa, Dart Analysis Protocol, Joern CPG, MCP, and the 2025–2026 agentic coding ecosystem.*
