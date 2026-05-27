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
    pub attachments: &'a [crate::smtp::EmailAttachment],
}

/// Send an email via EWS.
///
/// Without attachments: single `CreateItem` with `SendAndSaveCopy`.
/// With attachments: `CreateItem` as draft → `CreateAttachment` per file → `SendItem`.
/// EWS does not allow setting `Attachments` inline in `CreateItem` — that property
/// is read-only on the Message type and must go through the dedicated attachment endpoint.
pub async fn send_email(
    token_manager: &TokenManager,
    account_id: &str,
    params: &EwsSendParams<'_>,
) -> AppResult<()> {
    let to_xml = render_mailboxes(params.to);
    let cc_xml = render_mailboxes(params.cc);
    let bcc_xml = render_mailboxes(params.bcc);

    let cc_section = if params.cc.is_empty() {
        String::new()
    } else {
        format!("<t:CcRecipients>{cc_xml}</t:CcRecipients>")
    };
    let bcc_section = if params.bcc.is_empty() {
        String::new()
    } else {
        format!("<t:BccRecipients>{bcc_xml}</t:BccRecipients>")
    };

    // EWS takes RFC 2822 threading headers via InternetMessageHeaders. They
    // must be placed BEFORE ToRecipients in the XML — EWS is order-sensitive
    // and will reject out-of-order elements with a schema error.
    let headers_section = build_internet_headers(params.in_reply_to, params.references);

    let body_escaped = escape_xml(params.body);
    let subject_escaped = escape_xml(params.subject);
    let body_type = params.body_type;

    if params.attachments.is_empty() {
        // Fast path: single CreateItem with SendAndSaveCopy.
        let soap = format!(
            r#"<m:CreateItem MessageDisposition="SendAndSaveCopy">
          <m:SavedItemFolderId>
            <t:DistinguishedFolderId Id="sentitems"/>
          </m:SavedItemFolderId>
          <m:Items>
            <t:Message>
              <t:Subject>{subject_escaped}</t:Subject>
              <t:Body BodyType="{body_type}">{body_escaped}</t:Body>
              {headers_section}
              <t:ToRecipients>{to_xml}</t:ToRecipients>
              {cc_section}
              {bcc_section}
            </t:Message>
          </m:Items>
        </m:CreateItem>"#
        );
        let xml = ews_request(token_manager, account_id, &soap).await?;
        if xml.contains("ResponseClass=\"Error\"") {
            let msg = extract_xml_text(&xml, "MessageText").unwrap_or_default();
            return Err(AppError::Internal(format!("EWS send failed: {msg}")));
        }
        return Ok(());
    }

    // Attachment path: CreateItem as draft → CreateAttachment × N → SendItem.

    // Step 1: create draft in Drafts folder.
    let soap = format!(
        r#"<m:CreateItem MessageDisposition="SaveOnly">
      <m:SavedItemFolderId>
        <t:DistinguishedFolderId Id="drafts"/>
      </m:SavedItemFolderId>
      <m:Items>
        <t:Message>
          <t:Subject>{subject_escaped}</t:Subject>
          <t:Body BodyType="{body_type}">{body_escaped}</t:Body>
          {headers_section}
          <t:ToRecipients>{to_xml}</t:ToRecipients>
          {cc_section}
          {bcc_section}
        </t:Message>
      </m:Items>
    </m:CreateItem>"#
    );
    let xml = ews_request(token_manager, account_id, &soap).await?;
    if xml.contains("ResponseClass=\"Error\"") {
        let msg = extract_xml_text(&xml, "MessageText").unwrap_or_default();
        return Err(AppError::Internal(format!("EWS create draft failed: {msg}")));
    }
    let item_id = extract_attr(&xml, "ItemId", "Id").ok_or_else(|| {
        AppError::Internal("EWS CreateItem response missing ItemId".to_owned())
    })?;
    let change_key = extract_attr(&xml, "ItemId", "ChangeKey").unwrap_or_default();

    // Step 2: attach each file — each CreateAttachment returns a new ChangeKey.
    let mut change_key = change_key;
    for att in params.attachments {
        change_key = create_attachment(token_manager, account_id, &item_id, &change_key, att).await?;
    }

    // Step 3: fetch the current ChangeKey — Exchange updates it server-side during
    // attachment indexing so the key from CreateAttachment is already stale.
    let current_change_key =
        fetch_change_key(token_manager, account_id, &item_id).await?;
    let item_id_escaped = escape_xml(&item_id);
    let change_key_escaped = escape_xml(&current_change_key);
    let soap = format!(
        r#"<m:SendItem SaveItemToFolder="true">
      <m:ItemIds>
        <t:ItemId Id="{item_id_escaped}" ChangeKey="{change_key_escaped}"/>
      </m:ItemIds>
      <m:SavedItemFolderId>
        <t:DistinguishedFolderId Id="sentitems"/>
      </m:SavedItemFolderId>
    </m:SendItem>"#
    );
    let xml = ews_request(token_manager, account_id, &soap).await?;
    if xml.contains("ResponseClass=\"Error\"") {
        let msg = extract_xml_text(&xml, "MessageText").unwrap_or_default();
        return Err(AppError::Internal(format!("EWS send failed: {msg}")));
    }

    Ok(())
}

/// Attach a single file to an existing draft item via `CreateAttachment`.
/// Returns the updated ChangeKey from the response (EWS increments it on each mutation).
async fn create_attachment(
    token_manager: &TokenManager,
    account_id: &str,
    item_id: &str,
    change_key: &str,
    att: &crate::smtp::EmailAttachment,
) -> AppResult<String> {
    use base64::Engine;
    let name = escape_xml(&att.filename);
    let content_type = escape_xml(&att.content_type);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&att.content);
    let item_id_escaped = escape_xml(item_id);
    let change_key_escaped = escape_xml(change_key);

    let soap = format!(
        r#"<m:CreateAttachment>
      <m:ParentItemId Id="{item_id_escaped}" ChangeKey="{change_key_escaped}"/>
      <m:Attachments>
        <t:FileAttachment>
          <t:Name>{name}</t:Name>
          <t:ContentType>{content_type}</t:ContentType>
          <t:Content>{b64}</t:Content>
        </t:FileAttachment>
      </m:Attachments>
    </m:CreateAttachment>"#
    );
    let xml = ews_request(token_manager, account_id, &soap).await?;
    if xml.contains("ResponseClass=\"Error\"") {
        let msg = extract_xml_text(&xml, "MessageText").unwrap_or_default();
        return Err(AppError::Internal(format!(
            "EWS CreateAttachment failed for '{}': {msg}",
            att.filename
        )));
    }
    // RootItemChangeKey reflects the updated ChangeKey of the parent item.
    let new_change_key = extract_attr(&xml, "RootItemId", "RootItemChangeKey")
        .unwrap_or_else(|| change_key.to_owned());
    Ok(new_change_key)
}

/// Fetch the current ChangeKey for an item by doing a minimal GetItem.
/// Used before SendItem to get a fresh key after attachment indexing mutates it.
async fn fetch_change_key(
    token_manager: &TokenManager,
    account_id: &str,
    item_id: &str,
) -> AppResult<String> {
    let item_id_escaped = escape_xml(item_id);
    let soap = format!(
        r#"<m:GetItem>
      <m:ItemShape>
        <t:BaseShape>IdOnly</t:BaseShape>
      </m:ItemShape>
      <m:ItemIds>
        <t:ItemId Id="{item_id_escaped}"/>
      </m:ItemIds>
    </m:GetItem>"#
    );
    let xml = ews_request(token_manager, account_id, &soap).await?;
    extract_attr(&xml, "ItemId", "ChangeKey").ok_or_else(|| {
        AppError::Internal("EWS GetItem response missing ChangeKey".to_owned())
    })
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
use quick_xml::events::{BytesStart, Event};

/// Return the (unescaped) text content of the first element whose local
/// name matches `tag`. Returns `None` if the tag is not found or if XML is
/// malformed.
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
/// Kept as a general helper for future parsers (and for tests that
/// regression-check the XML parsing); marked `#[allow(dead_code)]` because
/// current parsers inline `attr_value` on their own event walk.
#[allow(dead_code)]
fn extract_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local_name_eq(&e, tag) => {
                return attr_value(&e, attr);
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// Parse a FindItem response — walks `<Message>` blocks and collects
/// subject / date / is_read / item_id / change_key / from (name + email).
fn parse_find_items_response(xml: &str) -> AppResult<Vec<EwsMessage>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut messages = Vec::new();

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
    let mut buf = Vec::new();

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
    let mut buf = Vec::new();
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
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
fn local_name_bytes_eq(qname: &[u8], name: &str) -> bool {
    let local = match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    };
    local == name.as_bytes()
}

/// Extract the local name of a `Start`/`Empty` element (strips any
/// `prefix:` namespace).
fn local_name(e: &BytesStart<'_>) -> String {
    let qname = e.name();
    let bytes = qname.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).into_owned()
}

/// Read the first attribute of `e` matching `attr_name` (local name match).
/// Returns the unescaped value.
fn attr_value(e: &BytesStart<'_>, attr_name: &str) -> Option<String> {
    for raw in e.attributes().flatten() {
        let key = raw.key;
        let key_bytes = key.as_ref();
        let local = match key_bytes.iter().position(|&b| b == b':') {
            Some(i) => &key_bytes[i + 1..],
            None => key_bytes,
        };
        if local == attr_name.as_bytes() {
            return Some(
                raw.unescape_value()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(raw.value.as_ref()).into_owned()),
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
    let mut buf = Vec::new();
    let mut text = String::new();
    let mut depth = 1;
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    return text;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    text.push_str(&s);
                }
            }
            Ok(Event::CData(t)) => {
                text.push_str(&String::from_utf8_lossy(t.as_ref()));
            }
            Ok(Event::Eof) | Err(_) => return text,
            _ => {}
        }
    }
}

/// After a `Start(Message)` event, accumulate one `EwsMessage` by reading
/// child elements until the matching `End(Message)`.
fn parse_message_block(reader: &mut Reader<&[u8]>) -> AppResult<EwsMessage> {
    let mut buf = Vec::new();
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
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
    let mut buf = Vec::new();
    let mut emails = Vec::new();
    // We're positioned right AFTER a Start("ToRecipients") / ("CcRecipients")
    // event, so depth starts at 1. Track depth to know when we exit.
    let mut depth = 1i32;
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
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
}
