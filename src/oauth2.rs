//! OAuth2 token management for IMAP and SMTP XOAUTH2 authentication
//!
//! Provides a [`TokenManager`] that caches access tokens per account and
//! refreshes them before expiry. Also provides the XOAUTH2 SASL helper and
//! an [`async_imap::Authenticator`] implementation for IMAP XOAUTH2 login.
//!
//! # Supported Providers
//!
//! - **Google**: `https://oauth2.googleapis.com/token`
//! - **Microsoft**: `https://login.microsoftonline.com/common/oauth2/v2.0/token`
//!
//! # Configuration
//!
//! Per-account OAuth2 settings are loaded from environment variables:
//! ```text
//! MAIL_OAUTH2_<SEGMENT>_PROVIDER=google|microsoft
//! MAIL_OAUTH2_<SEGMENT>_CLIENT_ID=...
//! MAIL_OAUTH2_<SEGMENT>_CLIENT_SECRET=...
//! MAIL_OAUTH2_<SEGMENT>_REFRESH_TOKEN=...
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Mutex;

use crate::errors::{AppError, AppResult};

/// Margin before token expiry at which we proactively refresh (seconds).
///
/// Matches the proven pattern from `email-oauth2-proxy` (600s = 10 minutes).
const TOKEN_REFRESH_MARGIN_SECS: u64 = 600;

/// Default token lifetime when the provider omits `expires_in` (seconds).
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;

// ─── Provider ────────────────────────────────────────────────────────────────

/// Supported OAuth2 providers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2Provider {
    Google,
    Microsoft,
}

impl OAuth2Provider {
    /// Parse provider name from configuration value
    pub fn parse(value: &str) -> AppResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "google" | "gmail" => Ok(Self::Google),
            "microsoft" | "outlook" | "office365" => Ok(Self::Microsoft),
            other => Err(AppError::InvalidInput(format!(
                "unsupported OAuth2 provider '{other}'; expected 'google' or 'microsoft'"
            ))),
        }
    }

    /// Token endpoint URL for this provider
    pub fn token_url(&self) -> &'static str {
        match self {
            Self::Google => "https://oauth2.googleapis.com/token",
            Self::Microsoft => {
                "https://login.microsoftonline.com/common/oauth2/v2.0/token"
            }
        }
    }
}

// ─── Per-account OAuth2 config ───────────────────────────────────────────────

/// OAuth2 configuration for a single account
#[derive(Debug, Clone)]
pub struct OAuth2AccountConfig {
    pub provider: OAuth2Provider,
    pub client_id: String,
    pub client_secret: SecretString,
    pub refresh_token: SecretString,
}

// ─── Cached token ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS) < self.expires_at
    }
}

// ─── Token manager ───────────────────────────────────────────────────────────

/// Manages OAuth2 access tokens with caching and automatic refresh.
///
/// Thread-safe via `Arc<Mutex<...>>` on the cache. Designed to be shared
/// across all MCP tool handlers.
#[derive(Debug, Clone)]
pub struct TokenManager {
    /// OAuth2 configs keyed by account_id
    configs: Arc<HashMap<String, OAuth2AccountConfig>>,
    /// Cached tokens keyed by account_id
    cache: Arc<Mutex<HashMap<String, CachedToken>>>,
    /// HTTP client for token requests (reused across refreshes)
    http: reqwest::Client,
}

impl TokenManager {
    /// Create a new token manager from OAuth2 account configs.
    pub fn new(configs: HashMap<String, OAuth2AccountConfig>) -> Self {
        Self {
            configs: Arc::new(configs),
            cache: Arc::new(Mutex::new(HashMap::new())),
            http: reqwest::Client::new(),
        }
    }

    /// Check whether an account has OAuth2 configured.
    pub fn has_oauth2(&self, account_id: &str) -> bool {
        self.configs.contains_key(account_id)
    }

    /// Get a valid access token for the given account.
    ///
    /// Returns a cached token if it is still valid (with refresh margin),
    /// otherwise performs a refresh_token grant. Implements a two-strike
    /// retry: if the first refresh fails, waits 1 second and retries once.
    pub async fn get_access_token(&self, account_id: &str) -> AppResult<String> {
        // Check cache first
        {
            let cache = self.cache.lock().await;
            if let Some(cached) = cache.get(account_id) {
                if cached.is_valid() {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        let oauth2_config = self.configs.get(account_id).ok_or_else(|| {
            AppError::InvalidInput(format!(
                "no OAuth2 configuration for account '{account_id}'"
            ))
        })?;

        // Two-strike refresh
        match self.refresh_token(account_id, oauth2_config).await {
            Ok(token) => Ok(token),
            Err(first_err) => {
                tracing::warn!(
                    account_id,
                    "OAuth2 token refresh failed (attempt 1), retrying: {first_err}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.refresh_token(account_id, oauth2_config)
                    .await
                    .map_err(|e| {
                        AppError::AuthFailed(format!(
                            "OAuth2 token refresh failed for account '{account_id}' after 2 attempts: {e}"
                        ))
                    })
            }
        }
    }

    /// Perform a refresh_token grant and cache the result.
    async fn refresh_token(
        &self,
        account_id: &str,
        config: &OAuth2AccountConfig,
    ) -> AppResult<String> {
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", &config.client_id),
            ("client_secret", config.client_secret.expose_secret()),
            ("refresh_token", config.refresh_token.expose_secret()),
        ];

        let response = self
            .http
            .post(config.provider.token_url())
            .form(&params)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                AppError::Internal(format!("OAuth2 token request failed: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::AuthFailed(format!(
                "OAuth2 token endpoint returned {status}: {body}"
            )));
        }

        let token_response: TokenResponse = response.json().await.map_err(|e| {
            AppError::Internal(format!("failed to parse OAuth2 token response: {e}"))
        })?;

        let expires_in = token_response
            .expires_in
            .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
        let cached = CachedToken {
            access_token: token_response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        };

        let mut cache = self.cache.lock().await;
        cache.insert(account_id.to_owned(), cached);

        tracing::info!(
            account_id,
            expires_in_secs = expires_in,
            "OAuth2 access token refreshed"
        );

        Ok(token_response.access_token)
    }
}

/// OAuth2 token endpoint response
#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    // token_type and scope are present but unused
}

// ─── XOAUTH2 SASL ───────────────────────────────────────────────────────────

/// Build the XOAUTH2 SASL response string.
///
/// Format per [Google XOAUTH2](https://developers.google.com/gmail/imap/xoauth2-protocol):
/// ```text
/// user=<email>\x01auth=Bearer <token>\x01\x01
/// ```
pub fn xoauth2_sasl(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

// ─── IMAP Authenticator ─────────────────────────────────────────────────────

/// XOAUTH2 authenticator for `async_imap::Client::authenticate`.
///
/// Holds the pre-computed SASL response and returns it on the first
/// challenge from the server.
pub struct XOAuth2Authenticator {
    response: Vec<u8>,
}

impl XOAuth2Authenticator {
    /// Create a new authenticator with the given XOAUTH2 SASL string.
    pub fn new(user: &str, access_token: &str) -> Self {
        Self {
            response: xoauth2_sasl(user, access_token).into_bytes(),
        }
    }
}

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // XOAUTH2 sends the full SASL response on the first (empty) challenge
        self.response.clone()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_sasl_format() {
        let sasl = xoauth2_sasl("user@gmail.com", "ya29.token123");
        assert_eq!(
            sasl,
            "user=user@gmail.com\x01auth=Bearer ya29.token123\x01\x01"
        );
    }

    #[test]
    fn xoauth2_authenticator_returns_sasl_bytes() {
        use async_imap::Authenticator;
        let mut auth = XOAuth2Authenticator::new("user@gmail.com", "token");
        let response = auth.process(b"");
        assert_eq!(
            response,
            b"user=user@gmail.com\x01auth=Bearer token\x01\x01"
        );
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(OAuth2Provider::parse("google").unwrap(), OAuth2Provider::Google);
        assert_eq!(OAuth2Provider::parse("gmail").unwrap(), OAuth2Provider::Google);
        assert_eq!(OAuth2Provider::parse("GOOGLE").unwrap(), OAuth2Provider::Google);
        assert_eq!(OAuth2Provider::parse("microsoft").unwrap(), OAuth2Provider::Microsoft);
        assert_eq!(OAuth2Provider::parse("outlook").unwrap(), OAuth2Provider::Microsoft);
        assert_eq!(OAuth2Provider::parse("office365").unwrap(), OAuth2Provider::Microsoft);
    }

    #[test]
    fn provider_parse_rejects_unknown() {
        assert!(OAuth2Provider::parse("yahoo").is_err());
        assert!(OAuth2Provider::parse("").is_err());
    }

    #[test]
    fn provider_token_urls() {
        assert_eq!(
            OAuth2Provider::Google.token_url(),
            "https://oauth2.googleapis.com/token"
        );
        assert!(OAuth2Provider::Microsoft.token_url().contains("login.microsoftonline.com"));
    }

    #[test]
    fn cached_token_validity() {
        let valid = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS + 100),
        };
        assert!(valid.is_valid());

        let expired = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS - 1),
        };
        assert!(!expired.is_valid());
    }
}
