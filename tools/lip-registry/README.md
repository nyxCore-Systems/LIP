# lip-registry

Self-hostable HTTP blob store for **LIP dependency slices** — pre-built, content-addressed symbol indexes for external packages.

Index a dependency once. Share it across your whole team. Never re-index `node_modules/`, `~/.cargo/registry/`, or `~/.pub-cache/` again.

```sh
cargo install lip-registry
```

## Quick start

```sh
# Serve slices from a local directory
lip-registry serve --store /var/lib/lip/slices --port 8080

# Push slices from your project
lip slice --cargo --push --registry http://localhost:8080

# Fetch a specific slice
lip fetch <sha256-hash> --registry http://localhost:8080
```

## Docker

```sh
docker build -f tools/lip-registry/Dockerfile -t lip-registry .

docker run -d \
  --name lip-registry \
  -p 8080:8080 \
  -v lip-slices:/slices \
  lip-registry
```

## API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/slices/:hash` | Download a slice by SHA-256 hash |
| `PUT` | `/slices/:hash` | Upload a slice |
| `GET` | `/health` | Health check |

All slices are keyed by their SHA-256 content hash. The server rejects uploads where the hash doesn't match the content.

## Pointing clients at your registry

```sh
# Push slices on build
lip slice --cargo --push --registry https://your-registry.internal

# Fetch on clone / CI
lip fetch <hash> --registry https://your-registry.internal

# Daemon (env var)
LIP_REGISTRY=https://your-registry.internal lip daemon
```

## Links

- [Registry & Slices docs](https://lip-sigma.vercel.app/docs/registry)
- [lip-cli on crates.io](https://crates.io/crates/lip-cli)
- [GitHub](https://github.com/nyxCore-Systems/LIP)

## License

MIT
