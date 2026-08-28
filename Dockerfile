FROM rust:1-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release

FROM alpine:latest
LABEL org.opencontainers.image.description="Secure email MCP server over stdio — IMAP, SMTP, EWS and Microsoft Graph with OAuth2 and multi-account support"
LABEL org.opencontainers.image.source="https://github.com/tecnologicachile/mail-mcp"
COPY --from=builder /app/target/release/mail-mcp /mail-mcp
ENTRYPOINT ["/mail-mcp"]
