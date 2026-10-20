//! Command-line parsing and account management.
//!
//! With no subcommand, [`prepare_runtime`] loads the config for the MCP server.
//! Subcommands never prompt unless they are the interactive account commands,
//! and those commands refuse a non-terminal before reading any input.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::cli_accounts::{
    self, INVALID_ACCOUNT_ID, account_in_file, apply_account, confirm_remove, has_account_keys,
    initial_document, keys_only_in, matches_account, process_env_account_keys, protocol_failure,
    remove_account, render_accounts, require_terminal, with_unchanged,
};
pub(crate) use crate::cli_accounts::{CliFailure, public_text};
use crate::config::{AuthMethod, ServerConfig};
use crate::config_file::{ConfigFile, ResolvedConfig, Settings, is_config_key, valid_account_id};
use crate::errors::{AppError, AppResult};
use crate::http::{HttpSettings, Transport};
use crate::oauth2::TokenManager;

#[derive(Debug, Parser)]
#[command(
    name = "mail-mcp",
    version,
    about = "Email MCP server for IMAP, SMTP, Microsoft Graph, and EWS",
    long_about = "Serves the Model Context Protocol when no subcommand is given. \
Stdio is the default transport and does not prompt or write ordinary output.\n\n\
The config file is --config, or MAIL_CONFIG_FILE, or ~/.mail-mcp/config.toml \
(USERPROFILE on Windows). A leading ~/ is expanded. \
MAIL_CONFIG_FILE=none selects no file. \
An explicit missing file is an error at startup. \
With no config file, MAIL_* variables and a legacy .env file are used, \
including when HOME is unset. A config file disables .env discovery. \
Config and account commands need --config, MAIL_CONFIG_FILE, or a home directory. \
Environment variables override file settings.\n\n\
Secrets are requested on the terminal and are never accepted as arguments.",
    propagate_version = true
)]
pub(crate) struct Cli {
    /// Config file. Overrides MAIL_CONFIG_FILE and the default config path.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create or check the configuration file.
    #[command(arg_required_else_help = true)]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage accounts stored in the configuration file.
    #[command(arg_required_else_help = true)]
    Accounts {
        #[command(subcommand)]
        command: AccountsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Write a commented config file and an empty account table.
    Init,
    /// Check effective settings without connecting to a mail server.
    Validate,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AccountsCommand {
    /// Show account ids, protocols, sources, and environment override names.
    List,
    /// Add an account. Prompts on the terminal.
    Add,
    /// Change an account. A blank secret keeps the current value.
    Edit {
        /// Account id.
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Remove an account from the config file.
    Remove {
        /// Account id.
        #[arg(value_name = "ID")]
        id: String,
        /// Remove without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Authenticate and run a read-only check. Does not send or change mail.
    Test {
        /// Account id.
        #[arg(value_name = "ID")]
        id: String,
    },
}

pub(crate) struct Runtime {
    pub config: ServerConfig,
    pub transport: Transport,
    pub http: Option<HttpSettings>,
    pub overrides: Vec<String>,
}

pub(crate) async fn execute(config: Option<PathBuf>, command: Command) -> Result<(), CliFailure> {
    match command {
        Command::Config { command } => match command {
            ConfigCommand::Init => init_config(config),
            ConfigCommand::Validate => validate_config(config),
        },
        Command::Accounts { command } => match command {
            AccountsCommand::List => list_accounts(config),
            AccountsCommand::Add => add_account(config),
            AccountsCommand::Edit { id } => edit_account(config, id),
            AccountsCommand::Remove { id, yes } => remove_account_cmd(config, id, yes),
            AccountsCommand::Test { id } => test_account(config, id).await,
        },
    }
}

pub(crate) fn prepare_runtime(config: Option<PathBuf>) -> Result<Runtime, CliFailure> {
    let file = ConfigFile::load_runtime(config.as_deref()).map_err(app_failure)?;
    let resolved = runtime_resolved(&file)?;
    let (transport, http) = checked_transport(&resolved.values)?;
    let server = load_server(&resolved.values, true)?;
    Ok(Runtime {
        config: server,
        transport,
        http,
        overrides: sorted_names(&resolved.overrides),
    })
}

fn init_config(config: Option<PathBuf>) -> Result<(), CliFailure> {
    let mut file = open(config.as_deref(), true)?;
    if file.exists {
        return Err(CliFailure::new(format!(
            "refusing to overwrite {}",
            file.path.display()
        )));
    }
    file.document = initial_document()?;
    warn_dotenv(&file);
    file.save(true)
        .map_err(|err| with_unchanged(app_failure(err)))?;
    println!("Wrote {}", file.path.display());
    Ok(())
}

fn validate_config(config: Option<PathBuf>) -> Result<(), CliFailure> {
    let file = open(config.as_deref(), false)?;
    let resolved = runtime_resolved(&file)?;
    let (transport, http) = match checked_transport(&resolved.values) {
        Ok(checked) => checked,
        Err(err) => return Err(fail_with_overrides(&resolved, err)),
    };
    if let Err(err) = load_server(&resolved.values, true) {
        return Err(fail_with_overrides(&resolved, err));
    }
    println!("config: {}", file.path.display());
    match transport {
        Transport::Stdio => println!("transport: stdio"),
        Transport::Http => {
            if let Some(settings) = http {
                println!("transport: http {}{}", settings.addr, settings.path);
            }
        }
    }
    println!("ok");
    print_override_report(&resolved.overrides);
    Ok(())
}

fn list_accounts(config: Option<PathBuf>) -> Result<(), CliFailure> {
    let file = open(config.as_deref(), false)?;
    let resolved = runtime_resolved(&file)?;
    let text = render_accounts(
        &file.path,
        &file.document,
        &resolved.values,
        &resolved.overrides,
        &process_env_account_keys(),
    );
    write_stdout(&text)
}

fn add_account(config: Option<PathBuf>) -> Result<(), CliFailure> {
    require_terminal()?;
    let mut file = open(config.as_deref(), true)?;
    if !file.exists {
        file.document = initial_document()?;
    }
    let draft = cli_accounts::prompt_add(&file.document)?;
    apply_account(&mut file.document, &draft).map_err(with_unchanged)?;
    let resolved = saved_resolved(&file).map_err(with_unchanged)?;
    if let Err(err) = accept_saved(&resolved) {
        return Err(fail_with_overrides(&resolved, with_unchanged(err)));
    }
    warn_dotenv(&file);
    file.save(false)
        .map_err(|err| with_unchanged(app_failure(err)))?;
    println!("Added account {} to {}", draft.id, file.path.display());
    print_override_report(&resolved.overrides);
    Ok(())
}

fn edit_account(config: Option<PathBuf>, id: String) -> Result<(), CliFailure> {
    require_terminal()?;
    require_id(&id)?;
    let mut file = open(config.as_deref(), false)?;
    if !file.exists {
        return Err(CliFailure::new(
            "no config file to edit. Run `mail-mcp config init` or `mail-mcp accounts add`.\nNo changes were written.",
        ));
    }
    if !account_in_file(&file.document, &id) {
        return Err(missing_file_account(&file, &id));
    }
    let draft = cli_accounts::prompt_edit(&file.document, &id)?;
    apply_account(&mut file.document, &draft).map_err(with_unchanged)?;
    let resolved = saved_resolved(&file).map_err(with_unchanged)?;
    if let Err(err) = accept_saved(&resolved) {
        return Err(fail_with_overrides(&resolved, with_unchanged(err)));
    }
    file.save(false)
        .map_err(|err| with_unchanged(app_failure(err)))?;
    println!("Updated account {id} in {}", file.path.display());
    print_override_report(&resolved.overrides);
    Ok(())
}

fn remove_account_cmd(config: Option<PathBuf>, id: String, yes: bool) -> Result<(), CliFailure> {
    require_id(&id)?;
    if !yes {
        require_terminal()?;
    }
    let mut file = open(config.as_deref(), false)?;
    if !file.exists {
        return Err(CliFailure::new(
            "no config file to edit. Run `mail-mcp config init` or `mail-mcp accounts add`.\nNo changes were written.",
        ));
    }
    if !account_in_file(&file.document, &id) {
        return Err(missing_file_account(&file, &id));
    }
    let preview = saved_resolved(&file)?;
    let survivors = survivor_names(&preview, &id);
    if !yes {
        print_survivors(&id, &survivors);
        confirm_remove(&file.path, &id)?;
    }
    remove_account(&mut file.document, &id).map_err(with_unchanged)?;
    let resolved = saved_resolved(&file).map_err(with_unchanged)?;
    file.save(false)
        .map_err(|err| with_unchanged(app_failure(err)))?;
    println!("Removed account {id} from {}", file.path.display());
    if yes {
        print_survivors(&id, &survivors);
    }
    print_override_report(&resolved.overrides);
    Ok(())
}

async fn test_account(config: Option<PathBuf>, id: String) -> Result<(), CliFailure> {
    require_id(&id)?;
    let file = open(config.as_deref(), false)?;
    let resolved = runtime_resolved(&file)?;
    let server = load_server(&resolved.values, true)?;
    let known = server.accounts.contains_key(&id)
        || server.smtp_accounts.contains_key(&id)
        || server.oauth2_accounts.contains_key(&id)
        || server.graph_oauth2_accounts.contains_key(&id)
        || server.ews_accounts.contains_key(&id)
        || server.ews_oauth2_accounts.contains_key(&id);
    if !known {
        return Err(CliFailure::new(format!("account '{id}' is not configured")));
    }

    let mut checked = 0;
    let mut failed = 0;
    if let Some(account) = server.accounts.get(&id) {
        checked += 1;
        report(&id, "imap", test_imap(&server, account).await, &mut failed);
    }
    if let Some(account) = server.smtp_accounts.get(&id) {
        checked += 1;
        report(&id, "smtp", test_smtp(&server, account).await, &mut failed);
    }
    if server.graph_oauth2_accounts.contains_key(&id) {
        checked += 1;
        let result = test_graph(&server, &id).await;
        let graph_failed = result.is_err();
        report(&id, "graph", result, &mut failed);
        if graph_failed {
            println!(
                "{id} graph: this check lists drafts, so a send-only token (Mail.Send alone) fails it"
            );
        }
    }
    if server.ews_accounts.contains_key(&id) || server.ews_oauth2_accounts.contains_key(&id) {
        checked += 1;
        report(&id, "ews", test_ews(&server, &id).await, &mut failed);
    }
    if checked == 0 {
        return Err(CliFailure::new(format!(
            "account '{id}' has no IMAP, SMTP, Graph, or EWS connection to test"
        )));
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(CliFailure::new(format!(
            "account '{id}' failed {failed} read-only check(s)"
        )))
    }
}

fn report(id: &str, protocol: &str, result: AppResult<()>, failed: &mut usize) {
    match result {
        Ok(()) => println!("{id} {protocol}: ok"),
        Err(err) => {
            *failed += 1;
            println!("{id} {}", protocol_failure(protocol, &err));
        }
    }
}

async fn test_imap(server: &ServerConfig, account: &crate::config::AccountConfig) -> AppResult<()> {
    let tokens = TokenManager::new(server.oauth2_accounts.clone());
    let token_manager = (account.auth_method == AuthMethod::OAuth2).then_some(&tokens);
    let mut session = crate::imap::connect_authenticated(server, account, token_manager).await?;
    crate::imap::noop(server, &mut session).await?;
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(server.socket_timeout_ms),
        session.logout(),
    )
    .await;
    Ok(())
}

async fn test_smtp(
    server: &ServerConfig,
    account: &crate::smtp::SmtpAccountConfig,
) -> AppResult<()> {
    let tokens = TokenManager::new(server.oauth2_accounts.clone());
    let token_manager = (account.auth_method == AuthMethod::OAuth2).then_some(&tokens);
    crate::smtp::verify_smtp(account, token_manager, server.smtp_connect_timeout_ms).await
}

async fn test_graph(server: &ServerConfig, id: &str) -> AppResult<()> {
    let tokens = TokenManager::new(server.graph_oauth2_accounts.clone());
    let _ = crate::graph::list_drafts(&tokens, id, 1).await?;
    Ok(())
}

async fn test_ews(server: &ServerConfig, id: &str) -> AppResult<()> {
    let tokens = TokenManager::new(server.ews_oauth2_accounts.clone());
    let _ = crate::ews::find_items(&tokens, id, "inbox", 1, 0).await?;
    Ok(())
}

fn accept_saved(resolved: &ResolvedConfig) -> Result<(), CliFailure> {
    checked_transport(&resolved.values)?;
    load_server(&resolved.values, false)?;
    Ok(())
}

fn open(config: Option<&Path>, allow_missing: bool) -> Result<ConfigFile, CliFailure> {
    ConfigFile::load(config, allow_missing).map_err(app_failure)
}

fn runtime_resolved(file: &ConfigFile) -> Result<ResolvedConfig, CliFailure> {
    let legacy_dotenv = !file.explicit && !file.exists;
    file.effective(legacy_dotenv).map_err(app_failure)
}

fn saved_resolved(file: &ConfigFile) -> Result<ResolvedConfig, CliFailure> {
    file.effective(false).map_err(app_failure)
}

fn checked_transport(values: &Settings) -> Result<(Transport, Option<HttpSettings>), CliFailure> {
    let transport =
        Transport::from_settings(values).map_err(|err| CliFailure::new(public_text(&err)))?;
    let http = if transport == Transport::Http {
        Some(
            HttpSettings::from_settings(values)
                .map_err(|err| CliFailure::new(public_text(&err)))?,
        )
    } else {
        None
    };
    Ok((transport, http))
}

fn load_server(values: &Settings, hint_if_empty: bool) -> Result<ServerConfig, CliFailure> {
    match ServerConfig::load(values) {
        Ok(config) => Ok(config),
        Err(err) => {
            let mut message = public_text(&err.to_string());
            if hint_if_empty && !has_account_keys(values) {
                message.push_str(
                    "\n\nNo account is configured. Run `mail-mcp config init`, then `mail-mcp accounts add`, or set MAIL_* variables for an account.",
                );
            }
            Err(CliFailure::new(message))
        }
    }
}

fn app_failure(err: AppError) -> CliFailure {
    CliFailure::new(public_text(&err.to_string()))
}

fn require_id(id: &str) -> Result<(), CliFailure> {
    if valid_account_id(id) {
        Ok(())
    } else {
        Err(CliFailure::new(INVALID_ACCOUNT_ID))
    }
}

fn missing_file_account(file: &ConfigFile, id: &str) -> CliFailure {
    let mut message = format!("account '{id}' is not in the config file");
    if let Ok(resolved) = saved_resolved(file) {
        let names = survivor_names(&resolved, id);
        if !names.is_empty() {
            message.push_str(&format!(
                "\nEnvironment variables that still reference account '{id}':"
            ));
            for name in names {
                message.push_str(&format!("\n  {name}"));
            }
        }
    }
    message.push_str("\nThe config file was not changed.");
    CliFailure::new(message)
}

fn survivor_names(resolved: &ResolvedConfig, id: &str) -> Vec<String> {
    let mut names: Vec<String> = resolved
        .overrides
        .iter()
        .filter(|key| matches_account(key, id))
        .cloned()
        .chain(
            process_env_account_keys()
                .into_iter()
                .filter(|key| matches_account(key, id)),
        )
        .collect();
    names.sort();
    names.dedup();
    names
}

fn print_survivors(id: &str, names: &[String]) {
    if names.is_empty() {
        return;
    }
    eprintln!("Environment variables that still reference account '{id}':");
    for name in names {
        eprintln!("  {name}");
    }
}

fn warn_dotenv(file: &ConfigFile) {
    if file.explicit || file.exists {
        return;
    }
    match (file.effective(true), file.effective(false)) {
        (Ok(with_dotenv), Ok(without_dotenv)) => {
            let extra = keys_only_in(&with_dotenv.values, &without_dotenv.values);
            if extra.is_empty() {
                return;
            }
            eprintln!(
                "Creating this file disables legacy .env discovery. These settings come from .env and will not apply afterward:"
            );
            for key in extra {
                eprintln!("  {key}");
            }
        }
        (Err(err), _) => eprintln!(
            "Creating this file disables legacy .env discovery. The .env file could not be read: {}",
            public_text(&err.to_string())
        ),
        _ => {}
    }
}

fn fail_with_overrides(resolved: &ResolvedConfig, err: CliFailure) -> CliFailure {
    if !resolved.overrides.is_empty() {
        eprintln!("Environment overrides these settings:");
        for name in sorted_names(&resolved.overrides) {
            eprintln!("  {name}");
        }
    }
    err
}

fn print_override_report(overrides: &[String]) {
    let names = sorted_names(overrides);
    if names.is_empty() {
        println!("overrides: (none)");
        return;
    }
    println!("overrides:");
    for name in names {
        println!("  {name}");
    }
}

fn sorted_names(overrides: &[String]) -> Vec<String> {
    let mut names: Vec<String> = overrides
        .iter()
        .filter(|key| is_config_key(key))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

fn write_stdout(text: &str) -> Result<(), CliFailure> {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(text.as_bytes()) {
        Ok(()) => stdout
            .flush()
            .map_err(|_| CliFailure::new("could not write output")),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(_) => Err(CliFailure::new("could not write output")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbed_protocol_failure_is_used_for_auth_bodies() {
        let err = AppError::Internal("Graph list drafts failed: super-secret-value".to_owned());
        let line = protocol_failure("graph", &err);
        assert_eq!(line, "graph: the connection failed");
        assert!(!line.contains("super-secret-value"));
    }

    #[test]
    fn require_id_does_not_echo_invalid_or_overlong_input() {
        let overlong = format!("work_{}", "super-secret-value".repeat(8));
        let err = require_id(&overlong).unwrap_err();
        let text = err.to_string();
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(!text.contains(&overlong), "{text}");
        assert_eq!(text, INVALID_ACCOUNT_ID);
        let odd = require_id("Not Valid").unwrap_err();
        assert!(!odd.to_string().contains("Not Valid"));
        assert!(require_id("work_").is_ok());
    }
}
