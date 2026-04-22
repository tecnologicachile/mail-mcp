FROM rust:1-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release

FROM python:3.12-alpine
LABEL org.opencontainers.image.description="mail-mcp wrapped with mcpo for Open WebUI/OpenAPI access"
RUN apk add --no-cache bash
WORKDIR /app
COPY --from=builder /app/target/release/mail-mcp /usr/local/bin/mail-mcp
COPY scripts/run-open-webui-adapter.sh /usr/local/bin/run-open-webui-adapter.sh
RUN chmod +x /usr/local/bin/run-open-webui-adapter.sh \
    && pip install --no-cache-dir mcpo
ENV MAIL_MCP_BIN=/usr/local/bin/mail-mcp \
    MCPO_HOST=0.0.0.0 \
    MCPO_PORT=8000 \
    MCPO_LOG_LEVEL=INFO
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/run-open-webui-adapter.sh"]