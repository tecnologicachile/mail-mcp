# Build Stage 1: Planner
FROM lukemathwalker/cargo-chef:latest-rust-1.85-alpine AS chef
# Install build dependencies
RUN apk add --no-cache musl-dev
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build Stage 2: Builder
FROM chef AS builder
# Use TARGETARCH to determine the correct binary path for both native and cross-builds
ARG TARGETARCH
RUN case "$TARGETARCH" in \
    "amd64") echo "x86_64-unknown-linux-musl" > /target.txt ;; \
    "arm64") echo "aarch64-unknown-linux-musl" > /target.txt ;; \
    *) echo "x86_64-unknown-linux-musl" > /target.txt ;; \
    esac

# Pre-install the rust target
RUN rustup target add $(cat /target.txt)

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - cached layer
RUN cargo chef cook --release --target $(cat /target.txt) --recipe-path recipe.json

# Copy source and build the application
COPY . .
RUN cargo build --release --bin mail-mcp --target $(cat /target.txt) && \
    cp target/$(cat /target.txt)/release/mail-mcp /mail-mcp-bin

# Final Stage: Runtime
# Using scratch for the smallest possible security footprint
FROM scratch

# Document the purpose of the image
LABEL org.opencontainers.image.description="Secure IMAP MCP server over stdio with cursor-based pagination, multi-account support, and TLS-only connections"
LABEL org.opencontainers.image.source="https://github.com/tecnologicachile/mail-mcp"

# Copy CA certificates from the builder stage
# Required for TLS connections to IMAP/SMTP servers
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy the statically linked binary
COPY --from=builder /mail-mcp-bin /mail-mcp

# The server communicates via stdio
ENTRYPOINT ["/mail-mcp"]
