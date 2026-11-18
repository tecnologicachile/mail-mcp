//! Exchange Web Services (EWS) client for Microsoft Exchange/Office 365
//!
//! Uses SOAP/XML over HTTPS with OAuth2 Bearer tokens. Works with both
//! personal and enterprise Microsoft accounts, including tenants that
//! have blocked Graph API and IMAP.
//!
//! # Configuration
//!
//! ```text
//! MAIL_EWS_<SEGMENT>_USER=user@company.com
//! MAIL_EWS_<SEGMENT>_CLIENT_ID=d3590ed6-52b3-4102-aeff-aad2292ab01c
//! MAIL_EWS_<SEGMENT>_CLIENT_SECRET=none
//! MAIL_EWS_<SEGMENT>_REFRESH_TOKEN=<token>
//! ```

use std::time::Duration;

use crate::errors::{AppError, AppResult};
use crate::oauth2::TokenManager;

/// EWS endpoint
const EWS_URL: &str = "https://outlook.office365.com/EWS/Exchange.asmx";

/// EWS XML namespaces
const SOAP_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const TYPES_NS: &str = "http://schemas.microsoft.com/exchange/services/2006/types";
const MESSAGES_NS: &str = "http://schemas.microsoft.com/exchange/services/2006/messages";

// ─── EWS account config ─────────────────────────────────────────────────────

/// EWS account configuration.
///
/// The account identifier lives in the outer `HashMap<String,
/// EwsAccountConfig>` key (see `config::ServerConfig::ews_accounts`), so
/// it is not duplicated inside the struct.
#[derive(Debug, Clone)]
pub struct EwsAccountConfig {
    pub user: String,
}

// ─── Response types ──────────────────────────────────────────────────────────

/// A message from EWS FindItem
#[derive(Debug, Clone, serde::Serialize)]
pub struct EwsMessage {
    pub item_id: String,
    pub change_key: String,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub date_received: String,
    pub is_read: bool,
}

/// A message body from EWS GetItem
#[derive(Debug, Clone, serde::Serialize)]
pub struct EwsMessageDetail {
    pub item_id: String,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub to: String,
    pub cc: String,
    pub date_received: String,
    pub body_text: String,
    pub is_read: bool,
    pub has_attachments: bool,
}

// ─── Client ──────────────────────────────────────────────────────────────────

/// Send a SOAP request to EWS with OAuth2 Bearer token.
async fn ews_request(
    token_manager: &TokenManager,
    account_id: &str,
    soap_body: &str,
) -> AppResult<String> {
    let access_token = token_manager.get_access_token(account_id).await?;

    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="{SOAP_NS}"
               xmlns:t="{TYPES_NS}"
               xmlns:m="{MESSAGES_NS}">
  <soap:Body>
    {soap_body}
  </soap:Body>
</soap:Envelope>"#
    );

    let client = reqwest::Client::new();
    let response = client
        .post(EWS_URL)
        .header("Content-Type", "text/xml")
        .bearer_auth(&access_token)
        .body(envelope)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("EWS request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(AppError::AuthFailed(format!(
                "EWS authentication failed ({status})"
            )));
        }
        return Err(AppError::Internal(format!(
            "EWS request failed ({status}): {body}"
        )));
    }

    response
        .text()
        .await
        .map_err(|e| AppError::Internal(format!("EWS response read failed: {e}")))
}

// ─── Operations ──────────────────────────────────────────────────────────────

/// List messages in a folder (default: inbox)
pub async fn find_items(
    token_manager: &TokenManager,
    account_id: &str,
    folder: &str,
    max_items: usize,
    offset: usize,
) -> AppResult<Vec<EwsMessage>> {
    let folder_id = match folder.to_ascii_lowercase().as_str() {
        "inbox" => "inbox",
        "sent" | "sentitems" | "sent items" => "sentitems",
        "drafts" => "drafts",
        "deleted" | "deleteditems" => "deleteditems",
        "junk" | "junkemail" => "junkemail",
        _ => folder,
    };

    let is_distinguished = matches!(
        folder_id,
        "inbox" | "sentitems" | "drafts" | "deleteditems" | "junkemail"
    );
    let folder_xml = if is_distinguished {
        format!(r#"<t:DistinguishedFolderId Id="{folder_id}"/>"#)
    } else {
        format!(r#"<t:FolderId Id="{folder_id}"/>"#)
    };

    let soap = format!(
        r#"<m:FindItem Traversal="Shallow">
      <m:ItemShape>
        <t:BaseShape>IdOnly</t:BaseShape>
        <t:AdditionalProperties>
          <t:FieldURI FieldURI="item:Subject"/>
          <t:FieldURI FieldURI="item:DateTimeReceived"/>
          <t:FieldURI FieldURI="message:From"/>
          <t:FieldURI FieldURI="message:IsRead"/>
        </t:AdditionalProperties>
      </m:ItemShape>
      <m:IndexedPageItemView MaxEntriesReturned="{max_items}" Offset="{offset}" BasePoint="Beginning"/>
      <m:SortOrder>
        <t:FieldOrder Order="Descending">
          <t:FieldURI FieldURI="item:DateTimeReceived"/>
        </t:FieldOrder>
      </m:SortOrder>
      <m:ParentFolderIds>
        {folder_xml}
      </m:ParentFolderIds>
    </m:FindItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    parse_find_items_response(&xml)
}

/// Get full message details
pub async fn get_item(
    token_manager: &TokenManager,
    account_id: &str,
    item_id: &str,
) -> AppResult<EwsMessageDetail> {
    let soap = format!(
        r#"<m:GetItem>
      <m:ItemShape>
        <t:BaseShape>Default</t:BaseShape>
        <t:AdditionalProperties>
          <t:FieldURI FieldURI="item:Body"/>
          <t:FieldURI FieldURI="item:HasAttachments"/>
          <t:FieldURI FieldURI="message:ToRecipients"/>
          <t:FieldURI FieldURI="message:CcRecipients"/>
        </t:AdditionalProperties>
        <t:BodyType>Text</t:BodyType>
      </m:ItemShape>
      <m:ItemIds>
        <t:ItemId Id="{item_id}"/>
      </m:ItemIds>
    </m:GetItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    parse_get_item_response(&xml)
}

/// Parameters for sending an email via EWS.
///
/// Groups the many optional inputs so the function signature stays stable
/// as we add features like threading or BCC.
pub struct EwsSendParams<'a> {
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub body_type: &'a str,
    pub in_reply_to: Option<&'a str>,
    pub references: Option<&'a str>,
    pub reply_to: Option<&'a str>,
    /// Uploaded with `CreateAttachment` by [`create_draft`]; EWS does not
    /// take attachments inside `CreateItem`, so [`send_email`] ignores them.
    pub attachments: &'a [EwsAttachment],
}

/// A file attachment carried inline in `CreateItem`
pub struct EwsAttachment {
    pub name: String,
    pub content_type: String,
    pub content_base64: String,
}

/// An EWS item reference. `change_key` identifies the item's current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EwsItemRef {
    pub item_id: String,
    pub change_key: String,
}

/// What `get_draft_state` reports about an item
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EwsDraftState {
    pub item: EwsItemRef,
    pub is_draft: bool,
}

/// A draft as listed by `list_drafts`
#[derive(Debug, Clone, serde::Serialize)]
pub struct EwsDraftSummary {
    pub item_id: String,
    pub subject: String,
    pub to: String,
    pub cc: String,
    pub date_created: String,
}

/// Send an email via EWS `CreateItem` with `SendAndSaveCopy` disposition.
///
/// Supports BCC and sets `InternetMessageHeaders` for In-Reply-To and
/// References so Exchange clients thread the reply correctly.
pub async fn send_email(
    token_manager: &TokenManager,
    account_id: &str,
    params: &EwsSendParams<'_>,
) -> AppResult<()> {
    let message = render_message(params);
    let soap = format!(
        r#"<m:CreateItem MessageDisposition="SendAndSaveCopy">
      <m:SavedItemFolderId>
        <t:DistinguishedFolderId Id="sentitems"/>
      </m:SavedItemFolderId>
      <m:Items>
        {message}
      </m:Items>
    </m:CreateItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "send")?;
    Ok(())
}

/// Save a message in the Drafts folder (`CreateItem` with `SaveOnly`), then
/// add its attachments with `CreateAttachment`.
///
/// Bcc, Reply-To and threading headers are stored on the draft. If the
/// attachments cannot be added, the half-built draft is deleted and the
/// error returned. Returns the new item's id.
pub async fn create_draft(
    token_manager: &TokenManager,
    account_id: &str,
    params: &EwsSendParams<'_>,
) -> AppResult<EwsItemRef> {
    let message = render_message(params);
    let soap = format!(
        r#"<m:CreateItem MessageDisposition="SaveOnly">
      <m:SavedItemFolderId>
        <t:DistinguishedFolderId Id="drafts"/>
      </m:SavedItemFolderId>
      <m:Items>
        {message}
      </m:Items>
    </m:CreateItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "CreateItem")?;
    let item = parse_item_ref(&xml)
        .ok_or_else(|| AppError::Internal("EWS CreateItem returned no ItemId".to_owned()))?;

    if !params.attachments.is_empty() {
        let soap = render_create_attachment(&item.item_id, params.attachments);
        let result = match ews_request(token_manager, account_id, &soap).await {
            Ok(xml) => check_response(&xml, "CreateAttachment"),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            if let Err(cleanup) = delete_item(token_manager, account_id, &item.item_id).await {
                tracing::warn!("EWS: failed to remove incomplete draft: {cleanup}");
            }
            return Err(error);
        }
    }
    Ok(item)
}

/// Read an item's current change key and whether it is a draft.
pub async fn get_draft_state(
    token_manager: &TokenManager,
    account_id: &str,
    item_id: &str,
) -> AppResult<EwsDraftState> {
    let item_id = escape_xml(item_id);
    let soap = format!(
        r#"<m:GetItem>
      <m:ItemShape>
        <t:BaseShape>IdOnly</t:BaseShape>
        <t:AdditionalProperties>
          <t:FieldURI FieldURI="item:IsDraft"/>
        </t:AdditionalProperties>
      </m:ItemShape>
      <m:ItemIds>
        <t:ItemId Id="{item_id}"/>
      </m:ItemIds>
    </m:GetItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "GetItem")?;
    parse_draft_state(&xml)
        .ok_or_else(|| AppError::Internal("EWS GetItem returned no ItemId".to_owned()))
}

/// Send a saved draft and move it to Sent Items (`SendItem`).
pub async fn send_draft(
    token_manager: &TokenManager,
    account_id: &str,
    item: &EwsItemRef,
) -> AppResult<()> {
    let item_id = escape_xml(&item.item_id);
    let change_key = escape_xml(&item.change_key);
    let soap = format!(
        r#"<m:SendItem SaveItemToFolder="true">
      <m:ItemIds>
        <t:ItemId Id="{item_id}" ChangeKey="{change_key}"/>
      </m:ItemIds>
      <m:SavedItemFolderId>
        <t:DistinguishedFolderId Id="sentitems"/>
      </m:SavedItemFolderId>
    </m:SendItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "SendItem")
}

/// Permanently delete an item (`DeleteItem` with `HardDelete`).
pub async fn delete_item(
    token_manager: &TokenManager,
    account_id: &str,
    item_id: &str,
) -> AppResult<()> {
    let item_id = escape_xml(item_id);
    let soap = format!(
        r#"<m:DeleteItem DeleteType="HardDelete">
      <m:ItemIds>
        <t:ItemId Id="{item_id}"/>
      </m:ItemIds>
    </m:DeleteItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "DeleteItem")
}

/// List drafts, newest first.
pub async fn list_drafts(
    token_manager: &TokenManager,
    account_id: &str,
    max_items: usize,
) -> AppResult<Vec<EwsDraftSummary>> {
    let soap = format!(
        r#"<m:FindItem Traversal="Shallow">
      <m:ItemShape>
        <t:BaseShape>IdOnly</t:BaseShape>
        <t:AdditionalProperties>
          <t:FieldURI FieldURI="item:Subject"/>
          <t:FieldURI FieldURI="item:DisplayTo"/>
          <t:FieldURI FieldURI="item:DisplayCc"/>
          <t:FieldURI FieldURI="item:DateTimeCreated"/>
        </t:AdditionalProperties>
      </m:ItemShape>
      <m:IndexedPageItemView MaxEntriesReturned="{max_items}" Offset="0" BasePoint="Beginning"/>
      <m:SortOrder>
        <t:FieldOrder Order="Descending">
          <t:FieldURI FieldURI="item:DateTimeCreated"/>
        </t:FieldOrder>
      </m:SortOrder>
      <m:ParentFolderIds>
        <t:DistinguishedFolderId Id="drafts"/>
      </m:ParentFolderIds>
    </m:FindItem>"#
    );

    let xml = ews_request(token_manager, account_id, &soap).await?;
    check_response(&xml, "FindItem")?;
    parse_draft_summaries(&xml)
}

/// Render a `<t:Message>` element for `CreateItem`.
///
/// EWS validates element order against its schema: `Subject`, `Body`,
/// `InternetMessageHeaders` (item fields), then `ToRecipients`,
/// `CcRecipients`, `BccRecipients`, `ReplyTo` (message fields).
fn render_message(params: &EwsSendParams<'_>) -> String {
    let to_xml = render_mailboxes(params.to);
    let cc_section = if params.cc.is_empty() {
        String::new()
    } else {
        format!(
            "<t:CcRecipients>{}</t:CcRecipients>",
            render_mailboxes(params.cc)
        )
    };
    let bcc_section = if params.bcc.is_empty() {
        String::new()
    } else {
        format!(
            "<t:BccRecipients>{}</t:BccRecipients>",
            render_mailboxes(params.bcc)
        )
    };
    let reply_to_section = match params.reply_to {
        Some(addr) => format!(
            "<t:ReplyTo>{}</t:ReplyTo>",
            render_mailboxes(&[addr.to_owned()])
        ),
        None => String::new(),
    };
    let headers_section = build_internet_headers(params.in_reply_to, params.references);

    let body_escaped = escape_xml(params.body);
    let subject_escaped = escape_xml(params.subject);
    // body_type is a trusted constant ("HTML" or "Text"), so it doesn't need
    // XML escaping — it's interpolated directly into the BodyType attribute.
    let body_type = params.body_type;

    format!(
        r#"<t:Message>
          <t:Subject>{subject_escaped}</t:Subject>
          <t:Body BodyType="{body_type}">{body_escaped}</t:Body>
          {headers_section}
          <t:ToRecipients>{to_xml}</t:ToRecipients>
          {cc_section}
          {bcc_section}
          {reply_to_section}
        </t:Message>"#
    )
}

/// Render a `CreateAttachment` request adding `attachments` to `item_id`.
fn render_create_attachment(item_id: &str, attachments: &[EwsAttachment]) -> String {
    let files: String = attachments
        .iter()
        .map(|a| {
            format!(
                "<t:FileAttachment><t:Name>{}</t:Name><t:ContentType>{}</t:ContentType><t:Content>{}</t:Content></t:FileAttachment>",
                escape_xml(&a.name),
                escape_xml(&a.content_type),
                escape_xml(&a.content_base64)
            )
        })
        .collect();
    format!(
        r#"<m:CreateAttachment>
      <m:ParentItemId Id="{}"/>
      <m:Attachments>{files}</m:Attachments>
    </m:CreateAttachment>"#,
        escape_xml(item_id)
    )
}

/// Turn an EWS error response into an `AppError`.
///
/// `ErrorItemNotFound` (and malformed ids) map to `NotFound`; anything else
/// is `Internal` with the server's message.
fn check_response(xml: &str, operation: &str) -> AppResult<()> {
    if !xml.contains("ResponseClass=\"Error\"") {
        return Ok(());
    }
    let msg = extract_xml_text(xml, "MessageText").unwrap_or_else(|| "unknown error".to_owned());
    let code = extract_xml_text(xml, "ResponseCode").unwrap_or_default();
    if matches!(
        code.as_str(),
        "ErrorItemNotFound" | "ErrorInvalidIdMalformed" | "ErrorInvalidIdNotAnItemAttachmentId"
    ) {
        return Err(AppError::NotFound(format!(
            "EWS {operation}: item not found ({code}): {msg}"
        )));
    }
    Err(AppError::Internal(format!("EWS {operation} failed: {msg}")))
}

/// Render a list of recipient addresses as EWS `<t:Mailbox>` elements.
fn render_mailboxes(addrs: &[String]) -> String {
    addrs
        .iter()
        .map(|addr| {
            let escaped = escape_xml(addr);
            format!(r#"<t:Mailbox><t:EmailAddress>{escaped}</t:EmailAddress></t:Mailbox>"#)
        })
        .collect()
}

/// Build an `<t:InternetMessageHeaders>` block for threading headers.
/// Returns an empty string if neither header is set.
fn build_internet_headers(in_reply_to: Option<&str>, references: Option<&str>) -> String {
    if in_reply_to.is_none() && references.is_none() {
        return String::new();
    }
    let mut headers = String::from("<t:InternetMessageHeaders>");
    if let Some(irt) = in_reply_to {
        headers.push_str(&format!(
            r#"<t:InternetMessageHeader HeaderName="In-Reply-To">{}</t:InternetMessageHeader>"#,
            escape_xml(irt)
        ));
    }
    if let Some(refs) = references {
        headers.push_str(&format!(
            r#"<t:InternetMessageHeader HeaderName="References">{}</t:InternetMessageHeader>"#,
            escape_xml(refs)
        ));
    }
    headers.push_str("</t:InternetMessageHeaders>");
    headers
}

/// XML-escape the five predefined entities plus quotes (for attribute-safe
/// content). Order matters: `&` must be replaced first.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ─── XML Parsing helpers ─────────────────────────────────────────────────────
//
// Backed by `quick-xml`'s pull parser. Matches by *local name* (ignoring
// the `t:` / `m:` namespace prefix), correctly handles XML entities, CDATA
// and nested tags, and short-circuits at the first match for the
// `extract_*` helpers.

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

/// Return the (unescaped) text content of the first element whose local
/// name matches `tag`. Returns `None` if the tag is not found or if XML is
/// malformed.
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if local_name_eq(&e, tag) => {
                return Some(read_text_until_end(&mut reader));
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// Return the value of `attr` on the first element whose local name is `tag`.
/// Works on both `Start` and `Empty` events (so `<t:ItemId Id="x"/>` parses).
///
fn extract_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local_name_eq(&e, tag) => {
                return attr_value(&e, attr);
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// First `<t:ItemId>` in a response, with its change key.
fn parse_item_ref(xml: &str) -> Option<EwsItemRef> {
    let item_id = extract_attr(xml, "ItemId", "Id")?;
    let change_key = extract_attr(xml, "ItemId", "ChangeKey").unwrap_or_default();
    Some(EwsItemRef {
        item_id,
        change_key,
    })
}

/// Parse a GetItem response requested with `item:IsDraft`.
fn parse_draft_state(xml: &str) -> Option<EwsDraftState> {
    let item = parse_item_ref(xml)?;
    let is_draft = extract_xml_text(xml, "IsDraft").is_some_and(|v| v.trim() == "true");
    Some(EwsDraftState { item, is_draft })
}

/// Parse a FindItem response on the Drafts folder.
fn parse_draft_summaries(xml: &str) -> AppResult<Vec<EwsDraftSummary>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut drafts = Vec::new();
    let mut current: Option<EwsDraftSummary> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "Message" => {
                    current = Some(EwsDraftSummary {
                        item_id: String::new(),
                        subject: String::new(),
                        to: String::new(),
                        cc: String::new(),
                        date_created: String::new(),
                    });
                }
                "ItemId" => {
                    if let Some(draft) = current.as_mut() {
                        draft.item_id = attr_value(&e, "Id").unwrap_or_default();
                    }
                }
                "Subject" => {
                    let text = read_text_until_end(&mut reader);
                    if let Some(draft) = current.as_mut() {
                        draft.subject = text;
                    }
                }
                "DisplayTo" => {
                    let text = read_text_until_end(&mut reader);
                    if let Some(draft) = current.as_mut() {
                        draft.to = text;
                    }
                }
                "DisplayCc" => {
                    let text = read_text_until_end(&mut reader);
                    if let Some(draft) = current.as_mut() {
                        draft.cc = text;
                    }
                }
                "DateTimeCreated" => {
                    let text = read_text_until_end(&mut reader);
                    if let Some(draft) = current.as_mut() {
                        draft.date_created = text;
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if local_name_bytes_eq(e.name().as_ref(), "Message") => {
                if let Some(draft) = current.take() {
                    drafts.push(draft);
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS FindItem XML parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }
    Ok(drafts)
}

/// Parse a FindItem response — walks `<Message>` blocks and collects
/// subject / date / is_read / item_id / change_key / from (name + email).
fn parse_find_items_response(xml: &str) -> AppResult<Vec<EwsMessage>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut messages = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if local_name_eq(&e, "Message") => {
                messages.push(parse_message_block(&mut reader)?);
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS FindItem XML parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }
    Ok(messages)
}

/// Parse a GetItem response into a single `EwsMessageDetail`.
///
/// Walks the outer envelope until it finds `<t:Message>`, then delegates
/// extraction to `parse_message_detail_block`. This avoids accidentally
/// matching outer-namespace elements like `<soap:Body>` against the local
/// name `Body` (which would otherwise swallow the entire payload).
fn parse_get_item_response(xml: &str) -> AppResult<EwsMessageDetail> {
    if xml.contains("ResponseClass=\"Error\"") {
        let msg =
            extract_xml_text(xml, "MessageText").unwrap_or_else(|| "unknown error".to_owned());
        return Err(AppError::Internal(format!("EWS GetItem failed: {msg}")));
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if local_name_eq(&e, "Message") => {
                return parse_message_detail_block(&mut reader);
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS GetItem XML parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }

    // No <t:Message> found — return empty defaults (matches historical behavior
    // when the response was structurally unexpected).
    Ok(EwsMessageDetail {
        item_id: String::new(),
        subject: String::new(),
        from_name: String::new(),
        from_email: String::new(),
        to: String::new(),
        cc: String::new(),
        date_received: String::new(),
        body_text: String::new(),
        is_read: false,
        has_attachments: false,
    })
}

/// After a `Start(<t:Message>)` event, extract all fields for a single
/// `EwsMessageDetail` by reading child elements until the matching End.
fn parse_message_detail_block(reader: &mut Reader<&[u8]>) -> AppResult<EwsMessageDetail> {
    let mut detail = EwsMessageDetail {
        item_id: String::new(),
        subject: String::new(),
        from_name: String::new(),
        from_email: String::new(),
        to: String::new(),
        cc: String::new(),
        date_received: String::new(),
        body_text: String::new(),
        is_read: false,
        has_attachments: false,
    };
    let mut captured_from_name = false;
    let mut captured_from_email = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "ItemId" => {
                    if detail.item_id.is_empty() {
                        detail.item_id = attr_value(&e, "Id").unwrap_or_default();
                    }
                }
                "Subject" => {
                    detail.subject = read_text_until_end(reader);
                }
                "DateTimeReceived" => {
                    detail.date_received = read_text_until_end(reader);
                }
                "IsRead" => {
                    detail.is_read = read_text_until_end(reader) == "true";
                }
                "HasAttachments" => {
                    detail.has_attachments = read_text_until_end(reader) == "true";
                }
                "Body" => {
                    detail.body_text = read_text_until_end(reader);
                }
                "Name" if !captured_from_name => {
                    detail.from_name = read_text_until_end(reader);
                    captured_from_name = true;
                }
                "EmailAddress" if !captured_from_email => {
                    detail.from_email = read_text_until_end(reader);
                    captured_from_email = true;
                }
                "ToRecipients" => {
                    detail.to = collect_recipient_emails(reader)?;
                }
                "CcRecipients" => {
                    detail.cc = collect_recipient_emails(reader)?;
                }
                _ => {}
            },
            Ok(Event::End(e)) if local_name_bytes_eq(e.name().as_ref(), "Message") => {
                return Ok(detail);
            }
            Ok(Event::Eof) => return Ok(detail),
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS Message detail parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }
}

// ─── Parser primitives ───────────────────────────────────────────────────────

/// `true` if the local name of `e` (without namespace prefix) equals `name`.
fn local_name_eq(e: &BytesStart<'_>, name: &str) -> bool {
    local_name_bytes_eq(e.name().as_ref(), name)
}

/// `true` if a raw qualified-name byte slice (e.g. `b"t:Subject"`)
/// has a local part equal to `name`.
fn local_name_bytes_eq(qname: &str, name: &str) -> bool {
    qname.rsplit(':').next().unwrap_or(qname) == name
}

/// Extract the local name of a `Start`/`Empty` element (strips any
/// `prefix:` namespace).
fn local_name(e: &BytesStart<'_>) -> String {
    let qname = e.name();
    let name: &str = qname.as_ref();
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

/// Read the first attribute of `e` matching `attr_name` (local name match).
/// Returns the unescaped value.
fn attr_value(e: &BytesStart<'_>, attr_name: &str) -> Option<String> {
    for raw in e.attributes().flatten() {
        let key: &str = raw.key.as_ref();
        let local = key.rsplit(':').next().unwrap_or(key);
        if local == attr_name {
            return Some(
                raw.normalized_value(XmlVersion::Implicit1_0)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| raw.value.as_ref().to_owned()),
            );
        }
    }
    None
}

/// After a `Start` event, drain events accumulating Text/CData until we
/// see the matching `End` event (handles one level of nesting — sufficient
/// for leaf-like text elements in EWS responses). On malformed XML, returns
/// whatever we collected so far rather than erroring (lossy but forward-
/// compatible with the previous substring-based helpers).
fn read_text_until_end(reader: &mut Reader<&[u8]>) -> String {
    let mut text = String::new();
    let mut depth = 1;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    return text;
                }
            }
            Ok(Event::Text(t)) => {
                text.push_str(t.as_ref());
            }
            // quick-xml 0.42 emits entity references as separate GeneralRef
            // events; resolve the predefined entities and numeric character
            // references, keep unknown entities verbatim (forward-compatible).
            Ok(Event::GeneralRef(r)) => {
                if let Some(ch) = r.resolve_char_ref().ok().flatten() {
                    text.push(ch);
                } else if let Some(predef) =
                    quick_xml::escape::resolve_predefined_entity(r.as_ref())
                {
                    text.push_str(predef);
                } else {
                    text.push('&');
                    text.push_str(r.as_ref());
                    text.push(';');
                }
            }
            Ok(Event::CData(t)) => {
                text.push_str(t.as_ref());
            }
            Ok(Event::Eof) | Err(_) => return text,
            _ => {}
        }
    }
}

/// After a `Start(Message)` event, accumulate one `EwsMessage` by reading
/// child elements until the matching `End(Message)`.
fn parse_message_block(reader: &mut Reader<&[u8]>) -> AppResult<EwsMessage> {
    let mut msg = EwsMessage {
        item_id: String::new(),
        change_key: String::new(),
        subject: String::new(),
        from_name: String::new(),
        from_email: String::new(),
        date_received: String::new(),
        is_read: false,
    };
    let mut captured_name = false;
    let mut captured_email = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "ItemId" => {
                    msg.item_id = attr_value(&e, "Id").unwrap_or_default();
                    msg.change_key = attr_value(&e, "ChangeKey").unwrap_or_default();
                }
                "Subject" => {
                    msg.subject = read_text_until_end(reader);
                }
                "DateTimeReceived" => {
                    msg.date_received = read_text_until_end(reader);
                }
                "IsRead" => {
                    msg.is_read = read_text_until_end(reader) == "true";
                }
                "Name" if !captured_name => {
                    msg.from_name = read_text_until_end(reader);
                    captured_name = true;
                }
                "EmailAddress" if !captured_email => {
                    msg.from_email = read_text_until_end(reader);
                    captured_email = true;
                }
                _ => {}
            },
            Ok(Event::End(e)) if local_name_bytes_eq(e.name().as_ref(), "Message") => {
                return Ok(msg);
            }
            Ok(Event::Eof) => return Ok(msg),
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS Message block parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }
}

/// Inside a `ToRecipients` / `CcRecipients` Start event, collect all
/// child `<EmailAddress>` text values and join them with ", ".
fn collect_recipient_emails(reader: &mut Reader<&[u8]>) -> AppResult<String> {
    let mut emails = Vec::new();
    // We're positioned right AFTER a Start("ToRecipients") / ("CcRecipients")
    // event, so depth starts at 1. Track depth to know when we exit.
    let mut depth = 1i32;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if local_name_eq(&e, "EmailAddress") {
                    emails.push(read_text_until_end(reader));
                } else {
                    depth += 1;
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(emails.join(", "));
                }
            }
            Ok(Event::Eof) => return Ok(emails.join(", ")),
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "EWS recipients parse error at position {}: {err}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_xml_text_works_with_t_prefix() {
        let xml = r#"<t:Subject>Hello World</t:Subject>"#;
        assert_eq!(
            extract_xml_text(xml, "Subject").as_deref(),
            Some("Hello World")
        );
    }

    #[test]
    fn extract_xml_text_works_with_m_prefix() {
        let xml = r#"<m:MessageText>boom</m:MessageText>"#;
        assert_eq!(
            extract_xml_text(xml, "MessageText").as_deref(),
            Some("boom")
        );
    }

    #[test]
    fn extract_xml_text_unescapes_entities() {
        // Regression: previous substring impl returned raw entities. The new
        // quick-xml parser must decode &amp; / &lt; / &gt; / &quot; / &apos;.
        let xml = r#"<t:Subject>A &amp; B &lt;C&gt;</t:Subject>"#;
        assert_eq!(
            extract_xml_text(xml, "Subject").as_deref(),
            Some("A & B <C>")
        );
    }

    #[test]
    fn extract_xml_text_handles_cdata() {
        // CDATA should be preserved verbatim (not interpreted as XML).
        let xml = r#"<t:Body><![CDATA[<div>raw</div>]]></t:Body>"#;
        assert_eq!(
            extract_xml_text(xml, "Body").as_deref(),
            Some("<div>raw</div>")
        );
    }

    #[test]
    fn extract_attr_works() {
        let xml = r#"<t:ItemId Id="abc123" ChangeKey="xyz"/>"#;
        assert_eq!(extract_attr(xml, "ItemId", "Id").as_deref(), Some("abc123"));
        assert_eq!(
            extract_attr(xml, "ItemId", "ChangeKey").as_deref(),
            Some("xyz")
        );
    }

    #[test]
    fn extract_attr_unescapes_value() {
        let xml = r#"<t:Foo Val="a &amp; b"/>"#;
        assert_eq!(extract_attr(xml, "Foo", "Val").as_deref(), Some("a & b"));
    }

    #[test]
    fn extract_attr_handles_equals_in_value() {
        // base64-ish values often have trailing '=' which must survive parsing.
        let xml = r#"<t:ItemId Id="AAMk=" ChangeKey="CQA="/>"#;
        assert_eq!(extract_attr(xml, "ItemId", "Id").as_deref(), Some("AAMk="));
        assert_eq!(
            extract_attr(xml, "ItemId", "ChangeKey").as_deref(),
            Some("CQA=")
        );
    }

    #[test]
    fn parse_get_item_finds_itemid_through_nested_envelope() {
        // Strip down the GetItem failing case: verify just the ItemId path.
        let xml = r#"<m:GetItemResponse><m:ResponseMessages>
            <m:GetItemResponseMessage ResponseClass="Success">
              <m:Items><t:Message>
                <t:ItemId Id="X=" ChangeKey="Y="/>
                <t:Subject>s</t:Subject>
              </t:Message></m:Items>
            </m:GetItemResponseMessage>
          </m:ResponseMessages></m:GetItemResponse>"#;
        let d = parse_get_item_response(xml).unwrap();
        assert_eq!(
            d.item_id, "X=",
            "item_id extraction failed; got {:?}",
            d.item_id
        );
    }

    #[test]
    fn parse_find_items_empty() {
        let xml = r#"<soap:Envelope><soap:Body><m:FindItemResponse></m:FindItemResponse></soap:Body></soap:Envelope>"#;
        let result = parse_find_items_response(xml).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_find_items_parses_single_message() {
        let xml = r#"<soap:Envelope><soap:Body><m:FindItemResponse><m:ResponseMessages>
            <m:FindItemResponseMessage ResponseClass="Success">
              <m:RootFolder>
                <t:Items>
                  <t:Message>
                    <t:ItemId Id="AAMk=" ChangeKey="CQAA"/>
                    <t:Subject>Hola — acentos</t:Subject>
                    <t:DateTimeReceived>2026-04-21T15:00:00Z</t:DateTimeReceived>
                    <t:From><t:Mailbox>
                      <t:Name>Juan Pérez</t:Name>
                      <t:EmailAddress>juan@example.com</t:EmailAddress>
                    </t:Mailbox></t:From>
                    <t:IsRead>false</t:IsRead>
                  </t:Message>
                </t:Items>
              </m:RootFolder>
            </m:FindItemResponseMessage>
          </m:ResponseMessages></m:FindItemResponse></soap:Body></soap:Envelope>"#;
        let result = parse_find_items_response(xml).unwrap();
        assert_eq!(result.len(), 1);
        let m = &result[0];
        assert_eq!(m.item_id, "AAMk=");
        assert_eq!(m.change_key, "CQAA");
        assert_eq!(m.subject, "Hola — acentos");
        assert_eq!(m.from_name, "Juan Pérez");
        assert_eq!(m.from_email, "juan@example.com");
        assert_eq!(m.date_received, "2026-04-21T15:00:00Z");
        assert!(!m.is_read);
    }

    #[test]
    fn parse_get_item_extracts_body_and_recipients() {
        let xml = r#"<soap:Envelope><soap:Body><m:GetItemResponse><m:ResponseMessages>
            <m:GetItemResponseMessage ResponseClass="Success">
              <m:Items><t:Message>
                <t:ItemId Id="X=" ChangeKey="Y="/>
                <t:Subject>Re: Test</t:Subject>
                <t:DateTimeReceived>2026-04-21T15:00:00Z</t:DateTimeReceived>
                <t:Body BodyType="Text">Hola &amp; adiós</t:Body>
                <t:HasAttachments>true</t:HasAttachments>
                <t:From><t:Mailbox>
                  <t:Name>Alice</t:Name>
                  <t:EmailAddress>alice@x.com</t:EmailAddress>
                </t:Mailbox></t:From>
                <t:ToRecipients>
                  <t:Mailbox><t:EmailAddress>bob@x.com</t:EmailAddress></t:Mailbox>
                  <t:Mailbox><t:EmailAddress>carol@x.com</t:EmailAddress></t:Mailbox>
                </t:ToRecipients>
                <t:CcRecipients>
                  <t:Mailbox><t:EmailAddress>dan@x.com</t:EmailAddress></t:Mailbox>
                </t:CcRecipients>
                <t:IsRead>true</t:IsRead>
              </t:Message></m:Items>
            </m:GetItemResponseMessage>
          </m:ResponseMessages></m:GetItemResponse></soap:Body></soap:Envelope>"#;
        let d = parse_get_item_response(xml).unwrap();
        assert_eq!(d.item_id, "X=");
        assert_eq!(d.subject, "Re: Test");
        assert_eq!(d.from_name, "Alice");
        assert_eq!(d.from_email, "alice@x.com");
        assert_eq!(d.body_text, "Hola & adiós");
        assert_eq!(d.to, "bob@x.com, carol@x.com");
        assert_eq!(d.cc, "dan@x.com");
        assert!(d.is_read);
        assert!(d.has_attachments);
    }

    #[test]
    fn parse_get_item_surfaces_error_response() {
        let xml = r#"<m:GetItemResponse><m:ResponseMessages>
            <m:GetItemResponseMessage ResponseClass="Error">
              <m:MessageText>The specified object was not found.</m:MessageText>
              <m:ResponseCode>ErrorItemNotFound</m:ResponseCode>
            </m:GetItemResponseMessage>
          </m:ResponseMessages></m:GetItemResponse>"#;
        let err = parse_get_item_response(xml).unwrap_err();
        assert!(
            err.to_string()
                .contains("The specified object was not found")
        );
    }

    #[test]
    fn escape_xml_escapes_all_predefined_entities() {
        assert_eq!(escape_xml("&"), "&amp;");
        assert_eq!(escape_xml("<"), "&lt;");
        assert_eq!(escape_xml(">"), "&gt;");
        assert_eq!(escape_xml("\""), "&quot;");
        assert_eq!(escape_xml("'"), "&apos;");
        // Order: & must be escaped first so we don't double-escape
        assert_eq!(escape_xml("a&b<c"), "a&amp;b&lt;c");
    }

    #[test]
    fn render_mailboxes_escapes_addresses() {
        // Addresses with XML-special chars (shouldn't happen in practice, but
        // prevents injection if the caller passes malformed input).
        let addrs = vec!["a&b@example.com".to_owned()];
        let xml = render_mailboxes(&addrs);
        assert!(xml.contains("a&amp;b@example.com"));
        assert!(!xml.contains("a&b@example.com"));
    }

    #[test]
    fn build_internet_headers_empty_when_no_threading() {
        assert_eq!(build_internet_headers(None, None), "");
    }

    #[test]
    fn build_internet_headers_includes_in_reply_to() {
        let headers = build_internet_headers(Some("<abc@example.com>"), None);
        assert!(headers.contains("<t:InternetMessageHeaders>"));
        assert!(headers.contains(r#"HeaderName="In-Reply-To""#));
        assert!(headers.contains("&lt;abc@example.com&gt;"));
        assert!(!headers.contains("References"));
    }

    #[test]
    fn build_internet_headers_includes_both_threading_headers() {
        let headers = build_internet_headers(Some("<a@x.com>"), Some("<a@x.com> <b@x.com>"));
        assert!(headers.contains("In-Reply-To"));
        assert!(headers.contains("References"));
        assert!(headers.contains("&lt;a@x.com&gt; &lt;b@x.com&gt;"));
    }

    fn draft_params<'a>(
        to: &'a [String],
        bcc: &'a [String],
        attachments: &'a [EwsAttachment],
    ) -> EwsSendParams<'a> {
        EwsSendParams {
            to,
            cc: &[],
            bcc,
            subject: "Q3 <plan> & budget",
            body: "<p>Hi</p>",
            body_type: "HTML",
            in_reply_to: Some("<orig@x.com>"),
            references: None,
            reply_to: Some("team@x.com"),
            attachments,
        }
    }

    #[test]
    fn render_message_follows_ews_schema_order() {
        let to = vec!["a@x.com".to_owned()];
        let bcc = vec!["hidden@x.com".to_owned()];
        let attachments = vec![EwsAttachment {
            name: "a&b.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            content_base64: "aGVsbG8=".to_owned(),
        }];
        let xml = render_message(&draft_params(&to, &bcc, &attachments));

        let position = |needle: &str| {
            xml.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from {xml}"))
        };
        let order = [
            "<t:Subject>",
            "<t:Body ",
            "<t:InternetMessageHeaders>",
            "<t:ToRecipients>",
            "<t:BccRecipients>",
            "<t:ReplyTo>",
        ];
        for pair in order.windows(2) {
            assert!(
                position(pair[0]) < position(pair[1]),
                "{} must come before {}: {xml}",
                pair[0],
                pair[1]
            );
        }
        assert!(xml.contains("Q3 &lt;plan&gt; &amp; budget"), "{xml}");
        assert!(!xml.contains("<t:CcRecipients>"), "{xml}");
        // Attachments never travel in CreateItem
        assert!(!xml.contains("Attachment"), "{xml}");

        let upload = render_create_attachment("AAMk+id=", &attachments);
        assert!(
            upload.contains(r#"<m:ParentItemId Id="AAMk+id="/>"#),
            "{upload}"
        );
        assert!(upload.contains("<t:Name>a&amp;b.txt</t:Name>"), "{upload}");
        assert!(
            upload.contains("<t:Content>aGVsbG8=</t:Content>"),
            "{upload}"
        );
    }

    #[test]
    fn render_message_omits_optional_sections() {
        let to = vec!["a@x.com".to_owned()];
        let mut params = draft_params(&to, &[], &[]);
        params.reply_to = None;
        params.in_reply_to = None;
        let xml = render_message(&params);
        for absent in [
            "<t:ReplyTo>",
            "<t:BccRecipients>",
            "<t:InternetMessageHeaders>",
        ] {
            assert!(!xml.contains(absent), "{absent} should be absent: {xml}");
        }
    }

    #[test]
    fn parses_create_item_and_draft_state() {
        let create = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
          <m:CreateItemResponse xmlns:m="m" xmlns:t="t"><m:ResponseMessages>
          <m:CreateItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode>
          <m:Items><t:Message><t:ItemId Id="AAMk+id/1=" ChangeKey="CQAAAB"/></t:Message></m:Items>
          </m:CreateItemResponseMessage></m:ResponseMessages></m:CreateItemResponse></s:Body></s:Envelope>"#;
        assert!(check_response(create, "CreateItem").is_ok());
        assert_eq!(
            parse_item_ref(create),
            Some(EwsItemRef {
                item_id: "AAMk+id/1=".to_owned(),
                change_key: "CQAAAB".to_owned(),
            })
        );

        let state = r#"<m:GetItemResponseMessage ResponseClass="Success"><m:Items><t:Message>
          <t:ItemId Id="X" ChangeKey="CK2"/><t:IsDraft>true</t:IsDraft>
          </t:Message></m:Items></m:GetItemResponseMessage>"#;
        let parsed = parse_draft_state(state).unwrap();
        assert!(parsed.is_draft);
        assert_eq!(parsed.item.change_key, "CK2");

        let sent = state.replace("<t:IsDraft>true", "<t:IsDraft>false");
        assert!(!parse_draft_state(&sent).unwrap().is_draft);
    }

    #[test]
    fn classifies_ews_errors() {
        let not_found = r#"<m:GetItemResponseMessage ResponseClass="Error">
          <m:MessageText>The specified object was not found in the store.</m:MessageText>
          <m:ResponseCode>ErrorItemNotFound</m:ResponseCode></m:GetItemResponseMessage>"#;
        assert!(matches!(
            check_response(not_found, "GetItem"),
            Err(AppError::NotFound(_))
        ));
        let other = not_found.replace("ErrorItemNotFound", "ErrorAccessDenied");
        let err = check_response(&other, "SendItem").unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
        assert!(err.to_string().contains("EWS SendItem failed"), "{err}");
    }

    #[test]
    fn parses_draft_summaries() {
        let xml = r#"<m:FindItemResponseMessage ResponseClass="Success"><m:RootFolder><t:Items>
          <t:Message><t:ItemId Id="D1" ChangeKey="A"/><t:Subject>First &amp; best</t:Subject>
            <t:DisplayTo>Ana</t:DisplayTo><t:DisplayCc></t:DisplayCc>
            <t:DateTimeCreated>2026-10-13T10:00:00Z</t:DateTimeCreated></t:Message>
          <t:Message><t:ItemId Id="D2" ChangeKey="B"/><t:Subject>Second</t:Subject></t:Message>
        </t:Items></m:RootFolder></m:FindItemResponseMessage>"#;
        let drafts = parse_draft_summaries(xml).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].item_id, "D1");
        assert_eq!(drafts[0].subject, "First & best");
        assert_eq!(drafts[0].to, "Ana");
        assert_eq!(drafts[0].date_created, "2026-10-13T10:00:00Z");
        assert_eq!(drafts[1].item_id, "D2");
        assert_eq!(drafts[1].to, "");
    }
}
