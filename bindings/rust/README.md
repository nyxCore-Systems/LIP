# lip-core

Rust library crate for **LIP — Linked Incremental Protocol**, a persistent, incremental code intelligence daemon.

This crate contains the daemon runtime, query graph, WAL journal, Tier 1/2/3 indexers, MCP/LSP wire protocol, semantic embedding layer, and registry client. It is the engine behind the [`lip-cli`](https://crates.io/crates/lip-cli) binary.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
lip = { package = "lip-core", version = "2.0.1" }
```

The package alias `lip` keeps import paths clean:

```rust
use lip::daemon::LipDaemon;
use lip::query_graph::{ClientMessage, ServerMessage};
use lip::schema::OwnedSymbolInfo;
```

## Links

- [Documentation](https://lip-sigma.vercel.app/docs)
- [Protocol Spec](https://lip-sigma.vercel.app/docs/spec)
- [GitHub](https://github.com/nyxCore-Systems/LIP)
- [lip-cli on crates.io](https://crates.io/crates/lip-cli)

## License

MIT
