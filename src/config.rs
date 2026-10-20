//! Configuration module for IMAP accounts and server settings
//!
//! Configuration is a `MAIL_*` map ([`crate::config_file::Settings`]). Account
//! segments are discovered by scanning for `MAIL_IMAP_*_HOST` keys. The map may
//! come from TOML, the process environment, or both.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use secrecy::SecretString;

use crate::config_file::{Settings, segments_with_suffix};
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
    /// Whether the draft tools may create, update and delete drafts
    /// (`MAIL_DRAFTS_ENABLED`). Independent of `write_enabled`: an agent can
    /// prepare drafts without being able to move or delete other mail.
    /// Sending a draft additionally requires `smtp_write_enabled`.
    pub drafts_enabled: bool,
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
    /// Maximum number of mailboxes `imap_list_mailboxes` returns
    /// (`MAIL_IMAP_MAX_MAILBOXES`, clamped to `1..=MAX_MAILBOXES_LIMIT`)
    pub max_mailboxes: usize,
    /// Parallel IMAP connections used by `imap_search_all_mailboxes`
    /// (`MAIL_IMAP_SEARCH_CONCURRENCY`, clamped to `1..=MAX_SEARCH_CONCURRENCY`)
    pub search_concurrency: usize,
    /// Default directory where `imap_get_attachment` saves downloaded
    /// attachments. `None` = fall back to the system temp dir. A per-call
    /// `output_dir` argument overrides this.
    pub attachment_download_dir: Option<String>,
    /// Directory that outbound `file_path` attachments must live under
    /// (canonicalized at startup). `None` = no restriction, any readable
    /// file may be attached.
    pub attachment_upload_dir: Option<PathBuf>,
}

impl ServerConfig {
    /// Load server configuration from an effective `MAIL_*` settings map.
    ///
    /// Discovers accounts from `MAIL_IMAP_*_HOST` keys. When the map has no
    /// IMAP, SMTP, Graph, or EWS account, a `default` IMAP account is required
    /// (`MAIL_IMAP_DEFAULT_HOST`, `MAIL_IMAP_DEFAULT_USER`, and
    /// `MAIL_IMAP_DEFAULT_PASS`). SMTP-only, Graph-only, and EWS-only maps load
    /// with an empty IMAP account list.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if required settings are missing or malformed.
    ///
    /// # Example keys
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
    pub fn load(settings: &Settings) -> AppResult<Self> {
        // Discover OAuth2 accounts first (needed to determine auth method)
        let oauth2_accounts = load_oauth2_accounts(settings)?;
        let graph_oauth2_accounts = load_graph_oauth2_accounts(settings)?;
        let (ews_accounts, ews_oauth2_accounts) = load_ews_accounts(settings)?;
        let smtp_accounts = load_smtp_accounts(settings, &oauth2_accounts)?;

        let mut account_segments = segments_with_suffix(settings, "MAIL_IMAP_", "HOST");
        let other_accounts = !smtp_accounts.is_empty()
            || !graph_oauth2_accounts.is_empty()
            || !ews_accounts.is_empty();
        if account_segments.is_empty() && !other_accounts {
            account_segments.push("DEFAULT".to_owned());
        }

        let mut accounts = BTreeMap::new();
        for seg in &account_segments {
            let account = load_account(settings, seg, &oauth2_accounts)?;
            accounts.insert(account.account_id.clone(), account);
        }

        Ok(Self {
            accounts,
            oauth2_accounts,
            graph_oauth2_accounts,
            ews_accounts,
            ews_oauth2_accounts,
            smtp_accounts,
            smtp_write_enabled: parse_bool_setting(settings, "MAIL_SMTP_WRITE_ENABLED", false)?,
            smtp_save_sent: parse_opt_bool_setting(settings, "MAIL_SMTP_SAVE_SENT")?,
            // Backward compat: MAIL_SMTP_TIMEOUT_MS (deprecated, single timeout)
            // is honored as the send timeout if set, but the new vars take priority.
            smtp_connect_timeout_ms: parse_u64_setting(
                settings,
                "MAIL_SMTP_CONNECT_TIMEOUT_MS",
                30_000,
            )?,
            smtp_send_timeout_ms: resolve_smtp_send_timeout(
                setting_opt_u64(settings, "MAIL_SMTP_SEND_TIMEOUT_MS")?,
                setting_opt_u64(settings, "MAIL_SMTP_TIMEOUT_MS")?,
            ),
            write_enabled: parse_bool_setting(settings, "MAIL_IMAP_WRITE_ENABLED", false)?,
            drafts_enabled: parse_bool_setting(settings, "MAIL_DRAFTS_ENABLED", false)?,
            connect_timeout_ms: parse_u64_setting(
                settings,
                "MAIL_IMAP_CONNECT_TIMEOUT_MS",
                30_000,
            )?,
            greeting_timeout_ms: parse_u64_setting(
                settings,
                "MAIL_IMAP_GREETING_TIMEOUT_MS",
                15_000,
            )?,
            socket_timeout_ms: parse_u64_setting(settings, "MAIL_IMAP_SOCKET_TIMEOUT_MS", 300_000)?,
            cursor_ttl_seconds: parse_u64_setting(settings, "MAIL_IMAP_CURSOR_TTL_SECONDS", 600)?,
            cursor_max_entries: parse_usize_setting(settings, "MAIL_IMAP_CURSOR_MAX_ENTRIES", 512)?,
            max_mailboxes: clamp_max_mailboxes(parse_usize_setting(
                settings,
                "MAIL_IMAP_MAX_MAILBOXES",
                DEFAULT_MAX_MAILBOXES,
            )?),
            search_concurrency: parse_usize_setting(
                settings,
                "MAIL_IMAP_SEARCH_CONCURRENCY",
                DEFAULT_SEARCH_CONCURRENCY,
            )?
            .clamp(1, MAX_SEARCH_CONCURRENCY),
            attachment_download_dir: settings
                .get("MAIL_ATTACHMENT_DOWNLOAD_DIR")
                .filter(|s| !s.trim().is_empty())
                .cloned(),
            attachment_upload_dir: parse_upload_dir(
                settings
                    .get("MAIL_ATTACHMENT_UPLOAD_DIR")
                    .map(String::as_str),
            )?,
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

/// Load a single IMAP account from the settings map.
///
/// Reads `MAIL_IMAP_<SEGMENT>_HOST`, `_USER`, `_PASS`, `_PORT`, and `_SECURE`.
/// Normalizes the segment name to lowercase for `account_id` (except `DEFAULT`
/// becomes `default`).
///
/// When a matching OAuth2 configuration exists for this segment, `PASS` becomes
/// optional and `auth_method` is set to `OAuth2`.
fn load_account(
    settings: &Settings,
    segment: &str,
    oauth2_accounts: &HashMap<String, OAuth2AccountConfig>,
) -> AppResult<AccountConfig> {
    let prefix = format!("MAIL_IMAP_{}_", segment.to_ascii_uppercase());
    let account_id = if segment == "DEFAULT" {
        "default".to_owned()
    } else {
        segment.to_ascii_lowercase()
    };

    let has_oauth2 = oauth2_accounts.contains_key(&account_id);

    let host = required_setting(settings, &format!("{prefix}HOST"))?;
    let user = required_setting(settings, &format!("{prefix}USER"))?;

    // Password is optional when OAuth2 is configured for this account
    let pass = match setting(settings, &format!("{prefix}PASS")) {
        Some(v) if !v.trim().is_empty() => Some(SecretString::new(v.to_owned().into())),
        _ if has_oauth2 => None,
        _ => {
            return Err(AppError::InvalidInput("No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS (or configure OAuth2 via MAIL_OAUTH2_<ID>_*).\nmail-mcp startup error: missing PASS.".to_string()));
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
        port: parse_u16_setting(settings, &format!("{prefix}PORT"), 993)?,
        secure: parse_bool_setting(settings, &format!("{prefix}SECURE"), true)?,
        user,
        pass,
        auth_method,
    })
}

/// Discover and load OAuth2 account configurations from the settings map.
///
/// Scans for `MAIL_OAUTH2_*_PROVIDER` keys. For each found, loads
/// `CLIENT_ID`, `CLIENT_SECRET`, and `REFRESH_TOKEN`.
fn load_oauth2_accounts(settings: &Settings) -> AppResult<HashMap<String, OAuth2AccountConfig>> {
    let segments = segments_with_suffix(settings, "MAIL_OAUTH2_", "PROVIDER");

    let mut oauth2_accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_OAUTH2_{}_", seg.to_ascii_uppercase());
        let account_id = account_id_from_segment(&seg);
        oauth2_accounts.insert(
            account_id.clone(),
            load_oauth2_fields(settings, &prefix, &account_id)?,
        );
    }

    Ok(oauth2_accounts)
}

/// Discover and load Graph API OAuth2 configurations from the settings map.
///
/// Scans for `MAIL_GRAPH_*_PROVIDER` keys. These are separate from
/// `MAIL_OAUTH2_*` to allow different scopes: IMAP OAuth2 uses
/// `outlook.office.com/IMAP.*` scopes while Graph uses
/// `graph.microsoft.com/Mail.*` scopes.
fn load_graph_oauth2_accounts(
    settings: &Settings,
) -> AppResult<HashMap<String, OAuth2AccountConfig>> {
    let segments = segments_with_suffix(settings, "MAIL_GRAPH_", "PROVIDER");

    let mut accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_GRAPH_{}_", seg.to_ascii_uppercase());
        let account_id = account_id_from_segment(&seg);
        accounts.insert(
            account_id.clone(),
            load_oauth2_fields(settings, &prefix, &account_id)?,
        );
    }

    Ok(accounts)
}

fn load_oauth2_fields(
    settings: &Settings,
    prefix: &str,
    account_id: &str,
) -> AppResult<OAuth2AccountConfig> {
    let provider_str =
        required_setting_for(settings, &format!("{prefix}PROVIDER"), account_id, "OAuth2")?;
    let provider = OAuth2Provider::parse(&provider_str).map_err(|_| {
        AppError::InvalidInput(format!("invalid OAuth2 provider setting {prefix}PROVIDER"))
    })?;
    let client_id = required_setting_for(
        settings,
        &format!("{prefix}CLIENT_ID"),
        account_id,
        "OAuth2",
    )?;
    let client_secret = required_setting_for(
        settings,
        &format!("{prefix}CLIENT_SECRET"),
        account_id,
        "OAuth2",
    )?;
    let refresh_token = required_setting_for(
        settings,
        &format!("{prefix}REFRESH_TOKEN"),
        account_id,
        "OAuth2",
    )?;
    Ok(OAuth2AccountConfig {
        provider,
        client_id,
        client_secret: SecretString::new(client_secret.into()),
        refresh_token: SecretString::new(refresh_token.into()),
    })
}

/// Discover and load EWS account configurations from the settings map.
///
/// Scans for `MAIL_EWS_*_USER` keys. For each found, loads OAuth2 credentials
/// from `MAIL_EWS_*_CLIENT_ID`, etc.
fn load_ews_accounts(
    settings: &Settings,
) -> AppResult<(
    HashMap<String, crate::ews::EwsAccountConfig>,
    HashMap<String, OAuth2AccountConfig>,
)> {
    let segments = segments_with_suffix(settings, "MAIL_EWS_", "USER");

    let mut ews_accounts = HashMap::new();
    let mut ews_oauth2 = HashMap::new();

    for seg in segments {
        let prefix = format!("MAIL_EWS_{}_", seg.to_ascii_uppercase());
        let account_id = account_id_from_segment(&seg);

        let user = match setting(settings, &format!("{prefix}USER")) {
            Some(v) if !v.trim().is_empty() => v.to_owned(),
            _ => continue,
        };

        ews_accounts.insert(account_id.clone(), crate::ews::EwsAccountConfig { user });

        // Load OAuth2 for EWS
        let client_id = match setting(settings, &format!("{prefix}CLIENT_ID")) {
            Some(v) if !v.trim().is_empty() => v.to_owned(),
            _ => "d3590ed6-52b3-4102-aeff-aad2292ab01c".to_owned(), // Default: Microsoft Office
        };
        let client_secret = match setting(settings, &format!("{prefix}CLIENT_SECRET")) {
            Some(v) if !v.trim().is_empty() => v.to_owned(),
            _ => "none".to_owned(),
        };
        let refresh_token = match setting(settings, &format!("{prefix}REFRESH_TOKEN")) {
            Some(v) if !v.trim().is_empty() => v.to_owned(),
            _ => continue,
        };

        ews_oauth2.insert(
            account_id,
            OAuth2AccountConfig {
                provider: OAuth2Provider::Microsoft,
                client_id,
                client_secret: SecretString::new(client_secret.into()),
                refresh_token: SecretString::new(refresh_token.into()),
            },
        );
    }

    Ok((ews_accounts, ews_oauth2))
}

/// Discover and load SMTP account configurations from the settings map.
///
/// Scans for `MAIL_SMTP_*_HOST` keys. For each found, loads `PORT`, `USER`,
/// `PASS`, and `SECURE`. OAuth2 configs are checked to determine the
/// authentication method.
fn load_smtp_accounts(
    settings: &Settings,
    oauth2_accounts: &HashMap<String, OAuth2AccountConfig>,
) -> AppResult<HashMap<String, SmtpAccountConfig>> {
    let segments = segments_with_suffix(settings, "MAIL_SMTP_", "HOST");

    let mut smtp_accounts = HashMap::new();
    for seg in segments {
        let prefix = format!("MAIL_SMTP_{}_", seg.to_ascii_uppercase());
        let account_id = account_id_from_segment(&seg);

        let has_oauth2 = oauth2_accounts.contains_key(&account_id);

        let host = required_setting_for(settings, &format!("{prefix}HOST"), &account_id, "SMTP")?;
        let user = required_setting_for(settings, &format!("{prefix}USER"), &account_id, "SMTP")?;

        let pass = match setting(settings, &format!("{prefix}PASS")) {
            Some(v) if !v.trim().is_empty() => Some(SecretString::new(v.to_owned().into())),
            _ if has_oauth2 => None,
            _ => None, // SMTP password is optional (some configs rely on OAuth2 only)
        };

        let default_port: u16 = 587; // STARTTLS default
        let port = parse_u16_setting(settings, &format!("{prefix}PORT"), default_port)?;

        let security = match setting(settings, &format!("{prefix}SECURE")) {
            Some(v) if !v.trim().is_empty() => SmtpSecurity::parse(v).map_err(|_| {
                AppError::InvalidInput(format!("invalid SMTP security setting {prefix}SECURE"))
            })?,
            _ => SmtpSecurity::Starttls,
        };

        let auth_method = if has_oauth2 {
            AuthMethod::OAuth2
        } else {
            AuthMethod::Password
        };

        let save_sent = parse_opt_bool_setting(settings, &format!("{prefix}SAVE_SENT"))?;

        let from_email = match setting(settings, &format!("{prefix}FROM_EMAIL")) {
            Some(v) => {
                let v = v.trim();
                if v.is_empty() {
                    None
                } else if !is_valid_email(v) {
                    return Err(AppError::InvalidInput(format!(
                        "SMTP account '{account_id}': {prefix}FROM_EMAIL is not a valid email address"
                    )));
                } else {
                    Some(v.to_owned())
                }
            }
            _ => None,
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
                save_sent,
                from_email,
            },
        );
    }

    Ok(smtp_accounts)
}

fn account_id_from_segment(segment: &str) -> String {
    if segment == "DEFAULT" {
        "default".to_owned()
    } else {
        segment.to_ascii_lowercase()
    }
}

fn setting<'a>(settings: &'a Settings, key: &str) -> Option<&'a str> {
    settings.get(key).map(String::as_str)
}

/// Read a required account setting, with a clear error message.
fn required_setting_for(
    settings: &Settings,
    key: &str,
    account_id: &str,
    protocol: &str,
) -> AppResult<String> {
    match setting(settings, key) {
        Some(v) if !v.trim().is_empty() => Ok(v.to_owned()),
        _ => Err(AppError::InvalidInput(format!(
            "{protocol} account '{account_id}' is missing {key}"
        ))),
    }
}

/// Lightweight format check for a `FROM_EMAIL` value.
///
/// Rejects obviously malformed addresses (missing `@`, embedded whitespace,
/// empty local part, multiple `@`) at startup so a typo doesn't surface only
/// on the first send. Intentionally permissive on everything else — this is a
/// sanity gate, not an RFC 5322 validator. Dotless hostnames (e.g.
/// `user@localhost`, `alerts@intranet`) are accepted because they are
/// legitimate on corporate internal relays.
fn is_valid_email(value: &str) -> bool {
    let at = value.matches('@').count();
    if at != 1 {
        return false;
    }
    let (local, domain) = value.split_once('@').unwrap();
    !local.is_empty() && !domain.is_empty() && !value.contains(char::is_whitespace)
}

/// Read a required setting, returning an error if it is missing or empty.
fn required_setting(settings: &Settings, key: &str) -> AppResult<String> {
    match setting(settings, key) {
        Some(v) if !v.trim().is_empty() => Ok(v.to_owned()),
        _ => {
            let var_name = key.strip_prefix("MAIL_IMAP_").unwrap_or(key);
            let suffix = var_name.split('_').next_back().unwrap_or(var_name);
            Err(AppError::InvalidInput(format!(
                "No IMAP accounts configured. Set MAIL_IMAP_<ID>_HOST/USER/PASS.\nmail-mcp startup error: missing {suffix}."
            )))
        }
    }
}

/// Parse a boolean setting with flexible values.
///
/// Accepts: `1`, `true`, `yes`, `y`, `on` (truthy) or `0`, `false`, `no`,
/// `n`, `off` (falsy). Case-insensitive. Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the setting is present but unrecognized. The
/// value is omitted so a misplaced secret is not echoed.
fn parse_bool_setting(settings: &Settings, key: &str, default: bool) -> AppResult<bool> {
    match setting(settings, key) {
        Some(v) => parse_bool_value(v)
            .ok_or_else(|| AppError::InvalidInput(format!("invalid boolean setting {key}"))),
        None => Ok(default),
    }
}

/// Parse and canonicalize `MAIL_ATTACHMENT_UPLOAD_DIR`
///
/// Canonicalizing once at startup resolves symlinks (e.g. macOS
/// `/tmp` -> `/private/tmp`) so per-call containment checks compare real
/// paths.
///
/// # Errors
///
/// Returns `InvalidInput` if the value is set but does not name an existing
/// directory.
fn parse_upload_dir(value: Option<&str>) -> AppResult<Option<PathBuf>> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let dir = std::fs::canonicalize(raw).map_err(|_| {
        AppError::InvalidInput("MAIL_ATTACHMENT_UPLOAD_DIR is not usable".to_owned())
    })?;
    if !dir.is_dir() {
        return Err(AppError::InvalidInput(
            "MAIL_ATTACHMENT_UPLOAD_DIR is not a directory".to_owned(),
        ));
    }
    Ok(Some(dir))
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Parse an optional boolean setting.
///
/// Returns `Ok(None)` when the setting is unset or empty, distinguishing
/// "not configured" from an explicit `true`/`false`. Used for settings with
/// a computed (non-constant) default, such as per-account / provider-aware
/// `SAVE_SENT`.
///
/// # Errors
///
/// Returns `InvalidInput` if the setting is present but unrecognized.
fn parse_opt_bool_setting(settings: &Settings, key: &str) -> AppResult<Option<bool>> {
    match setting(settings, key) {
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) => parse_bool_value(v)
            .map(Some)
            .ok_or_else(|| AppError::InvalidInput(format!("invalid boolean setting {key}"))),
        None => Ok(None),
    }
}

/// Parse a `u16` setting, returning `default` when unset.
fn parse_u16_setting(settings: &Settings, key: &str, default: u16) -> AppResult<u16> {
    match setting(settings, key) {
        Some(v) => v
            .trim()
            .parse::<u16>()
            .map_err(|_| AppError::InvalidInput(format!("invalid u16 setting {key}"))),
        None => Ok(default),
    }
}

/// Parse a `u64` setting, returning `default` when unset.
fn parse_u64_setting(settings: &Settings, key: &str, default: u64) -> AppResult<u64> {
    match setting(settings, key) {
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| AppError::InvalidInput(format!("invalid u64 setting {key}"))),
        None => Ok(default),
    }
}

/// Default for `MAIL_IMAP_SEARCH_CONCURRENCY`.
pub const DEFAULT_SEARCH_CONCURRENCY: usize = 4;

/// Upper bound for `MAIL_IMAP_SEARCH_CONCURRENCY`; providers cap concurrent
/// IMAP connections per account (iCloud and Gmail at around 10-15).
pub const MAX_SEARCH_CONCURRENCY: usize = 8;

/// Default for `MAIL_IMAP_MAX_MAILBOXES` (the historical fixed limit).
pub const DEFAULT_MAX_MAILBOXES: usize = 200;

/// Upper bound for `MAIL_IMAP_MAX_MAILBOXES`, keeping responses bounded.
pub const MAX_MAILBOXES_LIMIT: usize = 10_000;

/// Clamp a configured mailbox limit to `1..=MAX_MAILBOXES_LIMIT`.
fn clamp_max_mailboxes(n: usize) -> usize {
    n.clamp(1, MAX_MAILBOXES_LIMIT)
}

/// Parse a `usize` setting, returning `default` when unset.
fn parse_usize_setting(settings: &Settings, key: &str, default: usize) -> AppResult<usize> {
    match setting(settings, key) {
        Some(v) => v
            .trim()
            .parse::<usize>()
            .map_err(|_| AppError::InvalidInput(format!("invalid usize setting {key}"))),
        None => Ok(default),
    }
}

/// Read an optional `u64` setting. Empty and missing values are `None`.
fn setting_opt_u64(settings: &Settings, key: &str) -> AppResult<Option<u64>> {
    match setting(settings, key) {
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| AppError::InvalidInput(format!("invalid u64 setting {key}"))),
        None => Ok(None),
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
        AuthMethod, DEFAULT_MAX_MAILBOXES, MAX_MAILBOXES_LIMIT, ServerConfig, clamp_max_mailboxes,
        is_valid_email, parse_bool_value, parse_upload_dir, provider_auto_saves_sent,
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
                from_email: None,
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
            drafts_enabled: true,
            connect_timeout_ms: 5_000,
            greeting_timeout_ms: 5_000,
            socket_timeout_ms: 15_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 512,
            max_mailboxes: DEFAULT_MAX_MAILBOXES,
            search_concurrency: super::DEFAULT_SEARCH_CONCURRENCY,
            attachment_download_dir: None,
            attachment_upload_dir: None,
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
    fn upload_dir_unset_or_blank_means_unrestricted() {
        assert_eq!(parse_upload_dir(None).unwrap(), None);
        assert_eq!(parse_upload_dir(Some("  ")).unwrap(), None);
    }

    #[test]
    fn upload_dir_is_canonicalized() {
        let tmp = std::env::temp_dir();
        let dir = parse_upload_dir(tmp.to_str()).unwrap().unwrap();
        assert_eq!(dir, std::fs::canonicalize(&tmp).unwrap());
    }

    #[test]
    fn upload_dir_rejects_missing_and_non_directory_paths() {
        assert!(parse_upload_dir(Some("/definitely/not/a/real/dir")).is_err());
        let file = std::env::temp_dir().join(format!("upload-dir-file-{}", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"x").unwrap();
        assert!(parse_upload_dir(file.to_str()).is_err());
        std::fs::remove_file(&file).unwrap();
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

    #[test]
    fn is_valid_email_accepts_well_formed_addresses() {
        assert!(is_valid_email("user@example.com"));
        assert!(is_valid_email("alice.bob@example.org"));
        assert!(is_valid_email("team@company.com"));
        assert!(is_valid_email("a.b.c@sub.domain.example.org"));
        assert!(is_valid_email("user+tag@gmail.com"));
    }

    #[test]
    fn is_valid_email_rejects_missing_at_sign() {
        assert!(!is_valid_email("userexample.com"));
        assert!(!is_valid_email("alice.bobexample.org"));
    }

    #[test]
    fn is_valid_email_rejects_multiple_at_signs() {
        assert!(!is_valid_email("user@name@example.com"));
        assert!(!is_valid_email("@@example.com"));
    }

    #[test]
    fn is_valid_email_rejects_whitespace() {
        assert!(!is_valid_email("user @example.com"));
        assert!(!is_valid_email("user@ example.com"));
        assert!(!is_valid_email("user@example.com "));
    }

    #[test]
    fn is_valid_email_accepts_dotless_internal_hosts() {
        // Corporate internal relays use dotless hostnames — must not be
        // rejected (see PR #29 follow-up comment).
        assert!(is_valid_email("noreply@localhost"));
        assert!(is_valid_email("alerts@intranet"));
        assert!(is_valid_email("user@example"));
    }

    #[test]
    fn is_valid_email_rejects_empty_local_part() {
        assert!(!is_valid_email("@example.com"));
        assert!(!is_valid_email(""));
    }

    #[test]
    fn max_mailboxes_is_clamped() {
        assert_eq!(DEFAULT_MAX_MAILBOXES, 200);
        assert_eq!(clamp_max_mailboxes(0), 1);
        assert_eq!(clamp_max_mailboxes(1000), 1000);
        assert_eq!(clamp_max_mailboxes(usize::MAX), MAX_MAILBOXES_LIMIT);
    }

    fn settings(pairs: &[(&str, &str)]) -> crate::config_file::Settings {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn smtp_graph_and_ews_only_accounts_load_without_imap() {
        let secret = "super-secret-token-value";
        let smtp = ServerConfig::load(&settings(&[
            ("MAIL_SMTP_WORK_HOST", "smtp.example.com"),
            ("MAIL_SMTP_WORK_USER", "user@example.com"),
            ("MAIL_SMTP_WORK_PASS", secret),
        ]))
        .unwrap();
        assert!(smtp.accounts.is_empty());
        assert!(smtp.smtp_accounts.contains_key("work"));
        assert_eq!(smtp.smtp_accounts["work"].port, 587);
        assert_eq!(smtp.smtp_accounts["work"].security, SmtpSecurity::Starttls);

        let graph = ServerConfig::load(&settings(&[
            ("MAIL_GRAPH_WORK_PROVIDER", "microsoft"),
            ("MAIL_GRAPH_WORK_CLIENT_ID", "id"),
            ("MAIL_GRAPH_WORK_CLIENT_SECRET", secret),
            ("MAIL_GRAPH_WORK_REFRESH_TOKEN", "refresh"),
        ]))
        .unwrap();
        assert!(graph.accounts.is_empty());
        assert!(graph.graph_oauth2_accounts.contains_key("work"));
        assert!(graph.smtp_accounts.is_empty());

        let ews = ServerConfig::load(&settings(&[
            ("MAIL_EWS_WORK_USER", "ews@example.com"),
            ("MAIL_EWS_WORK_REFRESH_TOKEN", "refresh"),
        ]))
        .unwrap();
        assert!(ews.accounts.is_empty());
        assert!(ews.ews_accounts.contains_key("work"));
        assert!(ews.ews_oauth2_accounts.contains_key("work"));

        let imap = ServerConfig::load(&settings(&[
            ("MAIL_IMAP_WORK_HOST", "imap.example.com"),
            ("MAIL_IMAP_WORK_USER", "user@example.com"),
            ("MAIL_IMAP_WORK_PASS", secret),
        ]))
        .unwrap();
        assert!(imap.accounts.contains_key("work"));
        assert_eq!(imap.accounts["work"].port, 993);
        assert!(imap.accounts["work"].secure);
        assert_eq!(imap.accounts["work"].auth_method, AuthMethod::Password);
    }

    #[test]
    fn missing_every_account_still_requires_default_imap() {
        let secret = "super-secret-token-value";
        let error = ServerConfig::load(&settings(&[("MAIL_IMAP_WORK_PASS", secret)])).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(ServerConfig::load(&crate::config_file::Settings::new()).is_err());
        assert!(
            ServerConfig::load(&settings(&[
                ("MAIL_OAUTH2_WORK_PROVIDER", "google"),
                ("MAIL_OAUTH2_WORK_CLIENT_ID", "id"),
                ("MAIL_OAUTH2_WORK_CLIENT_SECRET", secret),
                ("MAIL_OAUTH2_WORK_REFRESH_TOKEN", "refresh"),
            ]))
            .is_err()
        );
    }

    #[test]
    fn invalid_setting_does_not_echo_another_fields_secret() {
        let secret = "super-secret-token-value";
        let error = ServerConfig::load(&settings(&[
            ("MAIL_IMAP_WORK_HOST", "imap.example.com"),
            ("MAIL_IMAP_WORK_USER", "user@example.com"),
            ("MAIL_IMAP_WORK_PASS", secret),
            ("MAIL_IMAP_WRITE_ENABLED", "nope"),
        ]))
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("MAIL_IMAP_WRITE_ENABLED"));
        assert!(!message.contains(secret));
    }

    #[test]
    fn work_and_work_underscore_keep_isolated_credentials() {
        use secrecy::ExposeSecret;
        let work_secret = "secret-for-work";
        let trail_secret = "secret-for-work-underscore";
        let loaded = ServerConfig::load(&settings(&[
            ("MAIL_IMAP_WORK_HOST", "imap.work.example"),
            ("MAIL_IMAP_WORK_USER", "work@example.com"),
            ("MAIL_IMAP_WORK_PASS", work_secret),
            ("MAIL_IMAP_WORK__HOST", "imap.trail.example"),
            ("MAIL_IMAP_WORK__USER", "trail@example.com"),
            ("MAIL_IMAP_WORK__PASS", trail_secret),
        ]))
        .unwrap();
        assert_eq!(loaded.accounts["work"].host, "imap.work.example");
        assert_eq!(loaded.accounts["work_"].host, "imap.trail.example");
        assert_eq!(
            loaded.accounts["work"]
                .pass
                .as_ref()
                .unwrap()
                .expose_secret(),
            work_secret
        );
        assert_eq!(
            loaded.accounts["work_"]
                .pass
                .as_ref()
                .unwrap()
                .expose_secret(),
            trail_secret
        );
    }

    #[test]
    fn invalid_provider_security_and_upload_dir_omit_raw_values() {
        let secret = "super-secret-token-value";
        let provider = ServerConfig::load(&settings(&[
            ("MAIL_OAUTH2_WORK_PROVIDER", secret),
            ("MAIL_OAUTH2_WORK_CLIENT_ID", "id"),
            ("MAIL_OAUTH2_WORK_CLIENT_SECRET", "c"),
            ("MAIL_OAUTH2_WORK_REFRESH_TOKEN", "r"),
        ]))
        .unwrap_err();
        let message = provider.to_string();
        assert!(message.contains("MAIL_OAUTH2_WORK_PROVIDER"));
        assert!(!message.contains(secret));

        let security = ServerConfig::load(&settings(&[
            ("MAIL_SMTP_WORK_HOST", "smtp.example.com"),
            ("MAIL_SMTP_WORK_USER", "user@example.com"),
            ("MAIL_SMTP_WORK_SECURE", secret),
        ]))
        .unwrap_err();
        let message = security.to_string();
        assert!(message.contains("MAIL_SMTP_WORK_SECURE"));
        assert!(!message.contains(secret));

        let missing = format!("/no/such/{secret}");
        let upload = parse_upload_dir(Some(&missing)).unwrap_err();
        let message = upload.to_string();
        assert!(message.contains("MAIL_ATTACHMENT_UPLOAD_DIR"));
        assert!(!message.contains(secret));
    }
}
