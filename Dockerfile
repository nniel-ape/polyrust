# ─────────────────────────────────────────────────────────────────────────────
# Stage 1: Builder - Compile Rust application with musl target
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1.92.0-alpine AS builder

# Install build dependencies for Alpine/musl
RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    openssl-dev \
    openssl-libs-static

WORKDIR /build

# Copy workspace manifests first (leverage Docker layer caching)
COPY Cargo.toml Cargo.lock ./

# Copy all crate sources
COPY crates/ ./crates/
COPY src/ ./src/
COPY examples/ ./examples/

# Build release binary with musl target for static linking
ENV RUSTFLAGS="-C target-feature=-crt-static"
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
