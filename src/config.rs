//! Configuration module for IMAP accounts and server settings
//!
//! All configuration is loaded from environment variables following the pattern
//! `MAIL_IMAP_<SEGMENT>_<KEY>`. Account segments are discovered by scanning for
//! `MAIL_IMAP_*_HOST` variables.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::env::VarError;

use regex::Regex;
use secrecy::SecretString;

use crate::errors::{AppError, AppResult};
use crate::oauth2::{OAuth2AccountConfig, OAuth2Provider};
use crate::smtp::{SmtpAccountConfig, SmtpSecurity};

/// Authentication method for an IMAP/SMTP account
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Traditional username/password (LOGIN command)
    Password,
    /// OAuth2 XOAUTH2 SASL mechanism
    OAuth2,
}

/// IMAP account configuration
///
/// Holds connection details and credentials for a single IMAP account.
/// Passwords are stored using `SecretString` to prevent accidental logging.
/// When `auth_method` is `OAuth2`, `pass` may be `None` and authentication
/// uses the token manager instead.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    /// Account identifier (lowercase, used as default `account_id` parameter)
    pub account_id: String,
    /// IMAP server hostname
    pub host: String,
    /// IMAP server port (typically 993 for TLS)
    pub port: u16,
    /// Whether to use TLS (currently enforced to `true`)
    pub secure: bool,
    /// Username for authentication
    pub user: String,
    /// Password stored in a type that prevents accidental logging.
    /// Optional when OAuth2 is used.
    pub pass: Option<SecretString>,
    /// Authentication method (password or OAuth2)
    pub auth_method: AuthMethod,
}

/// Server-wide configuration
///
/// Wraps all account configs and global server settings. Cloned into MCP tool
/// handlers via `Arc` for thread-safe shared access.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// All configured IMAP accounts, keyed by `account_id`
    pub accounts: BTreeMap<String, AccountConfig>,
    /// OAuth2 configurations keyed by `account_id` (only for accounts using OAuth2)
    pub oauth2_accounts: HashMap<String, OAuth2AccountConfig>,
    /// Configured SMTP accounts, keyed by `account_id`
    pub smtp_accounts: HashMap<String, SmtpAccountConfig>,
    /// Whether SMTP send operations are enabled
    pub smtp_write_enabled: bool,
    /// Whether to save sent messages to IMAP Sent folder
    pub smtp_save_sent: bool,
    /// SMTP operation timeout in milliseconds
    pub smtp_timeout_ms: u64,
    /// Whether write operations (copy, move, delete, flag updates) are enabled
    pub write_enabled: bool,
    /// TCP connection timeout in milliseconds
    pub connect_timeout_ms: u64,
    /// IMAP greeting/TLS handshake timeout in milliseconds
    pub greeting_timeout_ms: u64,
    /// Socket I/O timeout in milliseconds
    pub socket_timeout_ms: u64,
    /// Time-to-live for search cursors in seconds
    pub cursor_ttl_seconds: u64,
    /// Maximum number of cursors to retain (LRU eviction when exceeded)
    pub cursor_max_entries: usize,
}

impl ServerConfig {
    /// Load all configuration from environment variables
    ///
    /// Discovers accounts by scanning for `MAIL_IMAP_*_HOST` patterns.
    /// If no accounts are explicitly defined, a `default` account is required
    /// via `MAIL_IMAP_DEFAULT_HOST`, `MAIL_IMAP_DEFAULT_USER`, and
    /// `MAIL_IMAP_DEFAULT_PASS`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if required environment variables are missing
    /// or malformed.
    ///
    /// # Example Environment
    ///
    /// ```text
    /// MAIL_IMAP_DEFAULT_HOST=imap.gmail.com
    /// MAIL_IMAP_DEFAULT_USER=user@gmail.com
    /// MAIL_IMAP_DEFAULT_PASS=app-password
    /// MAIL_IMAP_WORK_HOST=outlook.office365.com
    /// MAIL_IMAP_WORK_USER=user@company.com
    /// MAIL_IMAP_WORK_PASS=work-pass
    /// MAIL_IMAP_WRITE_ENABLED=false
    /// ```
    pub fn load_from_env() -> AppResult<Self> {
        // Discover OAuth2 accounts first (needed to determine auth method)
        let oauth2_accounts = load_oauth2_accounts()?;

        let account_pattern = Regex::new(r"^MAIL_IMAP_([A-Z0-9_]+)_HOST$")
            .map_err(|e| AppError::Internal(format!("invalid account regex: {e}")))?;

        let mut account_segments: Vec<String> = env::vars()
            .filter_map(|(k, _)| {
                account_pattern
                    .captures(&k)
                    .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
            })
            .collect();

        if account_segments.is_empty() {
            account_segments.push("DEFAULT".to_owned());
        }

        account_segments.sort();
        account_segments.dedup();

        let mut accounts = BTreeMap::new();
        for seg in &account_segments {
            let account = load_account(seg, &oauth2_accounts)?;
            accounts.insert(account.account_id.clone(), account);
        }

        let smtp_accounts = load_smtp_accounts(&oauth2_accounts)?;

        Ok(Self {
            accounts,
            oauth2_accounts,
            smtp_accounts,
            smtp_write_enabled: parse_bool_env("MAIL_SMTP_WRITE_ENABLED", false)?,
            smtp_save_sent: parse_bool_env("MAIL_SMTP_SAVE_SENT", true)?,
            smtp_timeout_ms: parse_u64_env("MAIL_SMTP_TIMEOUT_MS", 30_000)?,
            write_enabled: parse_bool_env("MAIL_IMAP_WRITE_ENABLED", false)?,
            connect_timeout_ms: parse_u64_env("MAIL_IMAP_CONNECT_TIMEOUT_MS", 30_000)?,
            greeting_timeout_ms: parse_u64_env("MAIL_IMAP_GREETING_TIMEOUT_MS", 15_000)?,
            socket_timeout_ms: parse_u64_env("MAIL_IMAP_SOCKET_TIMEOUT_MS", 300_000)?,
            cursor_ttl_seconds: parse_u64_env("MAIL_IMAP_CURSOR_TTL_SECONDS", 600)?,
            cursor_max_entries: parse_usize_env("MAIL_IMAP_CURSOR_MAX_ENTRIES", 512)?,
        })
    }

    /// Get IMAP account configuration by ID
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if the account ID is not configured.
    pub fn get_account(&self, account_id: &str) -> AppResult<&AccountConfig> {
        self.accounts
            .get(account_id)
            .ok_or_else(|| AppError::NotFound(format!("IMAP account '{account_id}' is not configured")))
    }

    /// Get SMTP account configuration by ID
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if the SMTP account ID is not configured.
    pub fn get_smtp_account(&self, account_id: &str) -> AppResult<&SmtpAccountConfig> {
        self.smtp_accounts
            .get(account_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("SMTP account '{account_id}' is not configured"))
            })
    }
}

/// Load a single account configuration from environment
///
/// Reads `MAIL_IMAP_<SEGMENT>_HOST`, `_USER`, `_PASS`, `_PORT`, and `_SECURE`.
/// Normalizes the segment name to lowercase for `account_id` (except `DEFAULT`
/// becomes `default`).
///
/// When a matching OAuth2 configuration exists for this segment, `PASS` becomes
/// optional and `auth_method` is set to `OAuth2`.
fn load_account(
    segment: &str,
    oauth2_accounts: &HashMap<String, OAuth2AccountConfig>,
) -> AppResult<AccountConfig> {
    let prefix = format!("MAIL_IMAP_{}_", sanitize_segment(segment));
    let account_id = if segment == "DEFAULT" {
        "default".to_owned()
    } else {
        segment.to_ascii_lowercase()
    };

    let has_oauth2 = oauth2_accounts.contains_key(&account_id);

    let host = required_env(&format!("{prefix}HOST"))?;
    let user = required_env(&format!("{prefix}USER"))?;

    // Password is optional when OAuth2 is configured for this account
    let pass = match env::var(&format!("{prefix}PASS")) {
        Ok(v) if !v.trim().is_empty() => Some(SecretString::new(v.into())),
        _ if has_oauth2 => None,
        _ => {
            return Err(AppError::InvalidInput(format!(
                "No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS (or configure OAuth2 via MAIL_OAUTH2_<ID>_*).\nmail-imap-mcp-rs startup error: missing PASS."
            )));
        }
    };

    let auth_method = if has_oauth2 {
        AuthMethod::OAuth2
    } else {
        AuthMethod::Password
    };

    Ok(AccountConfig {
        account_id,
        host,
        port: parse_u16_env(&format!("{prefix}PORT"), 993)?,
        secure: parse_bool_env(&format!("{prefix}SECURE"), true)?,
        user,
        pass,
        auth_method,
    })
}

/// Discover and load OAuth2 account configurations from environment.
///
/// Scans for `MAIL_OAUTH2_*_PROVIDER` variables. For each found, loads
/// `CLIENT_ID`, `CLIENT_SECRET`, and `REFRESH_TOKEN`.
fn load_oauth2_accounts() -> AppResult<HashMap<String, OAuth2AccountConfig>> {
    let pattern = Regex::new(r"^MAIL_OAUTH2_([A-Z0-9_]+)_PROVIDER$")
        .map_err(|e| AppError::Internal(format!("invalid oauth2 regex: {e}")))?;

    let mut segments: Vec<String> = env::vars()
        .filter_map(|(k, _)| {
            pattern
                .captures(&k)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        })
        .collect();
    segments.sort();
    segments.dedup();

    let mut oauth2_accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_OAUTH2_{}_", sanitize_segment(&seg));
        let account_id = if seg == "DEFAULT" {
            "default".to_owned()
        } else {
            seg.to_ascii_lowercase()
        };

        let provider_str = required_oauth2_env(&format!("{prefix}PROVIDER"), &account_id)?;
        let provider = OAuth2Provider::parse(&provider_str)?;
        let client_id = required_oauth2_env(&format!("{prefix}CLIENT_ID"), &account_id)?;
        let client_secret = required_oauth2_env(&format!("{prefix}CLIENT_SECRET"), &account_id)?;
        let refresh_token = required_oauth2_env(&format!("{prefix}REFRESH_TOKEN"), &account_id)?;

        oauth2_accounts.insert(
            account_id,
            OAuth2AccountConfig {
                provider,
                client_id,
                client_secret: SecretString::new(client_secret.into()),
                refresh_token: SecretString::new(refresh_token.into()),
            },
        );
    }

    Ok(oauth2_accounts)
}

/// Discover and load SMTP account configurations from environment.
///
/// Scans for `MAIL_SMTP_*_HOST` variables. For each found, loads
/// `PORT`, `USER`, `PASS`, and `SECURE`. OAuth2 configs are checked to
/// determine the authentication method.
fn load_smtp_accounts(
    oauth2_accounts: &HashMap<String, OAuth2AccountConfig>,
) -> AppResult<HashMap<String, SmtpAccountConfig>> {
    let pattern = Regex::new(r"^MAIL_SMTP_([A-Z0-9_]+)_HOST$")
        .map_err(|e| AppError::Internal(format!("invalid smtp regex: {e}")))?;

    let mut segments: Vec<String> = env::vars()
        .filter_map(|(k, _)| {
            pattern
                .captures(&k)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        })
        .collect();
    segments.sort();
    segments.dedup();

    let mut smtp_accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_SMTP_{}_", sanitize_segment(&seg));
        let account_id = if seg == "DEFAULT" {
            "default".to_owned()
        } else {
            seg.to_ascii_lowercase()
        };

        let has_oauth2 = oauth2_accounts.contains_key(&account_id);

        let host = required_smtp_env(&format!("{prefix}HOST"), &account_id)?;
        let user = required_smtp_env(&format!("{prefix}USER"), &account_id)?;

        let pass = match env::var(format!("{prefix}PASS")) {
            Ok(v) if !v.trim().is_empty() => Some(SecretString::new(v.into())),
            _ if has_oauth2 => None,
            _ => None, // SMTP password is optional (some configs rely on OAuth2 only)
        };

        let default_port: u16 = 587; // STARTTLS default
        let port = parse_u16_env(&format!("{prefix}PORT"), default_port)?;

        let security = match env::var(format!("{prefix}SECURE")) {
            Ok(v) if !v.trim().is_empty() => SmtpSecurity::parse(&v)?,
            _ => SmtpSecurity::Starttls,
        };

        let auth_method = if has_oauth2 {
            AuthMethod::OAuth2
        } else {
            AuthMethod::Password
        };

        smtp_accounts.insert(
            account_id.clone(),
            SmtpAccountConfig {
                account_id,
                host,
                port,
                user,
                pass,
                security,
                auth_method,
            },
        );
    }

    Ok(smtp_accounts)
}

/// Read a required SMTP environment variable, with a clear error message.
fn required_smtp_env(key: &str, account_id: &str) -> AppResult<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(AppError::InvalidInput(format!(
            "SMTP account '{account_id}' is missing {key}"
        ))),
    }
}

/// Read a required OAuth2 environment variable, with a clear error message.
fn required_oauth2_env(key: &str, account_id: &str) -> AppResult<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(AppError::InvalidInput(format!(
            "OAuth2 account '{account_id}' is missing {key}"
        ))),
    }
}

/// Read a required environment variable, returning error if missing or empty
fn required_env(key: &str) -> AppResult<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => {
            let var_name = key.strip_prefix("MAIL_IMAP_").unwrap_or(key);
            let suffix = var_name.split('_').next_back().unwrap_or(var_name);
            Err(AppError::InvalidInput(format!(
                "No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS.\nmail-imap-mcp-rs startup error: missing {suffix}."
            )))
        }
    }
}

/// Sanitize an account segment to uppercase alphanumeric/underscore
///
/// Non-alphanumeric characters are replaced with underscores, and leading/
/// trailing underscores are trimmed.
fn sanitize_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for ch in seg.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_owned()
}

/// Parse a boolean environment variable with flexible values
///
/// Accepts: `1`, `true`, `yes`, `y`, `on` (truthy) or `0`, `false`, `no`,
/// `n`, `off` (falsy). Case-insensitive. Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set to an unrecognized value.
fn parse_bool_env(key: &str, default: bool) -> AppResult<bool> {
    match env::var(key) {
        Ok(v) => parse_bool_value(&v).ok_or_else(|| {
            AppError::InvalidInput(format!("invalid boolean environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Parse a `u16` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `u16`.
fn parse_u16_env(key: &str, default: u16) -> AppResult<u16> {
    match env::var(key) {
        Ok(v) => v.parse::<u16>().map_err(|_| {
            AppError::InvalidInput(format!("invalid u16 environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

/// Parse a `u64` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `u64`.
fn parse_u64_env(key: &str, default: u64) -> AppResult<u64> {
    match env::var(key) {
        Ok(v) => v.parse::<u64>().map_err(|_| {
            AppError::InvalidInput(format!("invalid u64 environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

/// Parse a `usize` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `usize`.
fn parse_usize_env(key: &str, default: usize) -> AppResult<usize> {
    match env::var(key) {
        Ok(v) => v.parse::<usize>().map_err(|_| {
            AppError::InvalidInput(format!("invalid usize environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bool_value;

    #[test]
    fn parse_bool_value_accepts_common_truthy_and_falsy_values() {
        for truthy in ["1", "true", "TRUE", " yes ", "Y", "on"] {
            assert_eq!(parse_bool_value(truthy), Some(true));
        }

        for falsy in ["0", "false", "FALSE", " no ", "N", "off"] {
            assert_eq!(parse_bool_value(falsy), Some(false));
        }
    }

    #[test]
    fn parse_bool_value_rejects_unrecognized_values() {
        for invalid in ["", "2", "maybe", "enabled", "disabled"] {
            assert_eq!(parse_bool_value(invalid), None);
        }
    }
}
