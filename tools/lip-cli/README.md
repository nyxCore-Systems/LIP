# lip-cli

Command-line interface for **LIP — Linked Incremental Protocol**, a persistent, incremental code intelligence daemon.

LIP keeps a live, queryable graph of your entire repository and updates only the **blast radius** of each change — the files and symbols actually affected — in milliseconds.

```sh
cargo install lip-cli
```

## Quick start

```sh
# Start the daemon
lip daemon --socket /tmp/lip.sock

# Query blast radius of a change
lip query blast-radius "lip://local/src/auth.rs#AuthService"

# Start the MCP server (Claude Code, Cursor, CKB, …)
lip mcp --socket /tmp/lip.sock

# Start the LSP bridge (any LSP editor)
lip lsp --socket /tmp/lip.sock
```

## Commands

| Command | Description |
|---------|-------------|
| `lip daemon` | Start the background daemon |
| `lip query` | Query the live graph (blast radius, symbols, search) |
| `lip mcp` | Expose the daemon as an MCP server for AI agents |
| `lip lsp` | Expose the daemon as an LSP server for editors |
| `lip index` | Force-index files or directories |
| `lip import` | Import a SCIP index artifact |
| `lip export` | Export the symbol graph |
| `lip slice` | Build dependency slices (Cargo, npm, pub, pip) |
| `lip fetch` | Download a slice from a registry |
| `lip push` | Upload a slice to a registry |
| `lip annotate` | Read/write symbol annotations |

## Links

- [Documentation](https://lip-sigma.vercel.app/docs)
- [Protocol Spec](https://lip-sigma.vercel.app/docs/spec)
- [MCP Integration](https://lip-sigma.vercel.app/docs/mcp)
- [GitHub](https://github.com/nyxCore-Systems/LIP)

## License

MIT
