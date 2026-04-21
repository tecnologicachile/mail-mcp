//! Input/output DTOs and schema-bearing types
//!
//! Defines all data structures used in MCP tool contracts. Each type is
//! annotated with `JsonSchema` for automatic schema generation.

use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Metadata included in all tool responses
///
/// Provides timing information and current UTC timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Meta {
    /// Current UTC timestamp in RFC 3339 format with milliseconds
    pub now_utc: String,
    /// Tool execution duration in milliseconds
    #[schemars(schema_with = "nonnegative_integer_schema")]
    pub duration_ms: u64,
}

impl Meta {
    /// Create metadata populated with current time and elapsed duration
    pub fn now(duration_ms: u64) -> Self {
        Self {
            now_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            duration_ms,
        }
    }
}

fn nonnegative_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0
    })
}

/// Standard response envelope for all tools
///
/// Wraps tool-specific data with human-readable summary and execution metadata.
/// This structure provides consistent response shape across all MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolEnvelope<T>
where
    T: JsonSchema,
{
    /// Human-readable summary of the operation outcome
    pub summary: String,
    /// Tool-specific data payload
    pub data: T,
    /// Execution metadata (timestamp, duration)
    pub meta: Meta,
}

/// Account metadata (no credentials)
///
/// Returned by `imap_list_accounts`. Password is intentionally excluded.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountInfo {
    /// Account identifier
    pub account_id: String,
    /// IMAP server hostname
    pub host: String,
    /// IMAP server port
    pub port: u16,
    /// Whether TLS is enabled (always true in this implementation)
    pub secure: bool,
}

/// Mailbox/folder metadata
///
/// Returned by `imap_list_mailboxes`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MailboxInfo {
    /// Mailbox name (may contain path separators like `/` or `.`)
    pub name: String,
    /// Hierarchy delimiter if supported by server (e.g., `/`, `.`)
    pub delimiter: Option<String>,
}

/// Message summary for search results
///
/// Lightweight representation returned by `imap_search_messages`. Includes
/// optional snippet for preview.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageSummary {
    /// Stable, opaque message identifier
    pub message_id: String,
    /// URI reference to message resource
    pub message_uri: String,
    /// URI reference to raw RFC822 source
    pub message_raw_uri: String,
    /// Mailbox name containing this message
    pub mailbox: String,
    /// Mailbox UIDVALIDITY at time of search
    pub uidvalidity: u32,
    /// Message UID within mailbox
    pub uid: u32,
    /// Parsed Date header
    pub date: Option<String>,
    /// Parsed From header
    pub from: Option<String>,
    /// Parsed Subject header
    pub subject: Option<String>,
    /// IMAP flags (e.g., `\Seen`, `\Flagged`)
    pub flags: Option<Vec<String>>,
    /// Optional subject snippet (if `include_snippet=true`)
    pub snippet: Option<String>,
}

/// Attachment metadata
///
/// Returned in message details. Includes optional extracted text for PDFs
/// when `extract_attachment_text=true`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentInfo {
    /// Filename if present in Content-Disposition or Content-Type
    pub filename: Option<String>,
    /// MIME content type (e.g., `application/pdf`, `image/jpeg`)
    pub content_type: String,
    /// Attachment size in bytes
    pub size_bytes: usize,
    /// Part ID for MIME structure (e.g., `1`, `2`, `3.1`)
    pub part_id: String,
    /// Extracted text from PDF (if enabled and extraction succeeded)
    pub extracted_text: Option<String>,
}

/// Full message detail
///
/// Rich representation returned by `imap_get_message`. Includes all headers,
/// body content, and attachment metadata.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageDetail {
    /// Stable, opaque message identifier
    pub message_id: String,
    /// URI reference to message resource
    pub message_uri: String,
    /// URI reference to raw RFC822 source
    pub message_raw_uri: String,
    /// Mailbox name containing this message
    pub mailbox: String,
    /// Mailbox UIDVALIDITY
    pub uidvalidity: u32,
    /// Message UID within mailbox
    pub uid: u32,
    /// Parsed Date header
    pub date: Option<String>,
    /// Parsed From header
    pub from: Option<String>,
    /// Parsed To header
    pub to: Option<String>,
    /// Parsed Cc header
    pub cc: Option<String>,
    /// Parsed Subject header
    pub subject: Option<String>,
    /// IMAP flags (e.g., `\Seen`, `\Flagged`)
    pub flags: Option<Vec<String>>,
    /// All headers or curated subset (if `include_headers=true`)
    pub headers: Option<Vec<(String, String)>>,
    /// Plain text body (truncated to `body_max_chars`)
    pub body_text: Option<String>,
    /// Sanitized HTML body (if `include_html=true`, truncated)
    pub body_html: Option<String>,
    /// Attachment metadata (up to `MAX_ATTACHMENTS`)
    pub attachments: Option<Vec<AttachmentInfo>>,
}

/// Input: account_id only
///
/// Used by `imap_list_accounts`, `imap_verify_account`, and
/// `imap_list_mailboxes`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AccountOnlyInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
}

/// Input: search messages with pagination
///
/// Used by `imap_search_messages`. Supports multiple search criteria and
/// cursor-based pagination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchMessagesInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Mailbox to search (e.g., `INBOX`, `Sent`, `Archive`)
    pub mailbox: String,
    /// Pagination cursor from previous search result
    pub cursor: Option<String>,
    /// Full-text search query
    pub query: Option<String>,
    /// Filter by From header
    pub from: Option<String>,
    /// Filter by To header
    pub to: Option<String>,
    /// Filter by Subject header
    pub subject: Option<String>,
    /// Filter to unread messages only
    pub unread_only: Option<bool>,
    /// Filter to messages from last N days
    pub last_days: Option<u16>,
    /// Filter to messages on or after this date (YYYY-MM-DD)
    pub start_date: Option<String>,
    /// Filter to messages before this date (YYYY-MM-DD)
    pub end_date: Option<String>,
    /// Maximum messages to return (1..50, default 10)
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Include subject snippet in results
    #[serde(default)]
    pub include_snippet: bool,
    /// Maximum snippet length (50..500, requires `include_snippet=true`)
    pub snippet_max_chars: Option<usize>,
}

/// Input: get parsed message details
///
/// Used by `imap_get_message`. Supports bounded enrichment (char limits,
/// optional HTML, optional attachment text extraction).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier (format: `imap:{account}:{mailbox}:{uidvalidity}:{uid}`)
    pub message_id: String,
    /// Maximum body characters (100..20000, default 2000)
    #[serde(default = "default_body_max_chars")]
    pub body_max_chars: usize,
    /// Include headers in response
    #[serde(default = "default_true")]
    pub include_headers: bool,
    /// Include all headers (if `true`, overrides curated header list)
    #[serde(default)]
    pub include_all_headers: bool,
    /// Include sanitized HTML body
    #[serde(default)]
    pub include_html: bool,
    /// Extract text from PDF attachments
    #[serde(default)]
    pub extract_attachment_text: bool,
    /// Maximum attachment text length (100..50000, requires `extract_attachment_text=true`)
    pub attachment_text_max_chars: Option<usize>,
}

/// Input: get raw RFC822 message source
///
/// Used by `imap_get_message_raw`. Returns bounded message bytes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageRawInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Maximum message bytes to return (1024..1000000, default 200000)
    #[serde(default = "default_raw_max_bytes")]
    pub max_bytes: usize,
}

/// Input: update message flags
///
/// Used by `imap_update_message_flags`. Requires at least one flag operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateMessageFlagsInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Flags to add (e.g., `\Seen`, `\Flagged`, `Important`)
    pub add_flags: Option<Vec<String>>,
    /// Flags to remove
    pub remove_flags: Option<Vec<String>>,
}

/// Input: copy message to mailbox
///
/// Used by `imap_copy_message`. Supports same-account or cross-account
/// copies.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CopyMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Destination mailbox name
    pub destination_mailbox: String,
    /// Destination account (if omitted, copies within same account)
    pub destination_account_id: Option<String>,
}

/// Input: move message to mailbox
///
/// Used by `imap_move_message`. Only supports same-account moves.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MoveMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Destination mailbox name
    pub destination_mailbox: String,
}

/// Input: delete message
///
/// Used by `imap_delete_message`. Requires explicit `confirm=true`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Explicit confirmation required (must be `true`)
    pub confirm: bool,
}

/// Default value for `account_id` field
pub fn default_account_id() -> String {
    "default".to_owned()
}

/// Default value for `bool` fields (true)
fn default_true() -> bool {
    true
}

/// Default value for `limit` in search
///
/// Chosen as a reasonable balance between response size and pagination overhead.
/// Most users need to see only the first few relevant messages.
fn default_limit() -> usize {
    10
}

/// Default value for `body_max_chars` in get_message
///
/// Provides enough context for most use cases without overwhelming output.
/// 2,000 characters is typically sufficient to understand message content.
fn default_body_max_chars() -> usize {
    2_000
}

/// Default value for `max_bytes` in get_message_raw
///
/// Large enough to capture full message headers and body for most messages,
/// but bounded to prevent excessive output. 200KB is a practical limit.
fn default_raw_max_bytes() -> usize {
    200_000
}

/// Input: create a new mailbox/folder
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateMailboxInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Name of the mailbox to create (e.g., `Archive/2024`, `Projects/Active`)
    pub mailbox_name: String,
}

/// Input: delete a mailbox/folder
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteMailboxInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Name of the mailbox to delete
    pub mailbox_name: String,
    /// Explicit confirmation required (must be `true`)
    pub confirm: bool,
}

/// Input: rename a mailbox/folder
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RenameMailboxInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Current mailbox name
    pub from_name: String,
    /// New mailbox name
    pub to_name: String,
}

/// Input: get mailbox status (message counts without selecting)
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MailboxStatusInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Mailbox name to check status of
    pub mailbox: String,
}

/// Input: bulk move messages to a mailbox
///
/// Moves up to 500 messages at once within the same account and source mailbox.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BulkMoveInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// List of stable message identifiers (all must be from the same mailbox)
    pub message_ids: Vec<String>,
    /// Destination mailbox name
    pub destination_mailbox: String,
}

/// Input: bulk delete messages
///
/// Deletes up to 500 messages at once from the same mailbox.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BulkDeleteInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// List of stable message identifiers (all must be from the same mailbox)
    pub message_ids: Vec<String>,
    /// Explicit confirmation required (must be `true`)
    pub confirm: bool,
}

/// Input: bulk update flags on messages
///
/// Updates flags on up to 500 messages at once from the same mailbox.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BulkUpdateFlagsInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// List of stable message identifiers (all must be from the same mailbox)
    pub message_ids: Vec<String>,
    /// Flags to add (e.g., `\\Seen`, `\\Flagged`)
    pub add_flags: Option<Vec<String>>,
    /// Flags to remove
    pub remove_flags: Option<Vec<String>>,
}

/// Input: search and move messages in one operation
///
/// Combines search + bulk move to avoid round-trip overhead. The server
/// executes the IMAP SEARCH, collects UIDs, and MOVEs them in a single
/// tool call. Up to 500 messages per call.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchAndMoveInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Source mailbox to search (e.g., `INBOX`)
    pub mailbox: String,
    /// Destination mailbox name
    pub destination_mailbox: String,
    /// Full-text search query
    pub query: Option<String>,
    /// Filter by From header
    pub from: Option<String>,
    /// Filter by To header
    pub to: Option<String>,
    /// Filter by Subject header
    pub subject: Option<String>,
    /// Filter to unread messages only
    pub unread_only: Option<bool>,
    /// Filter to messages from last N days
    pub last_days: Option<u16>,
    /// Filter to messages on or after this date (YYYY-MM-DD)
    pub start_date: Option<String>,
    /// Filter to messages before this date (YYYY-MM-DD)
    pub end_date: Option<String>,
    /// Maximum messages to move (1..500, default 500)
    #[serde(default = "default_search_and_move_limit")]
    pub limit: usize,
}

/// Default limit for search_and_move (process as many as possible)
fn default_search_and_move_limit() -> usize {
    500
}

/// Input: search and delete messages in one operation
///
/// Combines search + bulk delete. Requires `confirm=true`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchAndDeleteInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Source mailbox to search (e.g., `INBOX`)
    pub mailbox: String,
    /// Explicit confirmation required (must be `true`)
    pub confirm: bool,
    /// Full-text search query
    pub query: Option<String>,
    /// Filter by From header
    pub from: Option<String>,
    /// Filter by To header
    pub to: Option<String>,
    /// Filter by Subject header
    pub subject: Option<String>,
    /// Filter to unread messages only
    pub unread_only: Option<bool>,
    /// Filter to messages from last N days
    pub last_days: Option<u16>,
    /// Filter to messages on or after this date (YYYY-MM-DD)
    pub start_date: Option<String>,
    /// Filter to messages before this date (YYYY-MM-DD)
    pub end_date: Option<String>,
    /// Maximum messages to delete (1..500, default 500)
    #[serde(default = "default_search_and_move_limit")]
    pub limit: usize,
}

/// Input: append a raw message to a mailbox
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AppendMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Target mailbox name
    pub mailbox: String,
    /// Raw RFC822 message content (as UTF-8 string)
    pub raw_message: String,
}

// ─── Attachment model ────────────────────────────────────────────────────────

/// Email attachment — provide either file_path (preferred for large files) or content_base64
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct AttachmentInput {
    /// Filename (e.g., "report.pdf"). Auto-detected from file_path if omitted.
    #[serde(default)]
    pub filename: Option<String>,
    /// MIME type (e.g., "application/pdf", "image/png"). Auto-detected from extension if omitted.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Base64-encoded file content (use for small files)
    #[serde(default)]
    pub content_base64: Option<String>,
    /// Local file path (use for large files — MCP reads the file directly)
    #[serde(default)]
    pub file_path: Option<String>,
}

// ─── SMTP input models ───────────────────────────────────────────────────────

/// Input: send a new email via SMTP
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SmtpSendMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Recipient email addresses (1..50)
    pub to: Vec<String>,
    /// CC recipients (optional, max 50)
    #[serde(default)]
    pub cc: Vec<String>,
    /// BCC recipients (optional, max 50)
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Email subject (1..998 characters)
    pub subject: String,
    /// Plain text body (at least one of body_text or body_html required)
    pub body_text: Option<String>,
    /// HTML body (at least one of body_text or body_html required)
    pub body_html: Option<String>,
    /// Reply-To address (optional)
    pub reply_to: Option<String>,
    /// In-Reply-To message ID for threading (optional)
    pub in_reply_to: Option<String>,
    /// References header for threading (optional)
    pub references: Option<String>,
    /// File attachments (optional, base64-encoded)
    #[serde(default)]
    pub attachments: Vec<AttachmentInput>,
}

/// Input: reply to an existing message via SMTP
///
/// Fetches the original message via IMAP to build proper reply headers
/// (In-Reply-To, References, Re: Subject).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SmtpReplyMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message ID of the message to reply to
    pub message_id: String,
    /// Reply body (plain text)
    pub body_text: String,
    /// Reply body (HTML, optional)
    pub body_html: Option<String>,
    /// Reply to all recipients (default: false, reply to sender only)
    #[serde(default)]
    pub reply_all: bool,
    /// Include original email's attachments in the reply (default: false)
    #[serde(default)]
    pub include_original_attachments: bool,
    /// Additional file attachments (optional, base64-encoded)
    #[serde(default)]
    pub attachments: Vec<AttachmentInput>,
}

/// Input: forward an existing message via SMTP
///
/// Fetches the original message via IMAP and forwards it.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SmtpForwardMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Stable message ID of the message to forward
    pub message_id: String,
    /// Forward recipients (1..50)
    pub to: Vec<String>,
    /// Optional cover note (plain text). The original message is appended
    /// below as a quoted block.
    pub body_text: Option<String>,
    /// Optional cover note (HTML). Unlike `body_text`, this is sent as-is
    /// — the original message is NOT auto-quoted into the HTML part. If you
    /// need an HTML forward with the original embedded, compose it yourself
    /// and use `smtp_send_message`.
    pub body_html: Option<String>,
}

/// Input: verify SMTP account connectivity
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SmtpVerifyAccountInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
}

/// SMTP account metadata (no credentials)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmtpAccountInfo {
    /// Account identifier
    pub account_id: String,
    /// SMTP server hostname
    pub host: String,
    /// SMTP server port
    pub port: u16,
    /// Security mode (tls, starttls, plain)
    pub security: String,
}

// ─── Microsoft Graph input models ────────────────────────────────────────────

/// Input: send an email via Microsoft Graph API
///
/// Uses `POST /me/sendMail` instead of SMTP. Required for personal
/// Microsoft accounts (hotmail/outlook.com) where SMTP AUTH is disabled.
/// Requires OAuth2 with `Mail.Send` scope.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GraphSendMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Recipient email addresses (1..50)
    pub to: Vec<String>,
    /// CC recipients (optional, max 50)
    #[serde(default)]
    pub cc: Vec<String>,
    /// BCC recipients (optional, max 50)
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Email subject (1..998 characters)
    pub subject: String,
    /// Plain text body (at least one of body_text or body_html required)
    pub body_text: Option<String>,
    /// HTML body (at least one of body_text or body_html required)
    pub body_html: Option<String>,
    /// Reply-To address (optional)
    pub reply_to: Option<String>,
    /// In-Reply-To message ID for threading (optional)
    pub in_reply_to: Option<String>,
    /// References header for threading (optional)
    pub references: Option<String>,
    /// Save to Sent Items folder (default: true)
    #[serde(default = "default_true")]
    pub save_to_sent: bool,
    /// File attachments (optional, base64-encoded)
    #[serde(default)]
    pub attachments: Vec<AttachmentInput>,
}

// ─── EWS input models ────────────────────────────────────────────────────────

/// Input: search messages via EWS
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EwsSearchInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Folder name (inbox, sent, drafts, deleted, junk)
    pub folder: Option<String>,
    /// Maximum messages to return (1..50, default 10)
    pub limit: Option<usize>,
    /// Offset for pagination (default 0)
    pub offset: Option<usize>,
}

/// Input: get message details via EWS
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EwsGetMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// EWS Item ID
    pub item_id: String,
}

/// Input: send email via EWS
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EwsSendMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Recipient email addresses
    pub to: Vec<String>,
    /// CC recipients (optional)
    #[serde(default)]
    pub cc: Vec<String>,
    /// Email subject
    pub subject: String,
    /// Plain text body
    pub body_text: Option<String>,
    /// HTML body
    pub body_html: Option<String>,
}

/// Mailbox status information
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MailboxStatusInfo {
    /// Mailbox name
    pub name: String,
    /// Total number of messages
    pub messages: u32,
    /// Number of unseen/unread messages
    pub unseen: u32,
    /// Number of recent messages
    pub recent: u32,
}
