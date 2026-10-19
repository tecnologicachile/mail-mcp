//! Terminal account prompts and comment-preserving TOML edits.
//!
//! Secrets are accepted only from a hidden terminal prompt. This module never
//! prints secret values, and a cancelled prompt returns before the caller saves.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::path::Path;

use dialoguer::{Confirm, Input, Password, Select};
use toml_edit::{Decor, DocumentMut, Item, Table, TableLike, Value};

use crate::config_file::{
    account_id_from_env_key, is_account_env_key, is_config_key, valid_account_id,
};
use crate::errors::AppError;
use crate::oauth2::OAuth2Provider;
use crate::smtp::SmtpSecurity;

const INITIAL_CONFIG: &str = "\
# mail-mcp configuration.
# Remove the leading # on a settings line to override that default.
# Environment variables override this file.
# Add an account with `mail-mcp accounts add`.

[settings]
# write_enabled = false
# drafts_enabled = false
# smtp_write_enabled = false
# smtp_save_sent stays unset so each provider keeps its own default
# smtp_connect_timeout_ms = 30000
# smtp_send_timeout_ms = 300000
# connect_timeout_ms = 30000
# greeting_timeout_ms = 15000
# socket_timeout_ms = 300000
# cursor_ttl_seconds = 600
# cursor_max_entries = 512
# max_mailboxes = 200
# search_concurrency = 4
# attachment_download_dir stays unset and downloads use the system temp directory
# attachment_upload_dir stays unset and does not restrict attachment paths
# transport = \"stdio\"
# http_host = \"127.0.0.1\"
# http_port = 8000
# http_path = \"/mcp\"

[accounts]
";

/// Public Microsoft Office client id used when an EWS client id is unset.
const EWS_PUBLIC_CLIENT_ID: &str = "d3590ed6-52b3-4102-aeff-aad2292ab01c";

/// Public Graph client id documented for personal Microsoft accounts.
const GRAPH_PUBLIC_CLIENT_ID: &str = "9e5f94bc-e8a4-4e73-b8be-63364c29d753";

#[derive(Debug)]
pub(crate) struct CliFailure {
    message: String,
}

impl CliFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::new("Cancelled. No changes were written.")
    }
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliFailure {}

/// Rejection for an account id that failed [`valid_account_id`].
///
/// The rejected text is not included. A valid id may still be shown by the caller.
pub(crate) const INVALID_ACCOUNT_ID: &str = "account id is not valid; use 1 to 64 lowercase letters, digits, and underscores, starting with a letter or digit";

/// Keep loader and schema errors. Schema text is already `unknown key at line N`,
/// `invalid account id at line N`, `expected a string at line N`, or
/// `expected a table at line N`, with no quoted token. Env key names such as
/// `MAIL_OAUTH2_WORK_CLIENT_SECRET` stay. Only assigned secret values, bearer
/// tokens, and JWT-shaped text are removed.
pub(crate) fn public_text(text: &str) -> String {
    redact_jwt(&redact_bearer(&redact_assignments(text)))
}

const SECRET_KEYS: &[&str] = &["refresh_token", "client_secret", "password", "pass"];

fn redact_assignments(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if let Some((value_start, value_end)) = assignment_at(&lower, index) {
            out.push_str(&text[index..value_start]);
            out.push_str("[redacted]");
            index = value_end;
            continue;
        }
        let ch = text[index..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// A secret-key assignment at `index`, if the value starts there.
///
/// The key itself is not part of the returned span.
fn assignment_at(lower: &str, index: usize) -> Option<(usize, usize)> {
    if index > 0 {
        let previous = lower.as_bytes()[index - 1];
        if previous.is_ascii_alphanumeric() || previous == b'_' {
            return None;
        }
    }
    let rest = &lower[index..];
    let key = SECRET_KEYS.iter().find(|key| rest.starts_with(*key))?;
    let bytes = lower.as_bytes();
    let mut cursor = index + key.len();
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if cursor >= bytes.len() || bytes[cursor] != b'=' {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    let value_start = cursor;
    let value_end = if bytes[cursor] == b'"' || bytes[cursor] == b'\'' {
        let quote = bytes[cursor];
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        if cursor < bytes.len() {
            cursor + 1
        } else {
            cursor
        }
    } else {
        while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        cursor
    };
    (value_end > value_start).then_some((value_start, value_end))
}

fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let needle = "bearer ";
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(found) = lower[index..].find(needle) {
        let start = index + found;
        let token_start = start + needle.len();
        let mut token_end = token_start;
        let bytes = text.as_bytes();
        while token_end < bytes.len() && !bytes[token_end].is_ascii_whitespace() {
            token_end += 1;
        }
        out.push_str(&text[index..token_start]);
        if token_end > token_start {
            out.push_str("[redacted]");
            index = token_end;
        } else {
            index = token_start;
        }
    }
    out.push_str(&text[index..]);
    out
}

fn redact_jwt(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(found) = text[index..].find("eyJ") {
        let start = index + found;
        let bytes = text.as_bytes();
        let mut end = start;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte.is_ascii_whitespace()
                || matches!(byte, b'"' | b'\'' | b',' | b')' | b']' | b'}')
            {
                break;
            }
            end += 1;
        }
        out.push_str(&text[index..start]);
        out.push_str("[redacted]");
        index = end;
    }
    out.push_str(&text[index..]);
    out
}

pub(crate) fn with_unchanged(err: CliFailure) -> CliFailure {
    if err.message.contains("No changes were written") {
        err
    } else {
        CliFailure::new(format!("{}\nNo changes were written.", err.message))
    }
}

pub(crate) fn initial_document() -> Result<DocumentMut, CliFailure> {
    INITIAL_CONFIG
        .parse::<DocumentMut>()
        .map_err(|_| CliFailure::new("the built-in config template could not be prepared"))
}

pub(crate) enum SecretInput {
    Keep,
    Set(String),
}

enum OptionalString {
    Keep,
    Clear,
    Set(String),
}

enum SentPolicy {
    ProviderDefault,
    Save,
    Skip,
}

struct ImapFields {
    host: String,
    port: u16,
    secure: bool,
    user: String,
    pass: Option<SecretInput>,
}

struct SmtpFields {
    host: String,
    port: u16,
    user: String,
    pass: Option<SecretInput>,
    security: String,
    save_sent: SentPolicy,
    from_email: OptionalString,
}

struct OAuthFields {
    provider: String,
    client_id: String,
    client_secret: SecretInput,
    refresh_token: SecretInput,
}

struct EwsFields {
    user: String,
    client_id: String,
    client_secret: SecretInput,
    refresh_token: SecretInput,
}

pub(crate) struct AccountDraft {
    pub(crate) id: String,
    imap: Option<ImapFields>,
    smtp: Option<SmtpFields>,
    oauth2: Option<OAuthFields>,
    graph: Option<OAuthFields>,
    ews: Option<EwsFields>,
}

#[derive(Clone, Default)]
struct Protocols {
    imap: bool,
    smtp: bool,
    oauth2: bool,
    graph: bool,
    ews: bool,
}

impl Protocols {
    fn any(&self) -> bool {
        self.imap || self.smtp || self.oauth2 || self.graph || self.ews
    }

    fn or_with(&mut self, other: &Protocols) {
        self.imap |= other.imap;
        self.smtp |= other.smtp;
        self.oauth2 |= other.oauth2;
        self.graph |= other.graph;
        self.ews |= other.ews;
    }

    fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.imap {
            parts.push("imap");
        }
        if self.smtp {
            parts.push("smtp");
        }
        if self.oauth2 {
            parts.push("oauth2");
        }
        if self.graph {
            parts.push("graph");
        }
        if self.ews {
            parts.push("ews");
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

pub(crate) fn require_terminal() -> Result<(), CliFailure> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        Ok(())
    } else {
        Err(CliFailure::new(
            "This command needs an interactive terminal. Secrets are not read from arguments or pipes.\nNo changes were written.",
        ))
    }
}

fn prompt_failure(err: dialoguer::Error) -> CliFailure {
    match err {
        dialoguer::Error::IO(io_err) if io_err.kind() == io::ErrorKind::Interrupted => {
            CliFailure::cancelled()
        }
        dialoguer::Error::IO(io_err) if io_err.kind() == io::ErrorKind::NotConnected => {
            CliFailure::new(
                "This command needs an interactive terminal. Secrets are not read from arguments or pipes.\nNo changes were written.",
            )
        }
        dialoguer::Error::IO(_) => {
            CliFailure::new("Could not read an answer from the terminal.\nNo changes were written.")
        }
    }
}

fn prompt_text(
    prompt: &str,
    default: Option<&str>,
    allow_empty: bool,
) -> Result<String, CliFailure> {
    let mut input = Input::<String>::new()
        .with_prompt(prompt)
        .allow_empty(allow_empty);
    if let Some(value) = default {
        input = input.default(value.to_owned());
    }
    input.interact_text().map_err(prompt_failure)
}

fn prompt_port(prompt: &str, default: u16) -> Result<u16, CliFailure> {
    loop {
        let port = Input::<u16>::new()
            .with_prompt(prompt)
            .default(default)
            .interact_text()
            .map_err(prompt_failure)?;
        if port > 0 {
            return Ok(port);
        }
        eprintln!("Port must be between 1 and 65535.");
    }
}

fn prompt_select(prompt: &str, items: &[&str], default: usize) -> Result<usize, CliFailure> {
    Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact_opt()
        .map_err(prompt_failure)?
        .ok_or_else(CliFailure::cancelled)
}

fn prompt_bool(prompt: &str, default: bool) -> Result<bool, CliFailure> {
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact_opt()
        .map_err(prompt_failure)?
        .ok_or_else(CliFailure::cancelled)
}

fn prompt_secret(prompt: &str, allow_empty: bool) -> Result<String, CliFailure> {
    Password::new()
        .with_prompt(prompt)
        .report(false)
        .allow_empty_password(allow_empty)
        .interact()
        .map_err(prompt_failure)
}

fn prompt_new_secret(prompt: &str) -> Result<String, CliFailure> {
    prompt_secret_twice(prompt, false, None)
}

/// A blank entry stores `empty_value` (used for a public OAuth client).
fn prompt_public_or_secret(prompt: &str) -> Result<String, CliFailure> {
    prompt_secret_twice(prompt, true, Some("none"))
}

fn prompt_secret_twice(
    prompt: &str,
    allow_empty: bool,
    empty_value: Option<&str>,
) -> Result<String, CliFailure> {
    loop {
        let first = prompt_secret(prompt, allow_empty)?;
        if first.is_empty()
            && let Some(empty_value) = empty_value
        {
            return Ok(empty_value.to_owned());
        }
        let second = prompt_secret("Enter it again", false)?;
        if first == second {
            return Ok(first);
        }
        eprintln!("Entries did not match.");
    }
}

fn prompt_edit_secret(prompt: &str) -> Result<SecretInput, CliFailure> {
    let prompt = format!("{prompt} (blank keeps the current value)");
    loop {
        let first = prompt_secret(&prompt, true)?;
        if first.is_empty() {
            return Ok(SecretInput::Keep);
        }
        let second = prompt_secret("Enter it again (blank keeps the current value)", true)?;
        if second.is_empty() {
            return Ok(SecretInput::Keep);
        }
        if first == second {
            return Ok(SecretInput::Set(first));
        }
        eprintln!("Entries did not match.");
    }
}

fn prompt_required(prompt: &str, default: Option<&str>) -> Result<String, CliFailure> {
    loop {
        let value = prompt_text(prompt, default, false)?;
        let value = value.trim().to_owned();
        if !value.is_empty() && !value.chars().any(char::is_whitespace) {
            return Ok(value);
        }
        eprintln!("Enter a value without spaces.");
    }
}

fn looks_like_email(value: &str) -> bool {
    let at = value.matches('@').count();
    if at != 1 {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !value.chars().any(char::is_whitespace)
}

pub(crate) fn account_in_file(doc: &DocumentMut, id: &str) -> bool {
    doc.get("accounts")
        .and_then(Item::as_table_like)
        .and_then(|accounts| accounts.get(id))
        .is_some_and(Item::is_table_like)
}

fn prompt_new_id(doc: &DocumentMut) -> Result<String, CliFailure> {
    loop {
        let id = prompt_text(
            "Account id (lowercase letters, digits, and underscore; start with a letter or digit)",
            None,
            false,
        )?;
        let id = id.trim().to_owned();
        if !valid_account_id(&id) {
            eprintln!("{INVALID_ACCOUNT_ID}");
            continue;
        }
        if account_in_file(doc, &id) {
            eprintln!("Account '{id}' is already in the config file. Use accounts edit.");
            continue;
        }
        return Ok(id);
    }
}

struct HostChoice {
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    security: String,
    provider: Option<String>,
}

fn prompt_password_hosts() -> Result<HostChoice, CliFailure> {
    let choice = prompt_select(
        "Mail provider",
        &[
            "Gmail (imap.gmail.com:993, smtp.gmail.com:587)",
            "iCloud (imap.mail.me.com:993, smtp.mail.me.com:587)",
            "Microsoft 365 (outlook.office365.com:993, smtp.office365.com:587)",
            "Other",
        ],
        0,
    )?;
    match choice {
        0 => Ok(HostChoice {
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".to_owned(),
            smtp_port: 587,
            security: "starttls".to_owned(),
            provider: None,
        }),
        1 => Ok(HostChoice {
            imap_host: "imap.mail.me.com".to_owned(),
            imap_port: 993,
            smtp_host: "smtp.mail.me.com".to_owned(),
            smtp_port: 587,
            security: "starttls".to_owned(),
            provider: None,
        }),
        2 => Ok(HostChoice {
            imap_host: "outlook.office365.com".to_owned(),
            imap_port: 993,
            smtp_host: "smtp.office365.com".to_owned(),
            smtp_port: 587,
            security: "starttls".to_owned(),
            provider: None,
        }),
        _ => prompt_custom_hosts(None),
    }
}

fn prompt_oauth_hosts() -> Result<HostChoice, CliFailure> {
    let choice = prompt_select(
        "OAuth2 provider",
        &[
            "Gmail (google, imap.gmail.com, smtp.gmail.com)",
            "Microsoft 365 (microsoft, outlook.office365.com, smtp.office365.com)",
            "Other",
        ],
        0,
    )?;
    match choice {
        0 => Ok(HostChoice {
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".to_owned(),
            smtp_port: 587,
            security: "starttls".to_owned(),
            provider: Some("google".to_owned()),
        }),
        1 => Ok(HostChoice {
            imap_host: "outlook.office365.com".to_owned(),
            imap_port: 993,
            smtp_host: "smtp.office365.com".to_owned(),
            smtp_port: 587,
            security: "starttls".to_owned(),
            provider: Some("microsoft".to_owned()),
        }),
        _ => {
            let provider = prompt_provider(None)?;
            prompt_custom_hosts(Some(provider))
        }
    }
}

fn prompt_custom_hosts(provider: Option<String>) -> Result<HostChoice, CliFailure> {
    let imap_host = prompt_required("IMAP host", None)?;
    let imap_port = prompt_port("IMAP port", 993)?;
    let smtp_host = prompt_required("SMTP host", None)?;
    let smtp_port = prompt_port("SMTP port", 587)?;
    let security = prompt_security(None)?;
    Ok(HostChoice {
        imap_host,
        imap_port,
        smtp_host,
        smtp_port,
        security,
        provider,
    })
}

const PROVIDER_ITEMS: &[&str] = &["google", "microsoft"];
const SECURITY_ITEMS: &[&str] = &["starttls", "tls", "plain"];

fn prompt_provider(current: Option<&str>) -> Result<String, CliFailure> {
    let choice = prompt_select(
        "OAuth2 provider",
        PROVIDER_ITEMS,
        provider_default_index(current),
    )?;
    Ok(PROVIDER_ITEMS[choice].to_owned())
}

fn prompt_security(current: Option<&str>) -> Result<String, CliFailure> {
    let choice = prompt_select(
        "SMTP security",
        SECURITY_ITEMS,
        security_default_index(current),
    )?;
    Ok(SECURITY_ITEMS[choice].to_owned())
}

/// Menu index for the provider `OAuth2Provider::parse` accepts.
///
/// `outlook`, `office365`, and `MICROSOFT` highlight `microsoft`. An unknown
/// value stays on the first item only because it is not a provider.
fn provider_default_index(current: Option<&str>) -> usize {
    let Some(current) = current else {
        return 0;
    };
    let Ok(provider) = OAuth2Provider::parse(current) else {
        return 0;
    };
    let label = match provider {
        OAuth2Provider::Google => "google",
        OAuth2Provider::Microsoft => "microsoft",
    };
    PROVIDER_ITEMS
        .iter()
        .position(|item| *item == label)
        .unwrap_or(0)
}

/// Menu index for the mode `SmtpSecurity::parse` accepts.
///
/// `ssl` and `TLS` highlight `tls`. `none` highlights `plain`. Accepting the
/// highlight keeps that mode instead of rewriting it to `starttls`.
fn security_default_index(current: Option<&str>) -> usize {
    let Some(current) = current else {
        return 0;
    };
    let Ok(mode) = SmtpSecurity::parse(current) else {
        return 0;
    };
    let label = match mode {
        SmtpSecurity::Starttls => "starttls",
        SmtpSecurity::Tls => "tls",
        SmtpSecurity::Plain => "plain",
    };
    SECURITY_ITEMS
        .iter()
        .position(|item| *item == label)
        .unwrap_or(0)
}

fn prompt_sent_policy(current: Option<bool>) -> Result<SentPolicy, CliFailure> {
    let default = match current {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    let choice = prompt_select(
        "Save sent mail",
        &[
            "Provider default",
            "Save a copy to Sent",
            "Do not save a copy",
        ],
        default,
    )?;
    Ok(match choice {
        1 => SentPolicy::Save,
        2 => SentPolicy::Skip,
        _ => SentPolicy::ProviderDefault,
    })
}

fn prompt_from_email(current: Option<&str>) -> Result<OptionalString, CliFailure> {
    loop {
        let entered = if let Some(current) = current {
            prompt_text("From address (- clears)", Some(current), false)?
        } else {
            prompt_text("From address (blank omits it)", None, true)?
        };
        if entered.is_empty() {
            return Ok(if current.is_some() {
                OptionalString::Keep
            } else {
                OptionalString::Clear
            });
        }
        if entered == "-" {
            return Ok(OptionalString::Clear);
        }
        if !looks_like_email(&entered) {
            eprintln!("From address must contain one @ and no spaces.");
            continue;
        }
        if current == Some(entered.as_str()) {
            return Ok(OptionalString::Keep);
        }
        return Ok(OptionalString::Set(entered));
    }
}

fn confirm_save(summary: &str) -> Result<(), CliFailure> {
    eprint!("{summary}");
    let answer = Confirm::new()
        .with_prompt("Save this account")
        .default(true)
        .interact_opt()
        .map_err(prompt_failure)?;
    match answer {
        Some(true) => Ok(()),
        _ => Err(CliFailure::cancelled()),
    }
}

fn hidden_label(secret: &SecretInput) -> &'static str {
    match secret {
        SecretInput::Keep => "unchanged",
        SecretInput::Set(_) => "hidden",
    }
}

pub(crate) fn summary(draft: &AccountDraft) -> String {
    let mut out = format!("Account {}\n", draft.id);
    if let Some(imap) = &draft.imap {
        out.push_str(&format!(
            "  imap: {}:{} secure={} user={}\n",
            imap.host, imap.port, imap.secure, imap.user
        ));
        if let Some(pass) = &imap.pass {
            out.push_str(&format!("  imap password: {}\n", hidden_label(pass)));
        }
    }
    if let Some(smtp) = &draft.smtp {
        out.push_str(&format!(
            "  smtp: {}:{} security={} user={}\n",
            smtp.host, smtp.port, smtp.security, smtp.user
        ));
        if let Some(pass) = &smtp.pass {
            out.push_str(&format!("  smtp password: {}\n", hidden_label(pass)));
        }
        out.push_str("  save sent: ");
        out.push_str(match smtp.save_sent {
            SentPolicy::ProviderDefault => "provider default\n",
            SentPolicy::Save => "yes\n",
            SentPolicy::Skip => "no\n",
        });
        match &smtp.from_email {
            OptionalString::Set(value) => out.push_str(&format!("  from: {value}\n")),
            OptionalString::Clear => out.push_str("  from: omitted\n"),
            OptionalString::Keep => {}
        }
    }
    if let Some(oauth) = &draft.oauth2 {
        out.push_str(&format!(
            "  oauth2: provider={} client_id={} client_secret={} refresh_token={}\n",
            oauth.provider,
            oauth.client_id,
            hidden_label(&oauth.client_secret),
            hidden_label(&oauth.refresh_token)
        ));
    }
    if let Some(graph) = &draft.graph {
        out.push_str(&format!(
            "  graph: provider={} client_id={} client_secret={} refresh_token={}\n",
            graph.provider,
            graph.client_id,
            hidden_label(&graph.client_secret),
            hidden_label(&graph.refresh_token)
        ));
    }
    if let Some(ews) = &draft.ews {
        out.push_str(&format!(
            "  ews: user={} client_id={} client_secret={} refresh_token={}\n",
            ews.user,
            ews.client_id,
            hidden_label(&ews.client_secret),
            hidden_label(&ews.refresh_token)
        ));
    }
    out
}

pub(crate) fn prompt_add(doc: &DocumentMut) -> Result<AccountDraft, CliFailure> {
    let kind = prompt_select(
        "Account type",
        &[
            "IMAP and SMTP with a password",
            "IMAP and SMTP with OAuth2",
            "Microsoft Graph",
            "Exchange Web Services (EWS)",
        ],
        0,
    )?;
    let id = prompt_new_id(doc)?;
    let draft = match kind {
        0 => prompt_password_account(id)?,
        1 => prompt_oauth_account(id)?,
        2 => prompt_graph_account(id)?,
        _ => prompt_ews_account(id)?,
    };
    confirm_save(&summary(&draft))?;
    Ok(draft)
}

fn prompt_password_account(id: String) -> Result<AccountDraft, CliFailure> {
    let hosts = prompt_password_hosts()?;
    let secure = if hosts.imap_host == "imap.gmail.com"
        || hosts.imap_host == "imap.mail.me.com"
        || hosts.imap_host == "outlook.office365.com"
    {
        true
    } else {
        prompt_bool("IMAP TLS", true)?
    };
    let user = prompt_required("Email address or username", None)?;
    let smtp_user = prompt_text("SMTP username (blank uses the username above)", None, true)?;
    let smtp_user = if smtp_user.trim().is_empty() {
        user.clone()
    } else {
        smtp_user.trim().to_owned()
    };
    let password = prompt_new_secret("Password")?;
    let smtp_password = loop {
        let entered = prompt_secret("SMTP password (blank uses the password above)", true)?;
        if entered.is_empty() {
            break password.clone();
        }
        let confirm = prompt_secret("Enter the SMTP password again", false)?;
        if confirm == entered {
            break entered;
        }
        eprintln!("Entries did not match.");
    };
    let save_sent = prompt_sent_policy(None)?;
    let from_email = prompt_from_email(None)?;
    Ok(AccountDraft {
        id,
        imap: Some(ImapFields {
            host: hosts.imap_host,
            port: hosts.imap_port,
            secure,
            user,
            pass: Some(SecretInput::Set(password)),
        }),
        smtp: Some(SmtpFields {
            host: hosts.smtp_host,
            port: hosts.smtp_port,
            user: smtp_user,
            pass: Some(SecretInput::Set(smtp_password)),
            security: hosts.security,
            save_sent,
            from_email,
        }),
        oauth2: None,
        graph: None,
        ews: None,
    })
}

fn prompt_oauth_account(id: String) -> Result<AccountDraft, CliFailure> {
    let hosts = prompt_oauth_hosts()?;
    let provider = hosts
        .provider
        .clone()
        .map(Ok)
        .unwrap_or_else(|| prompt_provider(None))?;
    let user = prompt_required("Email address or username", None)?;
    let smtp_user = prompt_text("SMTP username (blank uses the username above)", None, true)?;
    let smtp_user = if smtp_user.trim().is_empty() {
        user.clone()
    } else {
        smtp_user.trim().to_owned()
    };
    let client_id = prompt_required("OAuth2 client id", None)?;
    let client_secret =
        prompt_public_or_secret("OAuth2 client secret (blank stores none for a public client)")?;
    let refresh_token = prompt_new_secret("OAuth2 refresh token")?;
    let save_sent = prompt_sent_policy(None)?;
    let from_email = prompt_from_email(None)?;
    Ok(AccountDraft {
        id,
        imap: Some(ImapFields {
            host: hosts.imap_host,
            port: hosts.imap_port,
            secure: true,
            user,
            pass: None,
        }),
        smtp: Some(SmtpFields {
            host: hosts.smtp_host,
            port: hosts.smtp_port,
            user: smtp_user,
            pass: None,
            security: hosts.security,
            save_sent,
            from_email,
        }),
        oauth2: Some(OAuthFields {
            provider,
            client_id,
            client_secret: SecretInput::Set(client_secret),
            refresh_token: SecretInput::Set(refresh_token),
        }),
        graph: None,
        ews: None,
    })
}

fn prompt_graph_account(id: String) -> Result<AccountDraft, CliFailure> {
    eprintln!("Microsoft Graph uses provider microsoft.");
    let client_id = prompt_required("Graph client id", Some(GRAPH_PUBLIC_CLIENT_ID))?;
    let client_secret =
        prompt_public_or_secret("Graph client secret (blank stores none for a public client)")?;
    let refresh_token = prompt_new_secret("Graph refresh token")?;
    Ok(AccountDraft {
        id,
        imap: None,
        smtp: None,
        oauth2: None,
        graph: Some(OAuthFields {
            provider: "microsoft".to_owned(),
            client_id,
            client_secret: SecretInput::Set(client_secret),
            refresh_token: SecretInput::Set(refresh_token),
        }),
        ews: None,
    })
}

fn prompt_ews_account(id: String) -> Result<AccountDraft, CliFailure> {
    let user = prompt_required("EWS user (email address)", None)?;
    let client_id = prompt_required("EWS client id", Some(EWS_PUBLIC_CLIENT_ID))?;
    let client_secret =
        prompt_public_or_secret("EWS client secret (blank stores none for a public client)")?;
    let refresh_token = prompt_new_secret("EWS refresh token")?;
    Ok(AccountDraft {
        id,
        imap: None,
        smtp: None,
        oauth2: None,
        graph: None,
        ews: Some(EwsFields {
            user,
            client_id,
            client_secret: SecretInput::Set(client_secret),
            refresh_token: SecretInput::Set(refresh_token),
        }),
    })
}

pub(crate) fn prompt_edit(doc: &DocumentMut, id: &str) -> Result<AccountDraft, CliFailure> {
    let Some(account) = account_table_ref(doc, id) else {
        return Err(CliFailure::new(format!(
            "account '{id}' is not in the config file"
        )));
    };
    let has_oauth = account.get("oauth2").is_some_and(Item::is_table_like);
    let imap = match account.get("imap").and_then(Item::as_table_like) {
        Some(table) => Some(prompt_edit_imap(table, has_oauth)?),
        None => None,
    };
    let smtp = match account.get("smtp").and_then(Item::as_table_like) {
        Some(table) => Some(prompt_edit_smtp(table, has_oauth)?),
        None => None,
    };
    let oauth2 = match account.get("oauth2").and_then(Item::as_table_like) {
        Some(table) => Some(prompt_edit_oauth("OAuth2", table, None, None)?),
        None => None,
    };
    let graph = match account.get("graph_oauth2").and_then(Item::as_table_like) {
        Some(table) => Some(prompt_edit_oauth(
            "Graph",
            table,
            Some("microsoft"),
            Some(GRAPH_PUBLIC_CLIENT_ID),
        )?),
        None => None,
    };
    let ews = match account.get("ews").and_then(Item::as_table_like) {
        Some(table) => Some(prompt_edit_ews(table)?),
        None => None,
    };
    if imap.is_none() && smtp.is_none() && oauth2.is_none() && graph.is_none() && ews.is_none() {
        return Err(CliFailure::new(format!(
            "account '{id}' has no IMAP, SMTP, OAuth2, Graph, or EWS table to edit"
        )));
    }
    let draft = AccountDraft {
        id: id.to_owned(),
        imap,
        smtp,
        oauth2,
        graph,
        ews,
    };
    confirm_save(&summary(&draft))?;
    Ok(draft)
}

fn prompt_edit_imap(table: &dyn TableLike, has_oauth: bool) -> Result<ImapFields, CliFailure> {
    let host = prompt_required("IMAP host", read_setting(table, "host").as_deref())?;
    let port = prompt_port("IMAP port", read_u16(table, "port").unwrap_or(993))?;
    let secure = prompt_bool("IMAP TLS", read_bool(table, "secure").unwrap_or(true))?;
    let user = prompt_required(
        "Email address or username",
        read_setting(table, "user").as_deref(),
    )?;
    let pass = if table.contains_key("pass") || !has_oauth {
        Some(prompt_edit_secret("IMAP password")?)
    } else {
        None
    };
    Ok(ImapFields {
        host,
        port,
        secure,
        user,
        pass,
    })
}

fn prompt_edit_smtp(table: &dyn TableLike, has_oauth: bool) -> Result<SmtpFields, CliFailure> {
    let host = prompt_required("SMTP host", read_setting(table, "host").as_deref())?;
    let port = prompt_port("SMTP port", read_u16(table, "port").unwrap_or(587))?;
    let user = prompt_required("SMTP username", read_setting(table, "user").as_deref())?;
    let security = prompt_security(read_setting(table, "security").as_deref())?;
    let save_sent = prompt_sent_policy(read_bool(table, "save_sent"))?;
    let from_email = prompt_from_email(read_setting(table, "from_email").as_deref())?;
    let pass = if table.contains_key("pass") || !has_oauth {
        Some(prompt_edit_secret("SMTP password")?)
    } else {
        None
    };
    Ok(SmtpFields {
        host,
        port,
        user,
        pass,
        security,
        save_sent,
        from_email,
    })
}

fn prompt_edit_oauth(
    label: &str,
    table: &dyn TableLike,
    fixed_provider: Option<&str>,
    default_client_id: Option<&str>,
) -> Result<OAuthFields, CliFailure> {
    let provider = if let Some(provider) = fixed_provider {
        if read_setting(table, "provider").as_deref() != Some(provider) {
            eprintln!("{label} uses provider {provider}.");
        }
        provider.to_owned()
    } else {
        prompt_provider(read_setting(table, "provider").as_deref())?
    };
    let client_id = prompt_required(
        &format!("{label} client id"),
        read_setting(table, "client_id")
            .as_deref()
            .or(default_client_id),
    )?;
    let client_secret = prompt_edit_secret(&format!("{label} client secret"))?;
    let refresh_token = prompt_edit_secret(&format!("{label} refresh token"))?;
    Ok(OAuthFields {
        provider,
        client_id,
        client_secret,
        refresh_token,
    })
}

fn prompt_edit_ews(table: &dyn TableLike) -> Result<EwsFields, CliFailure> {
    let user = prompt_required(
        "EWS user (email address)",
        read_setting(table, "user").as_deref(),
    )?;
    let client_id = prompt_required(
        "EWS client id",
        read_setting(table, "client_id")
            .as_deref()
            .or(Some(EWS_PUBLIC_CLIENT_ID)),
    )?;
    let client_secret = prompt_edit_secret("EWS client secret")?;
    let refresh_token = prompt_edit_secret("EWS refresh token")?;
    Ok(EwsFields {
        user,
        client_id,
        client_secret,
        refresh_token,
    })
}

pub(crate) fn confirm_remove(path: &Path, id: &str) -> Result<(), CliFailure> {
    let answer = Confirm::new()
        .with_prompt(format!("Remove account '{id}' from {}?", path.display()))
        .default(false)
        .interact_opt()
        .map_err(prompt_failure)?;
    match answer {
        Some(true) => Ok(()),
        _ => Err(CliFailure::cancelled()),
    }
}

pub(crate) fn apply_account(doc: &mut DocumentMut, draft: &AccountDraft) -> Result<(), CliFailure> {
    if let Some(imap) = &draft.imap {
        write_imap(protocol_table(doc, &draft.id, "imap")?, imap);
    }
    if let Some(smtp) = &draft.smtp {
        write_smtp(protocol_table(doc, &draft.id, "smtp")?, smtp);
    }
    if let Some(oauth) = &draft.oauth2 {
        write_oauth(protocol_table(doc, &draft.id, "oauth2")?, oauth);
    }
    if let Some(graph) = &draft.graph {
        write_oauth(protocol_table(doc, &draft.id, "graph_oauth2")?, graph);
    }
    if let Some(ews) = &draft.ews {
        write_ews(protocol_table(doc, &draft.id, "ews")?, ews);
    }
    Ok(())
}

pub(crate) fn remove_account(doc: &mut DocumentMut, id: &str) -> Result<(), CliFailure> {
    let present = doc
        .get("accounts")
        .and_then(Item::as_table_like)
        .is_some_and(|accounts| accounts.contains_key(id));
    if !present {
        return Err(missing_account(id));
    }
    // toml_edit stores the text before a header on that header. Keep the part
    // that belongs to the previous section, then drop the account.
    relocate_account_prefix(doc, id);
    let removed = doc
        .get_mut("accounts")
        .and_then(Item::as_table_like_mut)
        .is_some_and(|accounts| accounts.remove(id).is_some());
    if !removed {
        return Err(missing_account(id));
    }
    Ok(())
}

fn missing_account(id: &str) -> CliFailure {
    CliFailure::new(format!("account '{id}' is not in the config file"))
}

#[derive(Clone)]
enum PrefixTarget {
    Header {
        path: Vec<String>,
        array_index: Option<usize>,
    },
    Key {
        path: Vec<String>,
    },
    Trailing,
}

struct Anchor {
    removed: bool,
    harvest: bool,
    prefix: String,
    target: PrefixTarget,
}

struct Visited<'a> {
    position: isize,
    path: Vec<String>,
    array_index: Option<usize>,
    table: &'a Table,
}

fn relocate_account_prefix(doc: &mut DocumentMut, id: &str) {
    for (kept, target) in account_prefix_plan(doc, id) {
        attach_prefix(doc, &target, &kept);
    }
}

/// Kept comment text for each run of consecutive removed anchors, paired with
/// the next surviving anchor after that run, so every block stays where it was
/// in document order.
fn account_prefix_plan(doc: &DocumentMut, id: &str) -> Vec<(String, PrefixTarget)> {
    let anchors = document_anchors(doc, id);
    let mut plan = Vec::new();
    let mut kept = String::new();
    for anchor in &anchors {
        if anchor.removed {
            if anchor.harvest {
                kept.push_str(&kept_prefix(&anchor.prefix));
            }
        } else if !kept.is_empty() {
            plan.push((std::mem::take(&mut kept), anchor.target.clone()));
        }
    }
    if !kept.is_empty() {
        plan.push((kept, PrefixTarget::Trailing));
    }
    plan
}

fn document_anchors(doc: &DocumentMut, id: &str) -> Vec<Anchor> {
    let mut path = Vec::new();
    let mut last_position = 0;
    let mut visited = Vec::new();
    collect_tables(
        doc.as_table(),
        &mut path,
        None,
        &mut last_position,
        &mut visited,
    );
    visited.sort_by(|left, right| {
        let left_nested = !left.path.is_empty();
        let right_nested = !right.path.is_empty();
        left_nested
            .cmp(&right_nested)
            .then(left.position.cmp(&right.position))
    });

    let mut anchors = Vec::new();
    for table in &visited {
        let removed_table = under_removed_account(&table.path, id);
        if emits_header(table.table, &table.path) {
            anchors.push(Anchor {
                removed: removed_table,
                harvest: removed_table,
                prefix: decor_text(table.table.decor()),
                target: PrefixTarget::Header {
                    path: table.path.clone(),
                    array_index: table.array_index,
                },
            });
        }
        for (key_path, _) in table.table.get_values() {
            let mut full_path = table.path.clone();
            for key in &key_path {
                full_path.push(key.get().to_owned());
            }
            let removed = under_removed_account(&full_path, id);
            let prefix = key_path
                .last()
                .map(|key| decor_text(key.leaf_decor()))
                .unwrap_or_default();
            anchors.push(Anchor {
                removed,
                harvest: removed && !removed_table,
                prefix,
                target: PrefixTarget::Key { path: full_path },
            });
        }
    }
    anchors
}

fn collect_tables<'a>(
    table: &'a Table,
    path: &mut Vec<String>,
    array_index: Option<usize>,
    last_position: &mut isize,
    out: &mut Vec<Visited<'a>>,
) {
    let position = if let Some(position) = table.position() {
        *last_position = position;
        position
    } else {
        *last_position
    };
    if !table.is_dotted() {
        out.push(Visited {
            position,
            path: path.clone(),
            array_index,
            table,
        });
    }
    for (key, item) in table {
        if let Some(child) = item.as_table() {
            path.push(key.to_owned());
            collect_tables(child, path, None, last_position, out);
            path.pop();
        } else if let Some(array) = item.as_array_of_tables() {
            for (index, child) in array.iter().enumerate() {
                path.push(key.to_owned());
                collect_tables(child, path, Some(index), last_position, out);
                path.pop();
            }
        }
    }
}

fn emits_header(table: &Table, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    !(table.is_implicit() && table.get_values().is_empty())
}

fn under_removed_account(path: &[String], id: &str) -> bool {
    path.len() >= 2 && path[0] == "accounts" && path[1] == id
}

fn decor_text(decor: &Decor) -> String {
    decor
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("")
        .to_owned()
}

/// Text up to and including the last blank line.
///
/// Comment lines after that blank line sit directly above the removed header
/// and belong to the account. A prefix with no blank line belongs entirely to
/// the account. The newline that ends a comment is not itself a blank line.
fn kept_prefix(prefix: &str) -> String {
    let bytes = prefix.as_bytes();
    let mut last_blank_end = None;
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && bytes[index] != b'\n' {
            index += 1;
        }
        let mut content_end = index;
        let has_newline = index < bytes.len();
        if has_newline {
            index += 1;
        }
        if content_end > start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        let blank = prefix[start..content_end]
            .bytes()
            .all(|byte| byte == b' ' || byte == b'\t');
        if blank && has_newline {
            last_blank_end = Some(index);
        }
    }
    match last_blank_end {
        Some(end) => prefix[..end].to_owned(),
        None => String::new(),
    }
}

fn attach_prefix(doc: &mut DocumentMut, target: &PrefixTarget, kept: &str) {
    let attached = match target {
        PrefixTarget::Trailing => false,
        PrefixTarget::Header { path, array_index } => prepend_header(doc, path, *array_index, kept),
        PrefixTarget::Key { path } => prepend_key(doc, path, kept),
    };
    if !attached {
        prepend_trailing(doc, kept);
    }
}

fn prepend_header(
    doc: &mut DocumentMut,
    path: &[String],
    array_index: Option<usize>,
    kept: &str,
) -> bool {
    let Some(table) = header_table(doc, path, array_index) else {
        return false;
    };
    prepend_decor(table.decor_mut(), kept)
}

fn header_table<'a>(
    doc: &'a mut DocumentMut,
    path: &[String],
    array_index: Option<usize>,
) -> Option<&'a mut Table> {
    if let Some(index) = array_index {
        let parent_len = path.len().checked_sub(1)?;
        let parent = descend(doc, &path[..parent_len])?;
        let key = path.last()?;
        return parent
            .get_mut(key)?
            .as_array_of_tables_mut()?
            .get_mut(index);
    }
    descend(doc, path)
}

fn prepend_key(doc: &mut DocumentMut, path: &[String], kept: &str) -> bool {
    let Some((leaf, parent_path)) = path.split_last() else {
        return false;
    };
    let Some(parent) = descend(doc, parent_path) else {
        return false;
    };
    let Some(mut key) = parent.key_mut(leaf) else {
        return false;
    };
    prepend_decor(key.leaf_decor_mut(), kept)
}

fn descend<'a>(doc: &'a mut DocumentMut, path: &[String]) -> Option<&'a mut Table> {
    descend_from(doc.as_table_mut(), path)
}

fn descend_from<'a>(table: &'a mut Table, path: &[String]) -> Option<&'a mut Table> {
    let Some((key, rest)) = path.split_first() else {
        return Some(table);
    };
    let child = table.get_mut(key)?.as_table_mut()?;
    descend_from(child, rest)
}

fn prepend_decor(decor: &mut Decor, kept: &str) -> bool {
    if kept.is_empty() {
        return true;
    }
    let existing = match decor.prefix() {
        None => String::new(),
        Some(prefix) => {
            let Some(text) = prefix.as_str() else {
                return false;
            };
            text.to_owned()
        }
    };
    let mut merged = String::with_capacity(kept.len() + existing.len());
    merged.push_str(kept);
    merged.push_str(&existing);
    decor.set_prefix(merged);
    true
}

fn prepend_trailing(doc: &mut DocumentMut, kept: &str) {
    if kept.is_empty() {
        return;
    }
    let existing = doc.trailing().as_str().unwrap_or("");
    let mut merged = String::with_capacity(kept.len() + existing.len());
    merged.push_str(kept);
    merged.push_str(existing);
    doc.set_trailing(merged);
}

fn write_imap(table: &mut dyn TableLike, fields: &ImapFields) {
    set_string(table, "host", &fields.host);
    set_i64(table, "port", i64::from(fields.port));
    set_bool(table, "secure", fields.secure);
    set_string(table, "user", &fields.user);
    if let Some(pass) = &fields.pass {
        write_secret(table, "pass", pass);
    }
}

fn write_smtp(table: &mut dyn TableLike, fields: &SmtpFields) {
    set_string(table, "host", &fields.host);
    set_i64(table, "port", i64::from(fields.port));
    set_string(table, "user", &fields.user);
    if let Some(pass) = &fields.pass {
        write_secret(table, "pass", pass);
    }
    set_string(table, "security", &fields.security);
    match fields.save_sent {
        SentPolicy::ProviderDefault => {
            table.remove("save_sent");
        }
        SentPolicy::Save => set_bool(table, "save_sent", true),
        SentPolicy::Skip => set_bool(table, "save_sent", false),
    }
    write_optional(table, "from_email", &fields.from_email);
}

fn write_oauth(table: &mut dyn TableLike, fields: &OAuthFields) {
    set_string(table, "provider", &fields.provider);
    set_string(table, "client_id", &fields.client_id);
    write_secret(table, "client_secret", &fields.client_secret);
    write_secret(table, "refresh_token", &fields.refresh_token);
}

fn write_ews(table: &mut dyn TableLike, fields: &EwsFields) {
    set_string(table, "user", &fields.user);
    set_string(table, "client_id", &fields.client_id);
    write_secret(table, "client_secret", &fields.client_secret);
    write_secret(table, "refresh_token", &fields.refresh_token);
}

fn write_secret(table: &mut dyn TableLike, key: &str, secret: &SecretInput) {
    if let SecretInput::Set(value) = secret {
        set_string(table, key, value);
    }
}

fn write_optional(table: &mut dyn TableLike, key: &str, value: &OptionalString) {
    match value {
        OptionalString::Keep => {}
        OptionalString::Clear => {
            table.remove(key);
        }
        OptionalString::Set(text) => set_string(table, key, text),
    }
}

fn protocol_table<'a>(
    doc: &'a mut DocumentMut,
    id: &str,
    protocol: &str,
) -> Result<&'a mut dyn TableLike, CliFailure> {
    let (accounts, accounts_inline) = ensure_keyed(doc.as_table_mut(), "accounts", false)?;
    let (account, account_inline) = ensure_keyed(accounts, id, accounts_inline)?;
    let (table, _) = ensure_keyed(account, protocol, account_inline)?;
    Ok(table)
}

/// Return the child table, creating it when absent.
///
/// A missing child of an inline table is an inline table. Inserting a standard
/// table there panics inside toml_edit. An existing inline table is edited in
/// place so its comments and braces stay.
fn ensure_keyed<'a>(
    parent: &'a mut dyn TableLike,
    key: &str,
    parent_inline: bool,
) -> Result<(&'a mut dyn TableLike, bool), CliFailure> {
    let child_inline = match parent.get(key) {
        Some(item) if item.is_table_like() => item.is_inline_table(),
        Some(_) => {
            return Err(CliFailure::new(format!(
                "`{key}` is not a table in the config file"
            )));
        }
        None => parent_inline,
    };
    if parent.get(key).is_none() {
        if parent_inline {
            parent.insert(
                key,
                Item::Value(Value::InlineTable(toml_edit::InlineTable::new())),
            );
        } else {
            parent.insert(key, Item::Table(Table::new()));
        }
    }
    match parent.get_mut(key) {
        Some(item) => {
            let table = item.as_table_like_mut().ok_or_else(|| {
                CliFailure::new(format!("`{key}` is not a table in the config file"))
            })?;
            Ok((table, child_inline))
        }
        None => Err(CliFailure::new(format!(
            "`{key}` is not a table in the config file"
        ))),
    }
}

fn set_string(table: &mut dyn TableLike, key: &str, value: &str) {
    set_value(table, key, Value::from(value.to_owned()));
}

fn set_bool(table: &mut dyn TableLike, key: &str, value: bool) {
    set_value(table, key, Value::from(value));
}

fn set_i64(table: &mut dyn TableLike, key: &str, value: i64) {
    set_value(table, key, Value::from(value));
}

fn set_value(table: &mut dyn TableLike, key: &str, value: Value) {
    crate::config_file::assign_value(table, key, value);
}

fn account_table_ref<'a>(doc: &'a DocumentMut, id: &str) -> Option<&'a dyn TableLike> {
    let accounts = doc.get("accounts")?.as_table_like()?;
    accounts.get(id)?.as_table_like()
}

fn read_setting(table: &dyn TableLike, key: &str) -> Option<String> {
    if matches!(key, "pass" | "client_secret" | "refresh_token") {
        return None;
    }
    let value = table.get(key)?.as_value()?;
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(number) = value.as_integer() {
        return Some(number.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Some(flag.to_string());
    }
    None
}

fn read_u16(table: &dyn TableLike, key: &str) -> Option<u16> {
    read_setting(table, key)?.parse().ok()
}

fn read_bool(table: &dyn TableLike, key: &str) -> Option<bool> {
    match read_setting(table, key)?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub(crate) fn render_accounts(
    path: &Path,
    doc: &DocumentMut,
    values: &BTreeMap<String, String>,
    overrides: &[String],
    env_keys: &[String],
) -> String {
    let file_accounts = file_protocols(doc);
    let value_accounts = discover_value_protocols(values);
    let mut rows: BTreeMap<String, (String, Protocols)> = BTreeMap::new();

    for (id, mut protocols) in file_accounts {
        if let Some(extra) = value_accounts
            .get(&id)
            .or_else(|| value_accounts.get(&id.to_ascii_lowercase()))
        {
            protocols.or_with(extra);
        }
        let source = if env_defines(&id, overrides, env_keys) {
            "file+environment"
        } else {
            "file"
        };
        rows.insert(id, (source.to_owned(), protocols));
    }
    for (id, protocols) in value_accounts {
        if rows
            .keys()
            .any(|existing| existing.eq_ignore_ascii_case(&id))
        {
            continue;
        }
        rows.insert(id, ("environment".to_owned(), protocols));
    }

    let mut out = format!("config: {}\n", path.display());
    if rows.is_empty() {
        out.push_str("accounts: (none)\n");
    } else {
        out.push_str("accounts:\n");
        for (id, (source, protocols)) in &rows {
            out.push_str(&format!(
                "  {id}  source={source}  protocols={}\n",
                protocols.label()
            ));
        }
    }
    out.push_str(&override_section(overrides));
    out
}

fn override_section(overrides: &[String]) -> String {
    let mut names: Vec<&str> = overrides
        .iter()
        .map(String::as_str)
        .filter(|key| is_config_key(key))
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return "overrides: (none)\n".to_owned();
    }
    let mut out = String::from("overrides:\n");
    for name in names {
        out.push_str(&format!("  {name}\n"));
    }
    out
}

fn env_defines(id: &str, overrides: &[String], env_keys: &[String]) -> bool {
    overrides
        .iter()
        .chain(env_keys.iter())
        .any(|key| matches_account(key, id))
}

fn file_protocols(doc: &DocumentMut) -> BTreeMap<String, Protocols> {
    let Some(accounts) = doc.get("accounts").and_then(Item::as_table_like) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (key, item) in accounts.iter() {
        let Some(table) = item.as_table_like() else {
            continue;
        };
        let protocols = Protocols {
            imap: table.get("imap").is_some_and(Item::is_table_like),
            smtp: table.get("smtp").is_some_and(Item::is_table_like),
            oauth2: table.get("oauth2").is_some_and(Item::is_table_like),
            graph: table.get("graph_oauth2").is_some_and(Item::is_table_like),
            ews: table.get("ews").is_some_and(Item::is_table_like),
        };
        if protocols.any() {
            out.insert(key.to_owned(), protocols);
        }
    }
    out
}

fn discover_value_protocols(values: &BTreeMap<String, String>) -> BTreeMap<String, Protocols> {
    let mut out: BTreeMap<String, Protocols> = BTreeMap::new();
    for key in values.keys() {
        let Some(id) = account_id_from_env_key(key) else {
            continue;
        };
        let segment = id.to_ascii_uppercase();
        if key == &format!("MAIL_IMAP_{segment}_HOST") {
            out.entry(id).or_default().imap = true;
        } else if key == &format!("MAIL_SMTP_{segment}_HOST") {
            out.entry(id).or_default().smtp = true;
        } else if key == &format!("MAIL_OAUTH2_{segment}_PROVIDER") {
            out.entry(id).or_default().oauth2 = true;
        } else if key == &format!("MAIL_GRAPH_{segment}_PROVIDER") {
            out.entry(id).or_default().graph = true;
        } else if key == &format!("MAIL_EWS_{segment}_USER") {
            out.entry(id).or_default().ews = true;
        }
    }
    out
}

pub(crate) fn has_account_keys(values: &BTreeMap<String, String>) -> bool {
    !discover_value_protocols(values).is_empty()
}

pub(crate) fn matches_account(key: &str, id: &str) -> bool {
    account_id_from_env_key(key).is_some_and(|found| found.eq_ignore_ascii_case(id))
}

pub(crate) fn process_env_account_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if is_account_setting_key(key) {
            keys.push(key.to_owned());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

fn is_account_setting_key(key: &str) -> bool {
    is_account_env_key(key)
}

pub(crate) fn keys_only_in(
    left: &BTreeMap<String, String>,
    right: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut keys: Vec<String> = left
        .keys()
        .filter(|key| !right.contains_key(*key))
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[cfg(test)]
pub(crate) fn imap_draft(host: &str, user: &str, pass: SecretInput) -> AccountDraft {
    AccountDraft {
        id: "work".to_owned(),
        imap: Some(ImapFields {
            host: host.to_owned(),
            port: 993,
            secure: true,
            user: user.to_owned(),
            pass: Some(pass),
        }),
        smtp: None,
        oauth2: None,
        graph: None,
        ews: None,
    }
}

pub(crate) fn protocol_failure(protocol: &str, err: &AppError) -> String {
    match err {
        AppError::AuthFailed(_) => format!("{protocol}: authentication failed"),
        AppError::Timeout(_) => format!("{protocol}: timed out"),
        AppError::NotFound(_) => format!("{protocol}: the mailbox was not found"),
        AppError::Conflict(_) => format!("{protocol}: the server reported a conflict"),
        AppError::InvalidInput(message) => format!("{protocol}: {}", public_text(message)),
        AppError::Internal(_) => format!("{protocol}: the connection failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use toml_edit::DocumentMut;

    use super::*;
    use crate::errors::AppError;

    #[test]
    fn edit_defaults_preserve_provider_and_security_aliases() {
        for alias in ["outlook", "office365", "MICROSOFT", "microsoft"] {
            let index = provider_default_index(Some(alias));
            assert_eq!(PROVIDER_ITEMS[index], "microsoft", "{alias}");
            assert_eq!(
                OAuth2Provider::parse(alias).unwrap(),
                OAuth2Provider::parse(PROVIDER_ITEMS[index]).unwrap(),
                "{alias}"
            );
        }
        for alias in ["google", "gmail", "GOOGLE"] {
            let index = provider_default_index(Some(alias));
            assert_eq!(PROVIDER_ITEMS[index], "google", "{alias}");
            assert_eq!(
                OAuth2Provider::parse(alias).unwrap(),
                OAuth2Provider::parse(PROVIDER_ITEMS[index]).unwrap(),
                "{alias}"
            );
        }
        for alias in ["ssl", "TLS", "tls"] {
            let index = security_default_index(Some(alias));
            assert_eq!(SECURITY_ITEMS[index], "tls", "{alias}");
            assert_eq!(
                SmtpSecurity::parse(alias).unwrap(),
                SmtpSecurity::parse(SECURITY_ITEMS[index]).unwrap(),
                "{alias}"
            );
        }
        for alias in ["none", "PLAIN", "plain"] {
            let index = security_default_index(Some(alias));
            assert_eq!(SECURITY_ITEMS[index], "plain", "{alias}");
            assert_eq!(
                SmtpSecurity::parse(alias).unwrap(),
                SmtpSecurity::parse(SECURITY_ITEMS[index]).unwrap(),
                "{alias}"
            );
        }
        assert_eq!(
            SECURITY_ITEMS[security_default_index(Some("starttls"))],
            "starttls"
        );
        assert_eq!(provider_default_index(None), 0);
        assert_eq!(security_default_index(None), 0);
    }

    #[test]
    fn unknown_env_suffix_is_not_recognized_or_printed() {
        let secret_key = "MAIL_IMAP_WORK_SUPER_SECRET_VALUE";
        assert!(!is_account_env_key(secret_key));
        assert!(!is_account_setting_key(secret_key));
        assert!(!matches_account(secret_key, "work"));
        assert!(is_account_env_key("MAIL_IMAP_WORK_PASS"));
        assert!(is_account_env_key("MAIL_IMAP_WORK_HOST"));
        let doc = "[accounts.work.imap]\nhost = \"imap.example.com\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let text = render_accounts(
            Path::new("/tmp/config.toml"),
            &doc,
            &BTreeMap::new(),
            &[secret_key.to_owned(), "MAIL_IMAP_WORK_HOST".to_owned()],
            &[secret_key.to_owned()],
        );
        assert!(!text.contains("SUPER_SECRET_VALUE"), "{text}");
        assert!(text.contains("MAIL_IMAP_WORK_HOST"), "{text}");
    }

    #[test]
    fn invalid_id_notice_does_not_include_the_input() {
        let overlong = format!("{}super-secret-value", "a".repeat(64));
        assert!(!valid_account_id(&overlong));
        assert!(!INVALID_ACCOUNT_ID.contains("super-secret-value"));
        assert!(!INVALID_ACCOUNT_ID.contains(&overlong));
        assert!(valid_account_id("work_"));
    }

    #[test]
    fn account_key_match_is_the_exact_segment() {
        assert!(valid_account_id("work_"));
        assert!(matches_account("MAIL_IMAP_WORK_HOST", "work"));
        assert!(!matches_account("MAIL_IMAP_WORK_EXTRA_HOST", "work"));
        assert!(matches_account("MAIL_IMAP_WORK_EXTRA_HOST", "work_extra"));
        assert!(matches_account("MAIL_IMAP_WORK__HOST", "work_"));
        assert!(!matches_account("MAIL_IMAP_WORK_HOST", "work_"));
        assert!(!matches_account("MAIL_IMAP_WRITE_ENABLED", "write"));
        assert!(!matches_account("MAIL_SMTP_TIMEOUT_MS", "timeout"));
    }

    #[test]
    fn inline_account_tables_list_edit_and_remove() {
        let mut doc = "\
[accounts]
work = { imap = { host = \"old.example\", port = 993, secure = true, user = \"work@example.com\", pass = \"super-secret-value\" } }

[accounts.other.imap]
host = \"imap.example.org\" # other-host
"
        .parse::<DocumentMut>()
        .unwrap();
        assert!(file_protocols(&doc).get("work").is_some_and(|p| p.imap));
        assert!(account_in_file(&doc, "work"));
        {
            let host = doc
                .get_mut("accounts")
                .unwrap()
                .as_table_like_mut()
                .unwrap()
                .get_mut("work")
                .unwrap()
                .as_table_like_mut()
                .unwrap()
                .get_mut("imap")
                .unwrap()
                .as_table_like_mut()
                .unwrap()
                .get_mut("host")
                .unwrap()
                .as_value_mut()
                .unwrap();
            host.decor_mut().set_suffix(" # keep-me");
        }
        let draft = imap_draft("new.example", "work@example.com", SecretInput::Keep);
        apply_account(&mut doc, &draft).unwrap();
        let text = doc.to_string();
        assert!(text.contains("# keep-me"), "{text}");
        assert!(text.contains("new.example"), "{text}");
        assert!(!text.contains("old.example"), "{text}");
        assert!(text.contains("super-secret-value"), "{text}");
        assert!(text.contains("# other-host"), "{text}");
        assert!(text.contains('{'), "{text}");
        assert!(!text.contains("[accounts.work"), "{text}");
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(text.contains("# other-host"), "{text}");

        let root = "accounts = { work_ = { imap = { host = \"h\", port = 993, secure = true, user = \"a@b.c\", pass = \"super-secret-value\" } } }\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(file_protocols(&root).contains_key("work_"));
        assert!(account_in_file(&root, "work_"));
    }

    #[test]
    fn template_has_commented_settings_and_no_credentials() {
        let doc = initial_document().unwrap();
        let text = doc.to_string();
        assert!(text.contains("[accounts]"));
        assert!(!text.contains("pass ="));
        assert!(!text.contains("secret"));
        assert!(!text.contains("token ="));
    }

    #[test]
    fn public_text_drops_secret_assignments() {
        let text = public_text("invalid toml at line 3: pass = \"super-secret-value\"");
        assert!(!text.contains("super-secret-value"));
        assert!(text.contains("line 3"), "{text}");
        assert!(text.contains("pass"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }

    #[test]
    fn public_text_keeps_secret_key_names() {
        for message in [
            "unknown key at line 2",
            "invalid account id at line 4",
            "expected a string at line 5",
            "expected a table at line 6",
            "invalid input: OAuth2 account 'work' is missing MAIL_OAUTH2_WORK_CLIENT_SECRET",
            "invalid input: OAuth2 account 'work' is missing MAIL_OAUTH2_WORK_PROVIDER",
            "invalid input: OAuth2 account 'work' is missing MAIL_GRAPH_WORK_REFRESH_TOKEN",
        ] {
            assert_eq!(public_text(message), message);
        }
        let leaked = public_text("accounts.work.oauth2.client_secret = \"super-secret-value\"");
        assert!(leaked.contains("client_secret"), "{leaked}");
        assert!(leaked.contains("[redacted]"), "{leaked}");
        assert!(!leaked.contains("super-secret-value"), "{leaked}");
    }

    #[test]
    fn protocol_failure_hides_response_bodies() {
        let err = AppError::AuthFailed(
            "OAuth2 token endpoint returned 400: super-secret-value".to_owned(),
        );
        let line = protocol_failure("graph", &err);
        assert_eq!(line, "graph: authentication failed");
        assert!(!line.contains("super-secret-value"));
    }

    #[test]
    fn editing_a_value_keeps_its_comment_and_a_blank_secret() {
        let mut doc = "[accounts.work.imap]\nhost = \"old.example\" # keep-me\npass = \"super-secret-value\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let draft = imap_draft("new.example", "work@example.com", SecretInput::Keep);
        apply_account(&mut doc, &draft).unwrap();
        let text = doc.to_string();
        assert!(text.contains("# keep-me"), "{text}");
        assert!(text.contains("new.example"), "{text}");
        assert!(!text.contains("old.example"), "{text}");
        assert!(text.contains("super-secret-value"), "{text}");
        assert!(text.contains("port = 993"), "{text}");
    }

    #[test]
    fn summary_does_not_contain_the_secret() {
        let draft = imap_draft(
            "imap.example.com",
            "work@example.com",
            SecretInput::Set("super-secret-value".to_owned()),
        );
        let text = summary(&draft);
        assert!(text.contains("work@example.com"));
        assert!(text.contains("hidden"));
        assert!(!text.contains("super-secret-value"));
    }

    #[test]
    fn remove_drops_only_the_named_account() {
        let mut doc = "\
[accounts.work.imap]
pass = \"super-secret-value\"

# keep other
[accounts.other.imap]
host = \"imap.example.org\" # other-host
pass = \"kept-secret\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(text.contains("kept-secret"), "{text}");
        assert!(text.contains("other-host"), "{text}");
        assert!(remove_account(&mut doc, "work").is_err());
    }

    #[test]
    fn remove_keeps_comments_separated_from_the_account_by_a_blank_line() {
        let mut doc = "\
[settings]
# write_enabled = false
# drafts_enabled = false
# smtp_write_enabled = false

[accounts.work.imap]
host = \"imap.example.com\"
pass = \"super-secret-value\"

# keep other
[accounts.other.imap]
host = \"imap.example.org\"
pass = \"kept-secret\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        let settings = text
            .find("# write_enabled = false")
            .expect("settings comment");
        let other_comment = text.find("# keep other").expect("other comment");
        let other = text.find("[accounts.other.imap]").expect("other account");
        assert!(text.contains("# drafts_enabled = false"), "{text}");
        assert!(text.contains("# smtp_write_enabled = false"), "{text}");
        assert!(text.contains("[settings]"), "{text}");
        assert!(settings < other, "{text}");
        assert!(other_comment < other, "{text}");
        assert!(text.contains("kept-secret"), "{text}");
        assert!(text.contains("imap.example.org"), "{text}");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains("imap.example.com"), "{text}");
    }

    #[test]
    fn remove_keeps_comments_in_place_when_account_sections_are_interleaved() {
        let mut doc = "\
[settings]
# write_enabled = false

[accounts.work.imap]
host = \"imap.example.com\"

[accounts.other.imap]
host = \"imap.example.org\"

# smtp notes

[accounts.work.smtp]
host = \"smtp.example.com\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        let settings = text
            .find("# write_enabled = false")
            .expect("settings comment");
        let other = text.find("[accounts.other.imap]").expect("other account");
        let notes = text.find("# smtp notes").expect("later comment");
        assert!(settings < other, "{text}");
        assert!(other < notes, "{text}");
        assert!(!text.contains("example.com"), "{text}");
    }

    #[test]
    fn remove_drops_a_comment_glued_to_the_account_header() {
        let mut doc = "\
[settings]
# write_enabled = false

# work account
[accounts.work.imap]
host = \"imap.example.com\"
pass = \"super-secret-value\"

[accounts.other.imap]
host = \"imap.example.org\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        let settings = text
            .find("# write_enabled = false")
            .expect("settings comment");
        let other = text.find("[accounts.other.imap]").expect("other account");
        assert!(settings < other, "{text}");
        assert!(!text.contains("# work account"), "{text}");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains("imap.example.com"), "{text}");
        assert!(text.contains("imap.example.org"), "{text}");
    }

    #[test]
    fn remove_inline_account_keeps_the_other_account() {
        let mut doc = "\
[accounts]
work = { imap = { host = \"imap.example.com\", port = 993, secure = true, user = \"work@example.com\", pass = \"super-secret-value\" } }

# keep other
[accounts.other.imap]
host = \"imap.example.org\"
pass = \"kept-secret\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        let other_comment = text.find("# keep other").expect("other comment");
        let other = text.find("[accounts.other.imap]").expect("other account");
        assert!(other_comment < other, "{text}");
        assert!(text.contains("kept-secret"), "{text}");
        assert!(text.contains("imap.example.org"), "{text}");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains("work@example.com"), "{text}");
        assert!(!text.contains("work ="), "{text}");
    }

    #[test]
    fn remove_dotted_account_keeps_a_separated_comment_and_the_other_account() {
        let mut doc = "\
[accounts]

# kept note

work.imap.host = \"imap.example.com\"
work.imap.pass = \"super-secret-value\"

# keep other
[accounts.other.imap]
host = \"imap.example.org\"
pass = \"kept-secret\"
"
        .parse::<DocumentMut>()
        .unwrap();
        remove_account(&mut doc, "work").unwrap();
        let text = doc.to_string();
        let note = text.find("# kept note").expect("separated comment");
        let other_comment = text.find("# keep other").expect("other comment");
        let other = text.find("[accounts.other.imap]").expect("other account");
        assert!(note < other, "{text}");
        assert!(other_comment < other, "{text}");
        assert!(text.contains("kept-secret"), "{text}");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains("imap.example.com"), "{text}");
    }

    #[test]
    fn list_output_names_overrides_and_hides_values() {
        let doc = "\
[accounts.work.imap]
host = \"imap.example.com\"
pass = \"super-secret-value\"

[accounts.work.smtp]
host = \"smtp.example.com\"
"
        .parse::<DocumentMut>()
        .unwrap();
        let mut values = BTreeMap::new();
        values.insert(
            "MAIL_IMAP_WORK_HOST".to_owned(),
            "override.example".to_owned(),
        );
        values.insert(
            "MAIL_IMAP_WORK_PASS".to_owned(),
            "super-secret-value".to_owned(),
        );
        values.insert(
            "MAIL_GRAPH_PERSONAL_PROVIDER".to_owned(),
            "microsoft".to_owned(),
        );
        values.insert(
            "MAIL_GRAPH_PERSONAL_REFRESH_TOKEN".to_owned(),
            "super-secret-value".to_owned(),
        );
        let overrides = vec![
            "MAIL_IMAP_WORK_HOST".to_owned(),
            "MAIL_GRAPH_PERSONAL_REFRESH_TOKEN".to_owned(),
        ];
        let env_keys = vec!["MAIL_IMAP_WORK_HOST".to_owned()];
        let text = render_accounts(
            Path::new("/tmp/config.toml"),
            &doc,
            &values,
            &overrides,
            &env_keys,
        );
        assert!(text.contains("work"), "{text}");
        assert!(text.contains("MAIL_IMAP_WORK_HOST"), "{text}");
        assert!(text.contains("MAIL_GRAPH_PERSONAL_REFRESH_TOKEN"), "{text}");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains("override.example"), "{text}");
    }
}
