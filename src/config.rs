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
    /// Graph API OAuth2 configs keyed by `account_id` (separate from IMAP OAuth2)
    pub graph_oauth2_accounts: HashMap<String, OAuth2AccountConfig>,
    /// EWS account configs keyed by `account_id`
    pub ews_accounts: HashMap<String, crate::ews::EwsAccountConfig>,
    /// EWS OAuth2 configs keyed by `account_id`
    pub ews_oauth2_accounts: HashMap<String, OAuth2AccountConfig>,
    /// Configured SMTP accounts, keyed by `account_id`
    pub smtp_accounts: HashMap<String, SmtpAccountConfig>,
    /// Whether SMTP send operations are enabled
    pub smtp_write_enabled: bool,
    /// Global override for saving sent messages to the IMAP Sent folder.
    ///
    /// `None` = unset; the effective per-account value comes from a
    /// provider-aware default (Gmail/Zoho already save server-side, so the
    /// MCP must NOT append a duplicate; generic SMTP and Office 365 do not,
    /// so the MCP should). `Some(bool)` = explicit coarse override for all
    /// accounts. A per-account `MAIL_SMTP_<ID>_SAVE_SENT` takes precedence
    /// over this. See `should_save_sent`.
    pub smtp_save_sent: Option<bool>,
    /// SMTP connect/handshake/auth timeout in milliseconds
    ///
    /// Bounds the TCP connect, TLS handshake, and authentication phases.
    /// Should be short — if the server can't be reached promptly, it's down.
    pub smtp_connect_timeout_ms: u64,
    /// SMTP send (DATA transmission) timeout in milliseconds
    ///
    /// Bounds the full message transmission including attachments. Should be
    /// generous — large attachments over slow connections legitimately take
    /// minutes.
    pub smtp_send_timeout_ms: u64,
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

        let graph_oauth2_accounts = load_graph_oauth2_accounts()?;
        let (ews_accounts, ews_oauth2_accounts) = load_ews_accounts()?;
        let smtp_accounts = load_smtp_accounts(&oauth2_accounts)?;

        Ok(Self {
            accounts,
            oauth2_accounts,
            graph_oauth2_accounts,
            ews_accounts,
            ews_oauth2_accounts,
            smtp_accounts,
            smtp_write_enabled: parse_bool_env("MAIL_SMTP_WRITE_ENABLED", false)?,
            smtp_save_sent: parse_opt_bool_env("MAIL_SMTP_SAVE_SENT")?,
            // Backward compat: MAIL_SMTP_TIMEOUT_MS (deprecated, single timeout)
            // is honored as the send timeout if set, but the new vars take priority.
            smtp_connect_timeout_ms: parse_u64_env("MAIL_SMTP_CONNECT_TIMEOUT_MS", 30_000)?,
            smtp_send_timeout_ms: resolve_smtp_send_timeout(
                env_opt_u64("MAIL_SMTP_SEND_TIMEOUT_MS")?,
                env_opt_u64("MAIL_SMTP_TIMEOUT_MS")?,
            ),
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
        self.accounts.get(account_id).ok_or_else(|| {
            AppError::NotFound(format!("IMAP account '{account_id}' is not configured"))
        })
    }

    /// Get SMTP account configuration by ID
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if the SMTP account ID is not configured.
    pub fn get_smtp_account(&self, account_id: &str) -> AppResult<&SmtpAccountConfig> {
        self.smtp_accounts.get(account_id).ok_or_else(|| {
            AppError::NotFound(format!("SMTP account '{account_id}' is not configured"))
        })
    }

    /// Decide whether to append a copy of an outgoing SMTP message to the
    /// account's IMAP Sent folder.
    ///
    /// Precedence (highest first):
    /// 1. Per-account `MAIL_SMTP_<ID>_SAVE_SENT` (explicit).
    /// 2. Global `MAIL_SMTP_SAVE_SENT` (explicit, coarse override).
    /// 3. Provider-aware default: `false` for providers that already save a
    ///    server-side copy (Gmail, Zoho) — appending would duplicate the
    ///    Sent folder — and `true` otherwise (generic SMTP relays and
    ///    Office 365 SMTP submission do NOT auto-save, so the copy would be
    ///    lost without this).
    ///
    /// Unknown account IDs return `false` (nothing to save).
    pub fn should_save_sent(&self, account_id: &str) -> bool {
        let account = self.smtp_accounts.get(account_id);

        // 1. Per-account explicit override wins.
        if let Some(Some(explicit)) = account.map(|a| a.save_sent) {
            return explicit;
        }

        // 2. Global explicit override.
        if let Some(global) = self.smtp_save_sent {
            return global;
        }

        // 3. Provider-aware default based on the SMTP host.
        account
            .map(|a| !provider_auto_saves_sent(&a.host))
            .unwrap_or(false)
    }
}

/// Whether an SMTP provider saves a server-side copy of sent mail to the
/// Sent folder automatically (so the MCP must NOT append its own copy).
///
/// - Gmail saves and de-duplicates by Message-ID; a redundant APPEND is at
///   best wasteful.
/// - Zoho saves but does NOT de-duplicate, so an APPEND produces two
///   identical messages in the Sent folder.
/// - Office 365 SMTP submission and generic relays do NOT auto-save, so
///   they are intentionally excluded here (the MCP should save for them).
fn provider_auto_saves_sent(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h.contains("gmail") || h.contains("googlemail") || h.contains("zoho")
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
                "No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS (or configure OAuth2 via MAIL_OAUTH2_<ID>_*).\nmail-mcp startup error: missing PASS."
            )));
        }
    };

    // Use OAuth2 for IMAP only when no password is provided.
    // If both password and OAuth2 are configured, password is used for IMAP
    // and OAuth2 is reserved for Graph API / SMTP XOAUTH2.
    let auth_method = if has_oauth2 && pass.is_none() {
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
        let tenant = env::var(format!("{prefix}TENANT_ID"))
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "common".to_owned());
        let token_url = provider.token_url(&tenant);

        oauth2_accounts.insert(
            account_id,
            OAuth2AccountConfig {
                provider,
                client_id,
                client_secret: SecretString::new(client_secret.into()),
                refresh_token: SecretString::new(refresh_token.into()),
                token_url,
            },
        );
    }

    Ok(oauth2_accounts)
}

/// Discover and load Graph API OAuth2 configurations from environment.
///
/// Scans for `MAIL_GRAPH_*_PROVIDER` variables. These are separate from
/// `MAIL_OAUTH2_*` to allow different scopes: IMAP OAuth2 uses
/// `outlook.office.com/IMAP.*` scopes while Graph uses
/// `graph.microsoft.com/Mail.*` scopes.
fn load_graph_oauth2_accounts() -> AppResult<HashMap<String, OAuth2AccountConfig>> {
    let pattern = Regex::new(r"^MAIL_GRAPH_([A-Z0-9_]+)_PROVIDER$")
        .map_err(|e| AppError::Internal(format!("invalid graph oauth2 regex: {e}")))?;

    let mut segments: Vec<String> = env::vars()
        .filter_map(|(k, _)| {
            pattern
                .captures(&k)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        })
        .collect();
    segments.sort();
    segments.dedup();

    let mut accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_GRAPH_{}_", sanitize_segment(&seg));
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
        let tenant = env::var(format!("{prefix}TENANT_ID"))
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "common".to_owned());
        let token_url = provider.token_url(&tenant);

        accounts.insert(
            account_id,
            OAuth2AccountConfig {
                provider,
                client_id,
                client_secret: SecretString::new(client_secret.into()),
                refresh_token: SecretString::new(refresh_token.into()),
                token_url,
            },
        );
    }

    Ok(accounts)
}

/// Discover and load EWS account configurations from environment.
///
/// Scans for `MAIL_EWS_*_USER` variables. For each found, loads
/// OAuth2 credentials from `MAIL_EWS_*_CLIENT_ID`, etc.
fn load_ews_accounts() -> AppResult<(
    HashMap<String, crate::ews::EwsAccountConfig>,
    HashMap<String, OAuth2AccountConfig>,
)> {
    let pattern = Regex::new(r"^MAIL_EWS_([A-Z0-9_]+)_USER$")
        .map_err(|e| AppError::Internal(format!("invalid ews regex: {e}")))?;

    let mut segments: Vec<String> = env::vars()
        .filter_map(|(k, _)| {
            pattern
                .captures(&k)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        })
        .collect();
    segments.sort();
    segments.dedup();

    let mut ews_accounts = HashMap::new();
    let mut ews_oauth2 = HashMap::new();

    for seg in segments {
        let prefix = format!("MAIL_EWS_{}_", sanitize_segment(&seg));
        let account_id = if seg == "DEFAULT" {
            "default".to_owned()
        } else {
            seg.to_ascii_lowercase()
        };

        let user = match env::var(format!("{prefix}USER")) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => continue,
        };

        ews_accounts.insert(account_id.clone(), crate::ews::EwsAccountConfig { user });

        // Load OAuth2 for EWS
        let client_id = match env::var(format!("{prefix}CLIENT_ID")) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => "d3590ed6-52b3-4102-aeff-aad2292ab01c".to_owned(), // Default: Microsoft Office
        };
        let client_secret = match env::var(format!("{prefix}CLIENT_SECRET")) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => "none".to_owned(),
        };
        let refresh_token = match env::var(format!("{prefix}REFRESH_TOKEN")) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => continue,
        };
        let tenant = env::var(format!("{prefix}TENANT_ID"))
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "common".to_owned());
        let token_url = OAuth2Provider::Microsoft.token_url(&tenant);

        ews_oauth2.insert(
            account_id,
            OAuth2AccountConfig {
                provider: OAuth2Provider::Microsoft,
                client_id,
                client_secret: SecretString::new(client_secret.into()),
                refresh_token: SecretString::new(refresh_token.into()),
                token_url,
            },
        );
    }

    Ok((ews_accounts, ews_oauth2))
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

        let save_sent = parse_opt_bool_env(&format!("{prefix}SAVE_SENT"))?;

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
                save_sent,
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
                "No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS.\nmail-mcp startup error: missing {suffix}."
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

/// Parse an optional boolean environment variable.
///
/// Returns `Ok(None)` when the variable is unset or empty, distinguishing
/// "not configured" from an explicit `true`/`false`. Used for settings with
/// a computed (non-constant) default, such as per-account / provider-aware
/// `SAVE_SENT`.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set to an unrecognized value.
fn parse_opt_bool_env(key: &str) -> AppResult<Option<bool>> {
    match env::var(key) {
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => parse_bool_value(&v).map(Some).ok_or_else(|| {
            AppError::InvalidInput(format!("invalid boolean environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
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

/// Read a `u64` env var, returning `None` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `u64`.
fn env_opt_u64(key: &str) -> AppResult<Option<u64>> {
    match env::var(key) {
        Ok(v) => v.parse::<u64>().map(Some).map_err(|_| {
            AppError::InvalidInput(format!("invalid u64 environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

/// Resolve the SMTP send timeout from new and legacy env vars.
///
/// Priority: new var (`MAIL_SMTP_SEND_TIMEOUT_MS`) wins if set; otherwise the
/// legacy var (`MAIL_SMTP_TIMEOUT_MS`) is honored for backward compatibility;
/// otherwise the default of 300 000 ms (5 minutes).
fn resolve_smtp_send_timeout(new_var: Option<u64>, legacy_var: Option<u64>) -> u64 {
    new_var.or(legacy_var).unwrap_or(300_000)
}

#[cfg(test)]
mod tests {
    use super::{
        AuthMethod, ServerConfig, parse_bool_value, provider_auto_saves_sent,
        resolve_smtp_send_timeout,
    };
    use crate::smtp::{SmtpAccountConfig, SmtpSecurity};
    use std::collections::{BTreeMap, HashMap};

    /// Build a ServerConfig with one SMTP account at `host` whose per-account
    /// SAVE_SENT is `account_save_sent`, and a global `MAIL_SMTP_SAVE_SENT`
    /// of `global_save_sent`. Only the fields exercised by `should_save_sent`
    /// matter; the rest are filled with inert defaults.
    fn cfg_with_smtp(
        host: &str,
        account_save_sent: Option<bool>,
        global_save_sent: Option<bool>,
    ) -> ServerConfig {
        let mut smtp_accounts = HashMap::new();
        smtp_accounts.insert(
            "acct".to_owned(),
            SmtpAccountConfig {
                account_id: "acct".to_owned(),
                host: host.to_owned(),
                port: 587,
                user: "u@example.com".to_owned(),
                pass: None,
                security: SmtpSecurity::Starttls,
                auth_method: AuthMethod::Password,
                save_sent: account_save_sent,
            },
        );
        ServerConfig {
            accounts: BTreeMap::new(),
            oauth2_accounts: HashMap::new(),
            graph_oauth2_accounts: HashMap::new(),
            ews_accounts: HashMap::new(),
            ews_oauth2_accounts: HashMap::new(),
            smtp_accounts,
            smtp_write_enabled: true,
            smtp_save_sent: global_save_sent,
            smtp_connect_timeout_ms: 30_000,
            smtp_send_timeout_ms: 300_000,
            write_enabled: true,
            connect_timeout_ms: 5_000,
            greeting_timeout_ms: 5_000,
            socket_timeout_ms: 15_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 512,
        }
    }

    #[test]
    fn provider_auto_save_detection_matches_known_hosts() {
        // Providers that save server-side (MCP must NOT append a copy).
        assert!(provider_auto_saves_sent("smtp.gmail.com"));
        assert!(provider_auto_saves_sent("smtp-relay.gmail.com"));
        assert!(provider_auto_saves_sent("smtp.googlemail.com"));
        assert!(provider_auto_saves_sent("smtp.zoho.com"));
        assert!(provider_auto_saves_sent("smtp.zoho.eu"));
        assert!(provider_auto_saves_sent("SMTP.ZOHO.COM")); // case-insensitive
        // Providers that do NOT auto-save (MCP should append).
        assert!(!provider_auto_saves_sent("smtp.office365.com"));
        assert!(!provider_auto_saves_sent("smtp.sendgrid.net"));
        assert!(!provider_auto_saves_sent("mail.midominio.cl"));
    }

    #[test]
    fn should_save_sent_provider_aware_default_when_nothing_set() {
        // Zoho/Gmail auto-save → MCP default false (no duplicate).
        assert!(!cfg_with_smtp("smtp.zoho.com", None, None).should_save_sent("acct"));
        assert!(!cfg_with_smtp("smtp.gmail.com", None, None).should_save_sent("acct"));
        // Generic / Office 365 → MCP default true (or the copy is lost).
        assert!(cfg_with_smtp("smtp.office365.com", None, None).should_save_sent("acct"));
        assert!(cfg_with_smtp("mail.midominio.cl", None, None).should_save_sent("acct"));
    }

    #[test]
    fn should_save_sent_per_account_override_beats_everything() {
        // Zoho would default false, but explicit per-account true wins.
        assert!(cfg_with_smtp("smtp.zoho.com", Some(true), Some(false)).should_save_sent("acct"));
        // Generic would default true, but explicit per-account false wins.
        assert!(
            !cfg_with_smtp("mail.midominio.cl", Some(false), Some(true)).should_save_sent("acct")
        );
    }

    #[test]
    fn should_save_sent_global_override_beats_provider_default() {
        // Global true forces save even on Zoho (when no per-account override).
        assert!(cfg_with_smtp("smtp.zoho.com", None, Some(true)).should_save_sent("acct"));
        // Global false suppresses save even on a generic host.
        assert!(!cfg_with_smtp("mail.midominio.cl", None, Some(false)).should_save_sent("acct"));
    }

    #[test]
    fn should_save_sent_unknown_account_is_false() {
        assert!(!cfg_with_smtp("smtp.zoho.com", None, None).should_save_sent("nonexistent"));
    }

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

    #[test]
    fn smtp_send_timeout_defaults_to_five_minutes_when_unset() {
        assert_eq!(resolve_smtp_send_timeout(None, None), 300_000);
    }

    #[test]
    fn smtp_send_timeout_honors_legacy_var_when_new_var_unset() {
        // Existing deployments using MAIL_SMTP_TIMEOUT_MS keep working.
        assert_eq!(resolve_smtp_send_timeout(None, Some(60_000)), 60_000);
    }

    #[test]
    fn smtp_send_timeout_new_var_takes_priority_over_legacy() {
        // When both are set, the explicit new var wins.
        assert_eq!(
            resolve_smtp_send_timeout(Some(120_000), Some(60_000)),
            120_000
        );
    }

    #[test]
    fn smtp_send_timeout_uses_new_var_when_only_new_var_set() {
        assert_eq!(resolve_smtp_send_timeout(Some(45_000), None), 45_000);
    }
}
