# Advanced Configuration

This document covers advanced configuration options beyond the basic account setup.

## TOML Configuration

Use [guided account setup](account-setup.md#guided-setup-with-toml) to create and
manage the file, or edit it directly. This section is the field reference for
the schema in [`src/config_file.rs`](../src/config_file.rs); runtime defaults
and account requirements come from [`src/config.rs`](../src/config.rs).

### Configuration Sources and Precedence

The configuration path is selected in this order:

1. `mail-mcp --config PATH`, including for `config` and `accounts` commands.
2. A non-empty `MAIL_CONFIG_FILE` in the process environment. The value `none`
   selects no file at all (see below).
3. `~/.mail-mcp/config.toml`, using `HOME`, or `USERPROFILE` when `HOME` is unset
   or empty.

Leading `~/` and `~\` in a configuration path expand to that home directory.
Relative paths resolve from the process working directory. Prefer an absolute
path in MCP-client configuration. A missing explicitly selected file is a
startup error, not a fallback to environment-only mode. Unreadable or malformed
TOML also fails instead of being silently ignored.

The default file applies to every `mail-mcp` process the user starts without
`--config` or `MAIL_CONFIG_FILE`, so its accounts and write settings reach every
MCP client on the machine. A client configured only through environment
variables can opt out with `MAIL_CONFIG_FILE=none`: the server then reads only
the process environment and the legacy `.env` fallback. `--config` still wins
over `none`. Configuration and account
commands need a file, so they fail under `none` unless `--config` is given. A
file that is literally named `none` can be selected as `./none`.

For each recognized setting, a process `MAIL_*` variable overrides the matching
TOML field. The merge is field-by-field: an environment password can replace a
file password without repeating its host or username, and environment-only
accounts can coexist with file accounts. `accounts list` and `config validate`
report environment variable names without printing their values.

Implicit `.env` discovery runs only when no path is explicitly selected and the
default TOML file does not exist. Process variables win over `.env` values.
Once a TOML file is selected, an unrelated `.env` in the working directory is
not loaded. `MAIL_CONFIG_FILE` must be in the process environment, not only in
that implicit `.env`. Explicitly exporting variables or using Docker's
`--env-file` still supplies process variables and therefore overrides TOML.

The `.env` file is found the same way as before: in the working directory or
the nearest parent that has one. Its first occurrence of a variable wins, a
leading UTF-8 BOM is ignored, and reading stops at the first malformed line.
Only recognized `MAIL_*` settings are read from it, and the process environment
is not modified. Variables used by other components, such as `RUST_LOG` or proxy
settings, must be exported in the process environment instead.

An empty or whitespace-only `MAIL_*` value counts as unset, in the process
environment and in `.env`: the TOML value or the built-in default applies. A
blank `MAIL_ATTACHMENT_UPLOAD_DIR=` left in an MCP client entry therefore does
not remove a directory restriction set in the file.

With neither `HOME` nor `USERPROFILE` and no explicit path, the MCP server can
still start from environment variables and the legacy `.env` fallback.
Configuration and account management commands require a home directory or an
explicit path; they do not invent a file in the working directory.

For example, create a separate configuration with:

```bash
mail-mcp --config /absolute/path/mail-config.toml config init
mail-mcp --config /absolute/path/mail-config.toml accounts add
mail-mcp --config /absolute/path/mail-config.toml config validate
```

An MCP client can select it without embedding credentials:

```json
{
  "mcpServers": {
    "mail": {
      "command": "mail-mcp",
      "env": {
        "MAIL_CONFIG_FILE": "/absolute/path/mail-config.toml"
      }
    }
  }
}
```

Alternatively, use `"args": ["--config", "/absolute/path/mail-config.toml"]`.
For the default file, only `"command": "mail-mcp"` is needed inside the server
entry.

### File Structure and Account IDs

The only top-level tables are `[settings]` and `[accounts]`. Account
configuration lives in `[accounts.ID.imap]`, `[accounts.ID.smtp]`,
`[accounts.ID.oauth2]`, `[accounts.ID.graph_oauth2]`, and `[accounts.ID.ews]`.
Include only the protocols needed by an account. IMAP-only, SMTP-only,
Graph-only, and EWS-only configurations do not need dummy sections for other
protocols.

TOML account IDs use lowercase ASCII letters, digits, and underscores, start
with a letter or digit, and are at most 64 characters. Use IDs such as
`default`, `work`, or `personal_gmail`; a trailing underscore is also supported.
IDs must be unique. The ID maps to an uppercase environment segment:
`accounts.work.imap.pass` maps to `MAIL_IMAP_WORK_PASS`. MCP tool calls use the
lowercase account ID; their default is `default`.

Unknown keys and wrong TOML types are rejected even when an environment
variable would override the value. Use unquoted booleans and integers; ports
must fit `0..=65535`, and timeouts and counts must be non-negative integers.
The guided prompts require ports in `1..=65535`. Files must be UTF-8 without a
BOM and no larger than 1 MiB.

### Password Account Example

Replace the example addresses and secret placeholders with account credentials.
This example keeps all write gates disabled:

```toml
[settings]
write_enabled = false
drafts_enabled = false
smtp_write_enabled = false
smtp_connect_timeout_ms = 30000
smtp_send_timeout_ms = 300000
connect_timeout_ms = 30000
greeting_timeout_ms = 15000
socket_timeout_ms = 300000
cursor_ttl_seconds = 600
cursor_max_entries = 512
max_mailboxes = 200
search_concurrency = 4
# Leave smtp_save_sent unset to keep provider-aware defaults.
# Optional directories: replace these paths before uncommenting.
# attachment_download_dir = "/absolute/path/mail-downloads"
# attachment_upload_dir = "/absolute/path/mail-outbox"

[accounts.default.imap]
host = "imap.gmail.com"
port = 993
secure = true
user = "reader@gmail.com"
pass = "<gmail-app-password>"

[accounts.default.smtp]
host = "smtp.gmail.com"
port = 587
security = "starttls"
user = "reader@gmail.com"
pass = "<gmail-app-password>"
save_sent = false
from_email = "reader@gmail.com"
```

`attachment_upload_dir`, when set, must name an existing directory. It confines
outbound file attachments; see [outbound attachment scope](security.md#outbound-attachment-scope).
Transport settings (`MAIL_MCP_TRANSPORT` and `MAIL_MCP_HTTP_*`) are not TOML
fields: they are read only from the process environment or `.env`, so a shared
file cannot move stdio clients to HTTP. See
[remote HTTP transport](#remote-http-transport) before exposing a listener.

### OAuth2, Graph, and EWS Examples

These sections can coexist in one file under separate account IDs. Tokens must
already have been obtained with the permissions required by the corresponding
API; the CLI does not acquire initial tokens. The generic `oauth2` section is
for IMAP/SMTP authentication. `graph_oauth2` and `ews` use separate credentials
because their token scopes differ. For a public client, `client_secret = "none"`
is a literal sentinel, not a secret placeholder.

```toml
[accounts.work.imap]
host = "outlook.office365.com"
port = 993
secure = true
user = "reader@example.com"

[accounts.work.smtp]
host = "smtp.office365.com"
port = 587
security = "starttls"
user = "reader@example.com"

[accounts.work.oauth2]
provider = "microsoft"
client_id = "<registered-client-id>"
client_secret = "none"
refresh_token = "<imap-smtp-refresh-token>"

[accounts.graph.graph_oauth2]
provider = "microsoft"
client_id = "<registered-graph-client-id>"
client_secret = "none"
refresh_token = "<graph-refresh-token>"

[accounts.exchange.ews]
user = "reader@example.com"
client_id = "d3590ed6-52b3-4102-aeff-aad2292ab01c"
client_secret = "none"
refresh_token = "<ews-refresh-token>"
```

Omit `imap.pass` to select OAuth2 for IMAP. If both a password and generic
OAuth2 credentials are supplied, IMAP uses the password; SMTP uses OAuth2
whenever the generic OAuth2 configuration exists. A `graph_oauth2` section
does not supply IMAP/SMTP authentication.

### Supported Fields

All `[settings]` fields are optional. Omitted fields use the defaults below.
Account fields are required only for the protocol being configured, and may
be supplied by their corresponding process environment variable instead.

#### `[settings]`

| Field | TOML type | Default / effect | Environment equivalent |
|---|---|---|---|
| `write_enabled` | boolean | `false`; enables IMAP writes | `MAIL_IMAP_WRITE_ENABLED` |
| `drafts_enabled` | boolean | `false`; enables draft tools independently of IMAP writes | `MAIL_DRAFTS_ENABLED` |
| `smtp_write_enabled` | boolean | `false`; enables sending, including `draft_send` | `MAIL_SMTP_WRITE_ENABLED` |
| `smtp_save_sent` | boolean | Unset; account override wins, then this setting, then provider default | `MAIL_SMTP_SAVE_SENT` |
| `smtp_connect_timeout_ms` | integer | `30000`; SMTP connection/authentication timeout | `MAIL_SMTP_CONNECT_TIMEOUT_MS` |
| `smtp_send_timeout_ms` | integer | `300000`; SMTP send timeout | `MAIL_SMTP_SEND_TIMEOUT_MS` |
| `connect_timeout_ms` | integer | `30000`; IMAP TCP connection timeout | `MAIL_IMAP_CONNECT_TIMEOUT_MS` |
| `greeting_timeout_ms` | integer | `15000`; IMAP TLS, greeting, and authentication timeout | `MAIL_IMAP_GREETING_TIMEOUT_MS` |
| `socket_timeout_ms` | integer | `300000`; IMAP operation timeout | `MAIL_IMAP_SOCKET_TIMEOUT_MS` |
| `cursor_ttl_seconds` | integer | `600`; cursor lifetime | `MAIL_IMAP_CURSOR_TTL_SECONDS` |
| `cursor_max_entries` | integer | `512`; cursor capacity | `MAIL_IMAP_CURSOR_MAX_ENTRIES` |
| `max_mailboxes` | integer | `200`, clamped to `1..=10000` | `MAIL_IMAP_MAX_MAILBOXES` |
| `search_concurrency` | integer | `4`, clamped to `1..=8` | `MAIL_IMAP_SEARCH_CONCURRENCY` |
| `attachment_download_dir` | string | Unset; downloads use the system temp directory unless the tool supplies `output_dir` | `MAIL_ATTACHMENT_DOWNLOAD_DIR` |
| `attachment_upload_dir` | string | Unset; any readable outbound attachment path is allowed | `MAIL_ATTACHMENT_UPLOAD_DIR` |

`MAIL_SMTP_TIMEOUT_MS` is the legacy environment name for the send timeout.
An explicit `MAIL_SMTP_SEND_TIMEOUT_MS` wins; otherwise the legacy process
variable overrides the file's `smtp_send_timeout_ms`. There is no
`smtp_timeout_ms` TOML field.

#### `[accounts.ID.imap]`

Environment prefix: `MAIL_IMAP_<ID>_`, with uppercase `ID`.

| Field | TOML type | Required / default | Environment suffix |
|---|---|---|---|
| `host` | string | Required; enables IMAP account discovery | `HOST` |
| `port` | integer | `993` | `PORT` |
| `secure` | boolean | `true`; implicit TLS. `false` uses plaintext, only suitable for trusted local proxies/networks | `SECURE` |
| `user` | string | Required | `USER` |
| `pass` | string | Required unless the account has `oauth2`; omit for IMAP OAuth2 | `PASS` |

IMAP STARTTLS is not supported.

#### `[accounts.ID.smtp]`

Environment prefix: `MAIL_SMTP_<ID>_`, with uppercase `ID`.

| Field | TOML type | Required / default | Environment suffix |
|---|---|---|---|
| `host` | string | Required; enables SMTP account discovery | `HOST` |
| `port` | integer | `587`; set `465` explicitly for conventional implicit TLS | `PORT` |
| `user` | string | Required | `USER` |
| `pass` | string | Optional in configuration; supply for password authentication, omit for OAuth2 | `PASS` |
| `security` | string | `"starttls"`; also `"tls"` (alias `"ssl"`) or `"plain"` (alias `"none"`). Plaintext is only suitable for trusted networks | `SECURE` |
| `save_sent` | boolean | Unset; falls back to `settings.smtp_save_sent`, then `false` for Gmail/Zoho and `true` for other hosts | `SAVE_SENT` |
| `from_email` | string | Defaults to `user`; optional sender-address override, validated when set | `FROM_EMAIL` |

The TOML key is `security`, not `secure`, for SMTP.

#### `[accounts.ID.oauth2]` and `[accounts.ID.graph_oauth2]`

Environment prefixes are `MAIL_OAUTH2_<ID>_` and `MAIL_GRAPH_<ID>_` respectively.
Both sections have the same fields:

| Field | TOML type | Required / default | Environment suffix |
|---|---|---|---|
| `provider` | string | Required; `"google"` or `"microsoft"` for IMAP/SMTP; use `"microsoft"` for Graph | `PROVIDER` |
| `client_id` | string | Required | `CLIENT_ID` |
| `client_secret` | string | Required; use `"none"` for a public client | `CLIENT_SECRET` |
| `refresh_token` | string | Required; token with scopes for the selected protocol | `REFRESH_TOKEN` |

Graph sending needs `Mail.Send`; drafts need `Mail.ReadWrite`. The read-only
`accounts test` Graph check lists drafts, so send-only permission is not enough
for that check.

#### `[accounts.ID.ews]`

Environment prefix: `MAIL_EWS_<ID>_`, with uppercase `ID`. EWS uses the Microsoft
provider; there is no `provider` field in this section.

| Field | TOML type | Required / default | Environment suffix |
|---|---|---|---|
| `user` | string | Required to discover the EWS account; email address | `USER` |
| `client_id` | string | `"d3590ed6-52b3-4102-aeff-aad2292ab01c"` (Microsoft Office public client) | `CLIENT_ID` |
| `client_secret` | string | `"none"` | `CLIENT_SECRET` |
| `refresh_token` | string | Required to authenticate; obtain with the EWS scope | `REFRESH_TOKEN` |

Local validation does not prove a token's scopes, validity, or tenant approval.
Use `accounts test ID` for connection/authentication checks. See
[configuration file security](security.md#configuration-file-security) for
permissions, plaintext secret storage, and comment-preserving saves.

## Cursor Pagination Configuration

### Cursor TTL

Cursors expire after a configurable time-to-live. Shorter values improve memory usage but require more frequent re-searches.

```bash
# Default: 600 seconds (10 minutes)
MAIL_IMAP_CURSOR_TTL_SECONDS=600
```

**Trade-offs:**
- Shorter TTL: Less memory usage, more frequent re-searches needed
- Longer TTL: Better user experience for slow workflows, higher memory usage

### Cursor Storage Limit

Maximum number of cursors stored in memory. When exceeded, oldest unused cursors are evicted (LRU).

```bash
# Default: 512 entries
MAIL_IMAP_CURSOR_MAX_ENTRIES=512
```

**Trade-offs:**
- Lower limit: Less memory usage, cursors may expire sooner
- Higher limit: Supports more concurrent searches, higher memory usage

### Mailbox List Limit

`imap_list_mailboxes` returns at most this many mailboxes. Accounts with a deep folder tree can exceed the default; the response's `total` and `truncated` fields show when that happens.

```bash
# Default: 200 (clamped to 1..=10000)
MAIL_IMAP_MAX_MAILBOXES=1000
```

### Search Across All Mailboxes

`imap_search_all_mailboxes` runs one search in every selectable mailbox, spread over a few parallel IMAP connections. On iCloud each mailbox costs roughly one second (EXAMINE + UID SEARCH), so ~220 folders take about a minute with 4 connections and about half that with 8. Providers cap concurrent connections per account; 8 stayed well within iCloud's limit in testing.

```bash
# Default: 4 (clamped to 1..=8)
MAIL_IMAP_SEARCH_CONCURRENCY=8
```

## Timeout Configuration

All timeouts are in milliseconds. Adjust based on network conditions and server performance.

### Connection Timeout

Time to establish TCP connection and TLS handshake.

```bash
# Default: 30000 ms (30 seconds)
MAIL_IMAP_CONNECT_TIMEOUT_MS=30000
```

**When to increase:**
- High-latency networks
- Slow TLS handshake on constrained systems
- Network congestion issues

### Greeting Timeout

Time to receive IMAP server greeting after connection.

```bash
# Default: 15000 ms (15 seconds)
MAIL_IMAP_GREETING_TIMEOUT_MS=15000
```

**When to increase:**
- Overloaded IMAP servers
- Slow authentication backends
- Geographically distant servers

### Socket Timeout

Time for individual socket operations (idle, read, write). This is the timeout for most IMAP commands.

```bash
# Default: 300000 ms (5 minutes)
MAIL_IMAP_SOCKET_TIMEOUT_MS=300000
```

**When to increase:**
- Processing large mailboxes
- Slow message retrieval
- Complex search operations

**When to decrease:**
- Require faster failure detection
- Implement custom retry logic

## Write Operations Configuration

### Enabling Write Operations

```bash
# Default: false
MAIL_IMAP_WRITE_ENABLED=true
```

**Enables:**
- `imap_update_message_flags` - Flag operations
- `imap_copy_message` - Copy messages
- `imap_move_message` - Move messages
- `imap_delete_message` - Delete messages

**Security consideration:** Only enable if you need these operations. The server is safer with writes disabled.

## Per-Account Configuration

### Multiple Accounts

Configure multiple IMAP accounts with unique identifiers:

```bash
# Default account
MAIL_IMAP_DEFAULT_HOST=imap.gmail.com
MAIL_IMAP_DEFAULT_USER=user@gmail.com
MAIL_IMAP_DEFAULT_PASS=app-password-1
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_SECURE=true

# Work account
MAIL_IMAP_WORK_HOST=outlook.office365.com
MAIL_IMAP_WORK_USER=user@company.com
MAIL_IMAP_WORK_PASS=app-password-2
MAIL_IMAP_WORK_PORT=993
MAIL_IMAP_WORK_SECURE=true

# Personal account
MAIL_IMAP_PERSONAL_HOST=imap.fastmail.com
MAIL_IMAP_PERSONAL_USER=user@fastmail.com
MAIL_IMAP_PERSONAL_PASS=app-password-3
MAIL_IMAP_PERSONAL_PORT=993
MAIL_IMAP_PERSONAL_SECURE=true
```

### Account ID Rules

- TOML and CLI IDs follow the [configuration ID rules](#file-structure-and-account-ids).
- Environment examples use uppercase segments such as `WORK`; tool calls use `work`.
- MCP tool input accepts `^[A-Za-z0-9_-]{1,64}$`, but the ID must match a configured account.
- `default` is the default account ID when a tool call omits it.

### Port and Security

Standard IMAP configurations:

```bash
# IMAPS (implicit TLS) - recommended
MAIL_IMAP_<ACCOUNT>_PORT=993
MAIL_IMAP_<ACCOUNT>_SECURE=true

# IMAP with STARTTLS - not supported
# Only implicit TLS (IMAPS) is supported
```

## Environment Variable Priority

Process variables override matching TOML fields. Without an explicitly selected
or existing default TOML file, process variables override legacy `.env` values.
See [configuration sources and precedence](#configuration-sources-and-precedence)
for path selection and environment-only deployments.

## Trouleshooting Configuration Issues

### Account Not Found

```
Error: invalid input: account "unknown" not configured
```

**Cause:** The requested account ID does not exist in the effective configuration.

**Resolution:** Run `mail-mcp accounts list` with the same path and environment
as the server. Check the lowercase ID and any reported environment overrides.

### Missing Required Variables

```
Error: missing required environment variable: MAIL_IMAP_DEFAULT_HOST
```

**Cause:** Required configuration not set.

**Resolution:** Set all required variables: `HOST`, `USER`, `PASS`.

### Connection Timeout

```
Error: operation timed out: tcp connect timeout
```

**Cause:** Network connectivity or server unreachable.

**Resolution:**
1. Verify `MAIL_IMAP_<ACCOUNT>_HOST` is correct
2. Check network connectivity to host
3. Increase `MAIL_IMAP_CONNECT_TIMEOUT_MS`
4. Verify firewall allows outbound connections on port 993

### Authentication Failed

```
Error: authentication failed: [AUTHENTICATIONFAILED] Authentication failed.
```

**Cause:** Invalid credentials or authentication method.

**Resolution:**
1. Verify `USER` and `PASS` are correct
2. Use app-specific password for Gmail/Outlook
3. Check account allows IMAP access
4. Verify account not locked or requiring 2FA challenge

## Performance Tuning

### High-Volume Workloads

For high-volume operations across large mailboxes:

```bash
# Increase cursor capacity
MAIL_IMAP_CURSOR_MAX_ENTRIES=1024

# Longer cursor TTL for batch processing
MAIL_IMAP_CURSOR_TTL_SECONDS=1800

# Longer socket timeout for large searches
MAIL_IMAP_SOCKET_TIMEOUT_MS=600000
```

### Low-Latency Interactive Use

For interactive use with quick responses:

```bash
# Shorter timeouts for faster failure detection
MAIL_IMAP_CONNECT_TIMEOUT_MS=15000
MAIL_IMAP_GREETING_TIMEOUT_MS=10000
MAIL_IMAP_SOCKET_TIMEOUT_MS=120000

# Fewer stored cursors
MAIL_IMAP_CURSOR_MAX_ENTRIES=256
```

### Memory-Constrained Environments

For environments with limited memory:

```bash
# Fewer cursors stored
MAIL_IMAP_CURSOR_MAX_ENTRIES=128

# Shorter cursor TTL
MAIL_IMAP_CURSOR_TTL_SECONDS=300

# Tighter timeouts
MAIL_IMAP_SOCKET_TIMEOUT_MS=180000
```

## Remote HTTP Transport

By default the server speaks MCP over stdio and is spawned by the client. To
run it on another machine — for example next to an MCP gateway that several
clients share — serve MCP streamable HTTP instead:

```bash
MAIL_MCP_TRANSPORT=http        # default: stdio
MAIL_MCP_HTTP_HOST=0.0.0.0     # default: 127.0.0.1
MAIL_MCP_HTTP_PORT=8000        # default: 8000
MAIL_MCP_HTTP_PATH=/mcp        # default: /mcp
```

These are environment variables only, not TOML fields.

The endpoint runs in stateless mode (no `Mcp-Session-Id`); all requests share
one server instance, so pagination cursors and OAuth2 tokens persist across
requests. SIGINT and SIGTERM shut it down gracefully.

**The HTTP endpoint has no authentication.** Anyone who can reach it can read
— and, with the write flags on, send and delete — mail with the configured
accounts. Keep the loopback default unless the port is reachable only from
something that authenticates callers: an MCP gateway with OAuth, a reverse
proxy with auth, or a firewall that admits only that gateway.

## Docker-Specific Configuration

When running in Docker, ensure environment variables are passed correctly:

```bash
docker run --rm -i \
  --env-file .env \
  -e MAIL_IMAP_CONNECT_TIMEOUT_MS=45000 \
  mail-mcp
```

File-based env loading (`--env-file`) takes precedence over `-e` flags.

If `imap_verify_account` or `smtp_verify_account` time out inside a container
while the same host connects fine, check for an MTU mismatch: TCP connects and
the SMTP banner arrives, but the TLS handshake stalls because the certificate
packets exceed the host interface MTU (often 1450 on VPS and overlay networks,
while Docker bridges default to 1500). Set the bridge MTU to match, either in
`/etc/docker/daemon.json` (`{ "mtu": 1450 }`) or per Compose network with
`driver_opts: { com.docker.network.driver.mtu: "1450" }`. A full Docker plus
nginx deployment walkthrough is in
[issue #39](https://github.com/tecnologicachile/mail-mcp/issues/39).
