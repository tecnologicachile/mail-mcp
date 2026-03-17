//! mail-imap-mcp-rs: Secure IMAP MCP server over stdio
//!
//! This server provides read/write access to IMAP mailboxes via the Model
//! Context Protocol (MCP) over stdio. It features cursor-based pagination,
//! TLS-only connections, and security-first design.
//!
//! # Architecture
//!
//! - [`main`]: Process entry point with env loading and stdio serving
//! - [`config`]: Environment-driven configuration for accounts and server settings
//! - [`errors`]: Application error model with MCP error mapping
//! - [`imap`]: IMAP transport/session operations with timeout wrappers
//! - [`server`]: MCP tool handlers with validation and business orchestration
//! - [`models`]: Input/output DTOs and schema-bearing types
//! - [`mime`]: Message parsing, header/body extraction, and sanitization
//! - [`message_id`]: Stable, opaque message ID parse/encode logic
//! - [`pagination`]: Cursor storage with TTL and eviction behavior

mod config;
mod errors;
mod imap;
mod message_id;
mod mime;
mod models;
mod oauth2;
mod pagination;
mod server;
mod smtp;

use std::collections::BTreeMap;
use std::io::{self, Write};

use config::ServerConfig;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

/// Application entry point
///
/// Initializes tracing from environment, loads config, and serves the MCP
/// server over stdio. This process expects to be spawned by an MCP client
/// via `stdio` transport.
///
/// # Environment Variables
///
/// See [`ServerConfig::load_from_env`] for full configuration options.
///
/// # Example
///
/// ```no_run
/// MAIL_IMAP_DEFAULT_HOST=imap.example.com \
/// MAIL_IMAP_DEFAULT_USER=user@example.com \
/// MAIL_IMAP_DEFAULT_PASS=secret \
/// cargo run
/// ```
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    if should_print_help(std::env::args().skip(1)) {
        print_help_output()?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("starting MCP server transport=Stdio");
    let config = ServerConfig::load_from_env()?;
    let service = server::MailImapServer::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn should_print_help<I>(args: I) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    args.into_iter().any(|arg| {
        let arg = arg.as_ref();
        arg == "--help" || arg == "-h"
    })
}

fn print_help_output() -> io::Result<()> {
    let env_map: BTreeMap<String, String> = std::env::vars().collect();
    let output = build_help_output(&env_map);
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()
}

fn build_help_output(env_map: &BTreeMap<String, String>) -> String {
    let account_sections = discover_account_sections(env_map);
    let mut out = String::new();

    out.push_str("mail-imap-mcp-rs\n");
    out.push_str("Secure IMAP MCP server over stdio\n\n");

    out.push_str("Usage:\n");
    out.push_str("  mail-imap-mcp-rs\n");
    out.push_str("  mail-imap-mcp-rs --help\n\n");

    out.push_str("IMAP environment setup\n");
    out.push_str("  Required per account section MAIL_IMAP_<ACCOUNT>_:\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_HOST\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_USER\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_PASS\n");
    out.push_str("  Optional per account section:\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_PORT (default: 993)\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_SECURE (default: true)\n");
    out.push_str(
        "  If no account section is discovered from environment, DEFAULT is used by convention.\n\n",
    );

    out.push_str("Discovered account sections (from current environment)\n");
    if account_sections.is_empty() {
        out.push_str("  (none discovered)\n");
    } else {
        for section in &account_sections {
            out.push_str(&format!("  [{}]\n", section));
            for suffix in ["HOST", "USER", "PASS", "PORT", "SECURE"] {
                let key = format!("MAIL_IMAP_{}_{}", section, suffix);
                let value = env_map.get(&key).map(String::as_str);
                out.push_str(&format!("    {}={}\n", key, redact_value(&key, value)));
            }
        }
    }
    out.push('\n');

    // OAuth2 section
    let oauth2_sections = discover_oauth2_sections(env_map);
    out.push_str("OAuth2 environment setup (optional, per account)\n");
    out.push_str("  MAIL_OAUTH2_<ACCOUNT>_PROVIDER    (google | microsoft)\n");
    out.push_str("  MAIL_OAUTH2_<ACCOUNT>_CLIENT_ID\n");
    out.push_str("  MAIL_OAUTH2_<ACCOUNT>_CLIENT_SECRET\n");
    out.push_str("  MAIL_OAUTH2_<ACCOUNT>_REFRESH_TOKEN\n");
    out.push_str(
        "  When set, IMAP PASS becomes optional and XOAUTH2 is used for authentication.\n\n",
    );

    out.push_str("Discovered OAuth2 sections (from current environment)\n");
    if oauth2_sections.is_empty() {
        out.push_str("  (none discovered)\n");
    } else {
        for section in &oauth2_sections {
            out.push_str(&format!("  [{}]\n", section));
            for suffix in ["PROVIDER", "CLIENT_ID", "CLIENT_SECRET", "REFRESH_TOKEN"] {
                let key = format!("MAIL_OAUTH2_{}_{}", section, suffix);
                let value = env_map.get(&key).map(String::as_str);
                out.push_str(&format!("    {}={}\n", key, redact_value(&key, value)));
            }
        }
    }
    out.push('\n');

    // SMTP section
    let smtp_sections = discover_smtp_sections(env_map);
    out.push_str("SMTP environment setup (optional, per account)\n");
    out.push_str("  MAIL_SMTP_<ACCOUNT>_HOST\n");
    out.push_str("  MAIL_SMTP_<ACCOUNT>_PORT       (default: 587)\n");
    out.push_str("  MAIL_SMTP_<ACCOUNT>_USER\n");
    out.push_str("  MAIL_SMTP_<ACCOUNT>_PASS       (optional if OAuth2 configured)\n");
    out.push_str("  MAIL_SMTP_<ACCOUNT>_SECURE     (starttls | tls | plain, default: starttls)\n\n");

    out.push_str("Discovered SMTP sections (from current environment)\n");
    if smtp_sections.is_empty() {
        out.push_str("  (none discovered)\n");
    } else {
        for section in &smtp_sections {
            out.push_str(&format!("  [{}]\n", section));
            for suffix in ["HOST", "PORT", "USER", "PASS", "SECURE"] {
                let key = format!("MAIL_SMTP_{}_{}", section, suffix);
                let value = env_map.get(&key).map(String::as_str);
                out.push_str(&format!("    {}={}\n", key, redact_value(&key, value)));
            }
        }
    }
    out.push('\n');

    out.push_str("Global policy defaults\n");
    out.push_str("  MAIL_IMAP_WRITE_ENABLED=false\n");
    out.push_str("  MAIL_IMAP_CONNECT_TIMEOUT_MS=30000\n");
    out.push_str("  MAIL_IMAP_GREETING_TIMEOUT_MS=15000\n");
    out.push_str("  MAIL_IMAP_SOCKET_TIMEOUT_MS=300000\n");
    out.push_str("  MAIL_IMAP_CURSOR_TTL_SECONDS=600\n");
    out.push_str("  MAIL_IMAP_CURSOR_MAX_ENTRIES=512\n");
    out.push_str("  MAIL_SMTP_WRITE_ENABLED=false\n");
    out.push_str("  MAIL_SMTP_SAVE_SENT=true\n");
    out.push_str("  MAIL_SMTP_TIMEOUT_MS=30000\n\n");

    out.push_str("Send/write gate policy\n");
    out.push_str(
        "  IMAP write tools are blocked unless MAIL_IMAP_WRITE_ENABLED=true.\n",
    );
    out.push_str(
        "  SMTP send tools are blocked unless MAIL_SMTP_WRITE_ENABLED=true.\n",
    );
    out.push_str(
        "  These gates protect against accidental mutations and sending.\n",
    );

    out
}

fn discover_account_sections(env_map: &BTreeMap<String, String>) -> Vec<String> {
    let mut sections: Vec<String> = env_map
        .keys()
        .filter_map(|key| {
            let remainder = key.strip_prefix("MAIL_IMAP_")?;
            for suffix in ["_HOST", "_USER", "_PASS", "_PORT", "_SECURE"] {
                if let Some(section) = remainder.strip_suffix(suffix)
                    && !section.is_empty()
                {
                    return Some(section.to_owned());
                }
            }
            None
        })
        .collect();

    sections.sort();
    sections.dedup();
    sections
}

fn discover_oauth2_sections(env_map: &BTreeMap<String, String>) -> Vec<String> {
    let mut sections: Vec<String> = env_map
        .keys()
        .filter_map(|key| {
            let remainder = key.strip_prefix("MAIL_OAUTH2_")?;
            for suffix in [
                "_PROVIDER",
                "_CLIENT_ID",
                "_CLIENT_SECRET",
                "_REFRESH_TOKEN",
            ] {
                if let Some(section) = remainder.strip_suffix(suffix)
                    && !section.is_empty()
                {
                    return Some(section.to_owned());
                }
            }
            None
        })
        .collect();

    sections.sort();
    sections.dedup();
    sections
}

fn discover_smtp_sections(env_map: &BTreeMap<String, String>) -> Vec<String> {
    let mut sections: Vec<String> = env_map
        .keys()
        .filter_map(|key| {
            let remainder = key.strip_prefix("MAIL_SMTP_")?;
            for suffix in ["_HOST", "_PORT", "_USER", "_PASS", "_SECURE"] {
                if let Some(section) = remainder.strip_suffix(suffix)
                    && !section.is_empty()
                {
                    return Some(section.to_owned());
                }
            }
            None
        })
        .collect();

    sections.sort();
    sections.dedup();
    sections
}

fn redact_value(key: &str, value: Option<&str>) -> String {
    match value {
        Some(v) if is_secret_key(key) && !v.is_empty() => "<redacted>".to_owned(),
        Some("") => "<empty>".to_owned(),
        Some(v) => v.to_owned(),
        None => "<unset>".to_owned(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.contains("PASS") || key.contains("SECRET") || key.contains("TOKEN")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        build_help_output, discover_account_sections, is_secret_key, redact_value,
        should_print_help,
    };

    #[test]
    fn detects_short_and_long_help_flags() {
        assert!(should_print_help(["-h"]));
        assert!(should_print_help(["--help"]));
        assert!(should_print_help(["--verbose", "-h"]));
        assert!(!should_print_help(["--verbose"]));
    }

    #[test]
    fn discovers_account_sections_from_env_like_keys() {
        let mut env_map = BTreeMap::new();
        env_map.insert(
            "MAIL_IMAP_DEFAULT_HOST".to_owned(),
            "imap.example.com".to_owned(),
        );
        env_map.insert(
            "MAIL_IMAP_WORK_USER".to_owned(),
            "work@example.com".to_owned(),
        );
        env_map.insert("MAIL_IMAP_WORK_PASS".to_owned(), "secret".to_owned());
        env_map.insert("MAIL_IMAP_WRITE_ENABLED".to_owned(), "true".to_owned());

        assert_eq!(
            discover_account_sections(&env_map),
            vec!["DEFAULT".to_owned(), "WORK".to_owned()]
        );
    }

    #[test]
    fn redacts_secret_values_and_marks_unset() {
        assert_eq!(
            redact_value("MAIL_IMAP_DEFAULT_PASS", Some("abc")),
            "<redacted>"
        );
        assert_eq!(redact_value("MAIL_IMAP_DEFAULT_HOST", Some("imap")), "imap");
        assert_eq!(redact_value("MAIL_IMAP_DEFAULT_USER", None), "<unset>");
    }

    #[test]
    fn detects_secret_keys_case_insensitively() {
        assert!(is_secret_key("mail_imap_default_pass"));
        assert!(is_secret_key("MAIL_IMAP_API_TOKEN"));
        assert!(!is_secret_key("MAIL_IMAP_DEFAULT_HOST"));
    }

    #[test]
    fn help_output_includes_policy_defaults_and_redaction() {
        let mut env_map = BTreeMap::new();
        env_map.insert(
            "MAIL_IMAP_DEFAULT_HOST".to_owned(),
            "imap.example.com".to_owned(),
        );
        env_map.insert(
            "MAIL_IMAP_DEFAULT_USER".to_owned(),
            "user@example.com".to_owned(),
        );
        env_map.insert("MAIL_IMAP_DEFAULT_PASS".to_owned(), "top-secret".to_owned());

        let help = build_help_output(&env_map);
        assert!(help.contains("Global policy defaults"));
        assert!(help.contains("MAIL_IMAP_WRITE_ENABLED=false"));
        assert!(help.contains("Send/write gate policy"));
        assert!(help.contains("MAIL_IMAP_DEFAULT_PASS=<redacted>"));
    }
}
