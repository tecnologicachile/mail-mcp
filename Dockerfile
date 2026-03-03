FROM rust:1-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release

FROM alpine:latest
LABEL org.opencontainers.image.description="Secure IMAP MCP server over stdio with cursor-based pagination, multi-account support, and TLS-only connections"
COPY --from=builder /app/target/release/mail-imap-mcp-rs /mail-imap-mcp-rs
ENTRYPOINT ["/mail-imap-mcp-rs"]
