//! Streamable HTTP transport (opt-in)
//!
//! Serves the same [`MailImapServer`] over MCP streamable HTTP instead of
//! stdio, for deployments where the server runs on another host and is reached
//! through an MCP gateway. Enabled with `MAIL_MCP_TRANSPORT=http`.
//!
//! The endpoint has **no authentication of its own**. It binds to loopback by
//! default; bind it anywhere else only behind something that authenticates
//! callers (an MCP gateway, a reverse proxy with auth, a firewall that admits
//! only the gateway).
//!
//! The service runs in stateless mode: every request gets a clone of one
//! server instance, so the shared state behind its `Arc`s (the pagination
//! cursor store, OAuth2 token managers) is kept across requests.

use std::error::Error;
use std::net::SocketAddr;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::server::MailImapServer;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8000;
const DEFAULT_PATH: &str = "/mcp";

/// Transport selected by `MAIL_MCP_TRANSPORT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
}

impl Transport {
    /// Read `MAIL_MCP_TRANSPORT` from the settings map (`stdio`, the default, or `http`).
    pub fn from_settings(settings: &crate::config_file::Settings) -> Result<Self, String> {
        match nonempty(settings, "MAIL_MCP_TRANSPORT") {
            None => Ok(Self::Stdio),
            Some(value) => parse_transport(value),
        }
    }
}

fn parse_transport(raw: &str) -> Result<Transport, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "stdio" => Ok(Transport::Stdio),
        "http" | "streamable-http" => Ok(Transport::Http),
        _ => Err("MAIL_MCP_TRANSPORT is not supported (use 'stdio' or 'http')".to_owned()),
    }
}

/// Listen address and endpoint path for the HTTP transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    pub addr: SocketAddr,
    pub path: String,
}

impl HttpSettings {
    /// Read `MAIL_MCP_HTTP_HOST` (default `127.0.0.1`), `MAIL_MCP_HTTP_PORT`
    /// (default `8000`) and `MAIL_MCP_HTTP_PATH` (default `/mcp`) from the
    /// settings map.
    pub fn from_settings(settings: &crate::config_file::Settings) -> Result<Self, String> {
        parse_settings(
            nonempty(settings, "MAIL_MCP_HTTP_HOST"),
            nonempty(settings, "MAIL_MCP_HTTP_PORT"),
            nonempty(settings, "MAIL_MCP_HTTP_PATH"),
        )
    }
}

fn nonempty<'a>(settings: &'a crate::config_file::Settings, key: &str) -> Option<&'a str> {
    settings
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn parse_settings(
    host: Option<&str>,
    port: Option<&str>,
    path: Option<&str>,
) -> Result<HttpSettings, String> {
    let host = host.unwrap_or(DEFAULT_HOST).trim();
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => p
            .trim()
            .parse::<u16>()
            .map_err(|_| "MAIL_MCP_HTTP_PORT is not a valid port".to_owned())?,
    };
    // Bracket bare IPv6 literals so "::" and "::1" parse as socket addresses.
    let joined = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addr = joined
        .parse::<SocketAddr>()
        .map_err(|_| "MAIL_MCP_HTTP_HOST must be an IP address".to_owned())?;
    let path = path.unwrap_or(DEFAULT_PATH).trim();
    if !path.starts_with('/') {
        return Err("MAIL_MCP_HTTP_PATH must start with '/'".to_owned());
    }
    Ok(HttpSettings {
        addr,
        path: path.to_string(),
    })
}

/// Serve `server` over streamable HTTP until SIGINT or SIGTERM.
pub async fn serve(server: MailImapServer, settings: HttpSettings) -> Result<(), Box<dyn Error>> {
    let shutdown = CancellationToken::new();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig {
            stateful_mode: false,
            cancellation_token: shutdown.clone(),
            ..Default::default()
        },
    );
    let router = axum::Router::new().route_service(&settings.path, service);
    let listener = tokio::net::TcpListener::bind(settings.addr).await?;
    tracing::info!(
        "listening on http://{}{} (no built-in auth; keep it behind a gateway)",
        settings.addr,
        settings.path
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_defaults_to_stdio_and_accepts_http() {
        assert_eq!(parse_transport(""), Ok(Transport::Stdio));
        assert_eq!(parse_transport("STDIO"), Ok(Transport::Stdio));
        assert_eq!(parse_transport("http"), Ok(Transport::Http));
        assert_eq!(parse_transport("streamable-http"), Ok(Transport::Http));
        assert!(parse_transport("sse").is_err());
    }

    #[test]
    fn settings_default_to_loopback() {
        let s = parse_settings(None, None, None).unwrap();
        assert_eq!(s.addr, "127.0.0.1:8000".parse().unwrap());
        assert_eq!(s.path, "/mcp");
    }

    #[test]
    fn settings_parse_overrides_and_ipv6() {
        let s = parse_settings(Some("0.0.0.0"), Some("8010"), Some("/")).unwrap();
        assert_eq!(s.addr, "0.0.0.0:8010".parse().unwrap());
        assert_eq!(s.path, "/");
        let v6 = parse_settings(Some("::"), Some("9000"), None).unwrap();
        assert_eq!(v6.addr, "[::]:9000".parse().unwrap());
    }

    #[test]
    fn settings_reject_bad_values() {
        assert!(parse_settings(None, Some("http"), None).is_err());
        assert!(parse_settings(Some("mail.example.com"), None, None).is_err());
        assert!(parse_settings(None, None, Some("mcp")).is_err());
    }

    #[test]
    fn from_settings_reads_transport_and_http_keys() {
        let mut settings = crate::config_file::Settings::new();
        assert_eq!(
            Transport::from_settings(&settings).unwrap(),
            Transport::Stdio
        );
        settings.insert("MAIL_MCP_TRANSPORT".to_owned(), "http".to_owned());
        settings.insert("MAIL_MCP_HTTP_HOST".to_owned(), "::1".to_owned());
        settings.insert("MAIL_MCP_HTTP_PORT".to_owned(), "9000".to_owned());
        settings.insert("MAIL_MCP_HTTP_PATH".to_owned(), "/mail".to_owned());
        assert_eq!(
            Transport::from_settings(&settings).unwrap(),
            Transport::Http
        );
        let http = HttpSettings::from_settings(&settings).unwrap();
        assert_eq!(http.addr, "[::1]:9000".parse().unwrap());
        assert_eq!(http.path, "/mail");
        let secret = "super-secret-token-value";
        settings.insert("MAIL_MCP_HTTP_PORT".to_owned(), secret.to_owned());
        let error = HttpSettings::from_settings(&settings).unwrap_err();
        assert!(!error.contains(secret));
    }
}
