//! Message parsing and MIME handling
//!
//! Parses RFC822 messages using `mailparse`, extracts body text/HTML,
//! and handles attachments. Sanitizes HTML with `ammonia` and supports
//! optional PDF text extraction.

use std::collections::BTreeMap;

use mailparse::{DispositionType, MailHeader, ParsedMail};

use crate::errors::{AppError, AppResult};
use crate::models::AttachmentInfo;

/// Parsed message representation
///
/// Contains extracted headers, body content, and attachment metadata.
/// Bodies are truncated by caller to configured limits.
#[derive(Debug, Clone)]
pub struct ParsedMessage {
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
    /// All headers as key-value pairs
    pub headers_all: Vec<(String, String)>,
    /// Plain text body (untruncated)
    pub body_text: Option<String>,
    /// Sanitized HTML body (untruncated)
    pub body_html_sanitized: Option<String>,
    /// Attachment metadata
    pub attachments: Vec<AttachmentInfo>,
}

/// Parse RFC822 message into structured representation
///
/// Extracts headers, body text/HTML, and attachment info. Sanitizes
/// HTML and optionally extracts text from PDF attachments.
///
/// # Parameters
///
/// - `raw`: RFC822 message bytes
/// - `body_max_chars`: Maximum characters for body text/HTML (caller truncates)
/// - `include_html`: Whether to include HTML body
/// - `extract_attachment_text`: Whether to extract text from PDFs
/// - `attachment_text_max_chars`: Maximum characters for extracted PDF text
///
/// # Errors
///
/// - `Internal` if `mailparse` fails
pub fn parse_message(
    raw: &[u8],
    body_max_chars: usize,
    include_html: bool,
    extract_attachment_text: bool,
    attachment_text_max_chars: usize,
) -> AppResult<ParsedMessage> {
    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse RFC822 message: {e}")))?;

    let headers = parse_all_headers(raw)?;
    let mut body_text = None;
    let mut body_html = None;
    let mut attachments = Vec::new();

    walk_parts(
        &parsed,
        "1".to_owned(),
        &mut body_text,
        &mut body_html,
        &mut attachments,
        extract_attachment_text,
        attachment_text_max_chars,
    )?;

    let text = body_text.map(|t| truncate_chars(t, body_max_chars));
    let html = if include_html {
        body_html.map(|h| truncate_chars(h, body_max_chars))
    } else {
        None
    };

    let header_map = to_header_map(&headers);
    Ok(ParsedMessage {
        date: header_map.get("date").cloned(),
        from: header_map.get("from").cloned(),
        to: header_map.get("to").cloned(),
        cc: header_map.get("cc").cloned(),
        subject: header_map.get("subject").cloned(),
        headers_all: headers,
        body_text: text,
        body_html_sanitized: html,
        attachments,
    })
}

/// Walk MIME part tree recursively
///
/// Traverses all MIME parts to extract text/plain, text/html bodies,
/// and attachment metadata. Handles multipart structures correctly.
fn walk_parts(
    part: &ParsedMail<'_>,
    part_id: String,
    body_text: &mut Option<String>,
    body_html: &mut Option<String>,
    attachments: &mut Vec<AttachmentInfo>,
    extract_attachment_text: bool,
    attachment_text_max_chars: usize,
) -> AppResult<()> {
    if part.subparts.is_empty() {
        let ctype = part.ctype.mimetype.to_ascii_lowercase();
        let disp = part.get_content_disposition();
        let filename = attachment_filename(part, &disp.params);
        let is_attachment = disp.disposition == DispositionType::Attachment || filename.is_some();

        if !is_attachment {
            if ctype == "text/plain"
                && body_text.is_none()
                && let Ok(text) = part.get_body()
            {
                *body_text = Some(text);
            }

            if ctype == "text/html"
                && body_html.is_none()
                && let Ok(html) = part.get_body()
            {
                *body_html = Some(ammonia::clean(&html));
            }
        }

        if is_attachment {
            let raw_body = part
                .get_body_raw()
                .map_err(|e| AppError::Internal(format!("failed decoding attachment body: {e}")))?;
            let mut extracted_text = None;
            if extract_attachment_text
                && ctype == "application/pdf"
                && raw_body.len() <= 5_000_000
                && let Ok(text) = pdf_extract::extract_text_from_mem(&raw_body)
            {
                extracted_text = Some(truncate_chars(text, attachment_text_max_chars));
            }

            attachments.push(AttachmentInfo {
                filename,
                content_type: ctype,
                size_bytes: raw_body.len(),
                part_id,
                extracted_text,
            });
        }

        return Ok(());
    }

    for (idx, sub) in part.subparts.iter().enumerate() {
        let next_id = format!("{part_id}.{}", idx + 1);
        walk_parts(
            sub,
            next_id,
            body_text,
            body_html,
            attachments,
            extract_attachment_text,
            attachment_text_max_chars,
        )?;
    }
    Ok(())
}

/// Extract attachment filename from part
///
/// Checks Content-Disposition parameter first, falls back to Content-Type
/// name parameter.
fn attachment_filename(
    part: &ParsedMail<'_>,
    disp_params: &BTreeMap<String, String>,
) -> Option<String> {
    disp_params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned())
}

/// Return headers, either curated or all
///
/// If `include_all=true`, returns all headers. Otherwise, returns only
/// a safe subset (Date, From, To, Cc, Subject, Message-ID).
pub fn curated_headers(headers: &[(String, String)], include_all: bool) -> Vec<(String, String)> {
    if include_all {
        return headers.to_vec();
    }

    let allowed = ["date", "from", "to", "cc", "subject", "message-id"];
    headers
        .iter()
        .filter(|(k, _)| allowed.contains(&k.to_ascii_lowercase().as_str()))
        .cloned()
        .collect()
}

/// Parse header bytes into key-value pairs
pub fn parse_header_bytes(header_bytes: &[u8]) -> AppResult<Vec<(String, String)>> {
    let (headers, _) = mailparse::parse_headers(header_bytes)
        .map_err(|e| AppError::Internal(format!("failed to parse message headers: {e}")))?;
    Ok(to_tuples(headers))
}

/// Parse all headers from raw message
fn parse_all_headers(raw: &[u8]) -> AppResult<Vec<(String, String)>> {
    let (headers, _) = mailparse::parse_headers(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse message headers: {e}")))?;
    Ok(to_tuples(headers))
}

/// Convert mailparse headers to key-value tuples
///
/// Extracts header keys and values using mailparse's `get_key()` and `get_value()`
/// methods, which handle encoding and whitespace normalization.
fn to_tuples(headers: Vec<MailHeader<'_>>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|h| (h.get_key(), h.get_value()))
        .collect()
}

/// Convert header tuples to case-insensitive map
///
/// Returns the first value for each header key (case-insensitive). If a header
/// appears multiple times, only the first value is retained. Keys are normalized
/// to lowercase for case-insensitive lookup.
fn to_header_map(headers: &[(String, String)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (k, v) in headers {
        let key = k.to_ascii_lowercase();
        map.entry(key).or_insert_with(|| v.clone());
    }
    map
}

/// Truncate string to maximum characters (Unicode-aware)
///
/// Preserves complete characters, never splitting multi-byte sequences.
/// Extract plain text body from a parsed email message.
///
/// Walks the MIME parts and returns the first `text/plain` body found.
/// Returns `None` if no text/plain part exists.
pub fn extract_body_text(parsed: &mailparse::ParsedMail<'_>) -> Option<String> {
    if parsed.subparts.is_empty() {
        let ct = parsed.ctype.mimetype.to_ascii_lowercase();
        if ct == "text/plain" || ct == "text" {
            return parsed.get_body().ok();
        }
        return None;
    }
    for sub in &parsed.subparts {
        if let Some(text) = extract_body_text(sub) {
            return Some(text);
        }
    }
    None
}

pub fn truncate_chars(input: String, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{curated_headers, parse_message, truncate_chars};

    /// Tests that Unicode strings are truncated by character, not byte.
    #[test]
    fn truncates_unicode_by_character() {
        let input = "a😀b😀c".to_owned();
        let out = truncate_chars(input, 4);
        assert_eq!(out, "a😀b😀");
    }

    /// Tests that `curated_headers` filters headers unless `include_all` is true.
    #[test]
    fn curated_headers_filters_unless_include_all() {
        let headers = vec![
            (
                "Date".to_owned(),
                "Wed, 1 Jan 2025 00:00:00 +0000".to_owned(),
            ),
            ("From".to_owned(), "sender@example.com".to_owned()),
            ("X-Custom".to_owned(), "value".to_owned()),
        ];

        let curated = curated_headers(&headers, false);
        assert_eq!(curated.len(), 2);
        assert!(curated.iter().any(|(k, _)| k.eq_ignore_ascii_case("date")));
        assert!(curated.iter().any(|(k, _)| k.eq_ignore_ascii_case("from")));

        let all = curated_headers(&headers, true);
        assert_eq!(all.len(), 3);
    }

    /// Tests parsing of a simple plain text message and verifies header and body extraction.
    #[test]
    fn parses_simple_plain_text_message() {
        let raw = b"From: sender@example.com\r\nTo: user@example.com\r\nSubject: Hi\r\nDate: Wed, 1 Jan 2025 00:00:00 +0000\r\n\r\nHello there";
        let parsed = parse_message(raw, 2000, false, false, 10000).expect("parse should succeed");

        assert_eq!(parsed.subject.as_deref(), Some("Hi"));
        assert_eq!(parsed.from.as_deref(), Some("sender@example.com"));
        assert_eq!(parsed.to.as_deref(), Some("user@example.com"));
        assert_eq!(parsed.body_text.as_deref(), Some("Hello there"));
        assert!(parsed.attachments.is_empty());
    }
}
