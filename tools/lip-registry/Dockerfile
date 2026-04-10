# Multi-stage build for lip-registry.
#
# Build context: workspace root
#   docker build -f tools/lip-registry/Dockerfile -t lip-registry .
#
# The builder uses rust:alpine whose toolchain targets musl by default, giving
# a fully static binary that runs on scratch with no runtime dependencies.

# ── Build ─────────────────────────────────────────────────────────────────────
FROM rust:alpine AS builder
WORKDIR /src

RUN apk add --no-cache musl-dev protobuf

# ---- dependency cache layer --------------------------------------------------
# Copy only manifest files and stub out source so cargo can pre-fetch and
# compile all dependencies. The real source is copied afterwards so that
# editing source files doesn't bust the dependency layer.
COPY Cargo.toml Cargo.lock ./
COPY bindings/rust/Cargo.toml      bindings/rust/Cargo.toml
COPY tools/lip-registry/Cargo.toml tools/lip-registry/Cargo.toml
COPY tools/lip-cli/Cargo.toml      tools/lip-cli/Cargo.toml

RUN mkdir -p bindings/rust/src tools/lip-registry/src tools/lip-cli/src \
 && printf 'pub fn _stub() {}\n' > bindings/rust/src/lib.rs \
 && printf 'fn main() {}\n'      > tools/lip-registry/src/main.rs \
 && printf 'fn main() {}\n'      > tools/lip-cli/src/main.rs \
 && cargo build --release -p lip-registry 2>/dev/null; true

# ---- real build --------------------------------------------------------------
COPY . .
# Touch source files so cargo sees them as newer than the stub build artifacts.
RUN touch bindings/rust/src/lib.rs tools/lip-registry/src/main.rs \
 && cargo build --release -p lip-registry

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM scratch
COPY --from=builder /src/target/release/lip-registry /lip-registry

EXPOSE 8080
VOLUME ["/slices"]
ENTRYPOINT ["/lip-registry", "serve", "--store", "/slices", "--port", "8080"]
