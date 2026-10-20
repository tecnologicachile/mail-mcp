//! Draft tools: create, update, send, delete and list drafts
//!
//! One set of tools for every account type. The account's configuration
//! decides the backend (see [`DraftBackend`]):
//!
//! - IMAP: the draft is an RFC822 message in the Drafts folder, flagged
//!   `\Draft`. Its `draft_id` is the regular IMAP message id, so the read
//!   tools (`imap_get_message`, ...) work on it. Sending reads the stored
//!   bytes, strips `Bcc`, refreshes `Date`, sends through SMTP, archives the
//!   same bytes to Sent and removes the draft.
//! - EWS: `CreateItem` with `SaveOnly` in Drafts; `SendItem` sends and moves
//!   it to Sent Items. `draft_id` is the EWS `ItemId` (works with
//!   `ews_get_message`).
//! - Graph: `POST /me/messages` (or `createReply` when `in_reply_to` matches
//!   a message in the mailbox); `/send` sends and files it in Sent Items.
//!   Needs the `Mail.ReadWrite` scope.
//!
//! `draft_update` is "create the new version, then delete the old one" on
//! every backend, so the `draft_id` changes on each update.
//!
//! Every operation on an existing `draft_id` first checks that the target is
//! a draft. `MAIL_DRAFTS_ENABLED` therefore lets an agent prepare mail
//! without being able to delete or send anything else.

use base64::Engine;
use chrono::Utc;
use tracing::warn;

use super::{
    MailImapServer, ToolIssue, decode_attachments, ensure_uidvalidity_matches_readonly,
    ensure_uidvalidity_matches_readwrite, escape_imap_quoted, parse_and_validate_message_id,
    require_smtp_write_enabled, status_from_issue_and_counts, validate_account_id,
    validate_email_no_wrapper_leak, validate_email_recipients,
};
use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AttachmentInput, DraftCreateInput, DraftDeleteInput, DraftListInput, DraftSendInput,
    DraftUpdateInput,
};
use crate::smtp;

/// Default number of drafts `draft_list` returns
const DEFAULT_DRAFT_LIST_LIMIT: usize = 20;
/// Maximum number of drafts `draft_list` returns
const MAX_DRAFT_LIST_LIMIT: usize = 50;
/// Maximum accepted length of a `draft_id`
const MAX_DRAFT_ID_CHARS: usize = 1024;
/// Flags set on drafts appended over IMAP
const IMAP_DRAFT_FLAGS: &str = "\\Draft \\Seen";

/// Where an account's drafts live
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DraftBackend {
    /// Exchange Web Services (account has `MAIL_EWS_<ID>_*`)
    Ews,
    /// Microsoft Graph (account has `MAIL_GRAPH_<ID>_*`)
    Graph,
    /// IMAP Drafts folder, sent through SMTP
    Imap,
}

impl DraftBackend {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ews => "ews",
            Self::Graph => "graph",
            Self::Imap => "imap",
        }
    }
}

/// Pick the draft backend from the account's configuration.
///
/// EWS wins over Graph, Graph over IMAP. Only the dedicated EWS and Graph
/// configs count: a generic OAuth2 account (e.g. Gmail with XOAUTH2) stays
/// on IMAP.
pub(super) fn draft_backend_for(config: &ServerConfig, account_id: &str) -> Option<DraftBackend> {
    if config.ews_accounts.contains_key(account_id) {
        Some(DraftBackend::Ews)
    } else if config.graph_oauth2_accounts.contains_key(account_id) {
        Some(DraftBackend::Graph)
    } else if config.accounts.contains_key(account_id) {
        Some(DraftBackend::Imap)
    } else {
        None
    }
}

/// Draft fields shared by `draft_create` and `draft_update`
struct DraftContent {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body_text: Option<String>,
    body_html: Option<String>,
    reply_to: Option<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
    attachments: Vec<AttachmentInput>,
}

impl From<DraftCreateInput> for DraftContent {
    fn from(input: DraftCreateInput) -> Self {
        Self {
            to: input.to,
            cc: input.cc,
            bcc: input.bcc,
            subject: input.subject,
            body_text: input.body_text,
            body_html: input.body_html,
            reply_to: input.reply_to,
            in_reply_to: input.in_reply_to,
            references: input.references,
            attachments: input.attachments,
        }
    }
}

impl From<DraftUpdateInput> for DraftContent {
    fn from(input: DraftUpdateInput) -> Self {
        Self {
            to: input.to,
            cc: input.cc,
            bcc: input.bcc,
            subject: input.subject,
            body_text: input.body_text,
            body_html: input.body_html,
            reply_to: input.reply_to,
            in_reply_to: input.in_reply_to,
            references: input.references,
            attachments: input.attachments,
        }
    }
}

/// A file attachment resolved from disk or base64, ready for any backend
struct ResolvedAttachment {
    filename: String,
    content_type: String,
    content: Vec<u8>,
}

impl ResolvedAttachment {
    fn content_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.content)
    }
}

/// Result of writing a draft
struct CreatedDraft {
    /// New `draft_id`; `None` when the draft was saved but could not be
    /// located again (reported through `issues`)
    draft_id: Option<String>,
    /// RFC 5322 Message-ID header, when known
    message_id: Option<String>,
    issues: Vec<ToolIssue>,
}

/// Stored draft prepared for SMTP
#[derive(Debug)]
struct PreparedDraft {
    rfc822: Vec<u8>,
    envelope_from: String,
    recipients: Vec<String>,
    message_id: Option<String>,
}

impl MailImapServer {
    // ─── Tool entry points ──────────────────────────────────────────────────

    pub(super) async fn draft_create_impl(
        &self,
        input: DraftCreateInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_drafts_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        let account_id = input.account_id.clone();
        let content = DraftContent::from(input);
        validate_draft_content(&content)?;
        let backend = self.draft_backend(&account_id)?;
        let attachments = self.resolve_draft_attachments(&content.attachments)?;

        let created = self
            .write_draft(backend, &account_id, &content, &attachments)
            .await?;
        let status = if created.draft_id.is_some() {
            "ok"
        } else {
            "partial"
        };
        let summary = match created.draft_id {
            Some(_) => format!("Draft saved via {}", backend.as_str()),
            None => format!(
                "Draft saved via {} but its id could not be read back",
                backend.as_str()
            ),
        };
        let data = serde_json::json!({
            "status": status,
            "issues": created.issues,
            "account_id": account_id,
            "backend": backend.as_str(),
            "draft_id": created.draft_id,
            "message_id": created.message_id,
        });
        Ok((summary, data))
    }

    pub(super) async fn draft_update_impl(
        &self,
        input: DraftUpdateInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_drafts_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_draft_id(&input.draft_id)?;
        let account_id = input.account_id.clone();
        let old_draft_id = input.draft_id.clone();
        let content = DraftContent::from(input);
        validate_draft_content(&content)?;
        let backend = self.draft_backend(&account_id)?;
        let attachments = self.resolve_draft_attachments(&content.attachments)?;

        // Refuse before writing anything if the target is not a draft.
        self.ensure_is_draft(backend, &account_id, &old_draft_id)
            .await?;

        // Create first, delete second: a failure in between leaves two
        // drafts, never zero.
        let mut created = self
            .write_draft(backend, &account_id, &content, &attachments)
            .await?;
        let mut issues = std::mem::take(&mut created.issues);
        let old_removed = match self.remove_draft(backend, &account_id, &old_draft_id).await {
            Ok(()) => true,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("delete_previous_draft", &error)
                        .with_message_id(&old_draft_id),
                );
                false
            }
        };

        let status = if issues.is_empty() { "ok" } else { "partial" };
        let summary = if status == "ok" {
            format!("Draft updated via {}", backend.as_str())
        } else {
            format!("Draft updated via {} with issues", backend.as_str())
        };
        let data = serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": account_id,
            "backend": backend.as_str(),
            "draft_id": created.draft_id,
            "previous_draft_id": old_draft_id,
            "previous_draft_removed": old_removed,
            "message_id": created.message_id,
        });
        Ok((summary, data))
    }

    pub(super) async fn draft_send_impl(
        &self,
        input: DraftSendInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_drafts_enabled(&self.config)?;
        require_smtp_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_draft_id(&input.draft_id)?;
        let backend = self.draft_backend(&input.account_id)?;

        match backend {
            DraftBackend::Imap => {
                self.imap_send_draft(&input.account_id, &input.draft_id)
                    .await
            }
            DraftBackend::Ews => {
                self.ews_send_draft(&input.account_id, &input.draft_id)
                    .await
            }
            DraftBackend::Graph => {
                self.graph_send_draft(&input.account_id, &input.draft_id)
                    .await
            }
        }
    }

    pub(super) async fn draft_delete_impl(
        &self,
        input: DraftDeleteInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_drafts_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_draft_id(&input.draft_id)?;
        if !input.confirm {
            return Err(AppError::invalid("draft_delete requires confirm=true"));
        }
        let backend = self.draft_backend(&input.account_id)?;

        self.ensure_is_draft(backend, &input.account_id, &input.draft_id)
            .await?;
        self.remove_draft(backend, &input.account_id, &input.draft_id)
            .await?;

        let data = serde_json::json!({
            "status": "ok",
            "account_id": input.account_id,
            "backend": backend.as_str(),
            "draft_id": input.draft_id,
        });
        Ok(("Draft deleted".to_owned(), data))
    }

    pub(super) async fn draft_list_impl(
        &self,
        input: DraftListInput,
    ) -> AppResult<(String, serde_json::Value)> {
        require_drafts_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        let limit = input.limit.unwrap_or(DEFAULT_DRAFT_LIST_LIMIT);
        if limit == 0 || limit > MAX_DRAFT_LIST_LIMIT {
            return Err(AppError::invalid(format!(
                "limit must be 1..{MAX_DRAFT_LIST_LIMIT}"
            )));
        }
        let backend = self.draft_backend(&input.account_id)?;

        let (drafts, issues) = match backend {
            DraftBackend::Imap => self.imap_list_drafts(&input.account_id, limit).await?,
            DraftBackend::Ews => (
                self.ews_list_drafts(&input.account_id, limit).await?,
                Vec::new(),
            ),
            DraftBackend::Graph => (
                self.graph_list_drafts(&input.account_id, limit).await?,
                Vec::new(),
            ),
        };

        let status = status_from_issue_and_counts(&issues, !drafts.is_empty());
        let data = serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "backend": backend.as_str(),
            "returned": drafts.len(),
            "drafts": drafts,
        });
        Ok((format!("{} draft(s)", drafts.len()), data))
    }

    // ─── Backend dispatch ───────────────────────────────────────────────────

    fn draft_backend(&self, account_id: &str) -> AppResult<DraftBackend> {
        draft_backend_for(&self.config, account_id).ok_or_else(|| {
            AppError::NotFound(format!(
                "account '{account_id}' has no IMAP, EWS or Graph configuration for drafts"
            ))
        })
    }

    fn resolve_draft_attachments(
        &self,
        inputs: &[AttachmentInput],
    ) -> AppResult<Vec<ResolvedAttachment>> {
        Ok(
            decode_attachments(inputs, self.config.attachment_upload_dir.as_deref())?
                .into_iter()
                .map(|a| ResolvedAttachment {
                    filename: a.filename,
                    content_type: a.content_type,
                    content: a.content,
                })
                .collect(),
        )
    }

    async fn write_draft(
        &self,
        backend: DraftBackend,
        account_id: &str,
        content: &DraftContent,
        attachments: &[ResolvedAttachment],
    ) -> AppResult<CreatedDraft> {
        match backend {
            DraftBackend::Imap => {
                self.imap_create_draft(account_id, content, attachments)
                    .await
            }
            DraftBackend::Ews => {
                self.ews_create_draft(account_id, content, attachments)
                    .await
            }
            DraftBackend::Graph => {
                self.graph_create_draft(account_id, content, attachments)
                    .await
            }
        }
    }

    async fn ensure_is_draft(
        &self,
        backend: DraftBackend,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<()> {
        match backend {
            DraftBackend::Imap => {
                let msg_id = parse_and_validate_message_id(account_id, draft_id)?;
                let account = self.config.get_account(account_id)?;
                let mut session = imap::connect_authenticated(
                    &self.config,
                    account,
                    self.token_manager.as_deref(),
                )
                .await?;
                self.imap_ensure_drafts_mailbox(&mut session, &msg_id)
                    .await?;
                imap_require_exists(&self.config, &mut session, &msg_id).await
            }
            DraftBackend::Ews => self.ews_draft_state(account_id, draft_id).await.map(|_| ()),
            DraftBackend::Graph => self.graph_ensure_draft(account_id, draft_id).await,
        }
    }

    async fn remove_draft(
        &self,
        backend: DraftBackend,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<()> {
        match backend {
            DraftBackend::Imap => {
                let msg_id = parse_and_validate_message_id(account_id, draft_id)?;
                let account = self.config.get_account(account_id)?;
                let mut session = imap::connect_authenticated(
                    &self.config,
                    account,
                    self.token_manager.as_deref(),
                )
                .await?;
                self.imap_ensure_drafts_mailbox(&mut session, &msg_id)
                    .await?;
                imap_expunge_message(&self.config, &mut session, &msg_id).await
            }
            DraftBackend::Ews => {
                let tm = self.ews_drafts_token_manager(account_id)?;
                crate::ews::delete_item(tm, account_id, draft_id).await
            }
            DraftBackend::Graph => {
                let tm = self.graph_drafts_token_manager(account_id)?;
                crate::graph::delete_message(tm, account_id, draft_id).await
            }
        }
    }

    // ─── EWS backend ────────────────────────────────────────────────────────

    fn ews_drafts_token_manager(
        &self,
        account_id: &str,
    ) -> AppResult<&crate::oauth2::TokenManager> {
        self.ews_token_manager
            .as_deref()
            .filter(|tm| tm.has_oauth2(account_id))
            .ok_or_else(|| {
                AppError::InvalidInput(format!(
                    "account '{account_id}' has no EWS OAuth2 token; set MAIL_EWS_{}_REFRESH_TOKEN",
                    account_id.to_ascii_uppercase()
                ))
            })
    }

    /// Current state of an EWS draft; refuses ids that are not drafts.
    async fn ews_draft_state(
        &self,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<crate::ews::EwsDraftState> {
        reject_imap_id_for(DraftBackend::Ews, draft_id)?;
        let tm = self.ews_drafts_token_manager(account_id)?;
        let state = crate::ews::get_draft_state(tm, account_id, draft_id).await?;
        if !state.is_draft {
            return Err(AppError::invalid(
                "draft_id is not a draft (IsDraft=false); draft tools only act on drafts",
            ));
        }
        Ok(state)
    }

    async fn ews_create_draft(
        &self,
        account_id: &str,
        content: &DraftContent,
        attachments: &[ResolvedAttachment],
    ) -> AppResult<CreatedDraft> {
        let tm = self.ews_drafts_token_manager(account_id)?;
        let (body, body_type) = match (&content.body_html, &content.body_text) {
            (Some(html), _) => (html.as_str(), "HTML"),
            (None, Some(text)) => (text.as_str(), "Text"),
            (None, None) => ("", "Text"),
        };
        let ews_attachments: Vec<crate::ews::EwsAttachment> = attachments
            .iter()
            .map(|a| crate::ews::EwsAttachment {
                name: a.filename.clone(),
                content_type: a.content_type.clone(),
                content_base64: a.content_base64(),
            })
            .collect();
        let params = crate::ews::EwsSendParams {
            to: &content.to,
            cc: &content.cc,
            bcc: &content.bcc,
            subject: &content.subject,
            body,
            body_type,
            in_reply_to: content.in_reply_to.as_deref(),
            references: content.references.as_deref(),
            reply_to: content.reply_to.as_deref(),
            attachments: &ews_attachments,
        };
        let item = crate::ews::create_draft(tm, account_id, &params).await?;
        Ok(CreatedDraft {
            draft_id: Some(item.item_id),
            message_id: None,
            issues: Vec::new(),
        })
    }

    async fn ews_send_draft(
        &self,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<(String, serde_json::Value)> {
        let state = self.ews_draft_state(account_id, draft_id).await?;
        let tm = self.ews_drafts_token_manager(account_id)?;
        crate::ews::send_draft(tm, account_id, &state.item).await?;
        let data = serde_json::json!({
            "status": "ok",
            "issues": [],
            "account_id": account_id,
            "backend": DraftBackend::Ews.as_str(),
            "draft_id": draft_id,
            "saved_to_sent": true,
            "draft_removed": true,
        });
        Ok(("Draft sent via ews".to_owned(), data))
    }

    async fn ews_list_drafts(
        &self,
        account_id: &str,
        limit: usize,
    ) -> AppResult<Vec<serde_json::Value>> {
        let tm = self.ews_drafts_token_manager(account_id)?;
        let drafts = crate::ews::list_drafts(tm, account_id, limit).await?;
        Ok(drafts
            .into_iter()
            .map(|d| {
                serde_json::json!({
                    "draft_id": d.item_id,
                    "subject": d.subject,
                    "to": d.to,
                    "cc": d.cc,
                    "date": d.date_created,
                })
            })
            .collect())
    }

    // ─── Graph backend ──────────────────────────────────────────────────────

    /// Graph token for drafts: only the dedicated `MAIL_GRAPH_<ID>_*` config,
    /// never the generic IMAP/SMTP OAuth2 token.
    fn graph_drafts_token_manager(
        &self,
        account_id: &str,
    ) -> AppResult<&crate::oauth2::TokenManager> {
        self.graph_token_manager
            .as_deref()
            .filter(|tm| tm.has_oauth2(account_id))
            .ok_or_else(|| {
                AppError::InvalidInput(format!(
                    "account '{account_id}' has no Graph OAuth2 token; set MAIL_GRAPH_{}_REFRESH_TOKEN",
                    account_id.to_ascii_uppercase()
                ))
            })
    }

    async fn graph_ensure_draft(&self, account_id: &str, draft_id: &str) -> AppResult<()> {
        reject_imap_id_for(DraftBackend::Graph, draft_id)?;
        let tm = self.graph_drafts_token_manager(account_id)?;
        if !crate::graph::is_draft(tm, account_id, draft_id).await? {
            return Err(AppError::invalid(
                "draft_id is not a draft (isDraft=false); draft tools only act on drafts",
            ));
        }
        Ok(())
    }

    async fn graph_create_draft(
        &self,
        account_id: &str,
        content: &DraftContent,
        attachments: &[ResolvedAttachment],
    ) -> AppResult<CreatedDraft> {
        let tm = self.graph_drafts_token_manager(account_id)?;
        let params = crate::graph::GraphEmailParams {
            to: content.to.clone(),
            cc: content.cc.clone(),
            bcc: content.bcc.clone(),
            subject: content.subject.clone(),
            body_text: content.body_text.clone(),
            body_html: content.body_html.clone(),
            reply_to: content.reply_to.clone(),
            in_reply_to: content.in_reply_to.clone(),
            references: content.references.clone(),
            attachments: attachments
                .iter()
                .map(|a| crate::graph::GraphEmailAttachment {
                    filename: a.filename.clone(),
                    content_type: a.content_type.clone(),
                    content_base64: a.content_base64(),
                })
                .collect(),
            save_to_sent: true,
        };
        let id = crate::graph::create_draft(tm, account_id, &params).await?;
        Ok(CreatedDraft {
            draft_id: Some(id),
            message_id: None,
            issues: Vec::new(),
        })
    }

    async fn graph_send_draft(
        &self,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<(String, serde_json::Value)> {
        self.graph_ensure_draft(account_id, draft_id).await?;
        let tm = self.graph_drafts_token_manager(account_id)?;
        crate::graph::send_draft(tm, account_id, draft_id).await?;
        let data = serde_json::json!({
            "status": "ok",
            "issues": [],
            "account_id": account_id,
            "backend": DraftBackend::Graph.as_str(),
            "draft_id": draft_id,
            "saved_to_sent": true,
            "draft_removed": true,
        });
        Ok(("Draft sent via graph".to_owned(), data))
    }

    async fn graph_list_drafts(
        &self,
        account_id: &str,
        limit: usize,
    ) -> AppResult<Vec<serde_json::Value>> {
        let tm = self.graph_drafts_token_manager(account_id)?;
        let drafts = crate::graph::list_drafts(tm, account_id, limit).await?;
        Ok(drafts
            .into_iter()
            .map(|d| {
                serde_json::json!({
                    "draft_id": d.id,
                    "subject": d.subject,
                    "to": d.to,
                    "cc": d.cc,
                    "date": d.created,
                })
            })
            .collect())
    }

    // ─── IMAP backend ───────────────────────────────────────────────────────

    /// From address for drafts of an IMAP account: the SMTP sender when the
    /// account can send, otherwise the IMAP login.
    fn imap_draft_from(&self, account_id: &str) -> AppResult<String> {
        if let Some(smtp_config) = self.config.smtp_accounts.get(account_id) {
            return Ok(smtp_config.effective_from().to_owned());
        }
        Ok(self.config.get_account(account_id)?.user.clone())
    }

    /// Name of the account's Drafts folder, creating `Drafts` when the
    /// account has none. Only for writing a new draft.
    async fn imap_drafts_folder_or_create(
        &self,
        session: &mut imap::ImapSession,
    ) -> AppResult<String> {
        if let Some(folder) = self.imap_find_drafts_folder(session).await? {
            return Ok(folder);
        }
        imap::create_mailbox(&self.config, session, "Drafts").await?;
        Ok("Drafts".to_owned())
    }

    /// Name of the account's existing Drafts folder: `NotFound` when none.
    async fn imap_drafts_folder(&self, session: &mut imap::ImapSession) -> AppResult<String> {
        self.imap_find_drafts_folder(session)
            .await?
            .ok_or_else(|| AppError::NotFound("the account has no Drafts folder yet".to_owned()))
    }

    /// Locate the Drafts folder: the RFC 6154 `\Drafts` attribute first,
    /// then well-known names.
    async fn imap_find_drafts_folder(
        &self,
        session: &mut imap::ImapSession,
    ) -> AppResult<Option<String>> {
        let mailboxes = imap::list_all_mailboxes(&self.config, session).await?;
        let by_attribute = mailboxes.iter().find(|m| {
            m.attributes()
                .iter()
                .any(|a| matches!(a, async_imap::types::NameAttribute::Drafts))
        });
        Ok(by_attribute
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|m| imap::is_drafts_folder_name(m.name()))
            })
            .map(|m| m.name().to_owned()))
    }

    /// Refuse to act on a message outside the account's Drafts folder.
    async fn imap_ensure_drafts_mailbox(
        &self,
        session: &mut imap::ImapSession,
        msg_id: &MessageId,
    ) -> AppResult<()> {
        // Always compare with the resolved folder: a name that merely looks
        // like drafts (e.g. `Legal/Drafts`) may hold ordinary mail.
        let drafts_folder = self.imap_drafts_folder(session).await?;
        if msg_id.mailbox == drafts_folder {
            return Ok(());
        }
        Err(AppError::invalid(format!(
            "draft_id points to mailbox '{}', not the Drafts folder ('{drafts_folder}'); \
             draft tools only act on drafts",
            msg_id.mailbox
        )))
    }

    async fn imap_create_draft(
        &self,
        account_id: &str,
        content: &DraftContent,
        attachments: &[ResolvedAttachment],
    ) -> AppResult<CreatedDraft> {
        let composition = smtp::EmailComposition {
            from: self.imap_draft_from(account_id)?,
            to: content.to.clone(),
            cc: content.cc.clone(),
            bcc: content.bcc.clone(),
            subject: content.subject.clone(),
            body_text: content.body_text.clone(),
            body_html: content.body_html.clone(),
            reply_to: content.reply_to.clone(),
            in_reply_to: content.in_reply_to.clone(),
            references: content.references.clone(),
            attachments: attachments
                .iter()
                .map(|a| smtp::EmailAttachment {
                    filename: a.filename.clone(),
                    content_type: a.content_type.clone(),
                    content: a.content.clone(),
                })
                .collect(),
        };
        let rendered = smtp::render_draft(&composition)?;

        let account = self.config.get_account(account_id)?;
        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;
        let folder = self.imap_drafts_folder_or_create(&mut session).await?;
        imap::append(
            &self.config,
            &mut session,
            &folder,
            Some(IMAP_DRAFT_FLAGS),
            &rendered.rfc822,
        )
        .await?;

        // APPEND does not return the new UID without UIDPLUS; find the
        // draft again by its Message-ID.
        let mut issues = Vec::new();
        let draft_id = match imap_find_by_message_id(
            &self.config,
            &mut session,
            &folder,
            &rendered.message_id,
        )
        .await
        {
            Ok(Some((uidvalidity, uid))) => Some(
                MessageId {
                    account_id: account_id.to_owned(),
                    mailbox: folder,
                    uidvalidity,
                    uid,
                }
                .encode(),
            ),
            Ok(None) => {
                issues.push(ToolIssue::from_error(
                    "locate_draft",
                    &AppError::NotFound(format!(
                        "draft saved but not found by Message-ID {}; list drafts to get its id",
                        rendered.message_id
                    )),
                ));
                None
            }
            Err(error) => {
                issues.push(ToolIssue::from_error("locate_draft", &error));
                None
            }
        };

        Ok(CreatedDraft {
            draft_id,
            message_id: Some(rendered.message_id),
            issues,
        })
    }

    async fn imap_send_draft(
        &self,
        account_id: &str,
        draft_id: &str,
    ) -> AppResult<(String, serde_json::Value)> {
        let msg_id = parse_and_validate_message_id(account_id, draft_id)?;
        let smtp_config = self.config.get_smtp_account(account_id)?;
        let account = self.config.get_account(account_id)?;

        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;
        self.imap_ensure_drafts_mailbox(&mut session, &msg_id)
            .await?;
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;
        let raw = imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await?;
        let prepared = prepare_draft_for_send(&raw, &Utc::now().to_rfc2822())?;

        smtp::send_raw_email(
            smtp_config,
            self.token_manager.as_deref(),
            self.config.smtp_connect_timeout_ms,
            self.config.smtp_send_timeout_ms,
            &prepared.envelope_from,
            &prepared.recipients,
            &prepared.rfc822,
        )
        .await?;

        // The message is out. Anything that fails from here is reported,
        // not raised, so the caller never re-sends.
        let mut issues = Vec::new();
        let saved_to_sent = if self.config.should_save_sent(account_id) {
            match self.save_to_sent_folder(account_id, &prepared.rfc822).await {
                Ok(()) => Some(true),
                Err(error) => {
                    warn!(
                        account_id,
                        "failed to save sent draft to Sent folder: {error}"
                    );
                    issues.push(ToolIssue::from_error("save_to_sent", &error));
                    Some(false)
                }
            }
        } else {
            None
        };
        let draft_removed = match imap_expunge_message(&self.config, &mut session, &msg_id).await {
            Ok(()) => true,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("delete_sent_draft", &error).with_message_id(draft_id),
                );
                false
            }
        };

        let status = if issues.is_empty() { "ok" } else { "partial" };
        let recipients = prepared.recipients.len();
        let data = serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": account_id,
            "backend": DraftBackend::Imap.as_str(),
            "draft_id": draft_id,
            "message_id": prepared.message_id,
            "recipients_count": recipients,
            "saved_to_sent": saved_to_sent,
            "draft_removed": draft_removed,
        });
        Ok((format!("Draft sent to {recipients} recipient(s)"), data))
    }

    async fn imap_list_drafts(
        &self,
        account_id: &str,
        limit: usize,
    ) -> AppResult<(Vec<serde_json::Value>, Vec<ToolIssue>)> {
        let account = self.config.get_account(account_id)?;
        let mut session =
            imap::connect_authenticated(&self.config, account, self.token_manager.as_deref())
                .await?;
        let Some(folder) = self.imap_find_drafts_folder(&mut session).await? else {
            return Ok((Vec::new(), Vec::new()));
        };
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &folder).await?;
        let uids = imap::uid_search(&self.config, &mut session, "ALL").await?;

        let mut drafts = Vec::new();
        let mut issues = Vec::new();
        for uid in uids.into_iter().take(limit) {
            let header_bytes =
                match imap::fetch_headers_and_flags(&self.config, &mut session, uid).await {
                    Ok((bytes, _flags)) => bytes,
                    Err(error) => {
                        issues.push(
                            ToolIssue::from_error("fetch_headers_and_flags", &error).with_uid(uid),
                        );
                        continue;
                    }
                };
            let headers = mime::parse_header_bytes(&header_bytes).unwrap_or_default();
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone())
            };
            drafts.push(serde_json::json!({
                "draft_id": MessageId {
                    account_id: account_id.to_owned(),
                    mailbox: folder.clone(),
                    uidvalidity,
                    uid,
                }
                .encode(),
                "subject": header("Subject"),
                "to": header("To"),
                "cc": header("Cc"),
                "date": header("Date"),
            }));
        }
        Ok((drafts, issues))
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn require_drafts_enabled(config: &ServerConfig) -> AppResult<()> {
    if !config.drafts_enabled {
        return Err(AppError::invalid(
            "draft tools are disabled; set MAIL_DRAFTS_ENABLED=true",
        ));
    }
    Ok(())
}

/// IMAP message ids belong to the IMAP backend; passing one to an EWS or
/// Graph account would only produce a confusing "not found".
fn reject_imap_id_for(backend: DraftBackend, draft_id: &str) -> AppResult<()> {
    if MessageId::parse(draft_id).is_ok() {
        return Err(AppError::invalid(format!(
            "draft_id is an IMAP message id, but this account keeps drafts via {}; \
             use a draft_id from draft_create or draft_list",
            backend.as_str()
        )));
    }
    Ok(())
}

fn validate_draft_id(draft_id: &str) -> AppResult<()> {
    if draft_id.trim().is_empty() || draft_id.chars().count() > MAX_DRAFT_ID_CHARS {
        return Err(AppError::invalid(format!(
            "draft_id must be 1..{MAX_DRAFT_ID_CHARS} characters"
        )));
    }
    if draft_id.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "draft_id must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_draft_content(content: &DraftContent) -> AppResult<()> {
    validate_email_recipients(&content.to, "to")?;
    if !content.cc.is_empty() {
        validate_email_recipients(&content.cc, "cc")?;
    }
    if !content.bcc.is_empty() {
        validate_email_recipients(&content.bcc, "bcc")?;
    }
    if let Some(reply_to) = content.reply_to.as_deref() {
        validate_email_recipients(&[reply_to.to_owned()], "reply_to")?;
    }
    if content.subject.is_empty() || content.subject.len() > 998 {
        return Err(AppError::invalid("subject must be 1..998 characters"));
    }
    if content.body_text.is_none() && content.body_html.is_none() {
        return Err(AppError::invalid(
            "at least one of body_text or body_html is required",
        ));
    }
    if let Some(text) = content.body_text.as_deref() {
        validate_email_no_wrapper_leak("body_text", text)?;
    }
    if let Some(html) = content.body_html.as_deref() {
        validate_email_no_wrapper_leak("body_html", html)?;
    }
    Ok(())
}

/// Find a message in `mailbox` by its Message-ID header.
///
/// Returns `(uidvalidity, uid)` of the newest match.
async fn imap_find_by_message_id(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    mailbox: &str,
    message_id: &str,
) -> AppResult<Option<(u32, u32)>> {
    let uidvalidity = imap::select_mailbox_readonly(config, session, mailbox).await?;
    let query = format!("HEADER Message-ID \"{}\"", escape_imap_quoted(message_id)?);
    let uids = imap::uid_search(config, session, &query).await?;
    Ok(uids.first().map(|uid| (uidvalidity, *uid)))
}

/// `NotFound` unless the message still exists (STORE and EXPUNGE on a
/// missing UID succeed silently, so a stale id would look deleted).
async fn imap_require_exists(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    ensure_uidvalidity_matches_readonly(config, session, msg_id).await?;
    let uids = imap::uid_search(config, session, &format!("UID {}", msg_id.uid)).await?;
    if uids.contains(&msg_id.uid) {
        Ok(())
    } else {
        Err(AppError::NotFound(format!(
            "draft {} not found; it may have been sent, replaced or deleted",
            msg_id.encode()
        )))
    }
}

/// Flag a message `\Deleted` and expunge it.
async fn imap_expunge_message(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    ensure_uidvalidity_matches_readwrite(config, session, msg_id).await?;
    imap::uid_store(config, session, msg_id.uid, "+FLAGS.SILENT (\\Deleted)").await?;
    imap::uid_expunge(config, session, msg_id.uid).await
}

/// Turn a stored draft into the bytes and envelope to send.
///
/// Works on the header block only, so the MIME body is sent byte-for-byte:
/// drops every `Bcc` header (with its folded continuation lines), replaces
/// `Date` with `date`, and collects the envelope from `From`, `To`, `Cc` and
/// `Bcc`.
fn prepare_draft_for_send(raw: &[u8], date: &str) -> AppResult<PreparedDraft> {
    let header_end = find_header_end(raw).ok_or_else(|| {
        AppError::invalid("draft has no header/body separator; it is not a valid RFC822 message")
    })?;
    let (header_block, body) = raw.split_at(header_end);

    let (parsed, _) = mailparse::parse_headers(raw)
        .map_err(|e| AppError::invalid(format!("draft headers cannot be parsed: {e}")))?;
    let mut envelope_from = None;
    let mut recipients: Vec<String> = Vec::new();
    let mut message_id = None;
    for header in &parsed {
        let key = header.get_key().to_ascii_lowercase();
        match key.as_str() {
            "from" if envelope_from.is_none() => {
                envelope_from = header_addresses(header).into_iter().next();
            }
            "to" | "cc" | "bcc" => {
                for addr in header_addresses(header) {
                    if !recipients.iter().any(|r| r.eq_ignore_ascii_case(&addr)) {
                        recipients.push(addr);
                    }
                }
            }
            "message-id" if message_id.is_none() => {
                message_id = Some(header.get_value().trim().to_owned());
            }
            _ => {}
        }
    }
    let envelope_from =
        envelope_from.ok_or_else(|| AppError::invalid("draft has no From address"))?;
    if recipients.is_empty() {
        return Err(AppError::invalid(
            "draft has no recipients (To, Cc or Bcc); update it before sending",
        ));
    }

    let line_ending: &[u8] = if header_block.windows(2).any(|w| w == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    let mut date_written = false;
    for line in header_block.split_inclusive(|b| *b == b'\n') {
        let is_continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if is_continuation {
            if !skipping {
                out.extend_from_slice(line);
            }
            continue;
        }
        let name = line.iter().position(|b| *b == b':').map(|colon| {
            String::from_utf8_lossy(&line[..colon])
                .trim()
                .to_ascii_lowercase()
        });
        match name.as_deref() {
            Some("bcc") => skipping = true,
            Some("date") => {
                skipping = true;
                if !date_written {
                    out.extend_from_slice(format!("Date: {date}").as_bytes());
                    out.extend_from_slice(line_ending);
                    date_written = true;
                }
            }
            _ => {
                skipping = false;
                out.extend_from_slice(line);
            }
        }
    }
    if !date_written {
        let mut with_date = format!("Date: {date}").into_bytes();
        with_date.extend_from_slice(line_ending);
        with_date.extend_from_slice(&out);
        out = with_date;
    }
    out.extend_from_slice(body);

    Ok(PreparedDraft {
        rfc822: out,
        envelope_from,
        recipients,
        message_id,
    })
}

/// Byte offset where the header block ends: just past the line ending of
/// the last header, so the remainder starts with the blank separator line.
fn find_header_end(raw: &[u8]) -> Option<usize> {
    let crlf = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 2);
    let lf = raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 1);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Plain addresses from an address-list header (`To`, `Cc`, ...).
fn header_addresses(header: &mailparse::MailHeader<'_>) -> Vec<String> {
    let Ok(list) = mailparse::addrparse_header(header) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list.iter() {
        match entry {
            mailparse::MailAddr::Single(single) => out.push(single.addr.clone()),
            mailparse::MailAddr::Group(group) => {
                out.extend(group.addrs.iter().map(|single| single.addr.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &str = "Tue, 13 Oct 2026 10:00:00 +0000";

    fn prepared(raw: &str) -> PreparedDraft {
        prepare_draft_for_send(raw.as_bytes(), DATE).expect("draft prepares")
    }

    #[test]
    fn strips_bcc_and_keeps_blind_recipients_in_envelope() {
        let p = prepared(
            "From: Me <me@example.com>\r\nTo: a@example.com\r\nBcc: hidden@example.com\r\nSubject: Hi\r\nMessage-ID: <x@example.com>\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\nBody\r\n",
        );
        let text = String::from_utf8(p.rfc822).unwrap();
        assert!(!text.to_ascii_lowercase().contains("bcc:"), "{text}");
        assert!(!text.contains("hidden@example.com"), "{text}");
        assert!(text.contains(&format!("Date: {DATE}\r\n")), "{text}");
        assert!(!text.contains("Mon, 1 Jan 2024"), "{text}");
        assert!(text.ends_with("\r\n\r\nBody\r\n"), "{text}");
        assert_eq!(p.envelope_from, "me@example.com");
        assert_eq!(p.recipients, vec!["a@example.com", "hidden@example.com"]);
        assert_eq!(p.message_id.as_deref(), Some("<x@example.com>"));
    }

    #[test]
    fn strips_folded_multiline_bcc() {
        let p = prepared(
            "From: me@example.com\r\nBcc: one@example.com,\r\n two@example.com,\r\n\tthree@example.com\r\nTo: a@example.com\r\nSubject: Hi\r\n\r\nBody",
        );
        let text = String::from_utf8(p.rfc822).unwrap();
        for hidden in ["one@", "two@", "three@"] {
            assert!(!text.contains(hidden), "{hidden} leaked: {text}");
        }
        assert!(text.contains("To: a@example.com\r\n"), "{text}");
        assert!(text.contains("Subject: Hi\r\n"), "{text}");
        assert_eq!(p.recipients.len(), 4);
    }

    #[test]
    fn adds_date_when_missing_and_keeps_body_untouched() {
        let body = "\r\n--b\r\nContent-Type: text/plain\r\n\r\nBcc: not a header\r\n--b--\r\n";
        let p = prepared(&format!(
            "From: me@example.com\r\nTo: a@example.com\r\nContent-Type: multipart/mixed; boundary=b\r\n{body}"
        ));
        let text = String::from_utf8(p.rfc822).unwrap();
        assert!(text.starts_with(&format!("Date: {DATE}\r\n")), "{text}");
        assert!(text.ends_with(body), "body must be byte-identical: {text}");
    }

    #[test]
    fn handles_lf_only_drafts() {
        let p = prepared("From: me@example.com\nTo: a@example.com\nBcc: b@example.com\n\nBody\n");
        let text = String::from_utf8(p.rfc822).unwrap();
        assert!(text.starts_with(&format!("Date: {DATE}\n")), "{text}");
        assert!(!text.contains("b@example.com"), "{text}");
        assert_eq!(p.recipients, vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn deduplicates_recipients_and_expands_groups() {
        let p = prepared(
            "From: me@example.com\r\nTo: Team: x@example.com, y@example.com;\r\nCc: X@example.com\r\n\r\n",
        );
        assert_eq!(p.recipients, vec!["x@example.com", "y@example.com"]);
    }

    #[test]
    fn rejects_drafts_without_recipients_or_sender() {
        let err = prepare_draft_for_send(b"From: me@example.com\r\nSubject: x\r\n\r\n", DATE)
            .unwrap_err();
        assert!(err.to_string().contains("no recipients"), "{err}");
        let err = prepare_draft_for_send(b"To: a@example.com\r\n\r\n", DATE).unwrap_err();
        assert!(err.to_string().contains("no From"), "{err}");
        let err = prepare_draft_for_send(b"From: me@example.com", DATE).unwrap_err();
        assert!(err.to_string().contains("separator"), "{err}");
    }

    #[test]
    fn rendered_draft_keeps_bcc_and_prepares_cleanly() {
        let composition = smtp::EmailComposition {
            from: "me@example.com".to_owned(),
            to: vec!["a@example.com".to_owned()],
            cc: vec![],
            bcc: vec!["hidden@example.com".to_owned()],
            subject: "Hi".to_owned(),
            body_text: Some("Body".to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
        };
        let draft = smtp::render_draft(&composition).unwrap();
        let stored = String::from_utf8(draft.rfc822.clone()).unwrap();
        assert!(stored.contains("hidden@example.com"), "{stored}");

        let p = prepare_draft_for_send(&draft.rfc822, DATE).unwrap();
        let sent = String::from_utf8(p.rfc822).unwrap();
        assert!(!sent.contains("hidden@example.com"), "{sent}");
        assert_eq!(p.recipients, vec!["a@example.com", "hidden@example.com"]);
        assert_eq!(p.message_id.as_deref(), Some(draft.message_id.as_str()));
    }

    fn routing_test_config() -> ServerConfig {
        ServerConfig {
            accounts: std::collections::BTreeMap::new(),
            oauth2_accounts: std::collections::HashMap::new(),
            graph_oauth2_accounts: std::collections::HashMap::new(),
            ews_accounts: std::collections::HashMap::new(),
            ews_oauth2_accounts: std::collections::HashMap::new(),
            smtp_accounts: std::collections::HashMap::new(),
            smtp_write_enabled: false,
            smtp_save_sent: None,
            smtp_connect_timeout_ms: 30_000,
            smtp_send_timeout_ms: 300_000,
            write_enabled: false,
            drafts_enabled: true,
            connect_timeout_ms: 5_000,
            greeting_timeout_ms: 5_000,
            socket_timeout_ms: 5_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 16,
            max_mailboxes: crate::config::DEFAULT_MAX_MAILBOXES,
            search_concurrency: crate::config::DEFAULT_SEARCH_CONCURRENCY,
            attachment_download_dir: None,
            attachment_upload_dir: None,
        }
    }

    fn oauth2_config() -> crate::oauth2::OAuth2AccountConfig {
        crate::oauth2::OAuth2AccountConfig {
            provider: crate::oauth2::OAuth2Provider::Microsoft,
            client_id: "client".to_owned(),
            client_secret: secrecy::SecretString::new("none".into()),
            refresh_token: secrecy::SecretString::new("token".into()),
        }
    }

    #[test]
    fn backend_routing_prefers_ews_then_graph_then_imap() {
        let mut config = routing_test_config();
        assert_eq!(draft_backend_for(&config, "acct"), None);

        config.accounts.insert(
            "acct".to_owned(),
            crate::config::AccountConfig {
                account_id: "acct".to_owned(),
                host: "imap.example.com".to_owned(),
                port: 993,
                secure: true,
                user: "u@example.com".to_owned(),
                pass: None,
                auth_method: crate::config::AuthMethod::Password,
            },
        );
        assert_eq!(draft_backend_for(&config, "acct"), Some(DraftBackend::Imap));

        // A generic OAuth2 (XOAUTH2) account must stay on IMAP.
        config
            .oauth2_accounts
            .insert("acct".to_owned(), oauth2_config());
        assert_eq!(draft_backend_for(&config, "acct"), Some(DraftBackend::Imap));

        config
            .graph_oauth2_accounts
            .insert("acct".to_owned(), oauth2_config());
        assert_eq!(
            draft_backend_for(&config, "acct"),
            Some(DraftBackend::Graph)
        );

        config.ews_accounts.insert(
            "acct".to_owned(),
            crate::ews::EwsAccountConfig {
                user: "u@example.com".to_owned(),
            },
        );
        assert_eq!(draft_backend_for(&config, "acct"), Some(DraftBackend::Ews));
    }

    #[test]
    fn validates_draft_ids() {
        assert!(validate_draft_id("imap:default:Drafts:1:2").is_ok());
        assert!(validate_draft_id("AAMkAGI2T=").is_ok());
        assert!(validate_draft_id("").is_err());
        assert!(validate_draft_id("   ").is_err());
        assert!(validate_draft_id("a\r\nb").is_err());
        assert!(validate_draft_id(&"x".repeat(MAX_DRAFT_ID_CHARS + 1)).is_err());
    }

    fn greenmail_draft_config() -> ServerConfig {
        let host = std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "localhost".to_owned());
        let port = |name: &str, default: u16| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(default)
        };
        let user = std::env::var("GREENMAIL_USER").unwrap_or_else(|_| "test@localhost".to_owned());
        let pass = std::env::var("GREENMAIL_PASS").unwrap_or_else(|_| "test".to_owned());

        let mut config = routing_test_config();
        config.socket_timeout_ms = 15_000;
        config.smtp_write_enabled = true;
        config.smtp_save_sent = Some(true);
        config.accounts.insert(
            "default".to_owned(),
            crate::config::AccountConfig {
                account_id: "default".to_owned(),
                host: host.clone(),
                port: port("GREENMAIL_PLAIN_IMAP_PORT", 3144),
                secure: false,
                user: user.clone(),
                pass: Some(secrecy::SecretString::new(pass.clone().into())),
                auth_method: crate::config::AuthMethod::Password,
            },
        );
        config.smtp_accounts.insert(
            "default".to_owned(),
            smtp::SmtpAccountConfig {
                account_id: "default".to_owned(),
                host,
                port: port("GREENMAIL_SMTP_PORT", 3025),
                user,
                pass: Some(secrecy::SecretString::new(pass.into())),
                security: smtp::SmtpSecurity::Plain,
                auth_method: crate::config::AuthMethod::Password,
                save_sent: Some(true),
                from_email: None,
            },
        );
        config
    }

    fn draft_input(subject: &str, body: &str) -> DraftCreateInput {
        DraftCreateInput {
            account_id: "default".to_owned(),
            to: vec!["test@localhost".to_owned()],
            cc: vec![],
            bcc: vec!["hidden@localhost".to_owned()],
            subject: subject.to_owned(),
            body_text: Some(body.to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
            attachments: vec![AttachmentInput {
                filename: Some("note.txt".to_owned()),
                content_type: Some("text/plain".to_owned()),
                content_base64: Some("aGVsbG8=".to_owned()),
                file_path: None,
            }],
        }
    }

    async fn drafts_with_subject(server: &MailImapServer, subject: &str) -> Vec<serde_json::Value> {
        let (_, data) = server
            .draft_list_impl(DraftListInput {
                account_id: "default".to_owned(),
                limit: Some(50),
            })
            .await
            .expect("draft_list succeeds");
        data["drafts"]
            .as_array()
            .expect("drafts array")
            .iter()
            .filter(|d| d["subject"].as_str() == Some(subject))
            .cloned()
            .collect()
    }

    /// Full IMAP+SMTP draft cycle against GreenMail: create, update, guard
    /// against non-drafts, send, and check what the recipient and Sent got.
    ///
    /// Run with `scripts/test-greenmail.sh` (needs GreenMail's plaintext IMAP
    /// port, `GREENMAIL_PLAIN_IMAP_PORT`, default 3144).
    #[tokio::test]
    #[ignore = "requires GreenMail"]
    async fn greenmail_draft_cycle() {
        let config = greenmail_draft_config();
        let server = MailImapServer::new(config.clone(), None);
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let first_subject = format!("draft {tag} v1");
        let final_subject = format!("draft {tag} v2");

        // Create
        let (_, created) = server
            .draft_create_impl(draft_input(&first_subject, "first version"))
            .await
            .expect("draft_create succeeds");
        assert_eq!(created["status"], "ok", "{created}");
        assert_eq!(created["backend"], "imap");
        let first_id = created["draft_id"].as_str().expect("draft_id").to_owned();
        let listed = drafts_with_subject(&server, &first_subject).await;
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0]["draft_id"], first_id.as_str());

        // Update: new id, old one gone
        let (_, updated) = server
            .draft_update_impl(DraftUpdateInput {
                account_id: "default".to_owned(),
                draft_id: first_id.clone(),
                to: vec!["test@localhost".to_owned()],
                cc: vec![],
                bcc: vec!["hidden@localhost".to_owned()],
                subject: final_subject.clone(),
                body_text: Some("final version".to_owned()),
                body_html: None,
                reply_to: None,
                in_reply_to: None,
                references: None,
                attachments: draft_input("", "").attachments,
            })
            .await
            .expect("draft_update succeeds");
        assert_eq!(updated["status"], "ok", "{updated}");
        assert_eq!(updated["previous_draft_removed"], true);
        let final_id = updated["draft_id"]
            .as_str()
            .expect("new draft_id")
            .to_owned();
        assert_ne!(final_id, first_id);
        assert!(
            drafts_with_subject(&server, &first_subject)
                .await
                .is_empty()
        );
        assert_eq!(drafts_with_subject(&server, &final_subject).await.len(), 1);

        // The old id no longer resolves, for delete or send
        let stale_delete = server
            .draft_delete_impl(DraftDeleteInput {
                account_id: "default".to_owned(),
                draft_id: first_id.clone(),
                confirm: true,
            })
            .await
            .expect_err("deleting a replaced draft must fail");
        assert!(
            matches!(stale_delete, AppError::NotFound(_)),
            "{stale_delete}"
        );
        let gone = server
            .draft_send_impl(DraftSendInput {
                account_id: "default".to_owned(),
                draft_id: first_id.clone(),
            })
            .await;
        assert!(gone.is_err(), "sending a replaced draft must fail");

        // Guard: draft tools refuse messages outside Drafts
        let err = server
            .draft_delete_impl(DraftDeleteInput {
                account_id: "default".to_owned(),
                draft_id: "imap:default:INBOX:1:1".to_owned(),
                confirm: true,
            })
            .await
            .expect_err("deleting an INBOX message must be refused");
        assert!(err.to_string().contains("not the Drafts folder"), "{err}");
        let err = server
            .draft_send_impl(DraftSendInput {
                account_id: "default".to_owned(),
                draft_id: "imap:default:INBOX:1:1".to_owned(),
            })
            .await
            .expect_err("sending an INBOX message must be refused");
        assert!(err.to_string().contains("not the Drafts folder"), "{err}");

        // Send (GreenMail starts without a Sent folder)
        {
            let account = config.get_account("default").unwrap();
            let mut session = imap::connect_authenticated(&config, account, None)
                .await
                .expect("connect");
            let _ = imap::create_mailbox(&config, &mut session, "Sent").await;
        }
        let (_, sent) = server
            .draft_send_impl(DraftSendInput {
                account_id: "default".to_owned(),
                draft_id: final_id.clone(),
            })
            .await
            .expect("draft_send succeeds");
        assert_eq!(sent["status"], "ok", "{sent}");
        assert_eq!(sent["recipients_count"], 2);
        assert_eq!(sent["saved_to_sent"], true);
        assert_eq!(sent["draft_removed"], true);
        let message_id = sent["message_id"].as_str().expect("message_id").to_owned();
        assert!(
            drafts_with_subject(&server, &final_subject)
                .await
                .is_empty()
        );

        // What the recipient got: no Bcc, same Message-ID, attachment intact
        let account = config.get_account("default").unwrap();
        let mut session = imap::connect_authenticated(&config, account, None)
            .await
            .expect("connect");
        let mut delivered = None;
        for _ in 0..20 {
            if let Some(found) =
                imap_find_by_message_id(&config, &mut session, "INBOX", &message_id)
                    .await
                    .expect("search INBOX")
            {
                delivered = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let (_, uid) = delivered.expect("message delivered to INBOX");
        let raw = imap::fetch_raw_message(&config, &mut session, uid)
            .await
            .expect("fetch delivered message");
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("hidden@localhost"), "Bcc leaked: {text}");
        assert!(text.contains(&final_subject), "{text}");
        assert!(text.contains("final version"), "{text}");
        assert!(text.contains("note.txt"), "{text}");

        // Archived copy in Sent
        let sent_folder = imap::list_all_mailboxes(&config, &mut session)
            .await
            .expect("list")
            .iter()
            .map(|m| m.name().to_owned())
            .find(|name| imap::is_sent_folder_name(name))
            .expect("Sent folder exists after send");
        let archived = imap_find_by_message_id(&config, &mut session, &sent_folder, &message_id)
            .await
            .expect("search Sent");
        assert!(archived.is_some(), "sent draft archived in {sent_folder}");
    }

    /// Draft cycle against a real account configured in the environment
    /// (same variables as the server). Creates, updates and lists a draft,
    /// then deletes it, or sends it when `LIVE_DRAFTS_SEND=1`.
    ///
    /// ```text
    /// LIVE_DRAFTS_ACCOUNT=work LIVE_DRAFTS_TO=me@company.com \
    ///   cargo test live_draft_cycle -- --ignored --nocapture
    /// ```
    ///
    /// Only send to an address you own: the message really goes out.
    #[tokio::test]
    #[ignore = "uses a real mail account"]
    async fn live_draft_cycle() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let account_id = std::env::var("LIVE_DRAFTS_ACCOUNT").expect("LIVE_DRAFTS_ACCOUNT");
        let to = std::env::var("LIVE_DRAFTS_TO").expect("LIVE_DRAFTS_TO");
        let send = std::env::var("LIVE_DRAFTS_SEND").is_ok_and(|v| v == "1");
        let file = crate::config_file::ConfigFile::load(None, false).expect("config file");
        let resolved = file.effective(true).expect("resolved config");
        let mut config = ServerConfig::load(&resolved.values).expect("config from env");
        config.drafts_enabled = true;
        let server = MailImapServer::new(config, None);
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let subject = format!("mail-mcp draft test {tag}");

        let mut input = draft_input(&format!("{subject} v1"), "first version");
        input.account_id = account_id.clone();
        input.to = vec![to.clone()];
        input.bcc = vec![];
        let (_, created) = server.draft_create_impl(input).await.expect("create");
        println!("created: {created}");
        assert_eq!(created["status"], "ok", "{created}");
        let first_id = created["draft_id"].as_str().expect("draft_id").to_owned();

        let (_, updated) = server
            .draft_update_impl(DraftUpdateInput {
                account_id: account_id.clone(),
                draft_id: first_id.clone(),
                to: vec![to.clone()],
                cc: vec![],
                bcc: vec![],
                subject: subject.clone(),
                body_text: Some("final version".to_owned()),
                body_html: Some("<p>final <b>version</b></p>".to_owned()),
                reply_to: None,
                in_reply_to: None,
                references: None,
                attachments: draft_input("", "").attachments,
            })
            .await
            .expect("update");
        println!("updated: {updated}");
        assert_eq!(updated["status"], "ok", "{updated}");
        let final_id = updated["draft_id"].as_str().expect("draft_id").to_owned();

        let (_, listed) = server
            .draft_list_impl(DraftListInput {
                account_id: account_id.clone(),
                limit: Some(50),
            })
            .await
            .expect("list");
        let drafts = listed["drafts"].as_array().expect("drafts");
        assert!(
            drafts.iter().any(|d| d["draft_id"] == final_id.as_str()),
            "updated draft listed: {listed}"
        );
        assert!(
            !drafts.iter().any(|d| d["draft_id"] == first_id.as_str()),
            "replaced draft gone: {listed}"
        );

        let stale = server
            .draft_delete_impl(DraftDeleteInput {
                account_id: account_id.clone(),
                draft_id: first_id.clone(),
                confirm: true,
            })
            .await;
        println!("delete replaced draft: {stale:?}");
        assert!(stale.is_err(), "replaced draft id must not resolve");

        if send {
            let (_, sent) = server
                .draft_send_impl(DraftSendInput {
                    account_id: account_id.clone(),
                    draft_id: final_id.clone(),
                })
                .await
                .expect("send");
            println!("sent: {sent}");
            assert!(
                sent["status"] == "ok" || sent["status"] == "partial",
                "{sent}"
            );
        } else {
            let (_, deleted) = server
                .draft_delete_impl(DraftDeleteInput {
                    account_id: account_id.clone(),
                    draft_id: final_id.clone(),
                    confirm: true,
                })
                .await
                .expect("delete");
            println!("deleted: {deleted}");
        }
    }
}
