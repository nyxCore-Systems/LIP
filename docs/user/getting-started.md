# Getting Started with LIP

LIP keeps a live, queryable code intelligence graph of your repository. This guide gets you from zero to a running daemon with editor and AI agent integration in about five minutes.

---

## Prerequisites

- Rust 1.78+ (`rustup update stable`)
- No other requirements — no system `protoc`, no running compiler

---

## Install

```bash
git clone https://github.com/lip-protocol/lip
cd lip
cargo install --path tools/lip-cli
```

This puts the `lip` binary in `~/.cargo/bin/`. Verify:

```bash
lip --version
# lip 1.0.0
```

---

## Step 1 — Start the daemon

The daemon is the heart of LIP. It watches your files, maintains the query graph, and answers queries over a Unix socket.

```bash
lip daemon --socket /tmp/lip.sock
```

Leave this running in a terminal. On first start it indexes your repository with Tier 1 (tree-sitter, < 1 ms/file). From that point it watches each file individually and re-indexes only what changed.

You should see something like:

```
INFO  lip_daemon: journal replayed  entries=0
INFO  lip_daemon: listening         socket=/tmp/lip.sock
```

The daemon survives restarts — it replays its WAL journal on startup so the graph is immediately warm.

---

## Step 2 — Index your project

In a second terminal, send your project to the daemon:

```bash
lip index ./src
```

For a JSON event stream (useful for piping):

```bash
lip index ./src --json | lip push --registry https://registry.lip.dev
```

---

## Step 3 — Make your first query

```bash
# Search for a symbol by name
lip query symbols "AuthService"

# Find the blast radius of a change
lip query blast-radius "lip://local/src/auth.rs#AuthService.verifyToken"

# Go to definition at line 42, column 10
lip query definition file:///path/to/src/auth.rs 42 10
```

---

## Step 4 — Connect your editor (LSP)

LIP speaks standard LSP. Any editor that supports LSP works without a plugin.

```bash
# Terminal 2 — LSP bridge
lip lsp --socket /tmp/lip.sock
```

**VS Code** — add to `.vscode/settings.json`:

```json
{
  "languageServerExample.serverCommand": "lip",
  "languageServerExample.serverArgs": ["lsp", "--socket", "/tmp/lip.sock"]
}
```

**Neovim** (with `nvim-lspconfig`):

```lua
require('lspconfig').lip.setup({
  cmd = { 'lip', 'lsp', '--socket', '/tmp/lip.sock' },
  filetypes = { 'rust', 'typescript', 'python', 'dart' },
})
```

**Helix** — add to `languages.toml`:

```toml
[[language]]
name = "rust"
language-servers = ["lip"]

[language-server.lip]
command = "lip"
args = ["lsp", "--socket", "/tmp/lip.sock"]
```

---

## Step 5 — Connect an AI agent (MCP)

```bash
# Terminal 3 — MCP server
lip mcp --socket /tmp/lip.sock
```

Add to your MCP client config (e.g. Claude Code's `~/.claude/mcp.json`):

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

The agent now has access to `lip_blast_radius`, `lip_workspace_symbols`, `lip_definition`, `lip_references`, `lip_hover`, `lip_dead_symbols`, `lip_annotation_get`, and `lip_annotation_set`.

See [mcp-integration.md](mcp-integration.md) for details.

---

## Step 6 — Pre-build dependency slices (optional but recommended)

The first time you run on a new machine, build slices for your dependencies so they never need to be re-indexed:

```bash
# Cargo
lip slice --cargo

# npm
lip slice --npm

# Dart pub
lip slice --pub

# Build and push to a shared registry so teammates get it too
lip slice --cargo --push --registry https://registry.lip.dev
```

See [registry.md](registry.md) for running a private registry.

---

## What's running

After setup you have:

| Process | Command | Purpose |
|---------|---------|---------|
| Daemon | `lip daemon` | Query graph, file watcher, WAL journal |
| LSP bridge | `lip lsp` | Editor integration via standard LSP |
| MCP server | `lip mcp` | AI agent integration via MCP |

The daemon is the only required process. LSP and MCP bridges connect to it on demand.
