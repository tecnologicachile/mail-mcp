# mail-imap-mcp-rs

A secure, full-featured Model Context Protocol (MCP) server for email over stdio. Provides IMAP read/write, SMTP sending, and Microsoft Graph API support with multi-account configuration, OAuth2 authentication, and cursor-based pagination.

## Features

- **IMAP**: Read, search, copy, move, flag, delete with cursor-based pagination
- **SMTP**: Send, reply, forward emails via STARTTLS/TLS
- **Microsoft Graph API**: Send emails from Microsoft accounts (personal and enterprise) where SMTP AUTH is blocked
- **OAuth2**: Native XOAUTH2 support for Google and Microsoft (device code flow)
- **Multi-account**: Configure multiple email accounts via environment variables
- **Multi-provider**: Microsoft 365, Hotmail/Outlook.com, Gmail, Zoho, Fastmail, and any standard IMAP/SMTP server
- **Secure by default**: TLS-only connections, passwords never logged, write operations gated
- **Rust-powered**: Fast, memory-safe async implementation with tokio

## Supported Providers

| Provider | IMAP | SMTP | Graph API | Auth |
|----------|------|------|-----------|------|
| Microsoft 365 (enterprise) | Yes | Depends on admin | Yes | OAuth2 / App Password |
| Hotmail / Outlook.com | Yes | Blocked by Microsoft | Yes | OAuth2 + App Password |
| Gmail | Yes | Yes | — | App Password |
| Zoho | Yes | Yes | — | Password |
| Fastmail | Yes | Yes | — | App Password |
| Any IMAP/SMTP server | Yes | Yes | — | Password |

## Quick Start

### MCP Configuration

```json
{
  "mcpServers": {
    "mail-imap": {
      "command": "/path/to/mail-imap-mcp-rs",
      "env": {
        "MAIL_IMAP_DEFAULT_HOST": "imap.gmail.com",
        "MAIL_IMAP_DEFAULT_USER": "you@gmail.com",
        "MAIL_IMAP_DEFAULT_PASS": "your-app-password",
        "MAIL_SMTP_DEFAULT_HOST": "smtp.gmail.com",
        "MAIL_SMTP_DEFAULT_PORT": "587",
        "MAIL_SMTP_DEFAULT_USER": "you@gmail.com",
        "MAIL_SMTP_DEFAULT_PASS": "your-app-password",
        "MAIL_SMTP_DEFAULT_SECURE": "starttls",
        "MAIL_IMAP_WRITE_ENABLED": "true",
        "MAIL_SMTP_WRITE_ENABLED": "true"
      }
    }
  }
}
```

### Microsoft Account (Graph API)

For Microsoft accounts where SMTP is blocked, use Graph API with OAuth2:

```json
{
  "mcpServers": {
    "mail-imap": {
      "command": "/path/to/mail-imap-mcp-rs",
      "env": {
        "MAIL_IMAP_DEFAULT_HOST": "outlook.office365.com",
        "MAIL_IMAP_DEFAULT_USER": "you@hotmail.com",
        "MAIL_IMAP_DEFAULT_PASS": "your-app-password",
        "MAIL_IMAP_WRITE_ENABLED": "true",
        "MAIL_SMTP_WRITE_ENABLED": "true",
        "MAIL_OAUTH2_DEFAULT_PROVIDER": "microsoft",
        "MAIL_OAUTH2_DEFAULT_CLIENT_ID": "9e5f94bc-e8a4-4e73-b8be-63364c29d753",
        "MAIL_OAUTH2_DEFAULT_CLIENT_SECRET": "none",
        "MAIL_OAUTH2_DEFAULT_REFRESH_TOKEN": "<your-refresh-token>"
      }
    }
  }
}
```

See [Account Setup Guide](docs/account-setup.md) for step-by-step instructions per provider, including how to obtain OAuth2 tokens via device code flow.

### Build from Source

```bash
cargo build --release
# Binary at target/release/mail-imap-mcp-rs
```

## Tools (25 total)

### IMAP Read (7)

| Tool | Purpose |
|------|---------|
| `imap_list_accounts` | List configured accounts |
| `imap_verify_account` | Test connectivity and authentication |
| `imap_list_mailboxes` | List mailboxes/folders |
| `imap_mailbox_status` | Get message counts |
| `imap_search_messages` | Search with cursor-based pagination |
| `imap_get_message` | Get parsed message (text, HTML, attachments) |
| `imap_get_message_raw` | Get RFC822 source |

### IMAP Write (11)

| Tool | Purpose |
|------|---------|
| `imap_update_message_flags` | Add/remove flags |
| `imap_copy_message` | Copy to mailbox (cross-account supported) |
| `imap_move_message` | Move to mailbox |
| `imap_delete_message` | Delete with confirmation |
| `imap_create_mailbox` | Create folder |
| `imap_delete_mailbox` | Delete folder |
| `imap_rename_mailbox` | Rename folder |
| `imap_append_message` | Append raw message |
| `imap_bulk_move` | Move up to 500 messages |
| `imap_bulk_delete` | Delete up to 500 messages |
| `imap_bulk_update_flags` | Update flags on up to 500 messages |

### SMTP (5)

| Tool | Purpose |
|------|---------|
| `smtp_list_accounts` | List configured SMTP accounts |
| `smtp_send_message` | Send new email (text/HTML, CC/BCC) |
| `smtp_reply_message` | Reply with proper threading headers |
| `smtp_forward_message` | Forward with original message inline |
| `smtp_verify_account` | Test SMTP connectivity |

### Microsoft Graph API (1)

| Tool | Purpose |
|------|---------|
| `graph_send_message` | Send via Graph API (required for hotmail/outlook.com) |

### Search & Bulk (2)

| Tool | Purpose |
|------|---------|
| `imap_search_and_move` | Search and move matches |
| `imap_search_and_delete` | Search and delete matches |

Write/send tools require `MAIL_IMAP_WRITE_ENABLED=true` and/or `MAIL_SMTP_WRITE_ENABLED=true`.

## Configuration Reference

### IMAP (per account)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_IMAP_<ID>_HOST` | Yes | — | IMAP server |
| `MAIL_IMAP_<ID>_PORT` | No | 993 | IMAP port |
| `MAIL_IMAP_<ID>_USER` | Yes | — | Username |
| `MAIL_IMAP_<ID>_PASS` | Yes* | — | Password (*optional with OAuth2) |
| `MAIL_IMAP_<ID>_SECURE` | No | true | Use TLS |

### SMTP (per account)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_SMTP_<ID>_HOST` | Yes | — | SMTP server |
| `MAIL_SMTP_<ID>_PORT` | No | 587 | SMTP port |
| `MAIL_SMTP_<ID>_USER` | Yes | — | Username |
| `MAIL_SMTP_<ID>_PASS` | No | — | Password (optional with OAuth2) |
| `MAIL_SMTP_<ID>_SECURE` | No | starttls | `starttls`, `tls`, or `plain` |

### OAuth2 (per account, for IMAP XOAUTH2)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_OAUTH2_<ID>_PROVIDER` | Yes | — | `google` or `microsoft` |
| `MAIL_OAUTH2_<ID>_CLIENT_ID` | Yes | — | OAuth2 client ID |
| `MAIL_OAUTH2_<ID>_CLIENT_SECRET` | Yes | — | Client secret (`none` for public clients) |
| `MAIL_OAUTH2_<ID>_REFRESH_TOKEN` | Yes | — | Refresh token |

### Graph API OAuth2 (per account, for Microsoft Graph sending)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_GRAPH_<ID>_PROVIDER` | Yes | — | `microsoft` |
| `MAIL_GRAPH_<ID>_CLIENT_ID` | Yes | — | OAuth2 client ID |
| `MAIL_GRAPH_<ID>_CLIENT_SECRET` | Yes | — | Client secret (`none` for public clients) |
| `MAIL_GRAPH_<ID>_REFRESH_TOKEN` | Yes | — | Refresh token (Mail.Send scope) |

> **Note:** Enterprise Microsoft 365 accounts require separate tokens for IMAP and Graph API due to Microsoft's single-resource token restriction. Personal accounts (hotmail/outlook.com) can use a single token for both.

### Global Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `MAIL_IMAP_WRITE_ENABLED` | false | Enable IMAP write operations |
| `MAIL_SMTP_WRITE_ENABLED` | false | Enable SMTP/Graph send operations |
| `MAIL_SMTP_SAVE_SENT` | true | Save sent emails to IMAP Sent folder |
| `MAIL_SMTP_TIMEOUT_MS` | 30000 | SMTP operation timeout |
| `MAIL_IMAP_CONNECT_TIMEOUT_MS` | 30000 | TCP connection timeout |
| `MAIL_IMAP_GREETING_TIMEOUT_MS` | 15000 | TLS/greeting timeout |
| `MAIL_IMAP_SOCKET_TIMEOUT_MS` | 300000 | Socket I/O timeout |

## Documentation

- [Account Setup Guide](docs/account-setup.md) — Step-by-step per provider, OAuth2 device code flow, App Passwords, Azure Client ID registration
- [Tool Contract](docs/tool-contract.md) — Complete tool definitions, input/output schemas
- [Message ID Format](docs/message-id-format.md) — Stable message identifier format
- [Cursor Pagination](docs/cursor-pagination.md) — Pagination behavior and expiration
- [Security](docs/security.md) — Security features and best practices
- [Advanced Configuration](docs/advanced-configuration.md) — Timeouts and performance tuning

## Development

```bash
cargo test          # 40 unit tests
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

See `AGENTS.md` for contributor guidelines.

## License

MIT License — see [LICENSE](LICENSE) for details.
