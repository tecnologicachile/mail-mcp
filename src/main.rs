//! mail-mcp: email MCP server over stdio or streamable HTTP.
//!
//! # Architecture
//!
//! - [`main`]: Process entry point, config selection, and MCP serving
//! - [`cli`]: Config and account commands
//! - [`config_file`]: TOML config load and save
//! - [`http`]: Optional streamable HTTP transport
//! - [`config`]: Account and server settings
//! - [`errors`]: Application error model with MCP error mapping
//! - [`imap`]: IMAP transport and session operations
//! - [`server`]: MCP tool handlers
//! - [`models`]: Input and output DTOs
//! - [`mime`]: Message parsing and sanitization
//! - [`message_id`]: Stable message ID parse and encode
//! - [`pagination`]: Cursor storage with TTL and eviction
//!
//! With no subcommand the process serves MCP. Stdio is the default and does
//! not prompt or write ordinary stdout. `mail-mcp config` and
//! `mail-mcp accounts` manage the TOML config file.
//!
//! ```text
//! mail-mcp config init
//! mail-mcp accounts add
//! mail-mcp
//! ```

mod cli;
mod cli_accounts;
mod config;
mod config_file;
mod errors;
mod ews;
mod graph;
mod http;
mod imap;
mod message_id;
mod mime;
mod models;
mod oauth2;
mod pagination;
mod server;
mod smtp;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

/// Starts the MCP server, or runs a config or account command.
#[tokio::main]
async fn main() {
    // Install the rustls CryptoProvider globally so that both tokio-rustls (IMAP)
    // and lettre (SMTP) use the same provider without conflicts.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = cli::Cli::parse();
    let result = match cli.command {
        Some(command) => cli::execute(cli.config, command).await,
        None => serve(cli.config).await,
    };
    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn serve(config: Option<std::path::PathBuf>) -> Result<(), cli::CliFailure> {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .with_writer(std::io::stderr)
        .init();

    let runtime = cli::prepare_runtime(config)?;
    if !runtime.overrides.is_empty() {
        tracing::info!("environment overrides: {}", runtime.overrides.join(", "));
    }
    tracing::info!("starting MCP server transport={:?}", runtime.transport);
    let update_notice = check_for_updates().await;
    let server = server::MailImapServer::new(runtime.config, update_notice);
    match runtime.http {
        Some(settings) => http::serve(server, settings)
            .await
            .map_err(|err| cli::CliFailure::new(cli::public_text(&err.to_string())))?,
        None => {
            let service = server
                .serve(stdio())
                .await
                .map_err(|err| cli::CliFailure::new(cli::public_text(&err.to_string())))?;
            service
                .waiting()
                .await
                .map_err(|err| cli::CliFailure::new(cli::public_text(&err.to_string())))?;
        }
    }
    Ok(())
}

/// Check GitHub for newer releases. Returns a notice string if an update is available.
/// Times out after 2 seconds to avoid blocking startup.
async fn check_for_updates() -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    let url = "https://api.github.com/repos/tecnologicachile/mail-mcp/releases/latest";

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .header("User-Agent", "mail-mcp")
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let latest_tag = body["tag_name"].as_str()?;
        let latest = latest_tag.trim_start_matches('v');
        if latest != current && latest > current {
            Some(format!(
                "\n\nUpdate available: v{current} -> {latest_tag}. \
                 See https://github.com/tecnologicachile/mail-mcp/releases/tag/{latest_tag}"
            ))
        } else {
            None
        }
    })
    .await;

    match result {
        Ok(notice) => {
            if let Some(ref msg) = notice {
                tracing::info!("update check: {msg}");
            } else {
                tracing::debug!("update check: running latest version v{current}");
            }
            notice
        }
        Err(_) => {
            tracing::debug!("update check: timed out (2s)");
            None
        }
    }
}

/// Build the log filter from `RUST_LOG`.
///
/// At `info`, rmcp logs every JSON-RPC response in full, which includes
/// message subjects and bodies. When `RUST_LOG` sets a global level of
/// `info` or more verbose without naming rmcp, rmcp is capped at `warn`.
fn log_filter(rust_log: Option<&str>) -> EnvFilter {
    let filter = EnvFilter::new(rust_log.unwrap_or_default());
    if !caps_rmcp(rust_log) {
        return filter;
    }
    match "rmcp=warn".parse() {
        Ok(directive) => filter.add_directive(directive),
        Err(_) => filter,
    }
}

fn caps_rmcp(rust_log: Option<&str>) -> bool {
    let Some(value) = rust_log else {
        return false;
    };
    let directives: Vec<&str> = value.split(',').map(str::trim).collect();
    if directives.iter().any(|d| d.starts_with("rmcp")) {
        return false;
    }
    directives
        .iter()
        .any(|d| matches!(d.to_ascii_lowercase().as_str(), "info" | "debug" | "trace"))
}

#[cfg(test)]
mod tests {
    use super::{caps_rmcp, log_filter};

    #[test]
    fn rmcp_is_capped_only_for_verbose_global_levels() {
        for value in [
            None,
            Some("error"),
            Some("warn"),
            Some("off"),
            Some("mail_mcp=debug"),
        ] {
            assert!(!caps_rmcp(value), "{value:?}");
            assert!(!log_filter(value).to_string().contains("rmcp=warn"));
        }
        for value in ["info", "debug", "INFO,mail_mcp=debug", "trace"] {
            assert!(caps_rmcp(Some(value)), "{value}");
            assert!(log_filter(Some(value)).to_string().contains("rmcp=warn"));
        }
        for value in ["info,rmcp=info", "debug, rmcp::transport=debug"] {
            assert!(!caps_rmcp(Some(value)), "{value}");
        }
    }
}
