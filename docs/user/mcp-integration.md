# MCP Integration

`lip mcp` exposes the LIP daemon as a Model Context Protocol server. Any MCP-compatible client — Claude Code, CKB, Cursor, or a custom agent — gets live, always-current code intelligence without building its own indexing layer.

---

## What the MCP server provides

| Tool | Description |
|------|-------------|
| `lip_blast_radius` | Which files are affected if this symbol changes |
| `lip_workspace_symbols` | Semantic symbol search across the whole repo |
| `lip_definition` | Go-to-definition at (file, line, col) |
| `lip_references` | All call sites for a symbol URI |
| `lip_hover` | Type signature + docs at a position |
| `lip_document_symbols` | All symbols defined in a file |
| `lip_dead_symbols` | Symbols defined but never referenced |
| `lip_annotation_get` | Read a persistent symbol annotation |
| `lip_annotation_set` | Write a persistent symbol annotation |
| `lip_batch_query` | Execute multiple queries in one round-trip |

All tools are backed by the live LIP daemon — results are always current, never a stale snapshot.

---

## Setup

### 1. Start the daemon

```bash
lip daemon --socket /tmp/lip.sock
```

### 2. Start the MCP server

```bash
lip mcp --socket /tmp/lip.sock
```

The MCP server speaks JSON-RPC 2.0 over stdio. Keep it running alongside the daemon, or launch it on demand — it connects to the daemon per-request.

### 3. Add to your MCP client config

**Claude Code** (`~/.claude/mcp.json` or project `.mcp.json`):

```json
{
  "mcpServers": {
    "lip": {
      "command": "lip",
      "args": ["mcp", "--socket", "/tmp/lip.sock"]
    }
  }
}
```

**CKB** (`.ckb/config.json`):

```json
{
  "backends": {
    "lip": {
      "command": "lip",
      "args": ["mcp", "--socket", "/tmp/lip.sock"],
      "tools": ["lip_blast_radius", "lip_workspace_symbols", "lip_references"]
    }
  }
}
```

**Cursor** (`~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "lip": {
      "command": "lip",
      "args": ["mcp"]
    }
  }
}
```

---

## Tool reference

### lip_blast_radius

Call before modifying any function, class, or interface to understand the scope of impact.

**Input:**
```json
{ "symbol_uri": "lip://local/src/auth.rs#AuthService.verifyToken" }
```

**Output:**
```
Blast radius for `AuthService.verifyToken`:
direct dependents:     3
transitive dependents: 12
affected files (12):
  file:///src/middleware/auth_guard.rs
  file:///src/handlers/login.rs
  file:///src/handlers/register.rs
  ...
```

---

### lip_workspace_symbols

Semantic symbol search — finds by name across the entire workspace, returns kind, location, and confidence.

**Input:**
```json
{ "query": "verifyToken", "limit": 20 }
```

---

### lip_definition

Find where a symbol is defined.

**Input:**
```json
{ "uri": "file:///src/auth.rs", "line": 42, "col": 10 }
```

`line` and `col` are 0-based, UTF-8 byte offsets.

---

### lip_references

Find all call sites for a symbol.

**Input:**
```json
{ "symbol_uri": "lip://local/src/auth.rs#AuthService.verifyToken", "limit": 50 }
```

---

### lip_annotation_set / lip_annotation_get

Persistent annotations on symbols — survive daemon restarts, file changes, and re-indexes. Useful for AI agents to leave notes that persist across sessions.

**Set:**
```json
{
  "symbol_uri": "lip://local/src/payments.rs#processCharge",
  "key":        "agent:note",
  "value":      "Uses deprecated Stripe v1 API. Migration tracked in #472.",
  "author_id":  "agent:claude"
}
```

**Get:**
```json
{
  "symbol_uri": "lip://local/src/payments.rs#processCharge",
  "key":        "agent:note"
}
```

**Canonical key prefixes:**

| Prefix | Meaning |
|--------|---------|
| `lip:fragile` | Handle with care — high blast radius or known instability |
| `lip:do-not-touch` | Frozen — do not modify without explicit approval |
| `lip:{agent-id}-lock` | Symbol is claimed by a running agent instance (see below) |
| `team:owner` | Owning team or person |
| `agent:note` | AI agent observations (scoped per agent ID) |

#### `lip:agent-lock` — worktree collision prevention

When an agent starts working on a symbol it sets a lock annotation so other instances of the same agent type (e.g. two nyx workers on different worktrees) know to skip it:

```json
{
  "symbol_uri": "lip://local/src/payments.rs#processCharge",
  "key":        "lip:nyx-agent-lock",
  "value":      "{\"worktree\":\"/repo-work\",\"pid\":4821,\"started_at_ms\":1744320000000}",
  "author_id":  "agent:nyx"
}
```

Before claiming a symbol, check for the lock:

```json
{ "symbol_uri": "...", "key": "lip:nyx-agent-lock" }
```

If the value is non-empty, a peer is already working on it — skip and move on.

After finishing, clear the lock by writing an empty value:

```json
{ "symbol_uri": "...", "key": "lip:nyx-agent-lock", "value": "", "author_id": "agent:nyx" }
```

The lock key is agent-type-specific (`lip:nyx-agent-lock`, `lip:claude-agent-lock`, …) so two different agent types can work on the same symbol without blocking each other. To block all agents, set `lip:do-not-touch`.

---

### lip_batch_query

Execute multiple queries in a single round-trip. One socket connection instead of N — critical for planning workflows where you need several facts per symbol.

**Input:**
```json
{
  "queries": [
    { "type": "query_blast_radius", "symbol_uri": "lip://local/src/auth.rs#AuthService" },
    { "type": "query_references",   "symbol_uri": "lip://local/src/auth.rs#AuthService", "limit": 50 },
    { "type": "annotation_get",     "symbol_uri": "lip://local/src/auth.rs#AuthService", "key": "lip:fragile" },
    { "type": "annotation_get",     "symbol_uri": "lip://local/src/auth.rs#AuthService", "key": "lip:nyx-agent-lock" }
  ]
}
```

**Output:** one result block per query, separated by `---`:
```
[0]
Blast radius for `AuthService`:
direct dependents:     3
transitive dependents: 12
...
---
[1]
lip://local/src/middleware/auth_guard.rs  line 18
...
---
[2]
(not set)
---
[3]
(not set)
```

`Manifest` and `Delta` are not permitted inside a batch; they return an error for that slot without aborting the rest.

---

## How agents should use LIP tools

**Planning a multi-symbol refactor (use batch):**
```
1. lip_workspace_symbols → collect URIs for all symbols you plan to touch
2. lip_batch_query       → for each URI: blast_radius + references + annotation_get("lip:fragile")
                           + annotation_get("lip:nyx-agent-lock")
                           — 10 symbols = 1 round-trip, not 40
3. lip_annotation_set    → set lip:nyx-agent-lock on each claimed symbol
```

**Before modifying a single symbol:**
```
1. lip_workspace_symbols → find the symbol URI
2. lip_blast_radius       → understand scope of change
3. lip_references         → see all call sites
4. lip_annotation_get     → check for "lip:fragile" or "lip:do-not-touch"
```

**After modifying code** (if the change is notable):
```
lip_annotation_set("agent:note") → leave a note explaining what changed and why
lip_annotation_set("lip:nyx-agent-lock", value="") → release the lock
```

**Finding dead code:**
```
lip_dead_symbols → candidates for safe deletion
lip_blast_radius on each → confirm truly zero dependents
```

---

## The CKB → LIP mapping

If you use CKB, LIP tools map directly to CKB's existing capabilities:

| CKB tool | LIP tool | Notes |
|----------|----------|-------|
| `analyzeImpact` | `lip_blast_radius` | LIP is always live; CKB needs SCIP refresh |
| `searchSymbols` | `lip_workspace_symbols` | LIP uses live Tier 1 index |
| `findReferences` | `lip_references` | Same semantics |
| `getCallGraph` | `lip_references` + CPG edges | LIP CPG via Tier 2 |
| `prepareChange` | `lip_blast_radius` + `lip_annotation_get` | LIP adds annotation check |

With LIP as CKB's backend, `analyzeImpact` and `prepareChange` are always current — no more `ckb index` needed.
