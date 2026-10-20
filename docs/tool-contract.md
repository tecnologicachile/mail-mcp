# MCP Tool Contract

This document defines the first build artifact for `mail-mcp`: the server-facing MCP contract.
It is the source of truth for tool names, input/output shapes, validation bounds, and safety rules.

## Design Decisions

- Transport: stdio by default; opt-in streamable HTTP has no built-in authentication. See [remote HTTP transport](advanced-configuration.md#remote-http-transport).
- Auth/config: TOML configuration with process environment overrides; environment-only setup remains supported. See the [configuration reference](advanced-configuration.md#toml-configuration).
- Message locator: stable `message_id` format `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`.
- Destructive/write operations: disabled by default and explicitly gated.
- Output style: concise summaries with bounded structured data.
- Compatibility: no backward compatibility requirement with earlier implementations.

## Shared Input Types

### `account_id`

- Type: string
- Pattern: `^[A-Za-z0-9_-]{1,64}$`
- Default: `"default"`

### `mailbox`

- Type: string
- Length: 1..256

### `message_id`

- Type: string
- Format: `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`
- Validation rules:
  - Prefix must be `imap`.
  - `uidvalidity` and `uid` must be non-negative integers.
  - Parsed `account_id` must match requested account.

### `limit`

- Type: integer
- Range: 1..50
- Default: 10

## Shared Output Envelope

All tools return:

```json
{
  "summary": "human-readable one-line outcome",
  "data": {},
  "meta": {
    "now_utc": "ISO-8601 UTC timestamp",
    "duration_ms": 0
  }
}
```

Error responses use a consistent shape:

```json
{
  "error": {
    "code": "invalid_input|auth_failed|not_found|timeout|conflict|internal",
    "message": "actionable message",
    "details": {}
  },
  "meta": {
    "now_utc": "ISO-8601 UTC timestamp",
    "duration_ms": 0
  }
}
```

Runtime IMAP command failures are returned in successful `data` payloads whenever
possible (to preserve partial results for the LLM), using:

- `status`: `ok|partial|failed`
- `issues`: array of `{ code, stage, message, retryable, uid?, message_id? }`
- `next_action`: `{ instruction, tool, arguments }`

Hard MCP errors are reserved for validation/precondition failures (for example:
invalid input, malformed ids, conflicting cursor state, write-gate disabled).

## Tool Set

### 1) `imap_list_accounts`

Purpose: list configured accounts without exposing secrets.

Input:
- none

Output `data`:
- `accounts`: array (max 50) of `{ account_id, host, port, secure }`
- `next_action`: `{ instruction, tool, arguments }` (recommended follow-up is `imap_list_mailboxes`)

### 2) `imap_verify_account`

Purpose: verify account connectivity, auth, and capabilities.

Input:
- `account_id` (optional, default `default`)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `ok` (boolean; true unless `status=failed`)
- `latency_ms` (integer)
- `server`: `{ host, port, secure }`
- `capabilities`: string[] (max 256)

### 3) `imap_list_mailboxes`

Purpose: list visible mailboxes/folders.

Input:
- `account_id` (optional)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `mailboxes`: array (max `MAIL_IMAP_MAX_MAILBOXES`, default 200) of `{ name, delimiter? }`
- `total`: number of mailboxes the server listed before the cap
- `truncated`: `true` when `total` exceeds the cap and some mailboxes are not returned

### 4) `imap_search_messages`

Purpose: search mailbox and return paginated message summaries.

Input:
- `account_id` (optional)
- `mailbox` (required)
- one of:
  - `cursor` (string, opaque), or
  - search criteria fields:
    - `query?` (1..256)
    - `from?` (1..256)
    - `to?` (1..256)
    - `subject?` (1..256)
    - `thread_message_id?` (1..256): an RFC 5322 Message-ID (`<id@host>`, brackets optional); matches that message and every message whose `In-Reply-To` or `References` carries it
    - `unread_only?` (boolean)
    - `last_days?` (1..365)
    - `start_date?` (`YYYY-MM-DD`)
    - `end_date?` (`YYYY-MM-DD`)
- `limit` (optional)
- `include_snippet?` (boolean, default false)
- `snippet_max_chars?` (50..500, default 200; only valid if `include_snippet=true`)

Validation:
- `cursor` cannot be combined with search criteria.
- `last_days` cannot be combined with `start_date`/`end_date`.
- `start_date <= end_date`.
- `thread_message_id` must be a single token without whitespace; it is always searched in its bracketed `<id@host>` form, because some servers (iCloud) match HEADER searches on whole tokens only.
- Search text fields and mailbox values must not contain ASCII control characters.
- Searches matching more than 20,000 messages are rejected; narrow filters and retry.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `next_action`: `{ instruction, tool, arguments }`
- `account_id`
- `mailbox`
- `total` (integer)
- `attempted` (integer)
- `returned` (integer)
- `failed` (integer)
- `messages`: array (max 50) of:
  - `message_id`
  - `mailbox`
  - `uidvalidity`
  - `uid`
  - `date?`
  - `from?`
  - `subject?`
  - `flags?` (string[])
  - `snippet?`
- `next_cursor?` (string)
- `has_more` (boolean)

### 4b) `imap_search_all_mailboxes`

Purpose: run one search in every selectable mailbox of an account and return the newest matches, each labelled with its mailbox. Built for providers without an "All Mail" folder (iCloud), where one conversation is spread over INBOX, Sent Messages and project folders.

Input:
- `account_id` (optional)
- search criteria, same fields and validation as `imap_search_messages` (`query`, `from`, `to`, `subject`, `thread_message_id`, `unread_only`, `last_days`, `start_date`, `end_date`); at least one is required
- `include_mailboxes?` (string[]): only these mailboxes and their sub-mailboxes (exact names)
- `exclude_mailboxes?` (string[]): skip these mailboxes and their sub-mailboxes; wins over `include_mailboxes`
- `limit` (optional, 1..200, default 50)
- `include_snippet?`, `snippet_max_chars?` as in `imap_search_messages`

Behaviour:
- Mailboxes marked `\Noselect` are skipped.
- The work is spread over `MAIL_IMAP_SEARCH_CONCURRENCY` connections; each runs EXAMINE + UID SEARCH per mailbox. A mailbox that fails is reported in `mailboxes_failed` and `issues` and does not abort the search.
- No cursor: each mailbox contributes at most `limit` newest matches, and the merged list is sorted by Date header (newest first) and cut to `limit`.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `mailboxes_in_scope`: selectable mailboxes after include/exclude
- `mailboxes_searched`: mailboxes searched successfully
- `mailboxes_failed`: string[]
- `total`: matches across all searched mailboxes
- `returned`, `truncated` (`true` when `total` exceeds `returned`)
- `messages`: array of message summaries (same shape as `imap_search_messages`; `mailbox` names the folder, `message_id` works with `imap_get_message`)

### 5) `imap_get_message`

Purpose: return parsed message details with optional bounded enrichments.

Input:
- `account_id` (optional)
- `message_id` (required)
- `body_max_chars?` (100..20000, default 2000)
- `include_headers?` (boolean, default true)
- `include_all_headers?` (boolean, default false)
- `include_html?` (boolean, default false; returned HTML is sanitized)
- `extract_attachment_text?` (boolean, default false)
- `attachment_text_max_chars?` (100..50000, default 10000; only valid when extraction is enabled)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message`:
  - `message_id`
  - `mailbox`
  - `uidvalidity`
  - `uid`
  - `date?`
  - `from?`
  - `to?`
  - `cc?`
  - `subject?`
  - `flags?`
  - `headers?` (curated by default; full when requested)
  - `body_text?` (bounded)
  - `body_html?` (sanitized and bounded)
  - `attachments?`: array (max 50) of:
    - `filename?`
    - `content_type`
    - `size_bytes`
    - `part_id`
    - `extracted_text?` (bounded)

PDF extraction rules:
- only `application/pdf`
- max attachment size for extraction: 5 MB
- extraction failures do not fail the whole tool call

### 6) `imap_get_message_raw`

Purpose: return bounded RFC822 source for diagnostics.

Input:
- `account_id` (optional)
- `message_id` (required)
- `max_bytes?` (1024..1000000, default 200000)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message_id`
- `size_bytes`
- `raw_source_base64` (byte-faithful RFC822 source, base64 encoded)
- `raw_source_encoding` (`"base64"` on success)

### 7) `imap_update_message_flags`

Purpose: add/remove IMAP flags on a message.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional)
- `message_id` (required)
- `add_flags?`: string[] (1..20)
- `remove_flags?`: string[] (1..20)

Validation:
- at least one of `add_flags` or `remove_flags` is required.

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `message_id`
- `flags`: string[] (nullable when flag fetch fails)
- `requested_add_flags`: string[]
- `requested_remove_flags`: string[]
- `applied_add_flags`: boolean
- `applied_remove_flags`: boolean

### 8) `imap_copy_message`

Purpose: copy message to mailbox in same or different account.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional, source)
- `message_id` (required)
- `destination_mailbox` (required)
- `destination_account_id?` (defaults to source account)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `source_account_id`
- `destination_account_id`
- `source_mailbox`
- `destination_mailbox`
- `message_id`
- `new_message_id?` (present when server returns UID mapping)
- `steps_attempted`: integer
- `steps_succeeded`: integer

### 9) `imap_move_message`

Purpose: move message to mailbox in same account.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional)
- `message_id` (required)
- `destination_mailbox` (required)

Behavior:
- prefer IMAP MOVE capability
- fallback to COPY + DELETE when MOVE unsupported

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `source_mailbox`
- `destination_mailbox`
- `message_id`
- `new_message_id?`
- `steps_attempted`: integer
- `steps_succeeded`: integer

### 10) `imap_delete_message`

Purpose: delete message from mailbox.

Write gate: requires `MAIL_IMAP_WRITE_ENABLED=true`.

Input:
- `account_id` (optional)
- `message_id` (required)
- `confirm` (required literal `true`)

Output `data`:
- `status`: `ok|partial|failed`
- `issues`: array of diagnostic issues
- `account_id`
- `mailbox`
- `message_id`
- `steps_attempted`: integer
- `steps_succeeded`: integer

### 11) Draft tools: `draft_create`, `draft_update`, `draft_send`, `draft_delete`, `draft_list`

Purpose: let an agent prepare email for a person to review, on any account type.

Gate: all five require `MAIL_DRAFTS_ENABLED=true`, independent of `MAIL_IMAP_WRITE_ENABLED`. `draft_send` also requires `MAIL_SMTP_WRITE_ENABLED=true`.

Backend (per account, reported as `drafts_with` by `list_all_accounts` and as `backend` in every response):
- `ews` when the account has an EWS config (`MAIL_EWS_<ID>_*`)
- else `graph` when it has a Graph config (`MAIL_GRAPH_<ID>_*`; needs the `Mail.ReadWrite` scope)
- else `imap` (drafts in the IMAP Drafts folder; sending needs an SMTP config)

`draft_id`:
- IMAP: the regular `message_id` of the draft in the Drafts folder (works with `imap_get_message`)
- EWS: the EWS `ItemId` (works with `ews_get_message`)
- Graph: the Graph message id
- IMAP ids are rejected on EWS/Graph accounts.

Guard: every operation on an existing `draft_id` checks that it is a draft before acting (IMAP: the message is in the Drafts folder and still exists; EWS: `IsDraft`; Graph: `isDraft`). A non-draft id fails with `invalid_input`; a missing one with `not_found`.

`draft_create` input:
- `account_id` (optional)
- `to` (1..50), `cc?`, `bcc?` (max 50 each)
- `subject` (1..998 chars)
- `body_text?`, `body_html?` (at least one)
- `reply_to?`, `in_reply_to?`, `references?`
- `attachments?` (same shape as the send tools)

Output `data`: `status` (`ok|partial`), `issues`, `account_id`, `backend`, `draft_id` (null with `partial` when an IMAP draft was saved but could not be found again), `message_id` (RFC 5322 Message-ID; IMAP only).

`draft_update` input: `account_id`, `draft_id`, and the same fields as `draft_create`. Full replacement: attachments not listed again are dropped. The new version is created first and the old one deleted second, so a failure in between leaves two drafts, never none (`status: partial`, `previous_draft_removed: false`). Removed versions are expunged on IMAP and hard-deleted on EWS; Graph moves them to Deleted Items.

Output `data`: `status`, `issues`, `account_id`, `backend`, `draft_id` (new), `previous_draft_id`, `previous_draft_removed`, `message_id`.

`draft_send` input: `account_id`, `draft_id`.
- IMAP: sends the stored bytes through SMTP with `Bcc` removed and `Date` refreshed (envelope from `From`, `To`, `Cc`, `Bcc`), archives them to Sent when `should_save_sent` applies, then removes the draft. Failures after the message is sent are reported in `issues` (`status: partial`), never raised, so the caller does not send twice.
- EWS: `SendItem`, saved to Sent Items. Graph: `/send`, filed in Sent Items.

Output `data`: `status`, `issues`, `account_id`, `backend`, `draft_id`, `message_id` (IMAP), `recipients_count` (IMAP), `saved_to_sent`, `draft_removed`.

`draft_delete` input: `account_id`, `draft_id`, `confirm` (required literal `true`).

`draft_list` input: `account_id`, `limit?` (1..50, default 20). Output `data`: `status`, `issues`, `account_id`, `backend`, `returned`, `drafts[]` with `draft_id`, `subject`, `to`, `cc`, `date`.

## Security and Guardrails

- Never return secrets (`*_PASS`, tokens, cookies, auth headers).
- Redact secret-like keys in logs.
- Enforce all bounds before IMAP fetch/download when possible.
- Limit attachment bytes and text extraction output.
- Use TLS certificate and hostname verification by default.
- Reject ambiguous or conflicting inputs with explicit `invalid_input` errors.

## Environment Variables

These names also identify the environment equivalents of
[TOML fields](advanced-configuration.md#supported-fields). Recognized process
variables override matching file fields. An empty or whitespace-only value
counts as unset.

Configuration file selection:

- `MAIL_CONFIG_FILE`: optional TOML path, overridden by `--config PATH`; defaults
  to `~/.mail-mcp/config.toml` (`HOME`, or `USERPROFILE` as fallback). The
  value `none` selects no file: the server uses only the process environment
  and the legacy `.env` fallback, and config/account commands need `--config`.
- An explicitly selected missing file is a startup error. Malformed or
  unreadable files fail rather than falling back.
- Implicit `.env` discovery runs only without an explicit TOML path or an
  existing default file; process variables take precedence. Only recognized
  `MAIL_*` settings are read from it; the first occurrence of a variable wins.
- With no home directory and no explicit path, runtime environment-only setup
  still works. Configuration/account management commands need a home directory
  or an explicit path.

`config init`, `config validate`, and `accounts add|edit|list|remove|test` are
local CLI commands, not MCP tools. With no subcommand, the server starts without
prompts; stdio stdout is reserved for MCP protocol output.

Per account:

- `MAIL_IMAP_<ACCOUNT>_HOST` (required)
- `MAIL_IMAP_<ACCOUNT>_PORT` (default `993`)
- `MAIL_IMAP_<ACCOUNT>_SECURE` (default `true`)
- `MAIL_IMAP_<ACCOUNT>_USER` (required)
- `MAIL_IMAP_<ACCOUNT>_PASS` (required unless matching generic OAuth2 credentials are configured)

Server-wide:

- `MAIL_IMAP_WRITE_ENABLED` (default `false`)
- `MAIL_DRAFTS_ENABLED` (default `false`): enables the draft tools; `draft_send` also needs `MAIL_SMTP_WRITE_ENABLED`
- `MAIL_IMAP_CONNECT_TIMEOUT_MS` (default `30000`)
- `MAIL_IMAP_GREETING_TIMEOUT_MS` (default `15000`)
- `MAIL_IMAP_SOCKET_TIMEOUT_MS` (default `300000`)
- `MAIL_IMAP_MAX_MAILBOXES` (default `200`, clamped to `1..=10000`): cap for `imap_list_mailboxes`
- `MAIL_IMAP_SEARCH_CONCURRENCY` (default `4`, clamped to `1..=8`): parallel connections for `imap_search_all_mailboxes`

Transport (see `docs/advanced-configuration.md#remote-http-transport`):

- `MAIL_MCP_TRANSPORT` (default `stdio`; `http` serves MCP streamable HTTP)
- `MAIL_MCP_HTTP_HOST` (default `127.0.0.1`)
- `MAIL_MCP_HTTP_PORT` (default `8000`)
- `MAIL_MCP_HTTP_PATH` (default `/mcp`)

## Implementation Notes for Next Artifact

Next artifact will generate Rust types and schema definitions from this contract, then register all tools in an `rmcp` stdio server skeleton with unified error handling.
