# CLI Reference

All commands accept `--help` for full flag documentation.

Global flag: `--log <level>` (default: `warn`). Set `LIP_LOG=debug` for verbose output.

---

## lip daemon

Start the LIP daemon — persistent query graph server with per-file filesystem watcher.

```bash
lip daemon [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--socket <path>` | `/tmp/lip-daemon.sock` | Unix socket path |
| `--journal <path>` | `~/.local/share/lip/journal.lip` | WAL journal file |
| `--no-watch` | — | Disable filesystem watcher (manual delta mode) |

The daemon replays its journal on startup — the graph is warm immediately without re-indexing.

---

## lip index

Index a directory with the Tier 1 tree-sitter indexer and emit deltas.

```bash
lip index [OPTIONS] [PATH]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` | `.` | Root directory to index |
| `--language <lang>` | auto-detect | Force a language hint (`rust`, `typescript`, `python`, `dart`) |
| `--json` | — | Emit a JSON `EventStream` instead of plain text |
| `--limit <n>` | 0 (unlimited) | Stop after indexing N files |

```bash
# Index current directory, human-readable
lip index .

# Index and emit JSON (pipe to push)
lip index ./src --json | lip push --registry https://registry.lip.dev
```

---

## lip query

Query a running daemon. All subcommands accept `--socket <path>`.

```bash
lip query --socket /tmp/lip.sock <subcommand>
```

### lip query definition

Find the definition of the symbol at a position.

```bash
lip query definition <uri> <line> <col>
```

- `uri`: file URI or LIP symbol URI
- `line`, `col`: 0-based, UTF-8 byte offsets

```bash
lip query definition file:///src/auth.rs 42 10
```

### lip query references

Find all references to a symbol across the workspace.

```bash
lip query references <symbol_uri> [--limit <n>]
```

```bash
lip query references "lip://local/src/auth.rs#AuthService.verifyToken"
```

### lip query hover

Get type signature and documentation for a symbol at a position.

```bash
lip query hover <uri> <line> <col>
```

### lip query blast-radius

Compute the blast radius of a symbol — direct and transitive dependents.

```bash
lip query blast-radius <symbol_uri>
```

```bash
lip query blast-radius "lip://local/src/auth.rs#AuthService"
# direct dependents:     3
# transitive dependents: 12
# affected files:
#   file:///src/middleware/auth_guard.rs
#   file:///src/handlers/login.rs
#   ...
```

### lip query symbols

Search workspace symbols by name.

```bash
lip query symbols <query> [--limit <n>]
```

```bash
lip query symbols "verify"        # fuzzy match
lip query symbols "AuthService"   # exact prefix match
```

### lip query dead-symbols

Find symbols defined but never referenced.

```bash
lip query dead-symbols [--limit <n>]
```

### lip query batch

Execute multiple queries in a single round-trip. Reads a JSON array of query objects from a file or stdin.

```bash
lip query batch [FILE]
```

Each element of the array is a `ClientMessage` object (without `type: Manifest` or `type: Delta`):

```bash
lip query batch <<'EOF'
[
  {"type":"query_blast_radius","symbol_uri":"lip://local/src/auth.rs#AuthService"},
  {"type":"query_references",  "symbol_uri":"lip://local/src/auth.rs#AuthService","limit":50},
  {"type":"annotation_get",    "symbol_uri":"lip://local/src/auth.rs#AuthService","key":"lip:fragile"}
]
EOF

# From file
lip query batch queries.json
```

Output is JSON (`BatchResult`) with one entry per input query.

---

## lip lsp

Start a standard LSP server that bridges to the LIP daemon.

```bash
lip lsp [--socket <path>]
```

Reads from stdin, writes to stdout (stdio transport). Configure your editor to launch this as a language server — no custom plugin needed.

Supported LSP methods: `textDocument/definition`, `textDocument/references`, `textDocument/hover`, `workspace/symbol`, `textDocument/documentSymbol`.

---

## lip mcp

Start a Model Context Protocol server backed by the LIP daemon.

```bash
lip mcp [--socket <path>]
```

Reads JSON-RPC 2.0 from stdin, writes to stdout (stdio transport). Add to your MCP client config:

```json
{
  "mcpServers": {
    "lip": { "command": "lip", "args": ["mcp"] }
  }
}
```

Exposed tools: `lip_blast_radius`, `lip_workspace_symbols`, `lip_definition`, `lip_references`, `lip_hover`, `lip_document_symbols`, `lip_dead_symbols`, `lip_annotation_get`, `lip_annotation_set`.

See [mcp-integration.md](mcp-integration.md).

---

## lip import

Import an existing SCIP index file, upgrading all symbols to Tier 2 confidence (score 90).

```bash
lip import --from-scip <path.scip> [--socket <path>]
```

```bash
lip import --from-scip ./index.scip
```

Use this to bootstrap LIP on a repo that already has a SCIP pipeline. After import, LIP maintains freshness incrementally — SCIP never needs to run again unless you want a full Tier 2 refresh.

---

## lip export

Export the current daemon state as a SCIP index file.

```bash
lip export --to-scip <output.scip> [--socket <path>]
```

---

## lip fetch

Download a dependency slice from the registry.

```bash
lip fetch <sha256-hash> [--registry <url>] [--cache-dir <path>]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--registry` | `https://registry.lip.dev` | Registry URL |
| `--cache-dir` | `~/.cache/lip/slices` | Local slice cache |

---

## lip push

Publish a dependency slice to the registry.

```bash
lip push [slice.json] [--registry <url>] [--cache-dir <path>]
```

Reads from file or stdin. Prints the content hash on success.

```bash
# From file
lip push ./my-package.json --registry https://registry.lip.dev

# From stdin (pipe from index)
lip index ./src --json | lip push
```

---

## lip slice

Build pre-computed dependency slices from your package manager lockfiles.

```bash
lip slice [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--cargo [Cargo.toml]` | `./Cargo.toml` | Slice Cargo dependencies |
| `--npm [package.json]` | `./package.json` | Slice npm dependencies |
| `--pub [pubspec.yaml]` | `./pubspec.yaml` | Slice pub (Dart) dependencies |
| `--output <dir>` | `~/.cache/lip/slices` | Write slices to this directory |
| `--push` | — | Push slices to registry after building |
| `--registry <url>` | `https://registry.lip.dev` | Registry URL for `--push` |

```bash
# Build Cargo slices locally
lip slice --cargo

# Build and immediately share with team
lip slice --cargo --push --registry https://registry.lip.dev

# Build slices for a monorepo with multiple package managers
lip slice --cargo --npm --pub
```

See [registry.md](registry.md).

---

## lip annotate

Attach a persistent key/value annotation to a symbol.

```bash
lip annotate <symbol_uri> <key> <value> [--author <id>] [--socket <path>]
```

```bash
lip annotate "lip://local/src/payments.rs#processCharge" \
  "lip:fragile" \
  "Uses deprecated Stripe v1 API — do not refactor without platform review" \
  --author "human:alice"
```

Annotations survive daemon restarts, file changes, and re-indexes. They are stored in the WAL journal alongside file data.
