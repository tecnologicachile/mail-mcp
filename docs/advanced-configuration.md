# Advanced Configuration

This document covers advanced configuration options beyond the basic account setup.

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

- Pattern: `^[A-Za-z0-9_-]{1,64}$`
- Must be unique across all accounts
- `default` is the default account ID if not specified
- Examples: `default`, `work`, `personal`, `backup`, `archive`

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

1. **Required variables**: Must be set for each account
   - `MAIL_IMAP_<ACCOUNT>_HOST`
   - `MAIL_IMAP_<ACCOUNT>_USER`
   - `MAIL_IMAP_<ACCOUNT>_PASS`

2. **Optional with defaults**: Use defaults if not set
   - `MAIL_IMAP_<ACCOUNT>_PORT=993`
   - `MAIL_IMAP_<ACCOUNT>_SECURE=true`

3. **Server-wide**: Apply globally to all operations
   - `MAIL_IMAP_WRITE_ENABLED=false`
   - `MAIL_IMAP_CONNECT_TIMEOUT_MS=30000`
   - `MAIL_IMAP_GREETING_TIMEOUT_MS=15000`
   - `MAIL_IMAP_SOCKET_TIMEOUT_MS=300000`
   - `MAIL_IMAP_CURSOR_TTL_SECONDS=600`
   - `MAIL_IMAP_CURSOR_MAX_ENTRIES=512`

## Trouleshooting Configuration Issues

### Account Not Found

```
Error: invalid input: account "unknown" not configured
```

**Cause:** Account ID does not exist in environment variables.

**Resolution:** Check environment variables match requested account ID. Case-sensitive.

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
