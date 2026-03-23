<p align="center">
  <h1 align="center">mail-mcp</h1>
  <p align="center">
    <strong>Production-ready email MCP server for AI agents</strong><br>
    IMAP + SMTP + EWS + Microsoft Graph API — built in Rust
  </p>
  <p align="center">
    <a href="https://github.com/tecnologicachile/mail-mcp/actions"><img src="https://img.shields.io/github/actions/workflow/status/tecnologicachile/mail-mcp/ci.yml?label=build" alt="Build"></a>
    <a href="https://github.com/tecnologicachile/mail-mcp/releases"><img src="https://img.shields.io/github/v/release/tecnologicachile/mail-mcp?label=release" alt="Release"></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/tecnologicachile/mail-mcp" alt="License"></a>
    <a href="https://github.com/tecnologicachile/mail-mcp/stargazers"><img src="https://img.shields.io/github/stars/tecnologicachile/mail-mcp?style=social" alt="Stars"></a>
  </p>
</p>

---

Most email MCP servers only do IMAP reads. This one does **everything**: read, search, send, reply, forward, bulk operations, Microsoft Graph API, and Exchange Web Services — with real OAuth2, multi-account, and multi-provider support. Written in Rust for speed and safety.

## Why This Project

| | mail-mcp | Typical email MCP |
|---|:---:|:---:|
| IMAP read/write | 18 tools | 3-5 tools |
| SMTP send/reply/forward | Yes | No or broken |
| Microsoft Graph API | Yes | No |
| EWS (Exchange Web Services) | Yes | No |
| OAuth2 (XOAUTH2) | Native | No |
| Multi-account | Yes | Single account |
| Microsoft 365 + Hotmail | Both work | Usually neither |
| Language | Rust (fast, safe) | TypeScript/Python |
| Tests | 40 unit + integration | Mocks only |

## Feature Matrix

| Provider | IMAP | SMTP | Graph API | EWS | OAuth2 | Multi-account |
|----------|:----:|:----:|:---------:|:---:|:------:|:-------------:|
| Microsoft 365 (enterprise) | Yes | Admin-dependent | Yes | **Yes** | Yes | Yes |
| Hotmail / Outlook.com | Yes | Blocked by MS | Yes | **Yes** | Yes | Yes |
| Gmail | Yes | Yes | — | — | Yes | Yes |
| Zoho | Yes | Yes | — | — | — | Yes |
| Fastmail | Yes | Yes | — | — | — | Yes |
| Any IMAP/SMTP server | Yes | Yes | — | — | — | Yes |

> **EWS is the simplest way to add Microsoft accounts** — single OAuth2 token for both reading and sending. Works even on tenants that block Graph API and IMAP.

## Quickstart — Let Claude Code do it

Copy and paste this prompt into Claude Code and it will install, compile, and configure everything for you:

```
Install and configure the mail-mcp MCP server from https://github.com/tecnologicachile/mail-mcp

1. Clone the repo, build with cargo build --release
2. Add the MCP server to .claude.json with the binary path
3. For Microsoft accounts: use EWS (simplest) — run device code flow with
   client_id d3590ed6-52b3-4102-aeff-aad2292ab01c and scope
   https://outlook.office365.com/EWS.AccessAsUser.All offline_access
   Then configure MAIL_EWS_<ID>_USER and MAIL_EWS_<ID>_REFRESH_TOKEN
4. For Gmail: configure MAIL_IMAP + MAIL_SMTP with App Password from
   https://myaccount.google.com/apppasswords
5. For Zoho: configure MAIL_IMAP + MAIL_SMTP with standard password
6. Enable write/send: MAIL_IMAP_WRITE_ENABLED=true, MAIL_SMTP_WRITE_ENABLED=true

My email accounts to configure:
- <your-email@example.com>
```

Replace the last line with your email(s). Claude Code will guide you through each step including the OAuth2 device code flow for Microsoft accounts.

## Manual Setup (2 minutes)

```bash
git clone https://github.com/tecnologicachile/mail-mcp.git
cd mail-mcp
cargo build --release
```

Add to your MCP client config (Claude Code, Cursor, etc.):

```json
{
  "mcpServers": {
    "mail": {
      "command": "./target/release/mail-mcp",
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

That's it. Your AI agent can now read, search, send, reply, and manage emails.

### Microsoft Account? Use Graph API

Microsoft blocks SMTP on personal accounts. Use Graph API instead:

```json
{
  "env": {
    "MAIL_IMAP_DEFAULT_HOST": "outlook.office365.com",
    "MAIL_IMAP_DEFAULT_USER": "you@hotmail.com",
    "MAIL_IMAP_DEFAULT_PASS": "your-app-password",
    "MAIL_OAUTH2_DEFAULT_PROVIDER": "microsoft",
    "MAIL_OAUTH2_DEFAULT_CLIENT_ID": "9e5f94bc-e8a4-4e73-b8be-63364c29d753",
    "MAIL_OAUTH2_DEFAULT_CLIENT_SECRET": "none",
    "MAIL_OAUTH2_DEFAULT_REFRESH_TOKEN": "<your-token>"
  }
}
```

Get your token in 1 minute with device code flow. See [Account Setup Guide](docs/account-setup.md).

## 31 MCP Tools

### Read (7 tools)

| Tool | What it does |
|------|-------------|
| `imap_list_accounts` | List configured accounts |
| `imap_verify_account` | Test connectivity and auth |
| `imap_list_mailboxes` | List folders |
| `imap_mailbox_status` | Message counts |
| `imap_search_messages` | Search with cursor pagination |
| `imap_get_message` | Parsed message (text, HTML, attachments) |
| `imap_get_message_raw` | RFC822 source |

### Write (11 tools)

| Tool | What it does |
|------|-------------|
| `imap_update_message_flags` | Add/remove flags |
| `imap_copy_message` | Copy (cross-account supported) |
| `imap_move_message` | Move to folder |
| `imap_delete_message` | Delete with confirmation |
| `imap_create_mailbox` | Create folder |
| `imap_delete_mailbox` | Delete folder |
| `imap_rename_mailbox` | Rename folder |
| `imap_append_message` | Append raw message |
| `imap_bulk_move` | Move up to 500 at once |
| `imap_bulk_delete` | Delete up to 500 at once |
| `imap_bulk_update_flags` | Flag up to 500 at once |

### Send (6 tools)

| Tool | What it does |
|------|-------------|
| `smtp_list_accounts` | List SMTP accounts |
| `smtp_send_message` | Send email (text/HTML, CC/BCC) |
| `smtp_reply_message` | Reply with threading headers |
| `smtp_forward_message` | Forward with original inline |
| `smtp_verify_account` | Test SMTP connectivity |
| `graph_send_message` | Send via Microsoft Graph API |

### EWS — Exchange Web Services (3 tools)

| Tool | What it does |
|------|-------------|
| `ews_search_messages` | Search emails via EWS (inbox, sent, drafts, etc.) |
| `ews_get_message` | Get full email content via EWS |
| `ews_send_message` | Send email via EWS |

### Bulk Operations (2 tools)

| Tool | What it does |
|------|-------------|
| `imap_search_and_move` | Search + move matches |
| `imap_search_and_delete` | Search + delete matches |

## Multi-Account

Configure as many accounts as you need:

```bash
# Gmail
MAIL_IMAP_GMAIL_HOST=imap.gmail.com
MAIL_IMAP_GMAIL_USER=me@gmail.com
MAIL_IMAP_GMAIL_PASS=app-password

# Microsoft 365
MAIL_IMAP_WORK_HOST=outlook.office365.com
MAIL_IMAP_WORK_USER=me@company.com
MAIL_OAUTH2_WORK_PROVIDER=microsoft
MAIL_OAUTH2_WORK_CLIENT_ID=your-client-id
MAIL_OAUTH2_WORK_CLIENT_SECRET=none
MAIL_OAUTH2_WORK_REFRESH_TOKEN=your-token

# Zoho
MAIL_IMAP_DEFAULT_HOST=imap.zoho.com
MAIL_IMAP_DEFAULT_USER=info@mydomain.com
MAIL_IMAP_DEFAULT_PASS=password
MAIL_SMTP_DEFAULT_HOST=smtp.zoho.com
MAIL_SMTP_DEFAULT_USER=info@mydomain.com
MAIL_SMTP_DEFAULT_PASS=password
MAIL_SMTP_DEFAULT_SECURE=starttls
```

Use `account_id` in tool calls: `"account_id": "gmail"`, `"account_id": "work"`, `"account_id": "default"`.

## Security

- **TLS enforced** on all connections (except localhost proxies)
- **Passwords in SecretString** — never logged or returned in responses
- **Write operations gated** — require explicit `MAIL_IMAP_WRITE_ENABLED=true`
- **Send operations gated** — require explicit `MAIL_SMTP_WRITE_ENABLED=true`
- **Delete confirmation** — requires `confirm: true`
- **HTML sanitized** with ammonia (prevents XSS)
- **Bounded outputs** — body text, HTML, attachments truncated to configurable limits
- **OAuth2 tokens cached** with 10-minute refresh margin
- **No secrets in responses** — credentials never exposed via MCP tools

## Configuration Reference

<details>
<summary>Full environment variable reference</summary>

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

### OAuth2 (per account)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_OAUTH2_<ID>_PROVIDER` | Yes | — | `google` or `microsoft` |
| `MAIL_OAUTH2_<ID>_CLIENT_ID` | Yes | — | OAuth2 client ID |
| `MAIL_OAUTH2_<ID>_CLIENT_SECRET` | Yes | — | Client secret (`none` for public clients) |
| `MAIL_OAUTH2_<ID>_REFRESH_TOKEN` | Yes | — | Refresh token |

### Graph API OAuth2 (per account)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_GRAPH_<ID>_PROVIDER` | Yes | — | `microsoft` |
| `MAIL_GRAPH_<ID>_CLIENT_ID` | Yes | — | OAuth2 client ID |
| `MAIL_GRAPH_<ID>_CLIENT_SECRET` | Yes | — | Client secret (`none` for public clients) |
| `MAIL_GRAPH_<ID>_REFRESH_TOKEN` | Yes | — | Refresh token (Mail.Send scope) |

### EWS — Exchange Web Services (per account, simplest for Microsoft)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_EWS_<ID>_USER` | Yes | — | Email address |
| `MAIL_EWS_<ID>_REFRESH_TOKEN` | Yes | — | OAuth2 refresh token (EWS scope) |
| `MAIL_EWS_<ID>_CLIENT_ID` | No | `d3590ed6...` (Microsoft Office) | OAuth2 client ID |
| `MAIL_EWS_<ID>_CLIENT_SECRET` | No | `none` | Client secret |

> **Tip:** EWS only needs 2 variables (USER + REFRESH_TOKEN). Client ID defaults to Microsoft Office which has all permissions pre-approved.

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

</details>

## Roadmap

- [x] IMAP read operations (search, fetch, parse)
- [x] IMAP write operations (copy, move, delete, flags)
- [x] IMAP bulk operations (up to 500 per call)
- [x] Cursor-based pagination with TTL
- [x] SMTP send, reply, forward
- [x] Microsoft Graph API (sendMail)
- [x] OAuth2 XOAUTH2 (Google + Microsoft)
- [x] Separate Graph API tokens for enterprise
- [x] Multi-account via environment variables
- [x] PDF text extraction from attachments
- [x] HTML sanitization (ammonia)
- [x] Provider setup documentation with direct links
- [x] Attachment sending (SMTP/Graph)
- [x] Reply with original attachments
- [x] CDATA sanitization (Zoho bug fix)
- [x] Email confirmation protocol (preview before send)
- [x] Token-optimized instructions (75% reduction)
- [x] On-demand setup guide tool
- [x] **EWS (Exchange Web Services)** — single token for read + send on Microsoft
- [x] EWS with Microsoft Office Client ID (works on restricted tenants)

### Next — Local cache with instant search
- [ ] **SQLite + FTS5 local email cache** — instant searches (<10ms vs 3-10s)
- [ ] **Incremental sync** — UIDVALIDITY + last UID delta sync
- [ ] **Connection pooling** — persistent IMAP sessions per account
- [ ] **Cross-account search** — search all accounts at once
- [ ] **Email statistics** — counts, top senders, activity by date

### Future
- [ ] Docker image
- [ ] npm/npx distribution
- [ ] Draft management
- [ ] Contact search
- [ ] IMAP IDLE (real-time notifications)
- [ ] Hosted documentation site

## Documentation

| Guide | Description |
|-------|-------------|
| [Account Setup](docs/account-setup.md) | Step-by-step per provider, OAuth2, App Passwords, Azure Client ID |
| [Tool Contract](docs/tool-contract.md) | Complete tool definitions and schemas |
| [Message ID Format](docs/message-id-format.md) | Stable message identifier format |
| [Cursor Pagination](docs/cursor-pagination.md) | Pagination behavior and expiration |
| [Security](docs/security.md) | Security features and best practices |
| [Advanced Configuration](docs/advanced-configuration.md) | Timeouts and performance tuning |

## Development

```bash
cargo test              # 40 unit tests
cargo fmt -- --check    # formatting
cargo clippy --all-targets -- -D warnings  # linting
```

See `AGENTS.md` for contributor guidelines.

## Contributing

Contributions welcome! Check out the [issues](https://github.com/tecnologicachile/mail-mcp/issues) for good first issues.

## License

MIT License — see [LICENSE](LICENSE) for details.
