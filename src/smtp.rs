//! SMTP transport for sending emails
//!
//! Provides async SMTP sending via `lettre`, supporting both password and
//! OAuth2 (XOAUTH2) authentication. Connections use STARTTLS or direct TLS
//! with `rustls` to match the project's existing TLS stack.
//!
//! # Configuration
//!
//! Per-account SMTP settings are loaded from environment variables:
//! ```text
//! MAIL_SMTP_<SEGMENT>_HOST=smtp.gmail.com
//! MAIL_SMTP_<SEGMENT>_PORT=587
//! MAIL_SMTP_<SEGMENT>_USER=user@gmail.com
//! MAIL_SMTP_<SEGMENT>_PASS=app-password
//! MAIL_SMTP_<SEGMENT>_SECURE=starttls
//! ```

use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::{Mailbox, MessageBuilder, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use secrecy::{ExposeSecret, SecretString};

use crate::config::AuthMethod;
use crate::errors::{AppError, AppResult};
use crate::oauth2::{self, TokenManager};

// ─── SMTP account config ────────────────────────────────────────────────────

/// SMTP connection security mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Direct TLS connection (typically port 465)
    Tls,
    /// STARTTLS upgrade after plaintext connection (typically port 587)
    Starttls,
    /// Plaintext connection (only for trusted local networks)
    Plain,
}

impl SmtpSecurity {
    /// Parse security mode from configuration value
    pub fn parse(value: &str) -> AppResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tls" | "ssl" => Ok(Self::Tls),
            "starttls" => Ok(Self::Starttls),
            "plain" | "none" => Ok(Self::Plain),
            other => Err(AppError::InvalidInput(format!(
                "unsupported SMTP security mode '{other}'; expected 'tls', 'starttls', or 'plain'"
            ))),
        }
    }
}

/// SMTP account configuration
#[derive(Debug, Clone)]
pub struct SmtpAccountConfig {
    /// Account identifier (matches IMAP account_id)
    pub account_id: String,
    /// SMTP server hostname
    pub host: String,
    /// SMTP server port
    pub port: u16,
    /// SMTP username
    pub user: String,
    /// SMTP password (optional when OAuth2 is used)
    pub pass: Option<SecretString>,
    /// Connection security mode
    pub security: SmtpSecurity,
    /// Authentication method
    pub auth_method: AuthMethod,
}

// ─── Email composition ───────────────────────────────────────────────────────

/// Parameters for composing and sending an email
pub struct EmailComposition {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub reply_to: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

// ─── Send email ──────────────────────────────────────────────────────────────

/// Send an email via SMTP.
///
/// Builds the MIME message and sends it through the configured SMTP transport.
/// Supports both password and OAuth2 authentication.
///
/// # Errors
///
/// - `InvalidInput` if email addresses are malformed
/// - `AuthFailed` if SMTP authentication fails
/// - `Timeout` if connection or sending times out
/// - `Internal` for other SMTP errors
pub async fn send_email(
    smtp_config: &SmtpAccountConfig,
    token_manager: Option<&TokenManager>,
    timeout_ms: u64,
    composition: &EmailComposition,
) -> AppResult<String> {
    let message = build_message(composition)?;
    let message_id = message
        .headers()
        .get_raw("Message-ID")
        .unwrap_or_default()
        .to_owned();

    let transport = build_transport(smtp_config, token_manager, timeout_ms).await?;

    tokio::time::timeout(Duration::from_millis(timeout_ms), transport.send(message))
        .await
        .map_err(|_| AppError::Timeout("SMTP send timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| {
                let msg = e.to_string();
                if msg.to_ascii_lowercase().contains("auth")
                    || msg.contains("535")
                    || msg.contains("534")
                {
                    AppError::AuthFailed(format!("SMTP authentication failed: {msg}"))
                } else {
                    AppError::Internal(format!("SMTP send failed: {msg}"))
                }
            })
        })?;

    Ok(message_id)
}

/// Verify SMTP connectivity and authentication.
///
/// Attempts to connect and authenticate without sending any email.
pub async fn verify_smtp(
    smtp_config: &SmtpAccountConfig,
    token_manager: Option<&TokenManager>,
    timeout_ms: u64,
) -> AppResult<()> {
    let transport = build_transport(smtp_config, token_manager, timeout_ms).await?;

    tokio::time::timeout(Duration::from_millis(timeout_ms), transport.test_connection())
        .await
        .map_err(|_| AppError::Timeout("SMTP connection test timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| {
                let msg = e.to_string();
                if msg.to_ascii_lowercase().contains("auth") {
                    AppError::AuthFailed(format!("SMTP authentication failed: {msg}"))
                } else {
                    AppError::Internal(format!("SMTP connection test failed: {msg}"))
                }
            })
        })?;

    Ok(())
}

// ─── Internals ───────────────────────────────────────────────────────────────

/// Build a lettre Message from composition parameters.
fn build_message(comp: &EmailComposition) -> AppResult<Message> {
    let from_mailbox: Mailbox = comp
        .from
        .parse()
        .map_err(|e| AppError::InvalidInput(format!("invalid From address '{}': {e}", comp.from)))?;

    let mut builder: MessageBuilder = Message::builder()
        .from(from_mailbox);

    for addr in &comp.to {
        let mb: Mailbox = addr
            .parse()
            .map_err(|e| AppError::InvalidInput(format!("invalid To address '{addr}': {e}")))?;
        builder = builder.to(mb);
    }

    for addr in &comp.cc {
        let mb: Mailbox = addr
            .parse()
            .map_err(|e| AppError::InvalidInput(format!("invalid Cc address '{addr}': {e}")))?;
        builder = builder.cc(mb);
    }

    for addr in &comp.bcc {
        let mb: Mailbox = addr
            .parse()
            .map_err(|e| AppError::InvalidInput(format!("invalid Bcc address '{addr}': {e}")))?;
        builder = builder.bcc(mb);
    }

    builder = builder.subject(&comp.subject);

    if let Some(ref reply_to) = comp.reply_to {
        let mb: Mailbox = reply_to
            .parse()
            .map_err(|e| AppError::InvalidInput(format!("invalid Reply-To address '{reply_to}': {e}")))?;
        builder = builder.reply_to(mb);
    }

    if let Some(ref in_reply_to) = comp.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.clone());
    }

    if let Some(ref references) = comp.references {
        builder = builder.references(references.clone());
    }

    // Build body: multipart if both text and HTML, single part otherwise
    let message = match (&comp.body_text, &comp.body_html) {
        (Some(text), Some(html)) => builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .content_type(ContentType::TEXT_PLAIN)
                            .body(text.clone()),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .content_type(ContentType::TEXT_HTML)
                            .body(html.clone()),
                    ),
            )
            .map_err(|e| AppError::Internal(format!("failed to build multipart message: {e}")))?,
        (Some(text), None) => builder
            .body(text.clone())
            .map_err(|e| AppError::Internal(format!("failed to build text message: {e}")))?,
        (None, Some(html)) => builder
            .singlepart(
                SinglePart::builder()
                    .content_type(ContentType::TEXT_HTML)
                    .body(html.clone()),
            )
            .map_err(|e| AppError::Internal(format!("failed to build HTML message: {e}")))?,
        (None, None) => builder
            .body(String::new())
            .map_err(|e| AppError::Internal(format!("failed to build empty message: {e}")))?,
    };

    Ok(message)
}

/// Build the async SMTP transport with appropriate auth and security.
async fn build_transport(
    config: &SmtpAccountConfig,
    token_manager: Option<&TokenManager>,
    timeout_ms: u64,
) -> AppResult<AsyncSmtpTransport<Tokio1Executor>> {
    let timeout_duration = Duration::from_millis(timeout_ms);

    let mut builder = match config.security {
        SmtpSecurity::Starttls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| AppError::Internal(format!("SMTP STARTTLS relay error: {e}")))?
        }
        SmtpSecurity::Tls => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
                .map_err(|e| AppError::Internal(format!("SMTP TLS relay error: {e}")))?
        }
        SmtpSecurity::Plain => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host),
    };

    builder = builder.port(config.port).timeout(Some(timeout_duration));

    // Set up authentication
    match config.auth_method {
        AuthMethod::OAuth2 => {
            let tm = token_manager.ok_or_else(|| {
                AppError::Internal(format!(
                    "SMTP account '{}' requires OAuth2 but no token manager available",
                    config.account_id
                ))
            })?;
            let access_token = tm.get_access_token(&config.account_id).await?;
            // For XOAUTH2, lettre accepts Credentials where the password is the
            // full XOAUTH2 SASL string when using Mechanism::Xoauth2
            let sasl = oauth2::xoauth2_sasl(&config.user, &access_token);
            let credentials = Credentials::new(config.user.clone(), sasl);
            builder = builder
                .credentials(credentials)
                .authentication(vec![Mechanism::Xoauth2]);
        }
        AuthMethod::Password => {
            if let Some(ref pass) = config.pass {
                let credentials =
                    Credentials::new(config.user.clone(), pass.expose_secret().to_owned());
                builder = builder.credentials(credentials);
            }
        }
    }

    Ok(builder.build())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_security_parse_accepts_valid_values() {
        assert_eq!(SmtpSecurity::parse("tls").unwrap(), SmtpSecurity::Tls);
        assert_eq!(SmtpSecurity::parse("ssl").unwrap(), SmtpSecurity::Tls);
        assert_eq!(SmtpSecurity::parse("STARTTLS").unwrap(), SmtpSecurity::Starttls);
        assert_eq!(SmtpSecurity::parse("plain").unwrap(), SmtpSecurity::Plain);
        assert_eq!(SmtpSecurity::parse("none").unwrap(), SmtpSecurity::Plain);
    }

    #[test]
    fn smtp_security_parse_rejects_invalid() {
        assert!(SmtpSecurity::parse("invalid").is_err());
    }

    #[test]
    fn build_message_text_only() {
        let comp = EmailComposition {
            from: "sender@example.com".to_owned(),
            to: vec!["recipient@example.com".to_owned()],
            cc: vec![],
            bcc: vec![],
            subject: "Test subject".to_owned(),
            body_text: Some("Hello, world!".to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
        };
        let msg = build_message(&comp).unwrap();
        let binding = msg.formatted();
        let formatted = String::from_utf8_lossy(&binding);
        assert!(formatted.contains("Subject: Test subject"));
        assert!(formatted.contains("Hello, world!"));
    }

    #[test]
    fn build_message_multipart() {
        let comp = EmailComposition {
            from: "sender@example.com".to_owned(),
            to: vec!["recipient@example.com".to_owned()],
            cc: vec![],
            bcc: vec![],
            subject: "Multipart test".to_owned(),
            body_text: Some("Plain text".to_owned()),
            body_html: Some("<p>HTML text</p>".to_owned()),
            reply_to: None,
            in_reply_to: None,
            references: None,
        };
        let msg = build_message(&comp).unwrap();
        let binding = msg.formatted();
        let formatted = String::from_utf8_lossy(&binding);
        assert!(formatted.contains("multipart/alternative"));
        assert!(formatted.contains("Plain text"));
        assert!(formatted.contains("<p>HTML text</p>"));
    }

    #[test]
    fn build_message_with_reply_headers() {
        let comp = EmailComposition {
            from: "sender@example.com".to_owned(),
            to: vec!["recipient@example.com".to_owned()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Original".to_owned(),
            body_text: Some("Reply body".to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: Some("<original@example.com>".to_owned()),
            references: Some("<original@example.com>".to_owned()),
        };
        let msg = build_message(&comp).unwrap();
        let binding = msg.formatted();
        let formatted = String::from_utf8_lossy(&binding);
        assert!(formatted.contains("In-Reply-To: <original@example.com>"));
        assert!(formatted.contains("References: <original@example.com>"));
    }

    #[test]
    fn build_message_rejects_invalid_from() {
        let comp = EmailComposition {
            from: "not-an-email".to_owned(),
            to: vec!["recipient@example.com".to_owned()],
            cc: vec![],
            bcc: vec![],
            subject: "Test".to_owned(),
            body_text: Some("body".to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
        };
        assert!(build_message(&comp).is_err());
    }

    #[test]
    fn build_message_multiple_recipients() {
        let comp = EmailComposition {
            from: "sender@example.com".to_owned(),
            to: vec![
                "a@example.com".to_owned(),
                "b@example.com".to_owned(),
            ],
            cc: vec!["c@example.com".to_owned()],
            bcc: vec!["d@example.com".to_owned()],
            subject: "Multi-recipient".to_owned(),
            body_text: Some("body".to_owned()),
            body_html: None,
            reply_to: None,
            in_reply_to: None,
            references: None,
        };
        let msg = build_message(&comp).unwrap();
        let binding = msg.formatted();
        let formatted = String::from_utf8_lossy(&binding);
        assert!(formatted.contains("a@example.com"));
        assert!(formatted.contains("b@example.com"));
        assert!(formatted.contains("Cc:"));
        // Bcc headers are intentionally stripped from formatted output per RFC 2822
    }
}
