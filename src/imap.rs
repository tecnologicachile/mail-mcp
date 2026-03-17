//! IMAP transport and session operations
//!
//! Provides timeout-bounded wrappers around `async-imap` operations. Supports
//! both TLS and plaintext connections, with timeouts derived from server config.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::types::{Fetch, Flag};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::ServerName;
use secrecy::ExposeSecret;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::config::{AccountConfig, AuthMethod, ServerConfig};
use crate::errors::{AppError, AppResult};
use crate::oauth2::{TokenManager, XOAuth2Authenticator};

/// Wrapper enum that supports both TLS and plaintext IMAP streams.
#[derive(Debug)]
pub enum ImapStream {
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
    Plain(TcpStream),
}

impl AsyncRead for ImapStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
            ImapStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
            ImapStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s).poll_flush(cx),
            ImapStream::Plain(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
            ImapStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl Unpin for ImapStream {}

/// Type alias for authenticated IMAP session supporting both TLS and plaintext.
pub type ImapSession = Session<ImapStream>;

/// Get socket timeout duration from server config
///
/// Helper to avoid repeatedly accessing the config field.
fn socket_timeout(server: &ServerConfig) -> Duration {
    Duration::from_millis(server.socket_timeout_ms)
}

/// Connect to IMAP server and authenticate
///
/// Performs full connection sequence with timeouts:
/// 1. TCP connect
/// 2. TLS handshake (if `secure: true`) or plaintext (if `secure: false`)
/// 3. Read IMAP greeting
/// 4. Authentication (LOGIN or XOAUTH2 depending on `auth_method`)
///
/// # Security
///
/// When `secure` is false, plaintext IMAP is used. This is intended for
/// local proxies (e.g., OAuth2 proxy on localhost) and should only be used
/// on trusted networks.
///
/// # Timeouts
///
/// - TCP connect: `connect_timeout_ms`
/// - TLS handshake: `greeting_timeout_ms` (TLS mode only)
/// - Greeting read: `greeting_timeout_ms`
/// - LOGIN/AUTHENTICATE: `greeting_timeout_ms`
///
/// # Errors
///
/// - `InvalidInput` if hostname is invalid for TLS SNI (TLS mode only)
/// - `Timeout` if any connection phase times out
/// - `AuthFailed` if authentication fails
/// - `Internal` for TCP, TLS, or greeting failures
pub async fn connect_authenticated(
    server: &ServerConfig,
    account: &AccountConfig,
    token_manager: Option<&TokenManager>,
) -> AppResult<ImapSession> {
    let connect_duration = Duration::from_millis(server.connect_timeout_ms);
    let greeting_duration = Duration::from_millis(server.greeting_timeout_ms);

    let tcp = timeout(
        connect_duration,
        TcpStream::connect((account.host.as_str(), account.port)),
    )
    .await
    .map_err(|_| AppError::Timeout("tcp connect timeout".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("tcp connect failed: {e}"))))?;

    let mut client = if account.secure {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let server_name = ServerName::try_from(account.host.clone())
            .map_err(|_| AppError::InvalidInput("invalid IMAP host for TLS SNI".to_owned()))?;
        let tls_stream = timeout(greeting_duration, connector.connect(server_name, tcp))
            .await
            .map_err(|_| AppError::Timeout("TLS handshake timeout".to_owned()))
            .and_then(|r| {
                r.map_err(|e| AppError::Internal(format!("TLS handshake failed: {e}")))
            })?;

        Client::new(ImapStream::Tls(tls_stream))
    } else {
        Client::new(ImapStream::Plain(tcp))
    };
    let greeting = timeout(greeting_duration, client.read_response())
        .await
        .map_err(|_| AppError::Timeout("IMAP greeting timeout".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("IMAP greeting failed: {e}"))))?;

    if greeting.is_none() {
        return Err(AppError::Internal(
            "IMAP server closed connection before greeting".to_owned(),
        ));
    }

    let session = match account.auth_method {
        AuthMethod::OAuth2 => {
            let tm = token_manager.ok_or_else(|| {
                AppError::Internal(format!(
                    "account '{}' requires OAuth2 but no token manager available",
                    account.account_id
                ))
            })?;
            let access_token = tm.get_access_token(&account.account_id).await?;
            let authenticator = XOAuth2Authenticator::new(&account.user, &access_token);

            timeout(
                greeting_duration,
                client.authenticate("XOAUTH2", authenticator),
            )
            .await
            .map_err(|_| AppError::Timeout("IMAP XOAUTH2 authenticate timeout".to_owned()))
            .and_then(|r| {
                r.map_err(|(e, _)| {
                    let msg = e.to_string();
                    AppError::AuthFailed(format!("XOAUTH2 authentication failed: {msg}"))
                })
            })?
        }
        AuthMethod::Password => {
            let pass = account
                .pass
                .as_ref()
                .ok_or_else(|| {
                    AppError::Internal(format!(
                        "account '{}' uses password auth but no password configured",
                        account.account_id
                    ))
                })?;
            timeout(
                greeting_duration,
                client.login(account.user.as_str(), pass.expose_secret()),
            )
            .await
            .map_err(|_| AppError::Timeout("IMAP login timeout".to_owned()))
            .and_then(|r| {
                r.map_err(|(e, _)| {
                    let msg = e.to_string();
                    if msg.to_ascii_lowercase().contains("auth") || msg.contains("LOGIN") {
                        AppError::AuthFailed(msg)
                    } else {
                        AppError::Internal(msg)
                    }
                })
            })?
        }
    };

    Ok(session)
}

/// Send NOOP to test connection liveness
///
/// Typically used after connection to verify the server is responsive.
pub async fn noop(server: &ServerConfig, session: &mut ImapSession) -> AppResult<()> {
    timeout(socket_timeout(server), session.noop())
        .await
        .map_err(|_| AppError::Timeout("NOOP timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("NOOP failed: {e}"))))
}

/// Query server capabilities
///
/// Returns the IMAP capabilities supported by the server. Used to detect
/// support for features like `MOVE`.
pub async fn capabilities(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<async_imap::types::Capabilities> {
    timeout(socket_timeout(server), session.capabilities())
        .await
        .map_err(|_| AppError::Timeout("CAPABILITY timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("CAPABILITY failed: {e}"))))
}

/// List all visible mailboxes/folders
///
/// Returns up to the server's full mailbox list. Caller should truncate if
/// necessary (e.g., to 200 items).
pub async fn list_all_mailboxes(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<Vec<async_imap::types::Name>> {
    let stream = timeout(socket_timeout(server), session.list(None, Some("*")))
        .await
        .map_err(|_| AppError::Timeout("LIST timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST failed: {e}"))))?;

    timeout(socket_timeout(server), stream.try_collect::<Vec<_>>())
        .await
        .map_err(|_| AppError::Timeout("LIST stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST stream failed: {e}"))))
}

/// Select mailbox in read-only mode
///
/// Uses `EXAMINE` command to fetch mailbox state without marking messages
/// as read. Returns the `UIDVALIDITY` for message ID stability.
pub async fn select_mailbox_readonly(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<u32> {
    let selected = timeout(socket_timeout(server), session.examine(mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("EXAMINE timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| AppError::NotFound(format!("cannot examine mailbox '{mailbox}': {e}")))
        })?;
    selected
        .uid_validity
        .ok_or_else(|| AppError::Internal("mailbox missing UIDVALIDITY".to_owned()))
}

/// Select mailbox in read-write mode
///
/// Uses `SELECT` command to enable write operations. Returns the `UIDVALIDITY`
/// for message ID stability.
pub async fn select_mailbox_readwrite(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<u32> {
    let selected = timeout(socket_timeout(server), session.select(mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("SELECT timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| AppError::NotFound(format!("cannot select mailbox '{mailbox}': {e}")))
        })?;
    selected
        .uid_validity
        .ok_or_else(|| AppError::Internal("mailbox missing UIDVALIDITY".to_owned()))
}

/// Fetch a single message with custom query
///
/// Runs a `UID FETCH` for a specific UID and returns the first result.
/// Used internally by other fetch functions.
///
/// # Errors
///
/// - `NotFound` if UID does not exist in mailbox
/// - `Timeout` or `Internal` for network/protocol errors
pub async fn fetch_one(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    query: &str,
) -> AppResult<Fetch> {
    let stream = timeout(
        socket_timeout(server),
        session.uid_fetch(uid.to_string(), query),
    )
    .await
    .map_err(|_| AppError::Timeout("UID FETCH timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid fetch failed: {e}"))))?;
    let fetches: Vec<Fetch> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID FETCH stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid fetch stream failed: {e}"))))?;

    if let Some(fetch) = fetches.into_iter().next() {
        return Ok(fetch);
    }

    let existence_query = format!("UID {uid}");
    match uid_search(server, session, existence_query.as_str()).await {
        Ok(matches) if matches.contains(&uid) => Err(AppError::Internal(format!(
            "UID FETCH returned no data for existing uid {uid}; possible server FETCH incompatibility (query: {query})"
        ))),
        Ok(_) => Err(AppError::NotFound(format!("message uid {uid} not found"))),
        Err(error) => Err(AppError::Internal(format!(
            "UID FETCH returned no data for uid {uid}; failed to verify existence: {error}"
        ))),
    }
}

/// Fetch full RFC822 message source
///
/// Returns raw bytes of the entire message.
pub async fn fetch_raw_message(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<Vec<u8>> {
    let fetch = fetch_one(server, session, uid, "RFC822").await?;
    let body = fetch
        .body()
        .ok_or_else(|| AppError::Internal("message has no RFC822 body".to_owned()))?;
    Ok(body.to_vec())
}

/// Fetch curated headers and flags
///
/// Returns standard headers (Date, From, To, CC, Subject) and message flags.
/// Uses `BODY.PEEK` to avoid marking the message as read.
pub async fn fetch_headers_and_flags(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<(Vec<u8>, Vec<String>)> {
    let fetch = fetch_one(
        server,
        session,
        uid,
        "(FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT)])",
    )
    .await?;
    let header_bytes = fetch
        .header()
        .or_else(|| fetch.body())
        .ok_or_else(|| AppError::Internal("message headers not available".to_owned()))?
        .to_vec();
    Ok((header_bytes, flags_to_strings(&fetch)))
}

/// Fetch message flags only
///
/// Returns IMAP flags (e.g., `\Seen`, `\Flagged`, `\Draft`) as strings.
pub async fn fetch_flags(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<Vec<String>> {
    let fetch = fetch_one(server, session, uid, "FLAGS").await?;
    Ok(flags_to_strings(&fetch))
}

/// Convert fetch flags to IMAP string representation
///
/// Helper to serialize flag types to IMAP wire-format strings.
pub fn flags_to_strings(fetch: &Fetch) -> Vec<String> {
    fetch.flags().map(flag_to_string).collect()
}

fn flag_to_string(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_owned(),
        Flag::Answered => "\\Answered".to_owned(),
        Flag::Flagged => "\\Flagged".to_owned(),
        Flag::Deleted => "\\Deleted".to_owned(),
        Flag::Draft => "\\Draft".to_owned(),
        Flag::Recent => "\\Recent".to_owned(),
        Flag::MayCreate => "\\*".to_owned(),
        Flag::Custom(value) => value.into_owned(),
    }
}

/// Search for messages matching query
///
/// Runs `UID SEARCH` and returns matching UIDs in descending order (newest
/// first). Callers typically limit the result set via pagination.
pub async fn uid_search(
    server: &ServerConfig,
    session: &mut ImapSession,
    query: &str,
) -> AppResult<Vec<u32>> {
    let set = timeout(socket_timeout(server), session.uid_search(query))
        .await
        .map_err(|_| AppError::Timeout("UID SEARCH timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid search failed: {e}"))))?;
    let mut uids: Vec<u32> = set.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(uids)
}

/// Store flags on a message
///
/// Runs `UID STORE` with a flag query string. Use `+FLAGS.SILENT` to add
/// flags or `-FLAGS.SILENT` to remove flags.
pub async fn uid_store(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    query: &str,
) -> AppResult<()> {
    let stream = timeout(
        socket_timeout(server),
        session.uid_store(uid.to_string(), query),
    )
    .await
    .map_err(|_| AppError::Timeout("UID STORE timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid store failed: {e}"))))?;
    let _: Vec<Fetch> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID STORE stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid store stream failed: {e}"))))?;
    Ok(())
}

/// Copy message to another mailbox
///
/// Runs `UID COPY` to duplicate the message. Returns the new UID on success
/// (currently not captured due to protocol limitations).
pub async fn uid_copy(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    mailbox: &str,
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.uid_copy(uid.to_string(), mailbox),
    )
    .await
    .map_err(|_| AppError::Timeout("UID COPY timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID COPY failed: {e}"))))
}

/// Move message to another mailbox
///
/// Runs `UID MOVE` if server supports it (RFC 6851). More efficient than
/// copy+delete as it's atomic.
pub async fn uid_move(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    mailbox: &str,
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.uid_mv(uid.to_string(), mailbox),
    )
    .await
    .map_err(|_| AppError::Timeout("UID MOVE timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID MOVE failed: {e}"))))
}

/// Permanently delete a message
///
/// Runs `UID EXPUNGE` to immediately remove the message marked as `\Deleted`.
pub async fn uid_expunge(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<()> {
    let stream = timeout(socket_timeout(server), session.uid_expunge(uid.to_string()))
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID EXPUNGE failed: {e}"))))?;
    let _: Vec<u32> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE stream timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("UID EXPUNGE stream failed: {e}")))
        })?;
    Ok(())
}

/// Append raw RFC822 message to mailbox
///
/// Used for cross-account copy operations. Does not return the new UID
/// directly (would require `UIDPLUS` capability).
pub async fn append(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
    content: &[u8],
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.append(mailbox, None, None, content),
    )
    .await
    .map_err(|_| AppError::Timeout("APPEND timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("APPEND failed: {e}"))))
}

/// Create a new mailbox/folder
pub async fn create_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.create(mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("CREATE timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| {
                AppError::Internal(format!("CREATE failed for mailbox '{mailbox}': {e}"))
            })
        })
}

/// Delete a mailbox/folder
pub async fn delete_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.delete(mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("DELETE timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| {
                AppError::Internal(format!("DELETE failed for mailbox '{mailbox}': {e}"))
            })
        })
}

/// Rename a mailbox/folder
pub async fn rename_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    from: &str,
    to: &str,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.rename(from, to))
        .await
        .map_err(|_| AppError::Timeout(format!("RENAME timed out for mailbox '{from}'")))
        .and_then(|r| {
            r.map_err(|e| {
                AppError::Internal(format!("RENAME failed for mailbox '{from}' -> '{to}': {e}"))
            })
        })
}

/// Get mailbox status (message counts) without selecting it
///
/// Returns (messages, unseen, recent) counts.
pub async fn mailbox_status(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<(u32, u32, u32)> {
    let mbox = timeout(
        socket_timeout(server),
        session.status(mailbox, "(MESSAGES UNSEEN RECENT)"),
    )
    .await
    .map_err(|_| AppError::Timeout(format!("STATUS timed out for mailbox '{mailbox}'")))
    .and_then(|r| {
        r.map_err(|e| {
            AppError::Internal(format!("STATUS failed for mailbox '{mailbox}': {e}"))
        })
    })?;

    Ok((mbox.exists, mbox.unseen.unwrap_or(0) as u32, mbox.recent))
}

/// Store flags on multiple messages at once
///
/// `uid_set` is a comma-separated list of UIDs (e.g., "1,2,3" or "1:100").
pub async fn uid_store_bulk(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid_set: &str,
    query: &str,
) -> AppResult<()> {
    let stream = timeout(socket_timeout(server), session.uid_store(uid_set, query))
        .await
        .map_err(|_| AppError::Timeout("UID STORE (bulk) timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid store (bulk) failed: {e}"))))?;
    let _: Vec<Fetch> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID STORE (bulk) stream timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("uid store (bulk) stream failed: {e}")))
        })?;
    Ok(())
}

/// Move multiple messages to another mailbox at once
///
/// `uid_set` is a comma-separated list of UIDs.
pub async fn uid_move_bulk(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid_set: &str,
    mailbox: &str,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.uid_mv(uid_set, mailbox))
        .await
        .map_err(|_| AppError::Timeout("UID MOVE (bulk) timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID MOVE (bulk) failed: {e}"))))
}

/// Copy multiple messages to another mailbox at once
///
/// `uid_set` is a comma-separated list of UIDs.
pub async fn uid_copy_bulk(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid_set: &str,
    mailbox: &str,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.uid_copy(uid_set, mailbox))
        .await
        .map_err(|_| AppError::Timeout("UID COPY (bulk) timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID COPY (bulk) failed: {e}"))))
}

/// Expunge multiple messages at once
///
/// `uid_set` is a comma-separated list of UIDs that should already have \Deleted flag.
pub async fn uid_expunge_bulk(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid_set: &str,
) -> AppResult<()> {
    let stream = timeout(socket_timeout(server), session.uid_expunge(uid_set))
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE (bulk) timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("UID EXPUNGE (bulk) failed: {e}")))
        })?;
    let _: Vec<u32> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE (bulk) stream timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("UID EXPUNGE (bulk) stream failed: {e}")))
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use async_imap::Client;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
    use secrecy::{ExposeSecret, SecretString};
    use tokio::net::TcpStream;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use tokio_rustls::TlsConnector;

    use super::{
        append, fetch_flags, fetch_raw_message, list_all_mailboxes, noop, select_mailbox_readonly,
        select_mailbox_readwrite, socket_timeout, uid_copy, uid_expunge, uid_move, uid_search,
        uid_store,
    };
    use crate::config::{AccountConfig, ServerConfig};

    /// Holds connection details for a GreenMail test server instance.
    #[derive(Debug, Clone)]
    struct GreenmailEndpoints {
        host: String,
        smtp_port: u16,
        imap_port: u16,
        user: String,
        pass: String,
    }

    /// Parses a test port from the environment, falling back to a default.
    fn parse_test_port(key: &str, default: u16) -> u16 {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default)
    }

    /// Returns GreenmailEndpoints using environment variables or defaults.
    fn greenmail_endpoints() -> GreenmailEndpoints {
        GreenmailEndpoints {
            host: std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "localhost".to_owned()),
            smtp_port: parse_test_port("GREENMAIL_SMTP_PORT", 3025),
            imap_port: parse_test_port("GREENMAIL_IMAP_PORT", 3143),
            user: std::env::var("GREENMAIL_USER").unwrap_or_else(|_| "test@localhost".to_owned()),
            pass: std::env::var("GREENMAIL_PASS").unwrap_or_else(|_| "test".to_owned()),
        }
    }

    /// Constructs a ServerConfig for GreenMail integration tests.
    fn greenmail_test_config(endpoints: &GreenmailEndpoints) -> ServerConfig {
        let account = AccountConfig {
            account_id: "default".to_owned(),
            host: endpoints.host.clone(),
            port: endpoints.imap_port,
            secure: true,
            user: endpoints.user.clone(),
            pass: Some(SecretString::new(endpoints.pass.clone().into())),
            auth_method: crate::config::AuthMethod::Password,
        };

        let mut accounts = BTreeMap::new();
        accounts.insert(account.account_id.clone(), account);

        ServerConfig {
            accounts,
            oauth2_accounts: std::collections::HashMap::new(),
            smtp_accounts: std::collections::HashMap::new(),
            smtp_write_enabled: false,
            smtp_save_sent: false,
            smtp_timeout_ms: 30_000,
            write_enabled: true,
            connect_timeout_ms: 5_000,
            greeting_timeout_ms: 5_000,
            socket_timeout_ms: 15_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 128,
        }
    }

    /// Disables certificate verification for test TLS connections.
    #[derive(Debug)]
    struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, RustlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
            ]
        }
    }

    /// Connects and authenticates to the GreenMail IMAP server for integration tests.
    async fn connect_authenticated_greenmail(
        config: &ServerConfig,
    ) -> Result<super::ImapSession, String> {
        let account = config
            .get_account("default")
            .map_err(|e| format!("missing default account: {e}"))?;
        let connect_duration = Duration::from_millis(config.connect_timeout_ms);
        let greeting_duration = Duration::from_millis(config.greeting_timeout_ms);

        let tcp = timeout(
            connect_duration,
            TcpStream::connect((account.host.as_str(), account.port)),
        )
        .await
        .map_err(|_| "tcp connect timeout".to_owned())
        .and_then(|r| r.map_err(|e| format!("tcp connect failed: {e}")))?;

        let tls_config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let server_name = ServerName::try_from(account.host.clone())
            .map_err(|_| "invalid IMAP host for TLS SNI".to_owned())?;
        let tls_stream = timeout(greeting_duration, connector.connect(server_name, tcp))
            .await
            .map_err(|_| "TLS handshake timeout".to_owned())
            .and_then(|r| r.map_err(|e| format!("TLS handshake failed: {e}")))?;

        let mut client = Client::new(super::ImapStream::Tls(tls_stream));
        let greeting = timeout(greeting_duration, client.read_response())
            .await
            .map_err(|_| "IMAP greeting timeout".to_owned())
            .and_then(|r| r.map_err(|e| format!("IMAP greeting failed: {e}")))?;
        if greeting.is_none() {
            return Err("IMAP server closed connection before greeting".to_owned());
        }

        let login = timeout(
            greeting_duration,
            client.login(account.user.as_str(), account.pass.as_ref().unwrap().expose_secret()),
        )
        .await
        .map_err(|_| "IMAP login timeout".to_owned())?;

        login.map_err(|(e, _)| format!("IMAP login failed: {e}"))
    }

    /// Attempts to connect to the GreenMail IMAP port to verify server availability.
    async fn probe_greenmail_imap(endpoints: &GreenmailEndpoints) -> Result<(), String> {
        timeout(
            Duration::from_secs(1),
            TcpStream::connect((endpoints.host.as_str(), endpoints.imap_port)),
        )
        .await
        .map_err(|_| "tcp connect timeout".to_owned())
        .and_then(|r| r.map_err(|e| format!("tcp connect failed: {e}")))?;

        Ok(())
    }

    /// Waits until the GreenMail IMAP server is reachable and login succeeds.
    ///
    /// Retries for up to 60 seconds, returning an error if the server does not become ready.
    async fn wait_until_login_works(
        config: &ServerConfig,
        endpoints: &GreenmailEndpoints,
    ) -> Result<(), String> {
        let mut last_probe_err = String::new();
        let mut last_login_err = String::new();

        for _ in 0..60 {
            match probe_greenmail_imap(endpoints).await {
                Ok(()) => match connect_authenticated_greenmail(config).await {
                    Ok(_) => return Ok(()),
                    Err(e) => last_login_err = e,
                },
                Err(e) => last_probe_err = e,
            }

            sleep(Duration::from_secs(1)).await;
        }

        Err(format!(
            "greenmail unreachable at {}:{} (SMTP={}) after 60s; probe_error='{}'; login_error='{}'",
            endpoints.host,
            endpoints.imap_port,
            endpoints.smtp_port,
            if last_probe_err.is_empty() {
                "none"
            } else {
                last_probe_err.as_str()
            },
            if last_login_err.is_empty() {
                "none"
            } else {
                last_login_err.as_str()
            }
        ))
    }

    /// Creates a mailbox if it does not already exist.
    ///
    /// Ignores errors if the mailbox already exists.
    async fn create_mailbox_if_missing(
        config: &ServerConfig,
        session: &mut super::ImapSession,
        mailbox: &str,
    ) -> Result<(), String> {
        let result = timeout(socket_timeout(config), session.create(mailbox))
            .await
            .map_err(|_| format!("CREATE timed out for mailbox '{mailbox}'"))?;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("exists") || msg.contains("already") {
                    Ok(())
                } else {
                    Err(format!("CREATE failed for mailbox '{mailbox}': {e}"))
                }
            }
        }
    }

    /// Connects to a GreenMail IMAP server and verifies basic read operations.
    ///
    /// This test checks that the server is reachable, login works, and
    /// that expected mailboxes and seeded messages are present and accessible.
    /// It also verifies fetching a known message by subject.
    #[tokio::test]
    #[ignore = "requires running GreenMail IMAP server"]
    async fn greenmail_imap_smoke_test() {
        let endpoints = greenmail_endpoints();
        let config = greenmail_test_config(&endpoints);
        wait_until_login_works(&config, &endpoints)
            .await
            .expect("greenmail did not become ready");

        let mut session = connect_authenticated_greenmail(&config)
            .await
            .expect("imap login should work");

        noop(&config, &mut session)
            .await
            .expect("NOOP should succeed");

        let mailboxes = list_all_mailboxes(&config, &mut session)
            .await
            .expect("LIST should succeed");
        let mailbox_names: Vec<String> = mailboxes
            .into_iter()
            .map(|mailbox| mailbox.name().to_owned())
            .collect();

        for required_mailbox in ["INBOX", "Archive", "Spam", "Newsletters"] {
            assert!(
                mailbox_names
                    .iter()
                    .any(|name| name.contains(required_mailbox)),
                "expected seeded mailbox {required_mailbox:?}, got {mailbox_names:?}"
            );
        }

        assert!(
            mailbox_names.len() >= 5,
            "expected at least five seeded mailboxes, got {mailbox_names:?}"
        );

        let uidvalidity = select_mailbox_readonly(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable");
        assert!(uidvalidity > 0);

        let inbox_uids = uid_search(&config, &mut session, "ALL")
            .await
            .expect("UID SEARCH ALL should succeed");
        assert!(
            inbox_uids.len() >= 4,
            "expected seeded INBOX corpus with at least 4 messages, got {}",
            inbox_uids.len()
        );

        let unseen_uids = uid_search(&config, &mut session, "UNSEEN")
            .await
            .expect("UID SEARCH UNSEEN should succeed");
        assert!(
            !unseen_uids.is_empty(),
            "expected at least one unread seeded INBOX message"
        );

        let roadmap_uids = uid_search(&config, &mut session, "SUBJECT \"Roadmap Review\"")
            .await
            .expect("UID SEARCH SUBJECT should find seeded roadmap message");
        assert_eq!(
            roadmap_uids.len(),
            1,
            "expected exactly one seeded Roadmap Review message"
        );

        let archive_mailbox = mailbox_names
            .iter()
            .find(|name| name.contains("Archive") && name.contains("2025"))
            .cloned()
            .expect("expected archive mailbox in seeded dataset");

        select_mailbox_readonly(&config, &mut session, &archive_mailbox)
            .await
            .expect("archive mailbox should be selectable");
        let archive_uids = uid_search(&config, &mut session, "ALL")
            .await
            .expect("UID SEARCH in archive should succeed");
        assert!(
            archive_uids.len() >= 2,
            "expected at least two seeded archive messages, got {}",
            archive_uids.len()
        );

        select_mailbox_readonly(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be re-selectable");

        let raw = fetch_raw_message(&config, &mut session, roadmap_uids[0])
            .await
            .expect("fetching seeded roadmap message should succeed");
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(
            raw_text.contains("Subject: Roadmap Review"),
            "fetched seeded message should contain expected subject"
        );
    }

    /// Exercises IMAP write-path operations against GreenMail.
    ///
    /// This test covers appending a message, flag updates, mailbox creation,
    /// copying, moving, deleting, and expunging messages. It ensures that
    /// all write operations succeed and that mailbox/message state is correct
    /// after each operation.
    #[tokio::test]
    #[ignore = "requires running GreenMail IMAP server"]
    async fn greenmail_imap_write_paths_test() {
        let endpoints = greenmail_endpoints();
        let config = greenmail_test_config(&endpoints);
        wait_until_login_works(&config, &endpoints)
            .await
            .expect("greenmail did not become ready");

        let mut session = connect_authenticated_greenmail(&config)
            .await
            .expect("imap login should work");

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic enough for test")
            .as_nanos();
        let subject = format!("Greenmail write path {nonce}");
        let destination_mailbox = format!("INBOX.greenmail_{nonce}");

        let message = format!(
            "From: sender@example.com\r\nTo: user@example.com\r\nSubject: {subject}\r\n\r\nWrite-path body\r\n"
        );
        append(&config, &mut session, "INBOX", message.as_bytes())
            .await
            .expect("APPEND should succeed");

        select_mailbox_readwrite(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable read-write");
        let source_uids = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH by SUBJECT should succeed");
        assert_eq!(source_uids.len(), 1, "expected exactly one source message");
        let source_uid = source_uids[0];

        uid_store(&config, &mut session, source_uid, "+FLAGS.SILENT (\\Seen)")
            .await
            .expect("UID STORE should set \\Seen flag");
        let flags = fetch_flags(&config, &mut session, source_uid)
            .await
            .expect("fetch_flags should succeed after UID STORE");
        assert!(
            flags.iter().any(|f| f == "\\Seen"),
            "expected \\Seen in flags after UID STORE"
        );

        create_mailbox_if_missing(&config, &mut session, &destination_mailbox)
            .await
            .expect("CREATE destination mailbox should succeed");

        uid_copy(&config, &mut session, source_uid, &destination_mailbox)
            .await
            .expect("UID COPY should succeed");

        select_mailbox_readonly(&config, &mut session, &destination_mailbox)
            .await
            .expect("destination mailbox should be selectable");
        let copied_uids = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH in destination should succeed");
        assert_eq!(
            copied_uids.len(),
            1,
            "expected one copied message in destination mailbox"
        );

        select_mailbox_readwrite(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable read-write for move");
        uid_move(&config, &mut session, source_uid, &destination_mailbox)
            .await
            .expect("UID MOVE should succeed");

        select_mailbox_readonly(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable readonly after move");
        let remaining_in_source =
            uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
                .await
                .expect("UID SEARCH in source after move should succeed");
        assert!(
            remaining_in_source.is_empty(),
            "expected no source messages after UID MOVE"
        );

        select_mailbox_readwrite(&config, &mut session, &destination_mailbox)
            .await
            .expect("destination mailbox should be selectable read-write");
        let moved_and_copied = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH in destination after move should succeed");
        assert_eq!(
            moved_and_copied.len(),
            2,
            "expected two messages in destination after copy+move"
        );

        let uid_to_delete = moved_and_copied[0];
        uid_store(
            &config,
            &mut session,
            uid_to_delete,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        .expect("UID STORE should set \\Deleted flag");
        uid_expunge(&config, &mut session, uid_to_delete)
            .await
            .expect("UID EXPUNGE should remove deleted message");

        let after_delete = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH after delete should succeed");
        assert_eq!(
            after_delete.len(),
            1,
            "expected one message remaining after delete"
        );

        let raw = fetch_raw_message(&config, &mut session, after_delete[0])
            .await
            .expect("remaining message should be fetchable");
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(
            raw_text.contains(&subject),
            "remaining message should contain test subject"
        );
    }
}
