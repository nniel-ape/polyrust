# ─────────────────────────────────────────────────────────────────────────────
# Stage 1: Builder - Compile Rust application with optimized layer caching
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1.92.0-alpine AS builder

# Install build dependencies for Alpine/musl
RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    openssl-dev \
    openssl-libs-static

WORKDIR /build

# ── Layer 1: Copy workspace manifests ────────────────────────────────────────
COPY Cargo.toml Cargo.lock ./

# ── Layer 2: Copy all crate manifests (cache until deps change) ──────────────
COPY crates/polyrust-core/Cargo.toml ./crates/polyrust-core/
COPY crates/polyrust-market/Cargo.toml ./crates/polyrust-market/
COPY crates/polyrust-execution/Cargo.toml ./crates/polyrust-execution/
COPY crates/polyrust-store/Cargo.toml ./crates/polyrust-store/
COPY crates/polyrust-strategies/Cargo.toml ./crates/polyrust-strategies/
COPY crates/polyrust-dashboard/Cargo.toml ./crates/polyrust-dashboard/

# ── Layer 3: Create dummy sources to trigger dependency compilation ──────────
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && \
    mkdir -p crates/polyrust-core/src && echo "pub fn dummy() {}" > crates/polyrust-core/src/lib.rs && \
    mkdir -p crates/polyrust-market/src && echo "pub fn dummy() {}" > crates/polyrust-market/src/lib.rs && \
    mkdir -p crates/polyrust-execution/src && echo "pub fn dummy() {}" > crates/polyrust-execution/src/lib.rs && \
    mkdir -p crates/polyrust-store/src && echo "pub fn dummy() {}" > crates/polyrust-store/src/lib.rs && \
    mkdir -p crates/polyrust-strategies/src && echo "pub fn dummy() {}" > crates/polyrust-strategies/src/lib.rs && \
    mkdir -p crates/polyrust-dashboard/src && echo "pub fn dummy() {}" > crates/polyrust-dashboard/src/lib.rs

# ── Layer 4: Build dependencies ONLY (expensive, cached until deps change) ───
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release --locked

# ── Layer 5: Remove dummy sources (keep compiled deps in target/) ────────────
RUN rm -rf src crates/*/src

# ── Layer 6: Copy real source code ───────────────────────────────────────────
COPY src/ ./src/
COPY crates/ ./crates/
COPY examples/ ./examples/

# ── Layer 7: Build application (fast, only your code recompiles) ─────────────
RUN cargo build --release --locked --bin polyrust

# Verify binary exists (fail fast if build config is wrong)
RUN test -f target/release/polyrust || (echo "ERROR: Binary not found at target/release/polyrust" && exit 1)

# ─────────────────────────────────────────────────────────────────────────────
# Stage 2: Runtime - Minimal Alpine image
# ─────────────────────────────────────────────────────────────────────────────
FROM alpine:3.21

# Install minimal runtime dependencies
RUN apk add --no-cache \
    ca-certificates \
    libgcc \
    openssl

# Create non-root user for security
RUN adduser -D -u 1000 polyrust

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/polyrust /app/polyrust

# Create data directory for database persistence
RUN mkdir -p /app/data && chown -R polyrust:polyrust /app

# Switch to non-root user
USER polyrust

# Expose dashboard port
EXPOSE 3000

# Default command (can be overridden in docker-compose)
CMD ["./polyrust"]
