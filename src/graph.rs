//! Microsoft Graph API client for sending emails
//!
//! Uses the Microsoft Graph REST API (`POST /me/sendMail`) to send emails
//! from Microsoft accounts (personal and enterprise). This bypasses SMTP
//! entirely, which is necessary for personal hotmail/outlook.com accounts
//! where Microsoft has disabled SMTP AUTH.
//!
//! When `in_reply_to` is provided, uses the Graph reply flow
//! (`createReply` → PATCH → send) for proper threading.
//!
//! # Requirements
//!
//! - OAuth2 configured with `provider=microsoft`
//! - Token scope must include `https://graph.microsoft.com/Mail.Send`
//!
//! # Configuration
//!
//! Uses the same `MAIL_OAUTH2_<SEGMENT>_*` variables as IMAP/SMTP OAuth2.
//! No additional configuration needed beyond OAuth2.

use base64::Engine;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::errors::{AppError, AppResult};
use crate::oauth2::TokenManager;

/// Microsoft Graph API base URL
const GRAPH_API_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Threshold (in raw decoded bytes) above which an attachment must be
/// uploaded via `createUploadSession` instead of inlined in a single
/// POST to `/me/messages/{id}/attachments`. Per Microsoft Graph docs the
/// inline limit is 3 MB; we use a slightly conservative value to stay
/// well clear of base64-overhead edge cases on the wire.
const ATTACHMENT_INLINE_MAX_BYTES: usize = 3 * 1024 * 1024;

/// Chunk size for createUploadSession PUTs. Graph accepts up to ~4 MB
/// per chunk; we use exactly 4 MB which divides evenly and stays under
/// the documented ceiling.
const UPLOAD_CHUNK_BYTES: usize = 4 * 1024 * 1024;

// ─── Request types (sendMail) ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendMailRequest {
    message: GraphMessage,
    save_to_sent_items: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    subject: String,
    body: GraphBody,
    to_recipients: Vec<GraphRecipient>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cc_recipients: Vec<GraphRecipient>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bcc_recipients: Vec<GraphRecipient>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<Vec<GraphRecipient>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    internet_message_headers: Option<Vec<GraphHeader>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<GraphAttachment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphBody {
    content_type: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecipient {
    email_address: GraphEmailAddress,
}

#[derive(Debug, Serialize)]
struct GraphEmailAddress {
    address: String,
}

#[derive(Debug, Serialize)]
struct GraphHeader {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttachment {
    #[serde(rename = "@odata.type")]
    odata_type: &'static str,
    name: String,
    content_type: String,
    content_bytes: String,
}

// ─── Request types (reply / patch draft) ────────────────────────────────────

// NOTE: `attachments` is INTENTIONALLY ABSENT from this struct. Microsoft
// Graph treats `Message.attachments` as a navigation property — PATCH
// requests against `/me/messages/{id}` silently DISCARD the field
// (returns 2xx but the attachments never land on the draft). For the
// createReply → PATCH → send flow we must instead POST each attachment to
// `/me/messages/{id}/attachments` (or use createUploadSession for ≥3 MB)
// between PATCH and send. See `upload_attachment_to_draft` below.
//
// This was the v0.4.6 silent-data-loss bug — see BUG_GRAPH_ATTACHMENTS.md
// and the v0.4.7 release notes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PatchDraftRequest {
    subject: String,
    body: GraphBody,
    to_recipients: Vec<GraphRecipient>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cc_recipients: Vec<GraphRecipient>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bcc_recipients: Vec<GraphRecipient>,
}

// ─── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MessageListResponse {
    value: Vec<MessageItem>,
}

#[derive(Debug, Deserialize)]
struct MessageItem {
    id: String,
}

#[derive(Debug, Deserialize)]
struct DraftResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSessionResponse {
    upload_url: String,
}

// ─── Helper constructors ─────────────────────────────────────────────────────

fn recipient(addr: &str) -> GraphRecipient {
    GraphRecipient {
        email_address: GraphEmailAddress {
            address: addr.to_owned(),
        },
    }
}

fn recipients(addrs: &[String]) -> Vec<GraphRecipient> {
    addrs.iter().map(|a| recipient(a)).collect()
}

fn build_attachments(attachments: &[GraphEmailAttachment]) -> Vec<GraphAttachment> {
    attachments
        .iter()
        .map(|a| GraphAttachment {
            odata_type: "#microsoft.graph.fileAttachment",
            name: a.filename.clone(),
            content_type: a.content_type.clone(),
            content_bytes: a.content_base64.clone(),
        })
        .collect()
}

fn resolve_body(body_html: &Option<String>, body_text: &Option<String>) -> (&'static str, String) {
    match (body_html, body_text) {
        (Some(html), _) => ("HTML", sanitize_cdata(html)),
        (None, Some(text)) => ("Text", sanitize_cdata(text)),
        (None, None) => ("Text", String::new()),
    }
}

/// Upload one attachment to an existing draft message.
///
/// Used by the createReply → PATCH → send flow because Graph silently
/// discards `attachments` set via PATCH on `/me/messages/{id}`. After the
/// PATCH succeeds (body + recipients), every attachment must be POSTed
/// individually to `/me/messages/{id}/attachments`.
///
/// Routing:
/// - Raw size < 3 MB: single inline POST with `contentBytes` (base64).
/// - Raw size ≥ 3 MB: `createUploadSession` + 4 MB chunked PUTs to the
///   returned (pre-authenticated) upload URL.
async fn upload_attachment_to_draft(
    client: &reqwest::Client,
    access_token: &str,
    draft_id: &str,
    att: &GraphEmailAttachment,
) -> AppResult<()> {
    // Decode once to know the real size and to feed the chunked upload.
    let raw_bytes = base64::engine::general_purpose::STANDARD
        .decode(att.content_base64.as_bytes())
        .map_err(|e| {
            AppError::invalid(format!(
                "attachment '{}' has invalid base64 content: {e}",
                att.filename
            ))
        })?;

    if raw_bytes.len() < ATTACHMENT_INLINE_MAX_BYTES {
        upload_attachment_inline(client, access_token, draft_id, att).await
    } else {
        upload_attachment_via_session(client, access_token, draft_id, att, &raw_bytes).await
    }
}

/// POST `/me/messages/{id}/attachments` with the file inline in JSON.
/// Only safe for attachments whose raw (decoded) size is < 3 MB.
async fn upload_attachment_inline(
    client: &reqwest::Client,
    access_token: &str,
    draft_id: &str,
    att: &GraphEmailAttachment,
) -> AppResult<()> {
    let url = format!("{}/me/messages/{}/attachments", GRAPH_API_BASE, draft_id);
    let body = serde_json::json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": att.filename,
        "contentType": att.content_type,
        "contentBytes": att.content_base64,
    });
    let response = client
        .post(&url)
        .bearer_auth(access_token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "Graph attachment POST failed for '{}': {e}",
                att.filename
            ))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let resp_body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Graph attachment POST for '{}' failed ({status}): {resp_body}",
            att.filename
        )));
    }
    Ok(())
}

/// Create an upload session and PUT the file in 4 MB chunks.
/// Used for attachments ≥ 3 MB raw size.
///
/// The `uploadUrl` returned by `createUploadSession` is pre-authenticated
/// for the duration of the session — chunks must be PUT WITHOUT a Bearer
/// token (Graph rejects it with 401 if included).
async fn upload_attachment_via_session(
    client: &reqwest::Client,
    access_token: &str,
    draft_id: &str,
    att: &GraphEmailAttachment,
    raw_bytes: &[u8],
) -> AppResult<()> {
    // Step A: createUploadSession.
    let session_url = format!(
        "{}/me/messages/{}/attachments/createUploadSession",
        GRAPH_API_BASE, draft_id
    );
    let session_request = serde_json::json!({
        "AttachmentItem": {
            "attachmentType": "file",
            "name": att.filename,
            "size": raw_bytes.len(),
            "contentType": att.content_type,
        }
    });

    let response = client
        .post(&session_url)
        .bearer_auth(access_token)
        .json(&session_request)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "createUploadSession failed for '{}': {e}",
                att.filename
            ))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let resp_body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "createUploadSession for '{}' failed ({status}): {resp_body}",
            att.filename
        )));
    }

    let session: UploadSessionResponse = response.json().await.map_err(|e| {
        AppError::Internal(format!(
            "createUploadSession response parse failed for '{}': {e}",
            att.filename
        ))
    })?;

    // Step B: PUT chunks (no Bearer token — uploadUrl is pre-authenticated).
    let total = raw_bytes.len();
    let mut offset: usize = 0;
    while offset < total {
        let end = (offset + UPLOAD_CHUNK_BYTES).min(total);
        let chunk = raw_bytes[offset..end].to_vec();
        let range_header = format!("bytes {}-{}/{}", offset, end - 1, total);

        let put_response = client
            .put(&session.upload_url)
            .header("Content-Length", chunk.len().to_string())
            .header("Content-Range", range_header)
            .body(chunk)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "upload chunk PUT failed for '{}' at offset {offset}: {e}",
                    att.filename
                ))
            })?;

        if !put_response.status().is_success() {
            let status = put_response.status();
            let resp_body = put_response.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "upload chunk for '{}' at offset {offset} failed ({status}): {resp_body}",
                att.filename
            )));
        }
        offset = end;
    }
    Ok(())
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// An attachment for Graph API
pub struct GraphEmailAttachment {
    pub filename: String,
    pub content_type: String,
    pub content_base64: String,
}

/// Parameters for sending an email via Microsoft Graph
pub struct GraphEmailParams {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub reply_to: Option<String>,
    /// Original Message-ID we're replying to. When set, triggers the
    /// createReply → PATCH → send flow so Exchange generates proper
    /// threading headers server-side.
    pub in_reply_to: Option<String>,
    /// Accepted for API symmetry with SMTP/EWS but NOT sent to Graph.
    /// Microsoft Graph's `sendMail` strips non-`x-*` internetMessageHeaders,
    /// so a caller-supplied References header would be silently dropped —
    /// threading is instead driven server-side via the `in_reply_to` reply
    /// flow. Present so callers don't have to special-case Graph.
    #[allow(dead_code)]
    pub references: Option<String>,
    pub attachments: Vec<GraphEmailAttachment>,
    pub save_to_sent: bool,
}

/// Send an email using the Microsoft Graph API.
///
/// When `in_reply_to` is provided, attempts the reply flow:
///   1. Search for the original message by `internetMessageId`
///   2. `POST /me/messages/{id}/createReply` to get a threaded draft
///   3. `PATCH /me/messages/{draftId}` to set body, recipients, attachments
///   4. `POST /me/messages/{draftId}/send` to send the draft
///
/// If the original message is not found (e.g. it was sent from another
/// account), falls back to regular `sendMail` without threading.
///
/// # Errors
///
/// - `AuthFailed` if the token is invalid or lacks permissions
/// - `InvalidInput` if email addresses are malformed (caught by Graph API)
/// - `Internal` for network or API errors
pub async fn send_email(
    token_manager: &TokenManager,
    account_id: &str,
    params: &GraphEmailParams,
) -> AppResult<()> {
    let access_token = token_manager.get_access_token(account_id).await?;
    let client = reqwest::Client::new();

    // If in_reply_to is provided, try the reply flow for proper threading
    if let Some(ref irt) = params.in_reply_to {
        if let Some(graph_msg_id) = find_message_by_internet_id(&client, &access_token, irt).await?
        {
            return send_via_reply(&client, &access_token, &graph_msg_id, params).await;
        }
        // Message not found in this mailbox — threading will be lost. This
        // is expected when replying to a conversation whose original message
        // was deleted or lives in a different mailbox; log at DEBUG so it's
        // visible in troubleshooting without spamming normal operation.
        debug!(
            in_reply_to = %irt,
            "Graph: original message not found; sending without thread reply"
        );
    }

    send_via_sendmail(&client, &access_token, params).await
}

// ─── Private: sendMail flow ─────────────────────────────────────────────────

async fn send_via_sendmail(
    client: &reqwest::Client,
    access_token: &str,
    params: &GraphEmailParams,
) -> AppResult<()> {
    let (content_type, content) = resolve_body(&params.body_html, &params.body_text);

    // Note: Graph API internetMessageHeaders only supports x-* custom headers.
    // Standard headers like In-Reply-To and References are NOT allowed here.
    // Threading is handled via the reply flow instead.

    let message = GraphMessage {
        subject: params.subject.clone(),
        body: GraphBody {
            content_type,
            content,
        },
        to_recipients: recipients(&params.to),
        cc_recipients: recipients(&params.cc),
        bcc_recipients: recipients(&params.bcc),
        reply_to: params.reply_to.as_ref().map(|addr| vec![recipient(addr)]),
        internet_message_headers: None,
        attachments: build_attachments(&params.attachments),
    };

    let request_body = SendMailRequest {
        message,
        save_to_sent_items: params.save_to_sent,
    };

    let response = client
        .post(format!("{GRAPH_API_BASE}/me/sendMail"))
        .bearer_auth(access_token)
        .json(&request_body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Graph API request failed: {e}")))?;

    handle_response(response).await
}

// ─── Private: reply flow (createReply → patch → send) ───────────────────────

/// Search for a message by its RFC `Message-ID` header.
/// Returns the Graph API internal ID if found.
async fn find_message_by_internet_id(
    client: &reqwest::Client,
    access_token: &str,
    internet_message_id: &str,
) -> AppResult<Option<String>> {
    // Strip angle brackets for the filter if present
    let clean_id = internet_message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');

    let url = format!(
        "{}/me/messages?$filter=internetMessageId eq '<{}>'&$select=id&$top=1",
        GRAPH_API_BASE, clean_id
    );

    let response = client
        .get(&url)
        .bearer_auth(access_token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Graph search request failed: {e}")))?;

    if !response.status().is_success() {
        // Search failed (permissions, rate limit, 5xx…) — we don't propagate
        // the error so the caller can still send (without threading), but we
        // WARN so operators see that threading degraded due to a real error.
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_owned());
        warn!(
            status = %status,
            body = %body,
            internet_message_id = %clean_id,
            "Graph: message lookup for threading failed; falling back to sendMail without thread"
        );
        return Ok(None);
    }

    let list: MessageListResponse = response
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse Graph search response: {e}")))?;

    Ok(list.value.into_iter().next().map(|m| m.id))
}

/// Send a properly threaded reply using Graph API.
///   1. createReply → gets a draft with correct threading headers
///   2. PATCH the draft with our body, recipients, attachments
///   3. Send the draft
async fn send_via_reply(
    client: &reqwest::Client,
    access_token: &str,
    original_msg_id: &str,
    params: &GraphEmailParams,
) -> AppResult<()> {
    // Step 1: Create reply draft
    let create_reply_url = format!(
        "{}/me/messages/{}/createReply",
        GRAPH_API_BASE, original_msg_id
    );

    let response = client
        .post(&create_reply_url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .body("{}")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Graph createReply failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Graph createReply failed ({status}): {body}"
        )));
    }

    let draft: DraftResponse = response
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse createReply response: {e}")))?;

    let draft_id = draft.id;

    // Step 2: PATCH the draft with our content (body + recipients only).
    // Attachments are deliberately NOT set here — see PatchDraftRequest's
    // doc comment for the rationale.
    let (content_type, content) = resolve_body(&params.body_html, &params.body_text);

    let patch_body = PatchDraftRequest {
        subject: params.subject.clone(),
        body: GraphBody {
            content_type,
            content,
        },
        to_recipients: recipients(&params.to),
        cc_recipients: recipients(&params.cc),
        bcc_recipients: recipients(&params.bcc),
    };

    let patch_url = format!("{}/me/messages/{}", GRAPH_API_BASE, draft_id);

    let response = client
        .patch(&patch_url)
        .bearer_auth(access_token)
        .json(&patch_body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Graph PATCH draft failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Graph PATCH draft failed ({status}): {body}"
        )));
    }

    // Step 2.5: Upload each attachment via the dedicated endpoint.
    // This is the load-bearing step for the v0.4.7 fix — without it the
    // PATCH-only flow loses attachments silently because Graph treats
    // `Message.attachments` as a navigation property and ignores it on PATCH.
    for att in &params.attachments {
        upload_attachment_to_draft(client, access_token, &draft_id, att).await?;
    }

    // Step 3: Send the draft
    let send_url = format!("{}/me/messages/{}/send", GRAPH_API_BASE, draft_id);

    let response = client
        .post(&send_url)
        .bearer_auth(access_token)
        .header("Content-Length", "0")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Graph send draft failed: {e}")))?;

    handle_response(response).await
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

async fn handle_response(response: reqwest::Response) -> AppResult<()> {
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.as_u16() == 401 || status.as_u16() == 403 {
            Err(AppError::AuthFailed(format!(
                "Graph API authentication failed ({status}): {body}"
            )))
        } else {
            Err(AppError::Internal(format!(
                "Graph API sendMail failed ({status}): {body}"
            )))
        }
    }
}

/// Remove CDATA artifacts that some email clients leak into text.
fn sanitize_cdata(text: &str) -> String {
    text.replace("]]>", "").replace("<![CDATA[", "")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_builds_correct_structure() {
        let r = recipient("test@example.com");
        assert_eq!(r.email_address.address, "test@example.com");
    }

    #[test]
    fn recipients_builds_list() {
        let addrs = vec!["a@b.com".to_owned(), "c@d.com".to_owned()];
        let rs = recipients(&addrs);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].email_address.address, "a@b.com");
    }

    /// Regression test for the v0.4.7 fix. `PatchDraftRequest` MUST NOT
    /// serialize an `attachments` field — Graph silently drops it when
    /// set via PATCH on `/me/messages/{id}`, which was the v0.4.6 silent
    /// data-loss bug. Attachments must travel through the dedicated
    /// `/me/messages/{id}/attachments` endpoint instead.
    #[test]
    fn patch_draft_request_never_serializes_attachments() {
        let req = PatchDraftRequest {
            subject: "Re: test".to_owned(),
            body: GraphBody {
                content_type: "HTML",
                content: "<p>x</p>".to_owned(),
            },
            to_recipients: vec![recipient("to@test.com")],
            cc_recipients: vec![],
            bcc_recipients: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(
            json.get("attachments").is_none(),
            "PatchDraftRequest must not include attachments (Graph PATCH discards them); got: {json}"
        );
        // Sanity-check the fields that DO travel through PATCH.
        assert_eq!(json["subject"], "Re: test");
        assert_eq!(
            json["toRecipients"][0]["emailAddress"]["address"],
            "to@test.com"
        );
    }

    /// The inline-vs-session threshold must match the Microsoft Graph
    /// documented limit (3 MB raw). Hard-codes the constant so a future
    /// edit doesn't silently shift the boundary into an invalid range.
    #[test]
    fn attachment_inline_threshold_matches_graph_spec() {
        assert_eq!(ATTACHMENT_INLINE_MAX_BYTES, 3 * 1024 * 1024);
        assert!(UPLOAD_CHUNK_BYTES <= 4 * 1024 * 1024);
        assert!(UPLOAD_CHUNK_BYTES > 0);
    }

    #[test]
    fn send_mail_request_serializes_correctly() {
        let req = SendMailRequest {
            message: GraphMessage {
                subject: "Test".to_owned(),
                body: GraphBody {
                    content_type: "Text",
                    content: "Hello".to_owned(),
                },
                to_recipients: vec![recipient("to@test.com")],
                cc_recipients: vec![],
                bcc_recipients: vec![],
                reply_to: None,
                internet_message_headers: None,
                attachments: vec![],
            },
            save_to_sent_items: true,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["message"]["subject"], "Test");
        assert_eq!(json["message"]["body"]["contentType"], "Text");
        assert_eq!(
            json["message"]["toRecipients"][0]["emailAddress"]["address"],
            "to@test.com"
        );
        assert_eq!(json["saveToSentItems"], true);
        // cc and bcc should be absent (skip_serializing_if)
        assert!(json["message"].get("ccRecipients").is_none());
    }
}
