//! MCP server implementation with tool handlers
//!
//! Implements the `ServerHandler` trait and registers 18 MCP tools. Handles
//! input validation, business logic orchestration, and response formatting.

use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;
use tracing::{error, warn};

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AccountInfo, AccountOnlyInput, AppendMessageInput, BulkDeleteInput, BulkMoveInput,
    BulkUpdateFlagsInput, CopyMessageInput, CreateMailboxInput, DeleteMailboxInput,
    DeleteMessageInput, GetMessageInput, GetMessageRawInput, MailboxInfo, MailboxStatusInfo,
    MailboxStatusInput, MessageDetail, MessageSummary, Meta, MoveMessageInput, RenameMailboxInput,
    SearchAndDeleteInput, SearchAndMoveInput, SearchMessagesInput, SmtpAccountInfo,
    SmtpForwardMessageInput, SmtpReplyMessageInput, SmtpSendMessageInput, SmtpVerifyAccountInput,
    ToolEnvelope, UpdateMessageFlagsInput,
};
use crate::pagination::{CursorEntry, CursorStore};
use crate::smtp;

/// Maximum messages per search result page
const MAX_SEARCH_LIMIT: usize = 50;
/// Maximum attachments to return per message
const MAX_ATTACHMENTS: usize = 50;
/// Maximum UID search results stored in a cursor snapshot
const MAX_CURSOR_UIDS_STORED: usize = 20_000;
/// Maximum message IDs per bulk operation
const MAX_BULK_IDS: usize = 500;

/// IMAP MCP server
///
/// Holds shared configuration and cursor store. Implements MCP tool handlers via
/// `#[tool]` attribute macro and `ServerHandler` trait.
#[derive(Clone)]
pub struct MailImapServer {
    /// Server config (accounts, timeouts, write flag)
    config: Arc<ServerConfig>,
    /// Cursor store for search pagination (protected by mutex)
    cursors: Arc<Mutex<CursorStore>>,
    /// OAuth2 token manager (present when any account uses OAuth2)
    token_manager: Option<Arc<crate::oauth2::TokenManager>>,
    /// Tool router for dispatching MCP tool calls
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MailImapServer {
    /// Create a new MCP server instance
    ///
    /// Initializes cursor store with configured TTL and max entries.
    pub fn new(config: ServerConfig) -> Self {
        let cursor_store = CursorStore::new(config.cursor_ttl_seconds, config.cursor_max_entries);
        let token_manager = if config.oauth2_accounts.is_empty() {
            None
        } else {
            Some(Arc::new(crate::oauth2::TokenManager::new(
                config.oauth2_accounts.clone(),
            )))
        };
        Self {
            config: Arc::new(config),
            cursors: Arc::new(Mutex::new(cursor_store)),
            token_manager,
            tool_router: Self::tool_router(),
        }
    }

    /// Tool: List configured IMAP accounts
    ///
    /// Returns account metadata (host, port, secure) without exposing
    /// credentials.
    #[tool(
        name = "imap_list_accounts",
        description = "List configured IMAP accounts"
    )]
    async fn list_accounts(&self) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let accounts = self
            .config
            .accounts
            .values()
            .map(|a| AccountInfo {
                account_id: a.account_id.clone(),
                host: a.host.clone(),
                port: a.port,
                secure: a.secure,
            })
            .collect::<Vec<_>>();
        let next_account_id = accounts
            .first()
            .map(|a| a.account_id.clone())
            .unwrap_or_else(|| "default".to_owned());
        let data = serde_json::json!({
            "accounts": accounts,
            "next_action": next_action_list_mailboxes(&next_account_id),
        });
        finalize_tool(
            started,
            "imap_list_accounts",
            Ok((
                format!(
                    "{} account(s) configured",
                    self.config.accounts.values().len()
                ),
                data,
            )),
        )
    }

    /// Tool: Verify account connectivity and capabilities
    ///
    /// Tests TCP/TLS connection, authentication, and retrieves server
    /// capabilities list.
    #[tool(
        name = "imap_verify_account",
        description = "Verify account connectivity and capabilities"
    )]
    async fn verify_account(
        &self,
        Parameters(input): Parameters<AccountOnlyInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_verify_account",
            self.verify_account_impl(input)
                .await
                .map(|data| ("Account verification succeeded".to_owned(), data)),
        )
    }

    /// Tool: List mailboxes for an account
    ///
    /// Returns up to 200 visible mailboxes/folders.
    #[tool(
        name = "imap_list_mailboxes",
        description = "List mailboxes for an account"
    )]
    async fn list_mailboxes(
        &self,
        Parameters(input): Parameters<AccountOnlyInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_list_mailboxes",
            self.list_mailboxes_impl(input).await.map(|data| {
                (
                    format!(
                        "{} mailbox(es)",
                        data["mailboxes"].as_array().map_or(0, Vec::len)
                    ),
                    data,
                )
            }),
        )
    }

    /// Tool: Search messages with cursor pagination
    ///
    /// Supports multiple search criteria (query, from, to, subject, date
    /// ranges, unread filter). Returns cursors for efficient pagination
    /// across large result sets.
    #[tool(
        name = "imap_search_messages",
        description = "Search messages with cursor pagination"
    )]
    async fn search_messages(
        &self,
        Parameters(input): Parameters<SearchMessagesInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.search_messages_impl(input).await.and_then(|data| {
            let summary = format!("{} message(s) returned", data.messages.len());
            let serialized = serde_json::to_value(data)
                .map_err(|e| AppError::Internal(format!("serialization failure: {e}")))?;
            Ok((summary, serialized))
        });
        finalize_tool(started, "imap_search_messages", result)
    }

    /// Tool: Get parsed message details
    ///
    /// Returns structured message data with headers, body text/HTML, and
    /// attachments. Supports bounded enrichment (char limits, optional HTML).
    #[tool(name = "imap_get_message", description = "Get parsed message details")]
    async fn get_message(
        &self,
        Parameters(input): Parameters<GetMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_message",
            self.get_message_impl(input)
                .await
                .map(|data| ("Message retrieved".to_owned(), data)),
        )
    }

    /// Tool: Get bounded RFC822 message source
    ///
    /// Returns raw RFC822 bytes (as string) up to `max_bytes`. Useful for
    /// diagnostics or tools that need full message source.
    #[tool(
        name = "imap_get_message_raw",
        description = "Get bounded RFC822 source"
    )]
    async fn get_message_raw(
        &self,
        Parameters(input): Parameters<GetMessageRawInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_message_raw",
            self.get_message_raw_impl(input)
                .await
                .map(|data| ("Raw message retrieved".to_owned(), data)),
        )
    }

    /// Tool: Add or remove IMAP flags
    ///
    /// Modifies message flags (e.g., `\Seen`, `\Flagged`, `\Draft`,
    /// custom flags). Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_update_message_flags",
        description = "Add or remove IMAP flags"
    )]
    async fn update_message_flags(
        &self,
        Parameters(input): Parameters<UpdateMessageFlagsInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_update_message_flags",
            self.update_flags_impl(input)
                .await
                .map(|data| ("Flags updated".to_owned(), data)),
        )
    }

    /// Tool: Copy message to mailbox
    ///
    /// Copies message to same or different account. Cross-account copy uses
    /// `APPEND`. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_copy_message", description = "Copy a message to mailbox")]
    async fn copy_message(
        &self,
        Parameters(input): Parameters<CopyMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_copy_message",
            self.copy_message_impl(input)
                .await
                .map(|data| ("Message copied".to_owned(), data)),
        )
    }

    /// Tool: Move message to mailbox
    ///
    /// Moves message within same account. Prefers `MOVE` capability,
    /// falls back to `COPY` + `DELETE`. Requires
    /// `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_move_message", description = "Move a message to mailbox")]
    async fn move_message(
        &self,
        Parameters(input): Parameters<MoveMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_move_message",
            self.move_message_impl(input)
                .await
                .map(|data| ("Message moved".to_owned(), data)),
        )
    }

    /// Tool: Delete message from mailbox
    ///
    /// Marks message as `\Deleted` and immediately expunges. Requires
    /// explicit `confirm=true` and `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_delete_message", description = "Delete a message")]
    async fn delete_message(
        &self,
        Parameters(input): Parameters<DeleteMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_delete_message",
            self.delete_message_impl(input)
                .await
                .map(|data| ("Message deleted".to_owned(), data)),
        )
    }

    /// Tool: Create a new mailbox/folder
    ///
    /// Creates a mailbox. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_create_mailbox", description = "Create a new mailbox/folder")]
    async fn create_mailbox(
        &self,
        Parameters(input): Parameters<CreateMailboxInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_create_mailbox",
            self.create_mailbox_impl(input)
                .await
                .map(|data| ("Mailbox created".to_owned(), data)),
        )
    }

    /// Tool: Delete a mailbox/folder
    ///
    /// Deletes a mailbox. Requires explicit `confirm=true` and
    /// `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_delete_mailbox", description = "Delete a mailbox/folder")]
    async fn delete_mailbox(
        &self,
        Parameters(input): Parameters<DeleteMailboxInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_delete_mailbox",
            self.delete_mailbox_impl(input)
                .await
                .map(|data| ("Mailbox deleted".to_owned(), data)),
        )
    }

    /// Tool: Rename a mailbox/folder
    ///
    /// Renames a mailbox. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_rename_mailbox", description = "Rename a mailbox/folder")]
    async fn rename_mailbox(
        &self,
        Parameters(input): Parameters<RenameMailboxInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_rename_mailbox",
            self.rename_mailbox_impl(input)
                .await
                .map(|data| ("Mailbox renamed".to_owned(), data)),
        )
    }

    /// Tool: Get mailbox status (message counts)
    ///
    /// Returns message, unseen, and recent counts without selecting the mailbox.
    #[tool(
        name = "imap_mailbox_status",
        description = "Get mailbox message counts (total, unseen, recent) without selecting it"
    )]
    async fn mailbox_status(
        &self,
        Parameters(input): Parameters<MailboxStatusInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_mailbox_status",
            self.mailbox_status_impl(input)
                .await
                .map(|data| ("Mailbox status retrieved".to_owned(), data)),
        )
    }

    /// Tool: Bulk move messages to a mailbox
    ///
    /// Moves up to 500 messages at once. All messages must be from the same
    /// mailbox. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_bulk_move",
        description = "Move up to 500 messages to a mailbox in one operation"
    )]
    async fn bulk_move(
        &self,
        Parameters(input): Parameters<BulkMoveInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.bulk_move_impl(input).await.map(|data| {
            let moved = data["moved_count"].as_u64().unwrap_or(0);
            (format!("{moved} message(s) moved"), data)
        });
        finalize_tool(started, "imap_bulk_move", result)
    }

    /// Tool: Bulk delete messages
    ///
    /// Deletes up to 500 messages at once. All messages must be from the same
    /// mailbox. Requires explicit `confirm=true` and `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_bulk_delete",
        description = "Delete up to 500 messages in one operation"
    )]
    async fn bulk_delete(
        &self,
        Parameters(input): Parameters<BulkDeleteInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.bulk_delete_impl(input).await.map(|data| {
            let deleted = data["deleted_count"].as_u64().unwrap_or(0);
            (format!("{deleted} message(s) deleted"), data)
        });
        finalize_tool(started, "imap_bulk_delete", result)
    }

    /// Tool: Bulk update flags on messages
    ///
    /// Updates flags on up to 500 messages at once. All messages must be from
    /// the same mailbox. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_bulk_update_flags",
        description = "Update flags on up to 500 messages in one operation"
    )]
    async fn bulk_update_flags(
        &self,
        Parameters(input): Parameters<BulkUpdateFlagsInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.bulk_update_flags_impl(input).await.map(|data| {
            let updated = data["updated_count"].as_u64().unwrap_or(0);
            (format!("{updated} message(s) flags updated"), data)
        });
        finalize_tool(started, "imap_bulk_update_flags", result)
    }

    /// Tool: Append a raw RFC822 message to a mailbox
    ///
    /// Inserts a message into the specified mailbox. Requires
    /// `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_append_message",
        description = "Append a raw RFC822 message to a mailbox"
    )]
    async fn append_message(
        &self,
        Parameters(input): Parameters<AppendMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_append_message",
            self.append_message_impl(input)
                .await
                .map(|data| ("Message appended".to_owned(), data)),
        )
    }

    /// Tool: Search and move messages in one operation
    ///
    /// Combines IMAP SEARCH + MOVE to avoid round-trip overhead. Searches the
    /// source mailbox and moves up to 500 matching messages to the destination.
    /// Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_search_and_move",
        description = "Search messages and move matches to a mailbox in one operation (up to 500)"
    )]
    async fn search_and_move(
        &self,
        Parameters(input): Parameters<SearchAndMoveInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.search_and_move_impl(input).await.map(|data| {
            let moved = data["moved_count"].as_u64().unwrap_or(0);
            let has_more = data["has_more"].as_bool().unwrap_or(false);
            let suffix = if has_more { "; more remain" } else { "" };
            (format!("{moved} message(s) moved{suffix}"), data)
        });
        finalize_tool(started, "imap_search_and_move", result)
    }

    /// Tool: Search and delete messages in one operation
    ///
    /// Combines IMAP SEARCH + DELETE to avoid round-trip overhead. Searches the
    /// source mailbox and deletes up to 500 matching messages.
    /// Requires `MAIL_IMAP_WRITE_ENABLED=true` and `confirm=true`.
    #[tool(
        name = "imap_search_and_delete",
        description = "Search messages and delete matches in one operation (up to 500)"
    )]
    async fn search_and_delete(
        &self,
        Parameters(input): Parameters<SearchAndDeleteInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.search_and_delete_impl(input).await.map(|data| {
            let deleted = data["deleted_count"].as_u64().unwrap_or(0);
            let has_more = data["has_more"].as_bool().unwrap_or(false);
            let suffix = if has_more { "; more remain" } else { "" };
            (format!("{deleted} message(s) deleted{suffix}"), data)
        });
        finalize_tool(started, "imap_search_and_delete", result)
    }

    // ─── SMTP Tools ──────────────────────────────────────────────────────────

    /// Tool: List configured SMTP accounts
    #[tool(
        name = "smtp_list_accounts",
        description = "List configured SMTP accounts"
    )]
    async fn smtp_list_accounts(&self) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let accounts: Vec<SmtpAccountInfo> = self
            .config
            .smtp_accounts
            .values()
            .map(|a| SmtpAccountInfo {
                account_id: a.account_id.clone(),
                host: a.host.clone(),
                port: a.port,
                security: format!("{:?}", a.security).to_ascii_lowercase(),
            })
            .collect();
        let data = serde_json::json!({ "accounts": accounts });
        let summary = format!("{} SMTP account(s) configured", accounts.len());
        finalize_tool(started, "smtp_list_accounts", Ok((summary, data)))
    }

    /// Tool: Send a new email via SMTP
    #[tool(
        name = "smtp_send_message",
        description = "Send a new email via SMTP"
    )]
    async fn smtp_send_message(
        &self,
        Parameters(input): Parameters<SmtpSendMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.smtp_send_message_impl(input).await;
        finalize_tool(started, "smtp_send_message", result)
    }

    /// Tool: Reply to a message via SMTP
    #[tool(
        name = "smtp_reply_message",
        description = "Reply to an existing message via SMTP (fetches original for proper threading)"
    )]
    async fn smtp_reply_message(
        &self,
        Parameters(input): Parameters<SmtpReplyMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.smtp_reply_message_impl(input).await;
        finalize_tool(started, "smtp_reply_message", result)
    }

    /// Tool: Forward a message via SMTP
    #[tool(
        name = "smtp_forward_message",
        description = "Forward an existing message via SMTP"
    )]
    async fn smtp_forward_message(
        &self,
        Parameters(input): Parameters<SmtpForwardMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.smtp_forward_message_impl(input).await;
        finalize_tool(started, "smtp_forward_message", result)
    }

    /// Tool: Verify SMTP connectivity
    #[tool(
        name = "smtp_verify_account",
        description = "Test SMTP account connectivity and authentication"
    )]
    async fn smtp_verify_account(
        &self,
        Parameters(input): Parameters<SmtpVerifyAccountInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.smtp_verify_account_impl(input).await;
        finalize_tool(started, "smtp_verify_account", result)
    }
}

/// MCP server handler implementation
///
/// Provides server info and capabilities to MCP client.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailImapServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Secure IMAP MCP server. Read operations are enabled by default; write tools require MAIL_IMAP_WRITE_ENABLED=true. SMTP send tools require MAIL_SMTP_WRITE_ENABLED=true.".to_owned(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Search result data structure
#[derive(Debug, serde::Serialize)]
struct SearchResultData {
    status: String,
    issues: Vec<ToolIssue>,
    next_action: NextAction,
    account_id: String,
    mailbox: String,
    total: usize,
    attempted: usize,
    returned: usize,
    failed: usize,
    messages: Vec<MessageSummary>,
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct NextAction {
    instruction: String,
    tool: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ToolIssue {
    code: String,
    stage: String,
    message: String,
    retryable: bool,
    uid: Option<u32>,
    message_id: Option<String>,
}

impl ToolIssue {
    fn from_error(stage: &str, error: &AppError) -> Self {
        let (code, retryable) = match error {
            AppError::InvalidInput(_) => ("invalid_input", false),
            AppError::NotFound(_) => ("not_found", false),
            AppError::AuthFailed(_) => ("auth_failed", false),
            AppError::Timeout(_) => ("timeout", true),
            AppError::Conflict(_) => ("conflict", false),
            AppError::TokenRefresh(_) => ("token_refresh_failed", true),
            AppError::Internal(_) => ("internal", true),
        };
        Self {
            code: code.to_owned(),
            stage: stage.to_owned(),
            message: error.to_string(),
            retryable,
            uid: None,
            message_id: None,
        }
    }

    fn with_uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    fn with_message_id(mut self, message_id: &str) -> Self {
        self.message_id = Some(message_id.to_owned());
        self
    }
}

#[derive(Debug)]
struct SummaryBuildResult {
    messages: Vec<MessageSummary>,
    issues: Vec<ToolIssue>,
    attempted: usize,
    failed: usize,
}

/// Tool implementation methods
///
/// Private methods handle the actual business logic for each tool, separated
/// from the public `#[tool]` methods that handle response formatting.
impl MailImapServer {
    async fn verify_account_impl(&self, input: AccountOnlyInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let started = Instant::now();
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(ToolIssue::from_error("connect_authenticated", &error));
                log_runtime_issues(
                    "imap_verify_account",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "next_action": next_action_retry_verify(&input.account_id),
                    "account_id": account.account_id,
                    "ok": false,
                    "latency_ms": duration_ms(started),
                    "server": { "host": account.host, "port": account.port, "secure": account.secure },
                    "capabilities": []
                }));
            }
        };

        if let Err(error) = imap::noop(&self.config, &mut session).await {
            issues.push(ToolIssue::from_error("noop", &error));
        }

        let mut capabilities = match imap::capabilities(&self.config, &mut session).await {
            Ok(caps) => caps.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>(),
            Err(error) => {
                issues.push(ToolIssue::from_error("capabilities", &error));
                Vec::new()
            }
        };
        capabilities.sort();
        capabilities.truncate(256);

        let status = status_from_counts(issues.is_empty(), true);
        log_runtime_issues(
            "imap_verify_account",
            status,
            &input.account_id,
            None,
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "next_action": next_action_list_mailboxes(&input.account_id),
            "account_id": account.account_id,
            "ok": status != "failed",
            "latency_ms": duration_ms(started),
            "server": { "host": account.host, "port": account.port, "secure": account.secure },
            "capabilities": capabilities
        }))
    }

    async fn list_mailboxes_impl(&self, input: AccountOnlyInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(ToolIssue::from_error("connect_authenticated", &error));
                log_runtime_issues(
                    "imap_list_mailboxes",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "next_action": next_action_retry_verify(&input.account_id),
                    "account_id": account.account_id,
                    "mailboxes": []
                }));
            }
        };

        let items = match imap::list_all_mailboxes(&self.config, &mut session).await {
            Ok(items) => items,
            Err(error) => {
                issues.push(ToolIssue::from_error("list_mailboxes", &error));
                Vec::new()
            }
        };

        let mailboxes = items
            .into_iter()
            .take(200)
            .map(|item| MailboxInfo {
                name: item.name().to_owned(),
                delimiter: item.delimiter().map(|d| d.to_string()),
            })
            .collect::<Vec<_>>();

        let status = status_from_counts(issues.is_empty(), !mailboxes.is_empty());
        log_runtime_issues(
            "imap_list_mailboxes",
            status,
            &input.account_id,
            None,
            &issues,
        );
        let next_action = preferred_mailbox_name(&mailboxes)
            .map(|mailbox| next_action_search_mailbox(&input.account_id, &mailbox))
            .unwrap_or_else(|| next_action_retry_verify(&input.account_id));

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "next_action": next_action,
            "account_id": account.account_id,
            "mailboxes": mailboxes,
        }))
    }

    async fn search_messages_impl(
        &self,
        input: SearchMessagesInput,
    ) -> AppResult<SearchResultData> {
        validate_search_input(&input)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                let issue = ToolIssue::from_error("connect_authenticated", &error);
                let issues = vec![issue];
                log_runtime_issues(
                    "imap_search_messages",
                    "failed",
                    &input.account_id,
                    Some(&input.mailbox),
                    &issues,
                );
                return Ok(SearchResultData {
                    status: "failed".to_owned(),
                    issues,
                    next_action: next_action_retry_verify(&input.account_id),
                    account_id: input.account_id,
                    mailbox: input.mailbox,
                    total: 0,
                    attempted: 0,
                    returned: 0,
                    failed: 0,
                    messages: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                });
            }
        };
        let uidvalidity =
            match imap::select_mailbox_readonly(&self.config, &mut session, &input.mailbox).await {
                Ok(uidvalidity) => uidvalidity,
                Err(error) => {
                    let issue = ToolIssue::from_error("select_mailbox_readonly", &error);
                    let issues = vec![issue];
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_retry_verify(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            };

        let snapshot = if let Some(cursor) = input.cursor.clone() {
            resume_cursor_search(&self.cursors, &input, uidvalidity, cursor).await?
        } else {
            match start_new_search(&self.config, &mut session, &input).await {
                Ok(snapshot) => snapshot,
                Err(error) if is_hard_precondition_error(&error) => return Err(error),
                Err(error) => {
                    let issue = ToolIssue::from_error("uid_search", &error);
                    let issues = vec![issue];
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_retry_verify(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            }
        };

        let SearchSnapshot {
            uids_desc,
            offset,
            include_snippet,
            snippet_max_chars,
            cursor_id_from_request,
        } = snapshot;

        let total = uids_desc.len();
        if offset > total {
            return Err(AppError::InvalidInput(
                "cursor offset is out of range".to_owned(),
            ));
        }

        let limit = input.limit.clamp(1, MAX_SEARCH_LIMIT);
        let page_uids = uids_desc
            .iter()
            .skip(offset)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();

        let SummaryBuildResult {
            messages,
            issues,
            attempted,
            failed,
        } = build_message_summaries(
            &self.config,
            &mut session,
            &page_uids,
            SummaryBuildOptions {
                account_id: &input.account_id,
                mailbox: &input.mailbox,
                uidvalidity,
                include_snippet,
                snippet_max_chars,
            },
        )
        .await;

        let next_offset = offset + page_uids.len();
        let has_more = next_offset < total;
        let next_cursor = if has_more {
            let mut store = self.cursors.lock().await;
            if let Some(existing) = cursor_id_from_request {
                store.update_offset(&existing, next_offset);
                Some(existing)
            } else {
                let id = store.create(CursorEntry {
                    account_id: input.account_id.clone(),
                    mailbox: input.mailbox.clone(),
                    uidvalidity,
                    uids_desc,
                    offset: next_offset,
                    include_snippet,
                    snippet_max_chars,
                    expires_at: Instant::now(),
                });
                Some(id)
            }
        } else {
            if let Some(existing) = cursor_id_from_request {
                let mut store = self.cursors.lock().await;
                store.delete(&existing);
            }
            None
        };

        let status = status_from_issue_and_counts(&issues, !messages.is_empty()).to_owned();
        log_runtime_issues(
            "imap_search_messages",
            &status,
            &input.account_id,
            Some(&input.mailbox),
            &issues,
        );
        let next_action = next_action_for_search_result(
            &status,
            &input.account_id,
            &input.mailbox,
            input.limit,
            next_cursor.as_deref(),
            &messages,
        );

        Ok(SearchResultData {
            status,
            issues,
            next_action,
            account_id: input.account_id,
            mailbox: input.mailbox,
            total,
            attempted,
            returned: messages.len(),
            failed,
            messages,
            next_cursor: next_cursor.clone(),
            has_more: next_cursor.is_some(),
        })
    }

    async fn get_message_impl(&self, input: GetMessageInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        validate_chars(input.body_max_chars, 100, 20_000, "body_max_chars")?;
        let attachment_text_max_chars = input.attachment_text_max_chars.unwrap_or(10_000);
        if input.attachment_text_max_chars.is_some() && !input.extract_attachment_text {
            return Err(AppError::InvalidInput(
                "attachment_text_max_chars requires extract_attachment_text=true".to_owned(),
            ));
        }
        validate_chars(
            attachment_text_max_chars,
            100,
            50_000,
            "attachment_text_max_chars",
        )?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = match imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };

        let parsed = mime::parse_message(
            &raw,
            input.body_max_chars,
            input.include_html,
            input.extract_attachment_text,
            attachment_text_max_chars,
        );

        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("parse_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };

        let headers = if input.include_headers || input.include_all_headers {
            Some(mime::curated_headers(
                &parsed.headers_all,
                input.include_all_headers,
            ))
        } else {
            None
        };

        let flags = match imap::fetch_flags(&self.config, &mut session, msg_id.uid).await {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };

        let detail = MessageDetail {
            message_id: encoded_message_id.clone(),
            message_uri: build_message_uri(
                &input.account_id,
                &msg_id.mailbox,
                msg_id.uidvalidity,
                msg_id.uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &input.account_id,
                &msg_id.mailbox,
                msg_id.uidvalidity,
                msg_id.uid,
            ),
            mailbox: msg_id.mailbox.clone(),
            uidvalidity: msg_id.uidvalidity,
            uid: msg_id.uid,
            date: parsed.date,
            from: parsed.from,
            to: parsed.to,
            cc: parsed.cc,
            subject: parsed.subject,
            flags,
            headers,
            body_text: parsed.body_text,
            body_html: parsed.body_html_sanitized,
            attachments: Some(
                parsed
                    .attachments
                    .into_iter()
                    .take(MAX_ATTACHMENTS)
                    .collect(),
            ),
        };

        let status = status_from_issue_and_counts(&issues, true);
        log_runtime_issues(
            "imap_get_message",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "message": detail,
        }))
    }

    async fn get_message_raw_impl(
        &self,
        input: GetMessageRawInput,
    ) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        validate_chars(input.max_bytes, 1_024, 1_000_000, "max_bytes")?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();
        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message_id": encoded_message_id,
                    "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "size_bytes": 0,
                    "raw_source_base64": serde_json::Value::Null,
                    "raw_source_encoding": serde_json::Value::Null,
                }));
            }
        };
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = match imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message_id": encoded_message_id,
                    "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "size_bytes": 0,
                    "raw_source_base64": serde_json::Value::Null,
                    "raw_source_encoding": serde_json::Value::Null,
                }));
            }
        };
        if raw.len() > input.max_bytes {
            return Err(AppError::InvalidInput(
                "message exceeds max_bytes; increase max_bytes".to_owned(),
            ));
        }

        log_runtime_issues(
            "imap_get_message_raw",
            "ok",
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": "ok",
            "issues": issues,
            "account_id": input.account_id,
            "message_id": encoded_message_id,
            "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "size_bytes": raw.len(),
            "raw_source_base64": encode_raw_source_base64(&raw),
            "raw_source_encoding": "base64",
        }))
    }

    async fn update_flags_impl(
        &self,
        input: UpdateMessageFlagsInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;

        let add_flags = input.add_flags.unwrap_or_default();
        let remove_flags = input.remove_flags.unwrap_or_default();
        if add_flags.is_empty() && remove_flags.is_empty() {
            return Err(AppError::InvalidInput(
                "at least one of add_flags/remove_flags is required".to_owned(),
            ));
        }
        validate_flags(&add_flags, "add_flags")?;
        validate_flags(&remove_flags, "remove_flags")?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_update_message_flags",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message_id": encoded_message_id,
                    "flags": serde_json::Value::Null,
                    "requested_add_flags": add_flags,
                    "requested_remove_flags": remove_flags,
                    "applied_add_flags": false,
                    "applied_remove_flags": false,
                }));
            }
        };
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        let mut applied_add_flags = false;
        if !add_flags.is_empty() {
            let query = format!("+FLAGS.SILENT ({})", add_flags.join(" "));
            if let Err(error) =
                imap::uid_store(&self.config, &mut session, msg_id.uid, query.as_str()).await
            {
                issues.push(
                    ToolIssue::from_error("uid_store_add_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                applied_add_flags = true;
            }
        }

        let mut applied_remove_flags = false;
        if !remove_flags.is_empty() {
            let query = format!("-FLAGS.SILENT ({})", remove_flags.join(" "));
            if let Err(error) =
                imap::uid_store(&self.config, &mut session, msg_id.uid, query.as_str()).await
            {
                issues.push(
                    ToolIssue::from_error("uid_store_remove_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                applied_remove_flags = true;
            }
        }

        let flags = match imap::fetch_flags(&self.config, &mut session, msg_id.uid).await {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };

        let status = status_from_issue_and_counts(&issues, flags.is_some());
        log_runtime_issues(
            "imap_update_message_flags",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );
        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "message_id": encoded_message_id,
            "flags": flags,
            "requested_add_flags": add_flags,
            "requested_remove_flags": remove_flags,
            "applied_add_flags": applied_add_flags,
            "applied_remove_flags": applied_remove_flags,
        }))
    }

    async fn copy_message_impl(&self, input: CopyMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.destination_mailbox)?;
        let destination_account_id = input
            .destination_account_id
            .clone()
            .unwrap_or_else(|| input.account_id.clone());
        validate_account_id(&destination_account_id)?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();
        let mut issues = Vec::new();
        let mut steps_succeeded = 0usize;
        let mut steps_attempted = 0usize;

        if destination_account_id == input.account_id {
            let account = self.config.get_account(&input.account_id)?;
            steps_attempted += 1;
            let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
                Ok(session) => {
                    steps_succeeded += 1;
                    session
                }
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("connect_authenticated_source", &error)
                            .with_message_id(&encoded_message_id),
                    );
                    log_runtime_issues(
                        "imap_copy_message",
                        "failed",
                        &input.account_id,
                        Some(&msg_id.mailbox),
                        &issues,
                    );
                    return Ok(serde_json::json!({
                        "status": "failed",
                        "issues": issues,
                        "source_account_id": input.account_id,
                        "destination_account_id": destination_account_id,
                        "source_mailbox": msg_id.mailbox,
                        "destination_mailbox": input.destination_mailbox,
                        "message_id": encoded_message_id,
                        "new_message_id": serde_json::Value::Null,
                        "steps_attempted": steps_attempted,
                        "steps_succeeded": steps_succeeded,
                    }));
                }
            };
            ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;
            steps_attempted += 1;
            if let Err(error) = imap::uid_copy(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await
            {
                issues.push(
                    ToolIssue::from_error("uid_copy", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                steps_succeeded += 1;
            }
        } else {
            let source = self.config.get_account(&input.account_id)?;
            steps_attempted += 1;
            let mut source_session = match imap::connect_authenticated(&self.config, source, self.token_manager.as_deref()).await {
                Ok(session) => {
                    steps_succeeded += 1;
                    session
                }
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("connect_authenticated_source", &error)
                            .with_message_id(&encoded_message_id),
                    );
                    log_runtime_issues(
                        "imap_copy_message",
                        "failed",
                        &input.account_id,
                        Some(&msg_id.mailbox),
                        &issues,
                    );
                    return Ok(serde_json::json!({
                        "status": "failed",
                        "issues": issues,
                        "source_account_id": input.account_id,
                        "destination_account_id": destination_account_id,
                        "source_mailbox": msg_id.mailbox,
                        "destination_mailbox": input.destination_mailbox,
                        "message_id": encoded_message_id,
                        "new_message_id": serde_json::Value::Null,
                        "steps_attempted": steps_attempted,
                        "steps_succeeded": steps_succeeded,
                    }));
                }
            };
            ensure_uidvalidity_matches_readonly(&self.config, &mut source_session, &msg_id).await?;
            steps_attempted += 1;
            let raw = match imap::fetch_raw_message(&self.config, &mut source_session, msg_id.uid)
                .await
            {
                Ok(raw) => {
                    steps_succeeded += 1;
                    raw
                }
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("fetch_raw_message_source", &error)
                            .with_uid(msg_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                    log_runtime_issues(
                        "imap_copy_message",
                        "failed",
                        &input.account_id,
                        Some(&msg_id.mailbox),
                        &issues,
                    );
                    return Ok(serde_json::json!({
                        "status": "failed",
                        "issues": issues,
                        "source_account_id": input.account_id,
                        "destination_account_id": destination_account_id,
                        "source_mailbox": msg_id.mailbox,
                        "destination_mailbox": input.destination_mailbox,
                        "message_id": encoded_message_id,
                        "new_message_id": serde_json::Value::Null,
                        "steps_attempted": steps_attempted,
                        "steps_succeeded": steps_succeeded,
                    }));
                }
            };

            let destination = self.config.get_account(&destination_account_id)?;
            steps_attempted += 1;
            let mut destination_session =
                match imap::connect_authenticated(&self.config, destination, self.token_manager.as_deref()).await {
                    Ok(session) => {
                        steps_succeeded += 1;
                        session
                    }
                    Err(error) => {
                        issues.push(
                            ToolIssue::from_error("connect_authenticated_destination", &error)
                                .with_message_id(&encoded_message_id),
                        );
                        let status = status_from_issue_and_counts(&issues, steps_succeeded > 0);
                        log_runtime_issues(
                            "imap_copy_message",
                            status,
                            &input.account_id,
                            Some(&msg_id.mailbox),
                            &issues,
                        );
                        return Ok(serde_json::json!({
                            "status": status,
                            "issues": issues,
                            "source_account_id": input.account_id,
                            "destination_account_id": destination_account_id,
                            "source_mailbox": msg_id.mailbox,
                            "destination_mailbox": input.destination_mailbox,
                            "message_id": encoded_message_id,
                            "new_message_id": serde_json::Value::Null,
                            "steps_attempted": steps_attempted,
                            "steps_succeeded": steps_succeeded,
                        }));
                    }
                };
            steps_attempted += 1;
            if let Err(error) = imap::append(
                &self.config,
                &mut destination_session,
                input.destination_mailbox.as_str(),
                raw.as_slice(),
            )
            .await
            {
                issues.push(
                    ToolIssue::from_error("append_destination", &error)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                steps_succeeded += 1;
            }
        }

        let status = status_from_issue_and_counts(&issues, steps_succeeded > 0);
        log_runtime_issues(
            "imap_copy_message",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "source_account_id": input.account_id,
            "destination_account_id": destination_account_id,
            "source_mailbox": msg_id.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "message_id": encoded_message_id,
            "new_message_id": serde_json::Value::Null,
            "steps_attempted": steps_attempted,
            "steps_succeeded": steps_succeeded,
        }))
    }

    async fn move_message_impl(&self, input: MoveMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.destination_mailbox)?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();
        let mut steps_attempted = 0usize;
        let mut steps_succeeded = 0usize;

        steps_attempted += 1;
        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => {
                steps_succeeded += 1;
                session
            }
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_move_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "source_mailbox": msg_id.mailbox,
                    "destination_mailbox": input.destination_mailbox,
                    "message_id": encoded_message_id,
                    "new_message_id": serde_json::Value::Null,
                    "steps_attempted": steps_attempted,
                    "steps_succeeded": steps_succeeded,
                }));
            }
        };
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        steps_attempted += 1;
        let caps = match imap::capabilities(&self.config, &mut session).await {
            Ok(caps) => {
                steps_succeeded += 1;
                caps
            }
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("capabilities", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                let status = status_from_issue_and_counts(&issues, steps_succeeded > 0);
                log_runtime_issues(
                    "imap_move_message",
                    status,
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": status,
                    "issues": issues,
                    "account_id": input.account_id,
                    "source_mailbox": msg_id.mailbox,
                    "destination_mailbox": input.destination_mailbox,
                    "message_id": encoded_message_id,
                    "new_message_id": serde_json::Value::Null,
                    "steps_attempted": steps_attempted,
                    "steps_succeeded": steps_succeeded,
                }));
            }
        };

        if caps.has_str("MOVE") {
            steps_attempted += 1;
            if let Err(error) = imap::uid_move(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await
            {
                issues.push(
                    ToolIssue::from_error("uid_move", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                steps_succeeded += 1;
            }
        } else {
            steps_attempted += 1;
            let copied = if let Err(error) = imap::uid_copy(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await
            {
                issues.push(
                    ToolIssue::from_error("uid_copy", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                false
            } else {
                steps_succeeded += 1;
                true
            };

            if copied {
                steps_attempted += 1;
                let deleted = if let Err(error) = imap::uid_store(
                    &self.config,
                    &mut session,
                    msg_id.uid,
                    "+FLAGS.SILENT (\\Deleted)",
                )
                .await
                {
                    issues.push(
                        ToolIssue::from_error("uid_store_deleted", &error)
                            .with_uid(msg_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                    false
                } else {
                    steps_succeeded += 1;
                    true
                };

                if deleted {
                    steps_attempted += 1;
                    if let Err(error) =
                        imap::uid_expunge(&self.config, &mut session, msg_id.uid).await
                    {
                        issues.push(
                            ToolIssue::from_error("uid_expunge", &error)
                                .with_uid(msg_id.uid)
                                .with_message_id(&encoded_message_id),
                        );
                    } else {
                        steps_succeeded += 1;
                    }
                }
            }
        }

        let status = status_from_issue_and_counts(&issues, steps_succeeded > 0);
        log_runtime_issues(
            "imap_move_message",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "source_mailbox": msg_id.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "message_id": encoded_message_id,
            "new_message_id": serde_json::Value::Null,
            "steps_attempted": steps_attempted,
            "steps_succeeded": steps_succeeded,
        }))
    }

    async fn delete_message_impl(&self, input: DeleteMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        if !input.confirm {
            return Err(AppError::InvalidInput(
                "delete requires confirm=true".to_owned(),
            ));
        }

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();
        let mut steps_attempted = 0usize;
        let mut steps_succeeded = 0usize;

        steps_attempted += 1;
        let mut session = match imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await {
            Ok(session) => {
                steps_succeeded += 1;
                session
            }
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_delete_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "mailbox": msg_id.mailbox,
                    "message_id": encoded_message_id,
                    "steps_attempted": steps_attempted,
                    "steps_succeeded": steps_succeeded,
                }));
            }
        };
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        steps_attempted += 1;
        let flagged_deleted = if let Err(error) = imap::uid_store(
            &self.config,
            &mut session,
            msg_id.uid,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            issues.push(
                ToolIssue::from_error("uid_store_deleted", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            false
        } else {
            steps_succeeded += 1;
            true
        };

        if flagged_deleted {
            steps_attempted += 1;
            if let Err(error) = imap::uid_expunge(&self.config, &mut session, msg_id.uid).await {
                issues.push(
                    ToolIssue::from_error("uid_expunge", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            } else {
                steps_succeeded += 1;
            }
        }

        let status = status_from_issue_and_counts(&issues, steps_succeeded > 0);
        log_runtime_issues(
            "imap_delete_message",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "mailbox": msg_id.mailbox,
            "message_id": encoded_message_id,
            "steps_attempted": steps_attempted,
            "steps_succeeded": steps_succeeded,
        }))
    }

    async fn create_mailbox_impl(
        &self,
        input: CreateMailboxInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox_name)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;
        imap::create_mailbox(&self.config, &mut session, &input.mailbox_name).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox_name": input.mailbox_name,
        }))
    }

    async fn delete_mailbox_impl(
        &self,
        input: DeleteMailboxInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox_name)?;
        if !input.confirm {
            return Err(AppError::InvalidInput(
                "delete mailbox requires confirm=true".to_owned(),
            ));
        }

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;
        imap::delete_mailbox(&self.config, &mut session, &input.mailbox_name).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox_name": input.mailbox_name,
        }))
    }

    async fn rename_mailbox_impl(
        &self,
        input: RenameMailboxInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.from_name)?;
        validate_mailbox(&input.to_name)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;
        imap::rename_mailbox(&self.config, &mut session, &input.from_name, &input.to_name).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "from_name": input.from_name,
            "to_name": input.to_name,
        }))
    }

    async fn mailbox_status_impl(
        &self,
        input: MailboxStatusInput,
    ) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;
        let (messages, unseen, recent) =
            imap::mailbox_status(&self.config, &mut session, &input.mailbox).await?;

        let status_info = MailboxStatusInfo {
            name: input.mailbox.clone(),
            messages,
            unseen,
            recent,
        };

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox": status_info,
        }))
    }

    async fn search_and_move_impl(
        &self,
        input: SearchAndMoveInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;
        validate_mailbox(&input.destination_mailbox)?;

        // Validate search filters via a synthetic SearchMessagesInput
        let search_input = SearchMessagesInput {
            account_id: input.account_id.clone(),
            mailbox: input.mailbox.clone(),
            cursor: None,
            query: input.query.clone(),
            from: input.from.clone(),
            to: input.to.clone(),
            subject: input.subject.clone(),
            unread_only: input.unread_only,
            last_days: input.last_days,
            start_date: input.start_date.clone(),
            end_date: input.end_date.clone(),
            limit: 1, // irrelevant; we use our own limit
            include_snippet: false,
            snippet_max_chars: None,
        };
        validate_search_input(&search_input)?;

        let limit = input.limit.clamp(1, MAX_BULK_IDS);

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;

        // Search (read-only SELECT via EXAMINE)
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &input.mailbox).await?;
        let query = build_search_query(&search_input)?;
        let all_uids = imap::uid_search(&self.config, &mut session, &query).await?;

        let total_matched = all_uids.len();
        if total_matched == 0 {
            return Ok(serde_json::json!({
                "status": "ok",
                "account_id": input.account_id,
                "source_mailbox": input.mailbox,
                "destination_mailbox": input.destination_mailbox,
                "total_matched": 0,
                "moved_count": 0,
                "has_more": false,
            }));
        }

        let uids_to_move: Vec<u32> = all_uids.into_iter().take(limit).collect();
        let uid_set = uids_to_move
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        // Need read-write SELECT for MOVE; re-open the mailbox
        // (async-imap requires re-SELECT after EXAMINE)
        let rw_uidvalidity =
            imap::select_mailbox_readwrite(&self.config, &mut session, &input.mailbox).await?;
        if rw_uidvalidity != uidvalidity {
            return Err(AppError::Conflict(
                "UIDVALIDITY changed between search and move".to_owned(),
            ));
        }

        let caps = imap::capabilities(&self.config, &mut session).await?;
        if caps.has_str("MOVE") {
            imap::uid_move_bulk(&self.config, &mut session, &uid_set, &input.destination_mailbox)
                .await?;
        } else {
            imap::uid_copy_bulk(&self.config, &mut session, &uid_set, &input.destination_mailbox)
                .await?;
            imap::uid_store_bulk(
                &self.config,
                &mut session,
                &uid_set,
                "+FLAGS.SILENT (\\Deleted)",
            )
            .await?;
            imap::uid_expunge_bulk(&self.config, &mut session, &uid_set).await?;
        }

        let moved_count = uids_to_move.len();
        let remaining = total_matched - moved_count;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "source_mailbox": input.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "total_matched": total_matched,
            "moved_count": moved_count,
            "remaining": remaining,
            "has_more": remaining > 0,
        }))
    }

    async fn search_and_delete_impl(
        &self,
        input: SearchAndDeleteInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;
        if !input.confirm {
            return Err(AppError::InvalidInput(
                "search_and_delete requires confirm=true".to_owned(),
            ));
        }

        let search_input = SearchMessagesInput {
            account_id: input.account_id.clone(),
            mailbox: input.mailbox.clone(),
            cursor: None,
            query: input.query.clone(),
            from: input.from.clone(),
            to: input.to.clone(),
            subject: input.subject.clone(),
            unread_only: input.unread_only,
            last_days: input.last_days,
            start_date: input.start_date.clone(),
            end_date: input.end_date.clone(),
            limit: 1,
            include_snippet: false,
            snippet_max_chars: None,
        };
        validate_search_input(&search_input)?;

        let limit = input.limit.clamp(1, MAX_BULK_IDS);

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;

        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &input.mailbox).await?;
        let query = build_search_query(&search_input)?;
        let all_uids = imap::uid_search(&self.config, &mut session, &query).await?;

        let total_matched = all_uids.len();
        if total_matched == 0 {
            return Ok(serde_json::json!({
                "status": "ok",
                "account_id": input.account_id,
                "mailbox": input.mailbox,
                "total_matched": 0,
                "deleted_count": 0,
                "has_more": false,
            }));
        }

        let uids_to_delete: Vec<u32> = all_uids.into_iter().take(limit).collect();
        let uid_set = uids_to_delete
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let rw_uidvalidity =
            imap::select_mailbox_readwrite(&self.config, &mut session, &input.mailbox).await?;
        if rw_uidvalidity != uidvalidity {
            return Err(AppError::Conflict(
                "UIDVALIDITY changed between search and delete".to_owned(),
            ));
        }

        imap::uid_store_bulk(
            &self.config,
            &mut session,
            &uid_set,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await?;
        imap::uid_expunge_bulk(&self.config, &mut session, &uid_set).await?;

        let deleted_count = uids_to_delete.len();
        let remaining = total_matched - deleted_count;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox": input.mailbox,
            "total_matched": total_matched,
            "deleted_count": deleted_count,
            "remaining": remaining,
            "has_more": remaining > 0,
        }))
    }

    async fn bulk_move_impl(&self, input: BulkMoveInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.destination_mailbox)?;
        validate_bulk_ids(&input.message_ids)?;

        let parsed = parse_bulk_message_ids(&input.account_id, &input.message_ids)?;
        let uid_set = parsed
            .uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;

        let uidvalidity =
            imap::select_mailbox_readwrite(&self.config, &mut session, &parsed.mailbox).await?;
        if uidvalidity != parsed.uidvalidity {
            return Err(AppError::Conflict(
                "UIDVALIDITY changed; message IDs may be stale".to_owned(),
            ));
        }

        let caps = imap::capabilities(&self.config, &mut session).await?;
        if caps.has_str("MOVE") {
            imap::uid_move_bulk(&self.config, &mut session, &uid_set, &input.destination_mailbox)
                .await?;
        } else {
            imap::uid_copy_bulk(&self.config, &mut session, &uid_set, &input.destination_mailbox)
                .await?;
            imap::uid_store_bulk(
                &self.config,
                &mut session,
                &uid_set,
                "+FLAGS.SILENT (\\Deleted)",
            )
            .await?;
            imap::uid_expunge_bulk(&self.config, &mut session, &uid_set).await?;
        }

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "source_mailbox": parsed.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "moved_count": parsed.uids.len(),
        }))
    }

    async fn bulk_delete_impl(&self, input: BulkDeleteInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        if !input.confirm {
            return Err(AppError::InvalidInput(
                "bulk delete requires confirm=true".to_owned(),
            ));
        }
        validate_bulk_ids(&input.message_ids)?;

        let parsed = parse_bulk_message_ids(&input.account_id, &input.message_ids)?;
        let uid_set = parsed
            .uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;

        let uidvalidity =
            imap::select_mailbox_readwrite(&self.config, &mut session, &parsed.mailbox).await?;
        if uidvalidity != parsed.uidvalidity {
            return Err(AppError::Conflict(
                "UIDVALIDITY changed; message IDs may be stale".to_owned(),
            ));
        }

        imap::uid_store_bulk(
            &self.config,
            &mut session,
            &uid_set,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await?;
        imap::uid_expunge_bulk(&self.config, &mut session, &uid_set).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox": parsed.mailbox,
            "deleted_count": parsed.uids.len(),
        }))
    }

    async fn bulk_update_flags_impl(
        &self,
        input: BulkUpdateFlagsInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_bulk_ids(&input.message_ids)?;

        let has_add = input.add_flags.as_ref().is_some_and(|f| !f.is_empty());
        let has_remove = input.remove_flags.as_ref().is_some_and(|f| !f.is_empty());
        if !has_add && !has_remove {
            return Err(AppError::InvalidInput(
                "at least one of add_flags or remove_flags is required".to_owned(),
            ));
        }
        if let Some(ref flags) = input.add_flags {
            validate_flags(flags, "add_flags")?;
        }
        if let Some(ref flags) = input.remove_flags {
            validate_flags(flags, "remove_flags")?;
        }

        let parsed = parse_bulk_message_ids(&input.account_id, &input.message_ids)?;
        let uid_set = parsed
            .uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;

        let uidvalidity =
            imap::select_mailbox_readwrite(&self.config, &mut session, &parsed.mailbox).await?;
        if uidvalidity != parsed.uidvalidity {
            return Err(AppError::Conflict(
                "UIDVALIDITY changed; message IDs may be stale".to_owned(),
            ));
        }

        if let Some(ref flags) = input.add_flags {
            if !flags.is_empty() {
                let flag_str = format!("+FLAGS.SILENT ({})", flags.join(" "));
                imap::uid_store_bulk(&self.config, &mut session, &uid_set, &flag_str).await?;
            }
        }
        if let Some(ref flags) = input.remove_flags {
            if !flags.is_empty() {
                let flag_str = format!("-FLAGS.SILENT ({})", flags.join(" "));
                imap::uid_store_bulk(&self.config, &mut session, &uid_set, &flag_str).await?;
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox": parsed.mailbox,
            "updated_count": parsed.uids.len(),
            "add_flags": input.add_flags,
            "remove_flags": input.remove_flags,
        }))
    }

    async fn append_message_impl(
        &self,
        input: AppendMessageInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;

        if input.raw_message.is_empty() {
            return Err(AppError::InvalidInput(
                "raw_message must not be empty".to_owned(),
            ));
        }
        if input.raw_message.len() > 10_000_000 {
            return Err(AppError::InvalidInput(
                "raw_message exceeds 10MB limit".to_owned(),
            ));
        }

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account, self.token_manager.as_deref()).await?;
        imap::append(
            &self.config,
            &mut session,
            &input.mailbox,
            input.raw_message.as_bytes(),
        )
        .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "mailbox": input.mailbox,
            "size_bytes": input.raw_message.len(),
        }))
    }

    // ─── SMTP impl methods ───────────────────────────────────────────────────

    async fn smtp_send_message_impl(
        &self,
        input: SmtpSendMessageInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_smtp_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_email_recipients(&input.to, "to")?;
        if !input.cc.is_empty() {
            validate_email_recipients(&input.cc, "cc")?;
        }
        if !input.bcc.is_empty() {
            validate_email_recipients(&input.bcc, "bcc")?;
        }
        if input.subject.is_empty() || input.subject.len() > 998 {
            return Err(AppError::invalid("subject must be 1..998 characters"));
        }
        if input.body_text.is_none() && input.body_html.is_none() {
            return Err(AppError::invalid(
                "at least one of body_text or body_html is required",
            ));
        }

        let smtp_config = self.config.get_smtp_account(&input.account_id)?;

        let composition = smtp::EmailComposition {
            from: smtp_config.user.clone(),
            to: input.to.clone(),
            cc: input.cc.clone(),
            bcc: input.bcc.clone(),
            subject: input.subject.clone(),
            body_text: input.body_text,
            body_html: input.body_html,
            reply_to: input.reply_to,
            in_reply_to: input.in_reply_to,
            references: input.references,
        };

        let message_id = smtp::send_email(
            smtp_config,
            self.token_manager.as_deref(),
            self.config.smtp_timeout_ms,
            &composition,
        )
        .await?;

        // Optionally save to Sent folder via IMAP
        if self.config.smtp_save_sent {
            if let Err(e) = self
                .save_to_sent_folder(&input.account_id, &composition)
                .await
            {
                warn!(
                    account_id = input.account_id,
                    "failed to save sent message to IMAP Sent folder: {e}"
                );
            }
        }

        let recipient_count = input.to.len() + input.cc.len() + input.bcc.len();
        let summary = format!("Email sent to {recipient_count} recipient(s)");
        let data = serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "message_id": message_id,
            "recipients_count": recipient_count,
        });
        Ok((summary, data))
    }

    async fn smtp_reply_message_impl(
        &self,
        input: SmtpReplyMessageInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_smtp_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;

        // Fetch original message headers via IMAP
        let msg_id = MessageId::parse(&input.message_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &msg_id.mailbox).await?;
        if uidvalidity != msg_id.uidvalidity {
            return Err(AppError::Conflict(format!(
                "UIDVALIDITY mismatch: message_id has {} but mailbox has {}",
                msg_id.uidvalidity, uidvalidity
            )));
        }

        let raw_bytes = imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await?;
        let parsed = mailparse::parse_mail(&raw_bytes)
            .map_err(|e| AppError::Internal(format!("failed to parse original message: {e}")))?;

        // Extract headers for reply
        let get_header = |name: &str| -> Option<String> {
            parsed
                .headers
                .iter()
                .find(|h| h.get_key().eq_ignore_ascii_case(name))
                .map(|h| h.get_value())
        };

        let original_from = get_header("From").unwrap_or_default();
        let original_to = get_header("To").unwrap_or_default();
        let original_cc = get_header("Cc");
        let original_subject = get_header("Subject").unwrap_or_default();
        let original_message_id = get_header("Message-ID").unwrap_or_default();
        let original_references = get_header("References");

        // Build reply subject
        let reply_subject = if original_subject
            .to_ascii_lowercase()
            .starts_with("re:")
        {
            original_subject.clone()
        } else {
            format!("Re: {original_subject}")
        };

        // Build References header: original References + original Message-ID
        let references = match original_references {
            Some(refs) => format!("{refs} {original_message_id}"),
            None => original_message_id.clone(),
        };

        // Determine recipients
        let smtp_config = self.config.get_smtp_account(&input.account_id)?;
        let self_email = smtp_config.user.to_ascii_lowercase();

        let to = if input.reply_all {
            // Reply-all: reply to original From + original To (minus self)
            let mut recipients = vec![original_from.clone()];
            for addr in original_to.split(',').map(str::trim) {
                if !addr.is_empty() && !addr.to_ascii_lowercase().contains(&self_email) {
                    recipients.push(addr.to_owned());
                }
            }
            recipients
        } else {
            vec![original_from.clone()]
        };

        let cc = if input.reply_all {
            original_cc
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|a| !a.is_empty() && !a.to_ascii_lowercase().contains(&self_email))
                .map(String::from)
                .collect()
        } else {
            vec![]
        };

        let composition = smtp::EmailComposition {
            from: smtp_config.user.clone(),
            to: to.clone(),
            cc,
            bcc: vec![],
            subject: reply_subject,
            body_text: Some(input.body_text),
            body_html: input.body_html,
            reply_to: None,
            in_reply_to: Some(original_message_id),
            references: Some(references),
        };

        let sent_message_id = smtp::send_email(
            smtp_config,
            self.token_manager.as_deref(),
            self.config.smtp_timeout_ms,
            &composition,
        )
        .await?;

        if self.config.smtp_save_sent {
            if let Err(e) = self
                .save_to_sent_folder(&input.account_id, &composition)
                .await
            {
                warn!(
                    account_id = input.account_id,
                    "failed to save reply to IMAP Sent folder: {e}"
                );
            }
        }

        let summary = format!("Reply sent to {}", to.first().unwrap_or(&String::new()));
        let data = serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "message_id": sent_message_id,
            "in_reply_to": input.message_id,
        });
        Ok((summary, data))
    }

    async fn smtp_forward_message_impl(
        &self,
        input: SmtpForwardMessageInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_smtp_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_email_recipients(&input.to, "to")?;

        // Fetch original message via IMAP
        let msg_id = MessageId::parse(&input.message_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &msg_id.mailbox).await?;
        if uidvalidity != msg_id.uidvalidity {
            return Err(AppError::Conflict(format!(
                "UIDVALIDITY mismatch: message_id has {} but mailbox has {}",
                msg_id.uidvalidity, uidvalidity
            )));
        }

        let raw_bytes = imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await?;
        let parsed = mailparse::parse_mail(&raw_bytes)
            .map_err(|e| AppError::Internal(format!("failed to parse original message: {e}")))?;

        let get_header = |name: &str| -> Option<String> {
            parsed
                .headers
                .iter()
                .find(|h| h.get_key().eq_ignore_ascii_case(name))
                .map(|h| h.get_value())
        };

        let original_subject = get_header("Subject").unwrap_or_default();
        let original_from = get_header("From").unwrap_or_default();
        let original_date = get_header("Date").unwrap_or_default();

        let fwd_subject = if original_subject
            .to_ascii_lowercase()
            .starts_with("fwd:")
        {
            original_subject.clone()
        } else {
            format!("Fwd: {original_subject}")
        };

        // Build forward body with original message inline
        let original_body = mime::extract_body_text(&parsed).unwrap_or_default();
        let forward_text = format!(
            "{}\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\n\n{}",
            input.body_text.as_deref().unwrap_or(""),
            original_from,
            original_date,
            original_subject,
            original_body,
        );

        let smtp_config = self.config.get_smtp_account(&input.account_id)?;

        let composition = smtp::EmailComposition {
            from: smtp_config.user.clone(),
            to: input.to.clone(),
            cc: vec![],
            bcc: vec![],
            subject: fwd_subject,
            body_text: Some(forward_text),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
        };

        let sent_message_id = smtp::send_email(
            smtp_config,
            self.token_manager.as_deref(),
            self.config.smtp_timeout_ms,
            &composition,
        )
        .await?;

        if self.config.smtp_save_sent {
            if let Err(e) = self
                .save_to_sent_folder(&input.account_id, &composition)
                .await
            {
                warn!(
                    account_id = input.account_id,
                    "failed to save forwarded message to IMAP Sent folder: {e}"
                );
            }
        }

        let summary = format!(
            "Forwarded to {} recipient(s)",
            input.to.len()
        );
        let data = serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "message_id": sent_message_id,
            "forwarded_from": input.message_id,
        });
        Ok((summary, data))
    }

    async fn smtp_verify_account_impl(
        &self,
        input: SmtpVerifyAccountInput,
    ) -> AppResult<(String, serde_json::Value)> {
        validate_account_id(&input.account_id)?;
        let smtp_config = self.config.get_smtp_account(&input.account_id)?;

        smtp::verify_smtp(
            smtp_config,
            self.token_manager.as_deref(),
            self.config.smtp_timeout_ms,
        )
        .await?;

        let data = serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "host": smtp_config.host,
            "port": smtp_config.port,
            "security": format!("{:?}", smtp_config.security).to_ascii_lowercase(),
        });
        Ok(("SMTP connection verified".to_owned(), data))
    }

    /// Save a sent message to the IMAP Sent folder.
    ///
    /// Attempts to detect the correct Sent folder name for the provider.
    async fn save_to_sent_folder(
        &self,
        account_id: &str,
        composition: &smtp::EmailComposition,
    ) -> AppResult<()> {
        let account = self.config.get_account(account_id)?;
        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;

        // Detect Sent folder: try common names
        let mailboxes = imap::list_all_mailboxes(&self.config, &mut session).await?;
        let sent_folder = mailboxes
            .iter()
            .map(|m| m.name().to_owned())
            .find(|name| {
                let lower = name.to_ascii_lowercase();
                lower == "sent"
                    || lower == "sent items"
                    || lower.ends_with("/sent")
                    || lower.ends_with("/sent mail")
                    || lower.contains("[gmail]/sent")
            })
            .unwrap_or_else(|| "Sent".to_owned());

        // Build a minimal RFC822 message for IMAP APPEND
        let mut rfc822 = format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\n",
            composition.from,
            composition.to.join(", "),
            composition.subject,
            chrono::Utc::now().to_rfc2822(),
        );
        if !composition.cc.is_empty() {
            rfc822.push_str(&format!("Cc: {}\r\n", composition.cc.join(", ")));
        }
        if let Some(ref in_reply_to) = composition.in_reply_to {
            rfc822.push_str(&format!("In-Reply-To: {in_reply_to}\r\n"));
        }
        if let Some(ref references) = composition.references {
            rfc822.push_str(&format!("References: {references}\r\n"));
        }
        rfc822.push_str("\r\n");
        if let Some(ref body) = composition.body_text {
            rfc822.push_str(body);
        }

        imap::append(&self.config, &mut session, &sent_folder, rfc822.as_bytes()).await?;
        Ok(())
    }
}

/// Calculate elapsed milliseconds
fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn status_from_counts(no_issues: bool, has_data: bool) -> &'static str {
    if no_issues {
        "ok"
    } else if has_data {
        "partial"
    } else {
        "failed"
    }
}

fn status_from_issue_and_counts(issues: &[ToolIssue], has_data: bool) -> &'static str {
    status_from_counts(issues.is_empty(), has_data)
}

fn app_error_code(error: &AppError) -> &'static str {
    match error {
        AppError::InvalidInput(_) => "invalid_input",
        AppError::NotFound(_) => "not_found",
        AppError::AuthFailed(_) => "auth_failed",
        AppError::Timeout(_) => "timeout",
        AppError::Conflict(_) => "conflict",
        AppError::TokenRefresh(_) => "token_refresh_failed",
        AppError::Internal(_) => "internal",
    }
}

fn log_runtime_issues(
    tool: &str,
    status: &str,
    account_id: &str,
    mailbox: Option<&str>,
    issues: &[ToolIssue],
) {
    for issue in issues {
        let is_error = status == "failed" || matches!(issue.code.as_str(), "internal" | "timeout");
        if is_error {
            error!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        } else {
            warn!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        }
    }
}

fn next_action(instruction: &str, tool: &str, arguments: serde_json::Value) -> NextAction {
    NextAction {
        instruction: instruction.to_owned(),
        tool: tool.to_owned(),
        arguments,
    }
}

fn next_action_retry_verify(account_id: &str) -> NextAction {
    next_action(
        "Re-verify account connectivity before proceeding.",
        "imap_verify_account",
        serde_json::json!({
            "account_id": account_id,
        }),
    )
}

fn next_action_list_mailboxes(account_id: &str) -> NextAction {
    next_action(
        "List mailboxes to choose a mailbox for message search.",
        "imap_list_mailboxes",
        serde_json::json!({
            "account_id": account_id,
        }),
    )
}

fn next_action_search_mailbox(account_id: &str, mailbox: &str) -> NextAction {
    next_action(
        "Search for messages in the selected mailbox.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailbox": mailbox,
            "limit": 10,
            "include_snippet": false,
        }),
    )
}

fn preferred_mailbox_name(mailboxes: &[MailboxInfo]) -> Option<String> {
    mailboxes
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("INBOX"))
        .map(|m| m.name.clone())
        .or_else(|| mailboxes.first().map(|m| m.name.clone()))
}

fn next_action_for_search_result(
    status: &str,
    account_id: &str,
    mailbox: &str,
    limit: usize,
    cursor: Option<&str>,
    messages: &[MessageSummary],
) -> NextAction {
    if let Some(cursor) = cursor {
        return next_action(
            "Continue pagination to retrieve more messages.",
            "imap_search_messages",
            serde_json::json!({
                "account_id": account_id,
                "mailbox": mailbox,
                "cursor": cursor,
                "limit": limit,
                "include_snippet": false,
            }),
        );
    }

    if status == "failed" {
        return next_action_retry_verify(account_id);
    }

    if let Some(first) = messages.first() {
        return next_action(
            "Open a message to inspect full content and headers.",
            "imap_get_message",
            serde_json::json!({
                "account_id": account_id,
                "message_id": first.message_id,
            }),
        );
    }

    next_action(
        "Retry search with broader criteria.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailbox": mailbox,
            "limit": limit,
            "include_snippet": false,
        }),
    )
}

fn is_hard_precondition_error(error: &AppError) -> bool {
    matches!(error, AppError::InvalidInput(_) | AppError::Conflict(_))
}

/// Build a standardized MCP tool response envelope from business logic output
fn finalize_tool<T>(
    started: Instant,
    tool: &str,
    result: AppResult<(String, T)>,
) -> Result<Json<ToolEnvelope<T>>, ErrorData>
where
    T: schemars::JsonSchema,
{
    match result {
        Ok((summary, data)) => Ok(Json(ToolEnvelope {
            summary,
            data,
            meta: Meta::now(duration_ms(started)),
        })),
        Err(e) => {
            error!(
                tool,
                code = app_error_code(&e),
                message = %e,
                "hard mcp error"
            );
            Err(e.to_error_data())
        }
    }
}

/// Parse message_id, validate mailbox, and enforce account_id match.
fn parse_and_validate_message_id(account_id: &str, message_id: &str) -> AppResult<MessageId> {
    let msg_id = MessageId::parse(message_id)?;
    validate_mailbox(&msg_id.mailbox)?;
    if msg_id.account_id != account_id {
        return Err(AppError::InvalidInput(
            "message_id account does not match account_id".to_owned(),
        ));
    }
    Ok(msg_id)
}

/// Select mailbox readonly and ensure uidvalidity still matches message_id.
async fn ensure_uidvalidity_matches_readonly(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    let current_uidvalidity =
        imap::select_mailbox_readonly(config, session, &msg_id.mailbox).await?;
    if current_uidvalidity != msg_id.uidvalidity {
        return Err(AppError::Conflict(
            "message uidvalidity no longer matches mailbox".to_owned(),
        ));
    }
    Ok(())
}

/// Select mailbox readwrite and ensure uidvalidity still matches message_id.
async fn ensure_uidvalidity_matches_readwrite(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    let current_uidvalidity =
        imap::select_mailbox_readwrite(config, session, &msg_id.mailbox).await?;
    if current_uidvalidity != msg_id.uidvalidity {
        return Err(AppError::Conflict(
            "message uidvalidity no longer matches mailbox".to_owned(),
        ));
    }
    Ok(())
}

struct SearchSnapshot {
    uids_desc: Vec<u32>,
    offset: usize,
    include_snippet: bool,
    snippet_max_chars: usize,
    cursor_id_from_request: Option<String>,
}

async fn resume_cursor_search(
    cursors: &Arc<Mutex<CursorStore>>,
    input: &SearchMessagesInput,
    uidvalidity: u32,
    cursor_id: String,
) -> AppResult<SearchSnapshot> {
    let mut store = cursors.lock().await;
    let entry = store
        .get(&cursor_id)
        .ok_or_else(|| AppError::InvalidInput("cursor is invalid or expired".to_owned()))?;
    if entry.account_id != input.account_id || entry.mailbox != input.mailbox {
        return Err(AppError::InvalidInput(
            "cursor does not match account/mailbox".to_owned(),
        ));
    }
    if entry.uidvalidity != uidvalidity {
        store.delete(&cursor_id);
        return Err(AppError::Conflict(
            "mailbox snapshot changed; rerun search".to_owned(),
        ));
    }
    Ok(SearchSnapshot {
        uids_desc: entry.uids_desc,
        offset: entry.offset,
        include_snippet: entry.include_snippet,
        snippet_max_chars: entry.snippet_max_chars,
        cursor_id_from_request: Some(cursor_id),
    })
}

async fn start_new_search(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    input: &SearchMessagesInput,
) -> AppResult<SearchSnapshot> {
    let query = build_search_query(input)?;
    let searched_uids = imap::uid_search(config, session, &query).await?;
    if searched_uids.len() > MAX_CURSOR_UIDS_STORED {
        return Err(AppError::InvalidInput(format!(
            "search matched {} messages; narrow filters to at most {} results",
            searched_uids.len(),
            MAX_CURSOR_UIDS_STORED
        )));
    }

    Ok(SearchSnapshot {
        uids_desc: searched_uids,
        offset: 0,
        include_snippet: input.include_snippet,
        snippet_max_chars: input.snippet_max_chars.unwrap_or(200).clamp(50, 500),
        cursor_id_from_request: None,
    })
}

async fn build_message_summaries(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    uids: &[u32],
    options: SummaryBuildOptions<'_>,
) -> SummaryBuildResult {
    let mut messages = Vec::with_capacity(uids.len());
    let mut issues = Vec::new();
    let mut failed = 0usize;

    for uid in uids {
        let (header_bytes, flags) = match imap::fetch_headers_and_flags(config, session, *uid).await
        {
            Ok(result) => result,
            Err(error) => {
                failed += 1;
                issues
                    .push(ToolIssue::from_error("fetch_headers_and_flags", &error).with_uid(*uid));
                continue;
            }
        };

        let headers = match mime::parse_header_bytes(&header_bytes) {
            Ok(headers) => headers,
            Err(error) => {
                failed += 1;
                issues.push(ToolIssue::from_error("parse_header_bytes", &error).with_uid(*uid));
                continue;
            }
        };

        let date = header_value(&headers, "date");
        let from = header_value(&headers, "from");
        let subject = header_value(&headers, "subject");

        let snippet = if options.include_snippet {
            subject
                .clone()
                .map(|s| mime::truncate_chars(s, options.snippet_max_chars))
        } else {
            None
        };

        let message_id = MessageId {
            account_id: options.account_id.to_owned(),
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
        }
        .encode();
        let message_uri = build_message_uri(
            options.account_id,
            options.mailbox,
            options.uidvalidity,
            *uid,
        );
        let message_raw_uri = build_message_raw_uri(
            options.account_id,
            options.mailbox,
            options.uidvalidity,
            *uid,
        );

        messages.push(MessageSummary {
            message_id,
            message_uri,
            message_raw_uri,
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
            date,
            from,
            subject,
            flags: Some(flags),
            snippet,
        });
    }

    SummaryBuildResult {
        messages,
        issues,
        attempted: uids.len(),
        failed,
    }
}

struct SummaryBuildOptions<'a> {
    account_id: &'a str,
    mailbox: &'a str,
    uidvalidity: u32,
    include_snippet: bool,
    snippet_max_chars: usize,
}

/// Validate account_id format
fn validate_account_id(account_id: &str) -> AppResult<()> {
    if account_id.is_empty() || account_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "account_id must be 1..64 characters".to_owned(),
        ));
    }
    if !account_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(AppError::InvalidInput(
            "account_id must match [A-Za-z0-9_-]+".to_owned(),
        ));
    }
    Ok(())
}

/// Validate mailbox name format
fn validate_mailbox(mailbox: &str) -> AppResult<()> {
    if mailbox.is_empty() || mailbox.len() > 256 {
        return Err(AppError::InvalidInput(
            "mailbox must be 1..256 characters".to_owned(),
        ));
    }
    validate_no_controls(mailbox, "mailbox")?;
    Ok(())
}

/// Reject IMAP control characters in user-provided values
fn validate_no_controls(value: &str, field: &str) -> AppResult<()> {
    if value.chars().any(|ch| ch.is_ascii_control()) {
        return Err(AppError::InvalidInput(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Validate numeric value in range
fn validate_chars(value: usize, min: usize, max: usize, field: &str) -> AppResult<()> {
    if value < min || value > max {
        return Err(AppError::InvalidInput(format!(
            "{field} must be in range {min}..{max}"
        )));
    }
    Ok(())
}

/// Validate search messages input
fn validate_search_input(input: &SearchMessagesInput) -> AppResult<()> {
    validate_mailbox(&input.mailbox)?;
    validate_chars(input.limit, 1, 50, "limit")?;
    if let Some(v) = input.last_days
        && !(1..=365).contains(&v)
    {
        return Err(AppError::InvalidInput(
            "last_days must be in range 1..365".to_owned(),
        ));
    }
    if let Some(v) = input.snippet_max_chars {
        validate_chars(v, 50, 500, "snippet_max_chars")?;
        if !input.include_snippet {
            return Err(AppError::InvalidInput(
                "snippet_max_chars requires include_snippet=true".to_owned(),
            ));
        }
    }

    if let Some(v) = &input.query {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.from {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.to {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.subject {
        validate_search_text(v)?;
    }

    let has_filters = input.query.is_some()
        || input.from.is_some()
        || input.to.is_some()
        || input.subject.is_some()
        || input.unread_only.is_some()
        || input.last_days.is_some()
        || input.start_date.is_some()
        || input.end_date.is_some();
    if input.cursor.is_some() && has_filters {
        return Err(AppError::InvalidInput(
            "cursor cannot be combined with search criteria".to_owned(),
        ));
    }

    if input.last_days.is_some() && (input.start_date.is_some() || input.end_date.is_some()) {
        return Err(AppError::InvalidInput(
            "last_days cannot be combined with start_date/end_date".to_owned(),
        ));
    }

    if let (Some(start), Some(end)) = (&input.start_date, &input.end_date) {
        let start_d = parse_ymd(start)?;
        let end_d = parse_ymd(end)?;
        if start_d > end_d {
            return Err(AppError::InvalidInput(
                "start_date must be <= end_date".to_owned(),
            ));
        }
    }

    Ok(())
}

/// Validate search text field bounds and characters
fn validate_search_text(input: &str) -> AppResult<()> {
    if input.is_empty() || input.len() > 256 {
        return Err(AppError::InvalidInput(
            "search text fields must be 1..256 chars".to_owned(),
        ));
    }
    validate_no_controls(input, "search text")
}

/// Build IMAP SEARCH query string from input
fn build_search_query(input: &SearchMessagesInput) -> AppResult<String> {
    let mut parts = Vec::new();
    if let Some(v) = &input.query {
        parts.push(format!("TEXT \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.from {
        parts.push(format!("FROM \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.to {
        parts.push(format!("TO \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.subject {
        parts.push(format!("SUBJECT \"{}\"", escape_imap_quoted(v)?));
    }
    if input.unread_only.unwrap_or(false) {
        parts.push("UNSEEN".to_owned());
    }
    if let Some(days) = input.last_days {
        let since = Utc::now().date_naive() - ChronoDuration::days(i64::from(days));
        parts.push(format!("SINCE {}", imap_date(since)));
    }
    if let Some(start) = &input.start_date {
        parts.push(format!("SINCE {}", imap_date(parse_ymd(start)?)));
    }
    if let Some(end) = &input.end_date {
        let end_exclusive = parse_ymd(end)? + ChronoDuration::days(1);
        parts.push(format!("BEFORE {}", imap_date(end_exclusive)));
    }

    if parts.is_empty() {
        Ok("ALL".to_owned())
    } else {
        Ok(parts.join(" "))
    }
}

/// Escape backslashes and quotes for IMAP quoted strings
fn escape_imap_quoted(input: &str) -> AppResult<String> {
    validate_search_text(input)?;
    Ok(input.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Validate and normalize IMAP flag atoms
fn validate_flags(flags: &[String], field: &str) -> AppResult<()> {
    for flag in flags {
        validate_flag(flag).map_err(|_| {
            AppError::InvalidInput(format!(
                "{field} contains invalid flag '{flag}'; flags must not contain whitespace, control chars, quotes, parentheses, or braces"
            ))
        })?;
    }
    Ok(())
}

fn validate_flag(flag: &str) -> AppResult<()> {
    if flag.is_empty() || flag.len() > 64 {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    let atom = if let Some(rest) = flag.strip_prefix('\\') {
        if rest.is_empty() {
            return Err(AppError::InvalidInput("invalid flag".to_owned()));
        }
        rest
    } else {
        flag
    };

    if atom.chars().any(|ch| {
        ch.is_ascii_control()
            || ch.is_ascii_whitespace()
            || matches!(ch, '"' | '(' | ')' | '{' | '}' | '\\')
    }) {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    Ok(())
}

/// Format date as IMAP SEARCH date (e.g., "1-Jan-2025")
fn imap_date(date: NaiveDate) -> String {
    date.format("%-d-%b-%Y").to_string()
}

/// Parse YYYY-MM-DD date string
fn parse_ymd(input: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map_err(|_| AppError::InvalidInput(format!("invalid date '{input}', expected YYYY-MM-DD")))
}

/// Get header value by case-insensitive key
fn header_value(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case(key).then(|| v.clone()))
}

/// Check if write operations are enabled
fn require_write_enabled(config: &ServerConfig) -> AppResult<()> {
    if !config.write_enabled {
        return Err(AppError::InvalidInput(
            "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true".to_owned(),
        ));
    }
    Ok(())
}

/// Guard: SMTP write operations require explicit opt-in
fn require_smtp_write_enabled(config: &ServerConfig) -> AppResult<()> {
    if !config.smtp_write_enabled {
        return Err(AppError::InvalidInput(
            "SMTP send tools are disabled; set MAIL_SMTP_WRITE_ENABLED=true".to_owned(),
        ));
    }
    Ok(())
}

/// Validate a list of email recipients
fn validate_email_recipients(addrs: &[String], field: &str) -> AppResult<()> {
    if addrs.is_empty() {
        return Err(AppError::invalid(format!("{field} must have at least one recipient")));
    }
    if addrs.len() > 50 {
        return Err(AppError::invalid(format!("{field} must have at most 50 recipients")));
    }
    for addr in addrs {
        if !addr.contains('@') || addr.len() < 3 {
            return Err(AppError::invalid(format!("invalid email address in {field}: '{addr}'")));
        }
    }
    Ok(())
}

/// Build message URI for display
fn build_message_uri(account_id: &str, mailbox: &str, uidvalidity: u32, uid: u32) -> String {
    format!(
        "imap://{}/mailbox/{}/message/{}/{}",
        account_id,
        urlencoding::encode(mailbox),
        uidvalidity,
        uid
    )
}

/// Build raw message URI
fn build_message_raw_uri(account_id: &str, mailbox: &str, uidvalidity: u32, uid: u32) -> String {
    format!(
        "{}/raw",
        build_message_uri(account_id, mailbox, uidvalidity, uid)
    )
}

fn encode_raw_source_base64(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// Validate bulk message ID list (non-empty, within limit)
fn validate_bulk_ids(ids: &[String]) -> AppResult<()> {
    if ids.is_empty() {
        return Err(AppError::InvalidInput(
            "message_ids must not be empty".to_owned(),
        ));
    }
    if ids.len() > MAX_BULK_IDS {
        return Err(AppError::InvalidInput(format!(
            "message_ids exceeds maximum of {MAX_BULK_IDS}"
        )));
    }
    Ok(())
}

/// Parsed bulk message IDs result
struct BulkParsed {
    mailbox: String,
    uidvalidity: u32,
    uids: Vec<u32>,
}

/// Parse and validate a list of message IDs for bulk operations.
///
/// All message IDs must belong to the same account, mailbox, and uidvalidity.
fn parse_bulk_message_ids(account_id: &str, message_ids: &[String]) -> AppResult<BulkParsed> {
    let mut mailbox: Option<String> = None;
    let mut uidvalidity: Option<u32> = None;
    let mut uids = Vec::with_capacity(message_ids.len());

    for raw_id in message_ids {
        let msg_id = parse_and_validate_message_id(account_id, raw_id)?;
        match (&mailbox, &uidvalidity) {
            (None, None) => {
                mailbox = Some(msg_id.mailbox.clone());
                uidvalidity = Some(msg_id.uidvalidity);
            }
            (Some(m), Some(uv)) => {
                if *m != msg_id.mailbox {
                    return Err(AppError::InvalidInput(
                        "all message_ids must be from the same mailbox".to_owned(),
                    ));
                }
                if *uv != msg_id.uidvalidity {
                    return Err(AppError::InvalidInput(
                        "all message_ids must have the same uidvalidity".to_owned(),
                    ));
                }
            }
            _ => unreachable!(),
        }
        uids.push(msg_id.uid);
    }

    Ok(BulkParsed {
        mailbox: mailbox.unwrap(),
        uidvalidity: uidvalidity.unwrap(),
        uids,
    })
}

#[cfg(test)]
/// Tests for server-side validation and encoding helpers.
mod tests {
    use super::{
        encode_raw_source_base64, escape_imap_quoted, validate_flag, validate_mailbox,
        validate_search_text,
    };

    /// Tests that control characters in search text are rejected.
    #[test]
    fn rejects_control_chars_in_search_text() {
        let err = validate_search_text("hello\nworld").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that control characters in mailbox names are rejected.
    #[test]
    fn rejects_control_chars_in_mailbox() {
        let err = validate_mailbox("INBOX\r").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that line breaks are rejected in IMAP quoted strings.
    #[test]
    fn escape_rejects_linebreaks() {
        let err = escape_imap_quoted("a\nb").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that common IMAP flags are accepted.
    #[test]
    fn validate_flag_allows_common_flags() {
        validate_flag("\\Seen").expect("system flag must be valid");
        validate_flag("Important").expect("keyword flag must be valid");
        validate_flag("$MailFlagBit0").expect("keyword flag must be valid");
    }

    /// Tests that injection-like flag values are rejected.
    #[test]
    fn validate_flag_rejects_injection_like_value() {
        let err = validate_flag("\\Seen) UID FETCH 1:* (BODY[]").expect_err("must fail");
        assert!(err.to_string().contains("invalid flag"));
    }

    /// Tests that raw message sources are correctly base64 encoded.
    #[test]
    fn encodes_raw_source_as_base64() {
        let raw = [0_u8, 159, 255];
        assert_eq!(encode_raw_source_base64(&raw), "AJ//");
    }
}
