//! TOML configuration file: load, merge, and comment-preserving save.
//!
//! Effective runtime settings are a `MAIL_*` map. File values are overridden
//! by the process environment. Legacy `.env` discovery runs only when no
//! explicit or existing TOML file is selected, and it never changes the
//! process environment.

use std::collections::BTreeMap;
use std::env::VarError;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::errors::{AppError, AppResult};

/// Effective configuration values keyed by `MAIL_*` setting name.
pub type Settings = BTreeMap<String, String>;

/// MCP account ids are at most 64 bytes. Canonical ids are ascii, so this is
/// also the character bound.
const MAX_ACCOUNT_ID_LEN: usize = 64;

/// Refuse to parse or save a config larger than this. The limit is checked
/// before the bytes are decoded, so an oversized file cannot be echoed in an
/// error. Save checks the serialized body before it locks or writes.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// `MAIL_CONFIG_FILE` value that selects no config file: the server runs from
/// the process environment and the legacy `.env` fallback only.
const NO_CONFIG_FILE: &str = "none";

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// A config file selected by `--config`, `MAIL_CONFIG_FILE`, or the default path.
///
/// `document` is the editable TOML. The bytes last read from disk stay private
/// so a later save can refuse a conflicting edit without logging secrets.
pub struct ConfigFile {
    pub path: PathBuf,
    pub document: DocumentMut,
    pub exists: bool,
    /// `true` when `--config` or `MAIL_CONFIG_FILE` selected the path.
    pub explicit: bool,
    original: Option<Vec<u8>>,
    source: String,
}

/// File and environment settings after precedence is applied.
///
/// `overrides` lists process-environment key names whose values won. It does
/// not include the values themselves.
pub struct ResolvedConfig {
    pub values: Settings,
    pub overrides: Vec<String>,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("path", &self.path)
            .field("exists", &self.exists)
            .field("explicit", &self.explicit)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.values.keys().map(String::as_str).collect();
        f.debug_struct("ResolvedConfig")
            .field("keys", &keys)
            .field("overrides", &self.overrides)
            .finish()
    }
}

impl ConfigFile {
    /// Load the selected config file.
    ///
    /// `explicit` is the `--config` path. When it is absent, `MAIL_CONFIG_FILE`
    /// wins over `{HOME|USERPROFILE}/.mail-mcp/config.toml`. Literal `~/` and
    /// `~\` prefixes expand to the home directory.
    ///
    /// A missing explicit file is an error unless `allow_missing` is set (init
    /// and add). A missing default file returns `exists = false` either way.
    /// Unreadable and malformed files are always errors. This does not read
    /// `.env` or change the process environment.
    ///
    /// With no `--config`, no `MAIL_CONFIG_FILE`, and no home directory, this
    /// returns an error. Server startup should use [`Self::load_runtime`],
    /// which keeps running from the environment instead of inventing a path.
    pub fn load(explicit: Option<&Path>, allow_missing: bool) -> AppResult<Self> {
        let mail_config_file = mail_config_file_from_env()?;
        let home = home_dir();
        let location = resolve_config_path(explicit, mail_config_file.as_deref(), home.as_deref())?;
        load_at(&location, allow_missing)
    }

    /// Load config for server startup.
    ///
    /// This matches [`Self::load`] with `allow_missing = false`, except when
    /// no `--config` is given and either `MAIL_CONFIG_FILE=none` opts out of
    /// the config file, or no config path was selected and neither `HOME` nor
    /// `USERPROFILE` is set. Startup then uses the process environment and,
    /// when the caller asks, a legacy `.env`. No working-directory path is
    /// created.
    pub fn load_runtime(explicit: Option<&Path>) -> AppResult<Self> {
        if explicit.is_none() {
            let mail_config_file = mail_config_file_from_env()?;
            let opted_out = mail_config_file.as_deref() == Some(NO_CONFIG_FILE);
            if opted_out || (mail_config_file.is_none() && home_dir().is_none()) {
                return Ok(Self::from_environment());
            }
        }
        Self::load(explicit, false)
    }

    /// Merge file settings with the process environment.
    ///
    /// When `legacy_dotenv` is set and this file is neither explicit nor
    /// existing, recognized `MAIL_*` keys are also read from `.env` through
    /// an iterator. Process environment keys override both sources. `.env` is
    /// never applied for an explicit path or an existing TOML file.
    pub fn effective(&self, legacy_dotenv: bool) -> AppResult<ResolvedConfig> {
        let dotenv = if self.uses_legacy_dotenv(legacy_dotenv) {
            read_legacy_dotenv()
        } else {
            Settings::new()
        };
        self.resolve_from(dotenv, std::env::vars_os())
    }

    /// Validate the in-memory document and write it atomically.
    ///
    /// Schema checks keys and types only, so an empty accounts table can be
    /// saved. `create_only` refuses an existing path and creates it with
    /// noclobber. A file that changed since load is left untouched. The
    /// conflict check and the write share a same-directory advisory lock,
    /// released when this call returns. Failed validation does not write.
    /// An environment-only config has no path and cannot be saved.
    ///
    /// Serialized output larger than 1 MiB is refused before the file is
    /// locked or replaced. A symlink is written through to its target; a
    /// dangling symlink is refused and left in place. Messages keep the path
    /// that was loaded.
    pub fn save(&mut self, create_only: bool) -> AppResult<()> {
        if self.path.as_os_str().is_empty() {
            return Err(AppError::invalid(
                "no config file is selected (set HOME or USERPROFILE, or pass --config)",
            ));
        }
        let _ = read_settings(&self.document, &self.source)?;
        let body = self.document.to_string();
        if body.len() as u64 > MAX_CONFIG_BYTES {
            return Err(config_too_large(&self.path));
        }
        let target = resolve_save_target(&self.path)?;
        let _lock = acquire_save_lock(&self.path, &target)?;
        if create_only {
            noclobber_write(&self.path, &target, body.as_bytes())?;
        } else {
            let on_disk = read_file_bytes(&target)?;
            if on_disk != self.original {
                return Err(AppError::Conflict(format!(
                    "config file {} changed since it was loaded",
                    self.path.display()
                )));
            }
            atomic_write(&self.path, &target, body.as_bytes())?;
        }
        self.original = Some(body.into_bytes());
        self.exists = true;
        Ok(())
    }

    fn from_environment() -> Self {
        Self {
            path: PathBuf::new(),
            document: DocumentMut::new(),
            exists: false,
            explicit: false,
            original: None,
            source: String::new(),
        }
    }

    fn uses_legacy_dotenv(&self, legacy_dotenv: bool) -> bool {
        legacy_dotenv && !self.explicit && !self.exists
    }

    fn resolve_from(
        &self,
        dotenv: Settings,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> AppResult<ResolvedConfig> {
        let mut values = dotenv;
        let file_values = read_settings(&self.document, &self.source)?;
        values.extend(file_values);
        let overrides = apply_env_overrides(&mut values, env)?;
        apply_legacy_smtp_timeout(&mut values, &overrides);
        Ok(ResolvedConfig { values, overrides })
    }
}

/// Canonical account ids: 1..=64 lowercase ascii letters, digits, and
/// underscores, starting with a letter or digit. A trailing underscore is
/// valid, so `work` and `work_` are different accounts. The length matches
/// the MCP account id bound.
pub fn valid_account_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if !(1..=MAX_ACCOUNT_ID_LEN).contains(&bytes.len()) {
        return false;
    }
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !alnum(bytes[0]) {
        return false;
    }
    bytes.iter().all(|byte| alnum(*byte) || *byte == b'_')
}

/// Replace `table[key]` with `new_value`, keeping comments attached to the
/// previous value when that key was already a scalar.
///
/// `table` may be a standard table or an inline table. The inline form is left
/// inline; only the value changes.
pub fn assign_value(table: &mut dyn TableLike, key: &str, new_value: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        let decor = existing.decor().clone();
        *existing = new_value;
        *existing.decor_mut() = decor;
        return;
    }
    table.insert(key, Item::Value(new_value));
}

/// Account segments that have `prefix + SEGMENT + "_" + suffix` in `settings`.
pub(crate) fn segments_with_suffix(settings: &Settings, prefix: &str, suffix: &str) -> Vec<String> {
    let ending = format!("_{suffix}");
    let mut segments = Vec::new();
    for key in settings.keys() {
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        let Some(segment) = rest.strip_suffix(&ending) else {
            continue;
        };
        if is_env_segment(segment) {
            segments.push(segment.to_owned());
        }
    }
    segments.sort();
    segments.dedup();
    segments
}

pub(crate) fn is_config_key(key: &str) -> bool {
    GLOBAL_KEYS.contains(&key) || is_account_env_key(key)
}

/// True when `key` is a known account field, not an arbitrary suffix.
///
/// `MAIL_IMAP_WORK_PASS` matches. `MAIL_IMAP_WORK_SUPER_SECRET_VALUE` does not,
/// so a secret stuffed into a variable name is not recognized or printed.
pub(crate) fn is_account_env_key(key: &str) -> bool {
    is_scoped_key(key, "MAIL_IMAP_", IMAP_ENV_SUFFIXES)
        || is_scoped_key(key, "MAIL_SMTP_", SMTP_ENV_SUFFIXES)
        || is_scoped_key(key, "MAIL_OAUTH2_", OAUTH_ENV_SUFFIXES)
        || is_scoped_key(key, "MAIL_GRAPH_", OAUTH_ENV_SUFFIXES)
        || is_scoped_key(key, "MAIL_EWS_", EWS_ENV_SUFFIXES)
}

/// Lowercase account id of a scoped account key, if `key` is one.
///
/// The segment is the text between a known protocol prefix and one known field
/// suffix. `MAIL_IMAP_WORK_EXTRA_HOST` is `work_extra`, not `work`. A trailing
/// underscore in the id is kept, matching [`valid_account_id`].
pub(crate) fn account_id_from_env_key(key: &str) -> Option<String> {
    if !is_account_env_key(key) {
        return None;
    }
    scoped_segment(key, "MAIL_IMAP_", IMAP_ENV_SUFFIXES)
        .or_else(|| scoped_segment(key, "MAIL_SMTP_", SMTP_ENV_SUFFIXES))
        .or_else(|| scoped_segment(key, "MAIL_OAUTH2_", OAUTH_ENV_SUFFIXES))
        .or_else(|| scoped_segment(key, "MAIL_GRAPH_", OAUTH_ENV_SUFFIXES))
        .or_else(|| scoped_segment(key, "MAIL_EWS_", EWS_ENV_SUFFIXES))
        .map(str::to_ascii_lowercase)
}

fn scoped_segment<'a>(key: &'a str, prefix: &str, suffixes: &[&str]) -> Option<&'a str> {
    let rest = key.strip_prefix(prefix)?;
    let mut best: Option<(&str, usize)> = None;
    for suffix in suffixes {
        let marker = format!("_{suffix}");
        let Some(segment) = rest.strip_suffix(&marker) else {
            continue;
        };
        if !is_env_segment(segment) {
            continue;
        }
        match best {
            Some((_, len)) if marker.len() <= len => {}
            _ => best = Some((segment, marker.len())),
        }
    }
    best.map(|(segment, _)| segment)
}

struct ResolvedPath {
    path: PathBuf,
    explicit: bool,
}

fn resolve_config_path(
    explicit: Option<&Path>,
    mail_config_file: Option<&str>,
    home: Option<&Path>,
) -> AppResult<ResolvedPath> {
    if let Some(path) = explicit {
        return Ok(ResolvedPath {
            path: expand_tilde(path, home)?,
            explicit: true,
        });
    }
    if let Some(path) = mail_config_file {
        if path == NO_CONFIG_FILE {
            return Err(AppError::invalid(
                "MAIL_CONFIG_FILE=none selects no config file (pass --config to choose one)",
            ));
        }
        return Ok(ResolvedPath {
            path: expand_tilde(Path::new(path), home)?,
            explicit: true,
        });
    }
    let home = require_home(home)?;
    Ok(ResolvedPath {
        path: home.join(".mail-mcp").join("config.toml"),
        explicit: false,
    })
}

fn load_at(location: &ResolvedPath, allow_missing: bool) -> AppResult<ConfigFile> {
    let Some(bytes) = read_config_bytes(&location.path)? else {
        if location.explicit && !allow_missing {
            return Err(AppError::invalid(format!(
                "config file {} does not exist",
                location.path.display()
            )));
        }
        return Ok(ConfigFile {
            path: location.path.clone(),
            document: DocumentMut::new(),
            exists: false,
            explicit: location.explicit,
            original: None,
            source: String::new(),
        });
    };
    let text = decode_config_text(&location.path, &bytes)?.to_owned();
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| toml_parse_error(&location.path, &text, error.span().as_ref()))?;
    let _ = read_settings(&document, &text)?;
    Ok(ConfigFile {
        path: location.path.clone(),
        document,
        exists: true,
        explicit: location.explicit,
        original: Some(bytes),
        source: text,
    })
}

/// `Ok(None)` is a missing file. Oversized files error before their bytes are
/// decoded. IO errors are the OS message only, never file contents.
fn read_config_bytes(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_CONFIG_BYTES => Err(config_too_large(path)),
        Ok(_) => {
            let bytes = fs::read(path).map_err(|error| {
                AppError::invalid(format!(
                    "could not read config file {}: {error}",
                    path.display()
                ))
            })?;
            if bytes.len() as u64 > MAX_CONFIG_BYTES {
                return Err(config_too_large(path));
            }
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::invalid(format!(
            "could not read config file {}: {error}",
            path.display()
        ))),
    }
}

fn decode_config_text<'a>(path: &Path, bytes: &'a [u8]) -> AppResult<&'a str> {
    if bytes.starts_with(UTF8_BOM) {
        return Err(AppError::invalid(format!(
            "config file {} starts with a UTF-8 BOM",
            path.display()
        )));
    }
    std::str::from_utf8(bytes).map_err(|_| {
        AppError::invalid(format!("config file {} is not valid UTF-8", path.display()))
    })
}

fn config_too_large(path: &Path) -> AppError {
    AppError::invalid(format!("config file {} is too large", path.display()))
}

fn toml_parse_error(path: &Path, source: &str, span: Option<&std::ops::Range<usize>>) -> AppError {
    match span {
        Some(span) => AppError::invalid(format!(
            "invalid TOML in config file {} at line {}",
            path.display(),
            line_number(source, span.start)
        )),
        None => AppError::invalid(format!("invalid TOML in config file {}", path.display())),
    }
}

fn read_file_bytes(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::invalid(format!(
            "could not read config file {}: {error}",
            path.display()
        ))),
    }
}

fn apply_legacy_smtp_timeout(values: &mut Settings, overrides: &[String]) {
    if overrides
        .iter()
        .any(|key| key == "MAIL_SMTP_SEND_TIMEOUT_MS")
    {
        return;
    }
    if overrides.iter().any(|key| key == "MAIL_SMTP_TIMEOUT_MS")
        && let Some(legacy) = values.get("MAIL_SMTP_TIMEOUT_MS").cloned()
    {
        values.insert("MAIL_SMTP_SEND_TIMEOUT_MS".to_owned(), legacy);
    }
}

fn lock_path_for(config: &Path) -> PathBuf {
    let name = config.file_name().unwrap_or(OsStr::new("config.toml"));
    let mut lock_name = OsString::from(".");
    lock_name.push(name);
    lock_name.push(".lock");
    parent_dir(config).join(lock_name)
}

fn acquire_save_lock(display: &Path, target: &Path) -> AppResult<File> {
    ensure_private_dir(parent_dir(target))?;
    let lock_path = lock_path_for(target);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        // Do not follow a symlink planted at the lock path. A hit fails the
        // open, so nothing is chmodded or written through the link.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&lock_path).map_err(|error| {
        AppError::invalid(format!(
            "could not lock config file {}: {error}",
            display.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                AppError::invalid(format!(
                    "could not lock config file {}: {error}",
                    display.display()
                ))
            })?;
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::Conflict(format!(
            "config file {} is being edited",
            display.display()
        ))),
        Err(std::fs::TryLockError::Error(error)) => Err(AppError::invalid(format!(
            "could not lock config file {}: {error}",
            display.display()
        ))),
    }
}

/// Filesystem path save should lock and write. A symlink resolves to its
/// target. A dangling symlink is refused. Any other path is returned unchanged.
fn resolve_save_target(path: &Path) -> AppResult<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|error| {
                AppError::invalid(format!(
                    "could not save config file {}: {error}",
                    path.display()
                ))
            })
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(AppError::invalid(format!(
            "could not save config file {}: {error}",
            path.display()
        ))),
    }
}

fn noclobber_write(display: &Path, target: &Path, bytes: &[u8]) -> AppResult<()> {
    prepare_config_write(display, target, bytes)?
        .persist_noclobber(target)
        .map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::invalid(format!(
                    "refusing to overwrite existing config file {}",
                    display.display()
                ))
            } else {
                AppError::invalid(format!(
                    "could not save config file {}: {}",
                    display.display(),
                    error.error
                ))
            }
        })?;
    Ok(())
}

fn prepare_config_write(
    display: &Path,
    target: &Path,
    bytes: &[u8],
) -> AppResult<tempfile::NamedTempFile> {
    let dir = parent_dir(target);
    let mut builder = tempfile::Builder::new();
    builder.prefix(".mail-mcp-").suffix(".tmp").rand_bytes(8);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o600));
    }
    let mut temp = builder.tempfile_in(dir).map_err(|error| {
        AppError::invalid(format!(
            "could not create a temporary config file in {}: {error}",
            dir.display()
        ))
    })?;
    temp.write_all(bytes).map_err(|error| {
        AppError::invalid(format!(
            "could not write config file {}: {error}",
            display.display()
        ))
    })?;
    temp.as_file().sync_all().map_err(|error| {
        AppError::invalid(format!(
            "could not write config file {}: {error}",
            display.display()
        ))
    })?;
    Ok(temp)
}

fn atomic_write(display: &Path, target: &Path, bytes: &[u8]) -> AppResult<()> {
    prepare_config_write(display, target, bytes)?
        .persist(target)
        .map_err(|error| {
            AppError::invalid(format!(
                "could not save config file {}: {}",
                display.display(),
                error.error
            ))
        })?;
    Ok(())
}

fn ensure_private_dir(dir: &Path) -> AppResult<()> {
    if dir == Path::new(".") || dir.as_os_str().is_empty() {
        return Ok(());
    }
    let created = missing_dirs(dir);
    if created.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|error| {
        AppError::invalid(format!(
            "could not create config directory {}: {error}",
            dir.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in &created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
                AppError::invalid(format!(
                    "could not set permissions on config directory {}: {error}",
                    path.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = dir;
    loop {
        if current.exists() || current.parent().is_none() {
            break;
        }
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    missing
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn apply_env_overrides(
    values: &mut Settings,
    env: impl IntoIterator<Item = (OsString, OsString)>,
) -> AppResult<Vec<String>> {
    let mut overrides = Vec::new();
    for (key, value) in env {
        let Some(key) = key.to_str() else {
            continue;
        };
        if !is_config_key(key) {
            continue;
        }
        let Some(value) = value.to_str() else {
            return Err(AppError::invalid(format!(
                "environment variable {key} contains non-unicode data"
            )));
        };
        // An empty or whitespace-only value is unset: the file or default
        // stands, and the key is not reported as an override.
        if value.trim().is_empty() {
            continue;
        }
        overrides.push(key.to_owned());
        values.insert(key.to_owned(), value.to_owned());
    }
    overrides.sort();
    overrides.dedup();
    Ok(overrides)
}

/// Read recognized `MAIL_*` keys from the `.env` dotenvy would load.
///
/// The walk matches dotenvy 0.15 `find`: start at the current directory, take
/// the first regular `.env`, and stop with no settings on a non-NotFound
/// error or at the root. Read errors are ignored the same way `dotenv().ok()`
/// ignored them. Nothing is written to the process environment.
fn read_legacy_dotenv() -> Settings {
    let Some(path) = find_legacy_dotenv() else {
        return Settings::new();
    };
    let Ok(bytes) = fs::read(&path) else {
        return Settings::new();
    };
    mail_settings_from_dotenv_bytes(&bytes)
}

/// `dir/.env` via `fs::metadata`, then parents. A regular file wins, including
/// a symlink to one. A directory of that name does not.
fn find_legacy_dotenv() -> Option<PathBuf> {
    let mut directory = std::env::current_dir().ok()?;
    loop {
        let candidate = directory.join(".env");
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return Some(candidate),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        match directory.parent() {
            Some(parent) if parent != directory => directory = parent.to_path_buf(),
            _ => return None,
        }
    }
}

fn mail_settings_from_dotenv_bytes(bytes: &[u8]) -> Settings {
    let bytes = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    mail_settings_from_dotenv_iter(dotenvy::from_read_iter(Cursor::new(bytes)))
}

/// First occurrence of each key wins, including an empty one. A parse error
/// keeps the keys already read and drops the rest. Recognized keys whose
/// value is empty or whitespace are then unset.
fn mail_settings_from_dotenv_iter<I, E>(entries: I) -> Settings
where
    I: IntoIterator<Item = Result<(String, String), E>>,
{
    let mut settings = Settings::new();
    for entry in entries {
        let Ok((key, value)) = entry else {
            break;
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = settings.entry(key) {
            slot.insert(value);
        }
    }
    settings.retain(|key, value| is_config_key(key) && !value.trim().is_empty());
    settings
}

fn read_settings(document: &DocumentMut, source: &str) -> AppResult<Settings> {
    let mut settings = Settings::new();
    for (name, item) in document.iter() {
        match name {
            "settings" => {
                let table = expect_table(source, item, "settings")?;
                apply_known_fields(&mut settings, source, table, "settings", None, SETTINGS)?;
            }
            "accounts" => {
                let table = expect_table(source, item, "accounts")?;
                apply_accounts(&mut settings, source, table)?;
            }
            _ => {
                return Err(schema_err(source, name, "unknown key"));
            }
        }
    }
    Ok(settings)
}

fn apply_accounts(
    settings: &mut Settings,
    source: &str,
    accounts: &dyn TableLike,
) -> AppResult<()> {
    for (id, item) in accounts.iter() {
        let path = format!("accounts.{id}");
        if !valid_account_id(id) {
            return Err(schema_err(source, &path, "invalid account id"));
        }
        let segment = id.to_ascii_uppercase();
        let table = expect_table(source, item, &path)?;
        for (protocol, protocol_item) in table.iter() {
            let path = format!("accounts.{id}.{protocol}");
            let Some(spec) = PROTOCOLS.iter().find(|spec| spec.name == protocol) else {
                return Err(schema_err(source, &path, "unknown key"));
            };
            let fields = expect_table(source, protocol_item, &path)?;
            apply_known_fields(
                settings,
                source,
                fields,
                &path,
                Some((spec.env_prefix, segment.as_str())),
                spec.fields,
            )?;
        }
    }
    Ok(())
}

fn apply_known_fields(
    settings: &mut Settings,
    source: &str,
    table: &dyn TableLike,
    path: &str,
    account: Option<(&str, &str)>,
    fields: &[Field],
) -> AppResult<()> {
    for (name, item) in table.iter() {
        let field_path = format!("{path}.{name}");
        let Some(field) = fields.iter().find(|field| field.name == name) else {
            return Err(schema_err(source, &field_path, "unknown key"));
        };
        let value = stringify_value(item, field.kind)
            .map_err(|detail| schema_err(source, &field_path, detail))?;
        let key = match account {
            Some((prefix, segment)) => format!("{prefix}{segment}_{}", field.env_suffix),
            None => field.env_suffix.to_owned(),
        };
        settings.insert(key, value);
    }
    Ok(())
}

fn expect_table<'a>(source: &str, item: &'a Item, path: &str) -> AppResult<&'a dyn TableLike> {
    item.as_table_like()
        .ok_or_else(|| schema_err(source, path, "expected a table"))
}

fn stringify_value(item: &Item, kind: ValueKind) -> Result<String, &'static str> {
    match kind {
        ValueKind::Bool => match item.as_bool() {
            Some(true) => Ok("true".to_owned()),
            Some(false) => Ok("false".to_owned()),
            None => Err("expected a boolean"),
        },
        ValueKind::String => item.as_str().map(str::to_owned).ok_or("expected a string"),
        ValueKind::U16 => integer_string(item, 0, u64::from(u16::MAX), "0 to 65535"),
        ValueKind::U64 => integer_string(item, 0, i64::MAX as u64, "a non-negative integer"),
        ValueKind::Usize => {
            let text = integer_string(item, 0, i64::MAX as u64, "a non-negative integer")?;
            let value: i64 = text.parse().map_err(|_| "expected an integer")?;
            usize::try_from(value).map_err(|_| "expected an integer that fits usize")?;
            Ok(text)
        }
    }
}

fn integer_string(
    item: &Item,
    min: u64,
    max: u64,
    expected: &'static str,
) -> Result<String, &'static str> {
    let Some(value) = item.as_integer() else {
        return Err("expected an integer");
    };
    if value < 0 {
        return Err(expected);
    }
    let value = value as u64;
    if value < min || value > max {
        return Err(expected);
    }
    Ok(value.to_string())
}

/// Line of `path` (`settings.write_enabled`, `accounts.work.imap`) in the original
/// text. toml_edit drops spans when a document becomes editable, so this scan
/// is what keeps errors on a line without quoting the line.
fn line_of_path(source: &str, path: &str) -> Option<usize> {
    let mut current: Vec<String> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("").trim();
        if code.is_empty() {
            continue;
        }
        if code.starts_with('[') && code.ends_with(']') {
            let header = code.trim_matches('[').trim_matches(']');
            current = header
                .split('.')
                .map(|part| part.trim().trim_matches('"').to_owned())
                .filter(|part| !part.is_empty())
                .collect();
            let here = current.join(".");
            if here == path || here.starts_with(&format!("{path}.")) {
                return Some(index + 1);
            }
            continue;
        }
        let Some((key, _)) = code.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        if key.is_empty() {
            continue;
        }
        let mut full = current.clone();
        full.push(key.to_owned());
        if full.join(".") == path {
            return Some(index + 1);
        }
    }
    None
}

fn line_number(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    let boundary = if source.is_char_boundary(offset) {
        offset
    } else {
        (1..=offset)
            .rev()
            .find(|index| source.is_char_boundary(*index))
            .unwrap_or(0)
    };
    source[..boundary]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn schema_err(source: &str, path: &str, label: &str) -> AppError {
    match line_of_path(source, path) {
        Some(line) => AppError::invalid(format!("{label} at line {line}")),
        None => AppError::invalid(label.to_owned()),
    }
}

fn mail_config_file_from_env() -> AppResult<Option<String>> {
    match std::env::var("MAIL_CONFIG_FILE") {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_owned()))
            }
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(AppError::invalid(
            "environment variable MAIL_CONFIG_FILE contains non-unicode data",
        )),
    }
}

fn home_dir() -> Option<PathBuf> {
    home_from(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn home_from(home: Option<&OsStr>, userprofile: Option<&OsStr>) -> Option<PathBuf> {
    non_empty(home)
        .or_else(|| non_empty(userprofile))
        .map(PathBuf::from)
}

fn non_empty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| !value.is_empty())
}

fn require_home(home: Option<&Path>) -> AppResult<PathBuf> {
    home.map(Path::to_path_buf).ok_or_else(|| {
        AppError::invalid(
            "could not resolve a home directory (set HOME or USERPROFILE, or pass --config)",
        )
    })
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> AppResult<PathBuf> {
    let Some(text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if text == "~" {
        return require_home(home);
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return Ok(require_home(home)?.join(rest));
    }
    Ok(PathBuf::from(text))
}

fn is_scoped_key(key: &str, prefix: &str, suffixes: &[&str]) -> bool {
    let Some(rest) = key.strip_prefix(prefix) else {
        return false;
    };
    suffixes.iter().any(|suffix| {
        rest.strip_suffix(&format!("_{suffix}"))
            .is_some_and(is_env_segment)
    })
}

fn is_env_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    /// For `[settings]`, this is the full `MAIL_*` key. For account fields, it
    /// is the suffix appended after `MAIL_<PROTOCOL>_<ID>_`.
    env_suffix: &'static str,
    kind: ValueKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Bool,
    String,
    U16,
    U64,
    Usize,
}

struct Protocol {
    name: &'static str,
    env_prefix: &'static str,
    fields: &'static [Field],
}

const GLOBAL_KEYS: &[&str] = &[
    "MAIL_IMAP_WRITE_ENABLED",
    "MAIL_DRAFTS_ENABLED",
    "MAIL_SMTP_WRITE_ENABLED",
    "MAIL_SMTP_SAVE_SENT",
    "MAIL_SMTP_CONNECT_TIMEOUT_MS",
    "MAIL_SMTP_SEND_TIMEOUT_MS",
    "MAIL_SMTP_TIMEOUT_MS",
    "MAIL_IMAP_CONNECT_TIMEOUT_MS",
    "MAIL_IMAP_GREETING_TIMEOUT_MS",
    "MAIL_IMAP_SOCKET_TIMEOUT_MS",
    "MAIL_IMAP_CURSOR_TTL_SECONDS",
    "MAIL_IMAP_CURSOR_MAX_ENTRIES",
    "MAIL_IMAP_MAX_MAILBOXES",
    "MAIL_IMAP_SEARCH_CONCURRENCY",
    "MAIL_ATTACHMENT_DOWNLOAD_DIR",
    "MAIL_ATTACHMENT_UPLOAD_DIR",
    "MAIL_MCP_TRANSPORT",
    "MAIL_MCP_HTTP_HOST",
    "MAIL_MCP_HTTP_PORT",
    "MAIL_MCP_HTTP_PATH",
];

const IMAP_ENV_SUFFIXES: &[&str] = &["HOST", "PORT", "SECURE", "USER", "PASS"];
const SMTP_ENV_SUFFIXES: &[&str] = &[
    "HOST",
    "PORT",
    "USER",
    "PASS",
    "SECURE",
    "SAVE_SENT",
    "FROM_EMAIL",
];
const OAUTH_ENV_SUFFIXES: &[&str] = &["PROVIDER", "CLIENT_ID", "CLIENT_SECRET", "REFRESH_TOKEN"];
const EWS_ENV_SUFFIXES: &[&str] = &["USER", "CLIENT_ID", "CLIENT_SECRET", "REFRESH_TOKEN"];

const SETTINGS: &[Field] = &[
    Field {
        name: "write_enabled",
        env_suffix: "MAIL_IMAP_WRITE_ENABLED",
        kind: ValueKind::Bool,
    },
    Field {
        name: "drafts_enabled",
        env_suffix: "MAIL_DRAFTS_ENABLED",
        kind: ValueKind::Bool,
    },
    Field {
        name: "smtp_write_enabled",
        env_suffix: "MAIL_SMTP_WRITE_ENABLED",
        kind: ValueKind::Bool,
    },
    Field {
        name: "smtp_save_sent",
        env_suffix: "MAIL_SMTP_SAVE_SENT",
        kind: ValueKind::Bool,
    },
    Field {
        name: "smtp_connect_timeout_ms",
        env_suffix: "MAIL_SMTP_CONNECT_TIMEOUT_MS",
        kind: ValueKind::U64,
    },
    Field {
        name: "smtp_send_timeout_ms",
        env_suffix: "MAIL_SMTP_SEND_TIMEOUT_MS",
        kind: ValueKind::U64,
    },
    Field {
        name: "connect_timeout_ms",
        env_suffix: "MAIL_IMAP_CONNECT_TIMEOUT_MS",
        kind: ValueKind::U64,
    },
    Field {
        name: "greeting_timeout_ms",
        env_suffix: "MAIL_IMAP_GREETING_TIMEOUT_MS",
        kind: ValueKind::U64,
    },
    Field {
        name: "socket_timeout_ms",
        env_suffix: "MAIL_IMAP_SOCKET_TIMEOUT_MS",
        kind: ValueKind::U64,
    },
    Field {
        name: "cursor_ttl_seconds",
        env_suffix: "MAIL_IMAP_CURSOR_TTL_SECONDS",
        kind: ValueKind::U64,
    },
    Field {
        name: "cursor_max_entries",
        env_suffix: "MAIL_IMAP_CURSOR_MAX_ENTRIES",
        kind: ValueKind::Usize,
    },
    Field {
        name: "max_mailboxes",
        env_suffix: "MAIL_IMAP_MAX_MAILBOXES",
        kind: ValueKind::Usize,
    },
    Field {
        name: "search_concurrency",
        env_suffix: "MAIL_IMAP_SEARCH_CONCURRENCY",
        kind: ValueKind::Usize,
    },
    Field {
        name: "attachment_download_dir",
        env_suffix: "MAIL_ATTACHMENT_DOWNLOAD_DIR",
        kind: ValueKind::String,
    },
    Field {
        name: "attachment_upload_dir",
        env_suffix: "MAIL_ATTACHMENT_UPLOAD_DIR",
        kind: ValueKind::String,
    },
];

const IMAP_FIELDS: &[Field] = &[
    Field {
        name: "host",
        env_suffix: "HOST",
        kind: ValueKind::String,
    },
    Field {
        name: "port",
        env_suffix: "PORT",
        kind: ValueKind::U16,
    },
    Field {
        name: "secure",
        env_suffix: "SECURE",
        kind: ValueKind::Bool,
    },
    Field {
        name: "user",
        env_suffix: "USER",
        kind: ValueKind::String,
    },
    Field {
        name: "pass",
        env_suffix: "PASS",
        kind: ValueKind::String,
    },
];

const SMTP_FIELDS: &[Field] = &[
    Field {
        name: "host",
        env_suffix: "HOST",
        kind: ValueKind::String,
    },
    Field {
        name: "port",
        env_suffix: "PORT",
        kind: ValueKind::U16,
    },
    Field {
        name: "user",
        env_suffix: "USER",
        kind: ValueKind::String,
    },
    Field {
        name: "pass",
        env_suffix: "PASS",
        kind: ValueKind::String,
    },
    Field {
        name: "security",
        env_suffix: "SECURE",
        kind: ValueKind::String,
    },
    Field {
        name: "save_sent",
        env_suffix: "SAVE_SENT",
        kind: ValueKind::Bool,
    },
    Field {
        name: "from_email",
        env_suffix: "FROM_EMAIL",
        kind: ValueKind::String,
    },
];

const OAUTH_FIELDS: &[Field] = &[
    Field {
        name: "provider",
        env_suffix: "PROVIDER",
        kind: ValueKind::String,
    },
    Field {
        name: "client_id",
        env_suffix: "CLIENT_ID",
        kind: ValueKind::String,
    },
    Field {
        name: "client_secret",
        env_suffix: "CLIENT_SECRET",
        kind: ValueKind::String,
    },
    Field {
        name: "refresh_token",
        env_suffix: "REFRESH_TOKEN",
        kind: ValueKind::String,
    },
];

const EWS_FIELDS: &[Field] = &[
    Field {
        name: "user",
        env_suffix: "USER",
        kind: ValueKind::String,
    },
    Field {
        name: "client_id",
        env_suffix: "CLIENT_ID",
        kind: ValueKind::String,
    },
    Field {
        name: "client_secret",
        env_suffix: "CLIENT_SECRET",
        kind: ValueKind::String,
    },
    Field {
        name: "refresh_token",
        env_suffix: "REFRESH_TOKEN",
        kind: ValueKind::String,
    },
];

const PROTOCOLS: &[Protocol] = &[
    Protocol {
        name: "imap",
        env_prefix: "MAIL_IMAP_",
        fields: IMAP_FIELDS,
    },
    Protocol {
        name: "smtp",
        env_prefix: "MAIL_SMTP_",
        fields: SMTP_FIELDS,
    },
    Protocol {
        name: "oauth2",
        env_prefix: "MAIL_OAUTH2_",
        fields: OAUTH_FIELDS,
    },
    Protocol {
        name: "graph_oauth2",
        env_prefix: "MAIL_GRAPH_",
        fields: OAUTH_FIELDS,
    },
    Protocol {
        name: "ews",
        env_prefix: "MAIL_EWS_",
        fields: EWS_FIELDS,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::Table;

    const SECRET: &str = "super-secret-token-value";

    fn parsed(text: &str) -> ConfigFile {
        let document = text.parse::<DocumentMut>().expect("test toml");
        ConfigFile {
            path: PathBuf::from("config.toml"),
            document,
            exists: true,
            explicit: true,
            original: Some(text.as_bytes().to_vec()),
            source: text.to_owned(),
        }
    }

    fn shown(error: &AppError) -> String {
        error.to_string()
    }

    fn no_env() -> Vec<(OsString, OsString)> {
        Vec::new()
    }

    #[test]
    fn account_ids_accept_canonical_names_only() {
        for id in [
            "a", "z", "0", "9", "a0", "work", "work_", "a_", "a_b", "a__b", "default",
        ] {
            assert!(valid_account_id(id), "{id}");
        }
        let max = "a".repeat(MAX_ACCOUNT_ID_LEN);
        assert!(valid_account_id(&max));
        assert!(!valid_account_id(&format!("{max}a")));
        let trailing = format!("a{}", "_".repeat(MAX_ACCOUNT_ID_LEN - 1));
        assert_eq!(trailing.len(), MAX_ACCOUNT_ID_LEN);
        assert!(valid_account_id(&trailing));
        for id in [
            "", "_a", "_", "A", "Work", "ab-c", "ab.c", "a b", "ü", "a/b",
        ] {
            assert!(!valid_account_id(id), "{id}");
        }
    }

    #[test]
    fn home_fallback_stays_inside_the_injected_home() {
        let home = tempfile::tempdir().unwrap();
        let location = resolve_config_path(None, None, Some(home.path())).unwrap();
        let expected = home.path().join(".mail-mcp").join("config.toml");
        assert_eq!(location.path, expected);
        assert!(!location.explicit);
        let file = load_at(&location, false).unwrap();
        assert!(!file.exists);
        assert_eq!(file.path, expected);
        assert!(!expected.exists());
        let tilde =
            resolve_config_path(Some(Path::new("~/cfg.toml")), None, Some(home.path())).unwrap();
        assert_eq!(tilde.path, home.path().join("cfg.toml"));
        assert!(tilde.explicit);
    }

    #[test]
    fn path_precedence_expands_tilde_and_prefers_explicit_paths() {
        let home = Path::new("/home/me");
        let explicit =
            resolve_config_path(Some(Path::new("/cli.toml")), Some("/env.toml"), Some(home))
                .unwrap();
        assert_eq!(explicit.path, PathBuf::from("/cli.toml"));
        assert!(explicit.explicit);

        let from_env = resolve_config_path(None, Some("~/cfg.toml"), Some(home)).unwrap();
        assert_eq!(from_env.path, PathBuf::from("/home/me/cfg.toml"));
        assert!(from_env.explicit);

        let default_path = resolve_config_path(None, None, Some(home)).unwrap();
        assert_eq!(
            default_path.path,
            PathBuf::from("/home/me/.mail-mcp/config.toml")
        );
        assert!(!default_path.explicit);

        assert_eq!(
            resolve_config_path(Some(Path::new("~")), None, Some(home))
                .unwrap()
                .path,
            PathBuf::from("/home/me")
        );
        assert_eq!(
            resolve_config_path(Some(Path::new("~\\mail.toml")), None, Some(home))
                .unwrap()
                .path,
            PathBuf::from("/home/me").join("mail.toml")
        );
        assert_eq!(
            resolve_config_path(Some(Path::new("~foo")), None, Some(home))
                .unwrap()
                .path,
            PathBuf::from("~foo")
        );
        assert!(resolve_config_path(None, None, None).is_err());
        assert!(resolve_config_path(Some(Path::new("~/a.toml")), None, None).is_err());
        assert!(resolve_config_path(Some(Path::new("/a.toml")), None, None).is_ok());

        assert!(resolve_config_path(None, Some("none"), Some(home)).is_err());
        let cli_over_none =
            resolve_config_path(Some(Path::new("/cli.toml")), Some("none"), Some(home)).unwrap();
        assert_eq!(cli_over_none.path, PathBuf::from("/cli.toml"));
    }

    #[test]
    fn environment_only_config_is_not_a_cwd_path_and_cannot_be_saved() {
        let mut file = ConfigFile::from_environment();
        assert!(file.path.as_os_str().is_empty());
        assert!(!file.exists);
        assert!(!file.explicit);
        let message = shown(&file.save(false).unwrap_err());
        assert!(message.contains("--config"), "{message}");
        assert_ne!(file.path, PathBuf::from("config.toml"));
        assert_ne!(file.path, PathBuf::from("."));
    }

    #[test]
    fn home_prefers_home_then_userprofile() {
        assert_eq!(
            home_from(
                Some(OsStr::new("/home/me")),
                Some(OsStr::new("C:\\Users\\me"))
            ),
            Some(PathBuf::from("/home/me"))
        );
        assert_eq!(
            home_from(None, Some(OsStr::new("C:\\Users\\me"))),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        assert_eq!(home_from(Some(OsStr::new("")), Some(OsStr::new(""))), None);
    }

    #[test]
    fn legacy_dotenv_runs_only_without_an_explicit_or_existing_file() {
        let mut file = parsed("");
        file.exists = false;
        file.explicit = false;
        assert!(file.uses_legacy_dotenv(true));
        assert!(!file.uses_legacy_dotenv(false));
        file.explicit = true;
        assert!(!file.uses_legacy_dotenv(true));
        file.explicit = false;
        file.exists = true;
        assert!(!file.uses_legacy_dotenv(true));
    }

    #[test]
    fn dotenv_iterator_keeps_recognized_keys_and_drops_other_secrets() {
        let entries = [
            Ok(("OTHER".to_owned(), SECRET.to_owned())),
            Ok((
                "MAIL_IMAP_WORK_HOST".to_owned(),
                "imap.example.com".to_owned(),
            )),
            Err("BROKEN super-secret-token-value"),
            Ok(("MAIL_NOT_A_SETTING".to_owned(), SECRET.to_owned())),
        ];
        let settings = mail_settings_from_dotenv_iter(entries);
        assert_eq!(
            settings.get("MAIL_IMAP_WORK_HOST").map(String::as_str),
            Some("imap.example.com")
        );
        assert_eq!(settings.len(), 1);
        assert!(!format!("{settings:?}").contains(SECRET));
    }

    #[test]
    fn schema_errors_name_the_key_and_line_without_secret_values() {
        let text = format!(
            "[settings]\npassword = \"{SECRET}\"\nsmtp_timeout_ms = 1\n[accounts.Work.imap]\nhost = \"x\"\n"
        );
        let error = parsed(&text)
            .resolve_from(Settings::new(), no_env())
            .unwrap_err();
        let message = shown(&error);
        assert!(!message.contains(SECRET));
        assert!(message.contains("unknown key"));
        assert!(!message.contains("password"));
        assert!(message.contains("line"));

        let text = format!("[accounts.work.imap]\npass = \"{SECRET}\"\nport = \"993\"\n");
        let message = shown(
            &parsed(&text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(!message.contains(SECRET));
        assert!(!message.contains("accounts.work.imap.port"));
        assert!(message.contains("expected an integer"));

        let text = format!("pass = \"{SECRET}");
        let message = shown(&read_parse_error(&text));
        assert!(!message.contains(SECRET));
        assert!(message.contains("invalid TOML"));
        assert!(message.contains("line"));

        let text = "[accounts.work.caldav]\nhost = \"x\"\n";
        let message = shown(
            &parsed(text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("unknown key"));
        assert!(!message.contains("caldav"));

        let text = "[accounts.work.imap]\nport = 65536\n";
        let message = shown(
            &parsed(text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("0 to 65535"));
        assert!(!message.contains("accounts.work.imap.port"));
        assert!(!message.contains("65536"));

        let text = "[settings]\nwrite_enabled = \"false\"\n";
        let message = shown(
            &parsed(text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("expected a boolean"));
        assert!(!message.contains("false"));
    }

    fn read_parse_error(text: &str) -> AppError {
        text.parse::<DocumentMut>()
            .expect_err("malformed toml")
            .span()
            .as_ref()
            .map(|span| toml_parse_error(Path::new("config.toml"), text, Some(span)))
            .unwrap_or_else(|| toml_parse_error(Path::new("config.toml"), text, None))
    }

    #[test]
    fn empty_document_and_boundaries_are_schema_valid() {
        assert!(
            parsed("")
                .resolve_from(Settings::new(), no_env())
                .unwrap()
                .values
                .is_empty()
        );
        assert!(
            parsed("[accounts]\n")
                .resolve_from(Settings::new(), no_env())
                .is_ok()
        );
        assert!(
            parsed("[accounts.work]\n")
                .resolve_from(Settings::new(), no_env())
                .is_ok()
        );
        assert!(
            parsed("[accounts.work.imap]\nport = 0\n")
                .resolve_from(Settings::new(), no_env())
                .is_ok()
        );
        assert!(
            parsed("[accounts.work.imap]\nport = 65535\n")
                .resolve_from(Settings::new(), no_env())
                .is_ok()
        );
        assert!(
            parsed("[accounts.work.imap]\nport = -1\n")
                .resolve_from(Settings::new(), no_env())
                .is_err()
        );
    }

    #[test]
    fn transport_settings_are_not_read_from_a_file() {
        for key in [
            "transport = \"http\"",
            "http_host = \"0.0.0.0\"",
            "http_port = 9000",
            "http_path = \"/mcp\"",
        ] {
            let text = format!("[settings]\n{key}\n");
            let message = shown(
                &parsed(&text)
                    .resolve_from(Settings::new(), no_env())
                    .unwrap_err(),
            );
            assert!(message.contains("unknown key"), "{key}");
        }
    }

    #[test]
    fn explicit_missing_file_errors_only_when_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let error = ConfigFile::load(Some(&path), false).unwrap_err();
        assert!(shown(&error).contains("does not exist"));
        assert!(!shown(&error).contains(SECRET));
        let file = ConfigFile::load(Some(&path), true).unwrap();
        assert!(!file.exists);
        assert!(file.explicit);
        assert_eq!(file.path, path);
    }

    #[test]
    fn failed_save_keeps_the_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = format!(
            "# keep me\n[accounts.work.imap]\npass = \"{SECRET}\"\nhost = \"imap.example.com\" # stay\n"
        );
        fs::write(&path, &original).unwrap();
        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        file.document["settings"]["password"] = toml_edit::value(SECRET);
        let error = file.save(false).unwrap_err();
        assert!(!shown(&error).contains(SECRET));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        fs::write(&path, b"# replaced\n").unwrap();
        let error = file.save(false).unwrap_err();
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(fs::read(&path).unwrap(), b"# replaced\n");
    }

    #[test]
    fn save_preserves_comments_and_refuses_create_only_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# keep me\n[settings]\nwrite_enabled = false # stay\n\n[accounts.home.imap]\nhost = \"imap.home.example\" # home\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        let settings = file.document["settings"].as_table_mut().unwrap();
        assign_value(settings, "write_enabled", Value::from(true));
        file.save(false).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep me"), "{saved}");
        assert!(saved.contains("# stay"), "{saved}");
        assert!(saved.contains("# home"), "{saved}");
        assert!(saved.contains("true"), "{saved}");
        let before = fs::read(&path).unwrap();
        let error = file.save(true).unwrap_err();
        assert!(shown(&error).contains("refusing to overwrite"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn empty_init_creates_a_private_file_without_clobbering_parent_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut file = ConfigFile::load(Some(&path), true).unwrap();
        assert!(!file.exists);
        file.document["accounts"] = toml_edit::Item::Table(Table::new());
        file.save(true).unwrap();
        assert!(file.exists);
        assert!(path.is_file());
        assert!(file.save(true).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir.path()), 0o755);
            assert_eq!(mode(&dir.path().join("nested")), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
    }

    #[test]
    fn bom_utf8_and_oversized_files_do_not_reveal_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut bom = UTF8_BOM.to_vec();
        bom.extend(format!("pass = \"{SECRET}\"\n").into_bytes());
        fs::write(&path, &bom).unwrap();
        let message = shown(&ConfigFile::load(Some(&path), false).unwrap_err());
        assert!(message.contains("UTF-8 BOM"));
        assert!(!message.contains(SECRET));

        let mut invalid = format!("pass = \"{SECRET}\"\n").into_bytes();
        invalid.push(0xFF);
        fs::write(&path, &invalid).unwrap();
        let message = shown(&ConfigFile::load(Some(&path), false).unwrap_err());
        assert!(message.contains("not valid UTF-8"));
        assert!(!message.contains(SECRET));

        let mut oversized = SECRET.as_bytes().to_vec();
        oversized.resize(MAX_CONFIG_BYTES as usize + 1, b'x');
        fs::write(&path, &oversized).unwrap();
        let message = shown(&ConfigFile::load(Some(&path), false).unwrap_err());
        assert!(message.contains("too large"));
        assert!(!message.contains(SECRET));

        let id = SECRET.repeat(4);
        let text = format!("[accounts.{id}.imap]\nhost = \"x\"\n");
        let message = shown(
            &parsed(&text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("invalid account id"));
        assert!(!message.contains(SECRET));
        assert!(!message.contains(&id));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_errors_without_exposing_its_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, format!("pass = \"{SECRET}\"\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let error = ConfigFile::load(Some(&path), true).unwrap_err();
        assert!(!shown(&error).contains(SECRET));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains(SECRET));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_env_values_do_not_leak_file_secrets() {
        use std::os::unix::ffi::OsStringExt;
        let text = format!("[accounts.work.imap]\npass = \"{SECRET}\"\nhost = \"from-file\"\n");
        let file = parsed(&text);
        let env = [(
            OsString::from("MAIL_IMAP_WORK_HOST"),
            OsString::from_vec(vec![0xff, 0xfe]),
        )];
        let message = shown(&file.resolve_from(Settings::new(), env).unwrap_err());
        assert!(message.contains("MAIL_IMAP_WORK_HOST"));
        assert!(!message.contains(SECRET));
        assert!(!message.contains("from-file"));
    }

    #[test]
    fn unknown_key_and_invalid_id_do_not_echo_the_token() {
        let text = format!("\"{SECRET}\" = true\n");
        let message = shown(
            &parsed(&text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("unknown key"));
        assert!(message.contains("line"));
        assert!(!message.contains(SECRET));

        let text = format!("[accounts.\"{SECRET}\"]\nhost = \"x\"\n");
        let message = shown(
            &parsed(&text)
                .resolve_from(Settings::new(), no_env())
                .unwrap_err(),
        );
        assert!(message.contains("invalid account id"));
        assert!(!message.contains(SECRET));
    }

    #[test]
    fn inline_and_underscore_accounts_keep_isolated_credentials() {
        let text = format!(
            "[accounts.work.imap]\nhost = \"imap.work.example\"\nuser = \"work@example.com\"\npass = \"secret-for-work\"\n\n[accounts.work_.imap]\nhost = \"imap.trail.example\"\nuser = \"trail@example.com\"\npass = \"{SECRET}\"\n\n[accounts.extra]\nimap = {{ host = \"imap.extra.example\", user = \"extra@example.com\", pass = \"inline-secret\" }}\n"
        );
        let resolved = parsed(&text)
            .resolve_from(Settings::new(), no_env())
            .unwrap();
        assert_eq!(
            resolved.values.get("MAIL_IMAP_WORK_HOST").unwrap(),
            "imap.work.example"
        );
        assert_eq!(
            resolved.values.get("MAIL_IMAP_WORK__HOST").unwrap(),
            "imap.trail.example"
        );
        assert_eq!(resolved.values.get("MAIL_IMAP_WORK__PASS").unwrap(), SECRET);
        assert_eq!(
            resolved.values.get("MAIL_IMAP_EXTRA_PASS").unwrap(),
            "inline-secret"
        );
        let loaded = crate::config::ServerConfig::load(&resolved.values).unwrap();
        assert_eq!(loaded.accounts["work"].host, "imap.work.example");
        assert_eq!(loaded.accounts["work_"].host, "imap.trail.example");
        assert_eq!(loaded.accounts["extra"].host, "imap.extra.example");
    }

    #[test]
    fn legacy_smtp_timeout_env_overrides_file_send_timeout() {
        let text = "[settings]\nsmtp_send_timeout_ms = 1111\n[accounts.work.imap]\nhost = \"imap.example.com\"\nuser = \"user@example.com\"\npass = \"pw\"\n";
        let file = parsed(text);
        let legacy = file
            .resolve_from(
                Settings::new(),
                [(
                    OsString::from("MAIL_SMTP_TIMEOUT_MS"),
                    OsString::from("2222"),
                )],
            )
            .unwrap();
        assert_eq!(
            legacy.values.get("MAIL_SMTP_SEND_TIMEOUT_MS").unwrap(),
            "2222"
        );
        assert_eq!(
            crate::config::ServerConfig::load(&legacy.values)
                .unwrap()
                .smtp_send_timeout_ms,
            2222
        );
        let both = file
            .resolve_from(
                Settings::new(),
                [
                    (
                        OsString::from("MAIL_SMTP_TIMEOUT_MS"),
                        OsString::from("2222"),
                    ),
                    (
                        OsString::from("MAIL_SMTP_SEND_TIMEOUT_MS"),
                        OsString::from("3333"),
                    ),
                ],
            )
            .unwrap();
        assert_eq!(
            both.values.get("MAIL_SMTP_SEND_TIMEOUT_MS").unwrap(),
            "3333"
        );
        assert_eq!(
            crate::config::ServerConfig::load(&both.values)
                .unwrap()
                .smtp_send_timeout_ms,
            3333
        );
    }

    #[test]
    fn save_does_not_write_while_the_advisory_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[accounts]\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        file.document["settings"]["write_enabled"] = toml_edit::value(false);
        let held = acquire_save_lock(&path, &path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock_path = lock_path_for(&path);
            assert_eq!(lock_path.parent(), Some(dir.path()));
            assert_eq!(
                fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(matches!(
            acquire_save_lock(&path, &path),
            Err(AppError::Conflict(_))
        ));
        let error = file.save(false).unwrap_err();
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        drop(held);
        file.save(false).unwrap();
        let reloaded = ConfigFile::load(Some(&path), false).unwrap();
        assert_eq!(
            reloaded
                .resolve_from(Settings::new(), no_env())
                .unwrap()
                .values["MAIL_IMAP_WRITE_ENABLED"],
            "false"
        );
    }

    #[test]
    fn create_only_refuses_an_existing_file_without_rewriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut file = ConfigFile::load(Some(&path), true).unwrap();
        file.document["accounts"] = toml_edit::Item::Table(Table::new());
        file.save(true).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(file.save(true).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn process_overrides_file_without_losing_other_account_fields() {
        use secrecy::ExposeSecret;

        let file = parsed(
            "[settings]\nwrite_enabled = true\n[accounts.work.imap]\nhost = \"imap.example.com\"\nuser = \"reader@example.com\"\npass = \"file-password\"\n",
        );
        let resolved = file
            .resolve_from(
                Settings::new(),
                [
                    (
                        OsString::from("MAIL_IMAP_WORK_PASS"),
                        OsString::from(SECRET),
                    ),
                    (
                        OsString::from("MAIL_IMAP_WRITE_ENABLED"),
                        OsString::from("false"),
                    ),
                ],
            )
            .unwrap();
        let server = crate::config::ServerConfig::load(&resolved.values).unwrap();
        let account = &server.accounts["work"];
        assert_eq!(account.host, "imap.example.com");
        assert_eq!(account.user, "reader@example.com");
        assert_eq!(account.pass.as_ref().unwrap().expose_secret(), SECRET);
        assert!(!server.write_enabled);
        assert!(!format!("{resolved:?}").contains(SECRET));
    }

    #[test]
    fn legacy_dotenv_recognizes_a_bom_prefixed_key() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend(b"MAIL_IMAP_WORK_HOST=imap.example.com\n");
        let settings = mail_settings_from_dotenv_bytes(&bytes);
        assert_eq!(
            settings.get("MAIL_IMAP_WORK_HOST").map(String::as_str),
            Some("imap.example.com")
        );
    }

    #[test]
    fn legacy_dotenv_keeps_the_first_value_even_when_it_is_empty() {
        let bytes = b"\
MAIL_MCP_TRANSPORT=stdio
MAIL_MCP_TRANSPORT=http
MAIL_IMAP_WORK_HOST=
MAIL_IMAP_WORK_HOST=imap.example.com
MAIL_ATTACHMENT_UPLOAD_DIR=\"   \"
MAIL_ATTACHMENT_UPLOAD_DIR=/uploads
";
        let settings = mail_settings_from_dotenv_bytes(bytes);
        assert_eq!(
            settings.get("MAIL_MCP_TRANSPORT").map(String::as_str),
            Some("stdio")
        );
        assert!(!settings.contains_key("MAIL_IMAP_WORK_HOST"));
        assert!(!settings.contains_key("MAIL_ATTACHMENT_UPLOAD_DIR"));
    }

    #[test]
    fn legacy_dotenv_stops_at_the_first_malformed_line() {
        let bytes = b"\
MAIL_IMAP_WORK_HOST=imap.example.com
<><><>
MAIL_MCP_TRANSPORT=http
";
        let settings = mail_settings_from_dotenv_bytes(bytes);
        assert_eq!(
            settings.get("MAIL_IMAP_WORK_HOST").map(String::as_str),
            Some("imap.example.com")
        );
        assert!(!settings.contains_key("MAIL_MCP_TRANSPORT"));
    }

    #[test]
    fn save_refuses_output_larger_than_the_load_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let head = "[settings]\nattachment_upload_dir = \"";
        let tail = "\"\n";
        let within = MAX_CONFIG_BYTES as usize - head.len() - tail.len() - 16;
        let original = format!("{head}{}{tail}", "a".repeat(within));
        assert_eq!(original.len(), MAX_CONFIG_BYTES as usize - 16);
        fs::write(&path, &original).unwrap();
        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        let settings = file.document["settings"].as_table_mut().unwrap();
        assign_value(
            settings,
            "attachment_upload_dir",
            Value::from("a".repeat(MAX_CONFIG_BYTES as usize)),
        );
        let error = file.save(false).unwrap_err();
        assert!(shown(&error).contains("too large"));
        assert!(!shown(&error).contains(&"a".repeat(64)));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!lock_path_for(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_config_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("config.toml");
        fs::write(&target, "[settings]\nwrite_enabled = false\n").unwrap();
        symlink(&target, &link).unwrap();
        let mut file = ConfigFile::load(Some(&link), false).unwrap();
        let settings = file.document["settings"].as_table_mut().unwrap();
        assign_value(settings, "write_enabled", Value::from(true));
        file.save(false).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), target);
        let saved = fs::read_to_string(&target).unwrap();
        assert!(saved.contains("true"), "{saved}");

        let dangling = dir.path().join("missing.toml");
        let absent = dir.path().join("absent.toml");
        symlink(&absent, &dangling).unwrap();
        let mut created = ConfigFile::load(Some(&dangling), true).unwrap();
        created.document["accounts"] = toml_edit::Item::Table(Table::new());
        assert!(created.save(true).is_err());
        assert!(!absent.exists());
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_symlinked_lock_without_changing_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[accounts]\n";
        fs::write(&path, original).unwrap();
        let unrelated = dir.path().join("unrelated");
        fs::write(&unrelated, b"leave-me").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o644)).unwrap();
        let lock_path = lock_path_for(&path);
        symlink(&unrelated, &lock_path).unwrap();
        let mut file = ConfigFile::load(Some(&path), false).unwrap();
        file.document["settings"]["write_enabled"] = toml_edit::value(false);
        assert!(file.save(false).is_err());
        assert_eq!(
            fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&unrelated).unwrap(), b"leave-me");
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(
            fs::symlink_metadata(&lock_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn empty_process_value_keeps_the_file_and_is_not_an_override() {
        let file = parsed("[settings]\nattachment_upload_dir = \"/uploads\"\n");
        for value in ["", " ", "\t", "\n"] {
            let resolved = file
                .resolve_from(
                    Settings::new(),
                    [(
                        OsString::from("MAIL_ATTACHMENT_UPLOAD_DIR"),
                        OsString::from(value),
                    )],
                )
                .unwrap();
            assert_eq!(
                resolved
                    .values
                    .get("MAIL_ATTACHMENT_UPLOAD_DIR")
                    .map(String::as_str),
                Some("/uploads"),
                "{value:?}"
            );
            assert!(
                !resolved
                    .overrides
                    .iter()
                    .any(|key| key == "MAIL_ATTACHMENT_UPLOAD_DIR"),
                "{value:?}"
            );
        }
    }
}
