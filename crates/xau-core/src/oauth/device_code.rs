//! Microsoft OAuth device-code flow against `login.live.com`.
//!
//! The authenticator shows a URL + short code to the user, who approves sign-in
//! in a browser. Meanwhile we poll `oauth20_token.srf` until Microsoft returns
//! an access token (or we time out).
//!
//! This replaces the `WebView2` + code-flow path from the original C# app,
//! which required a GUI.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::oauth::consts;

/// Microsoft's immediate response to the device-code request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub message: Option<String>,
}

fn default_interval() -> u64 {
    5
}

/// Access-token response (and refreshed equivalent) returned by
/// `oauth20_token.srf`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MicrosoftOAuthToken {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub user_id: Option<String>,
}

// Manual Debug — redact access_token + refresh_token. Without this a
// stray `tracing::debug!(?token, ...)` (or `?err` on a chain that walks
// into this struct) prints both bearer credentials into logs.
impl std::fmt::Debug for MicrosoftOAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrosoftOAuthToken")
            .field(
                "access_token",
                if self.access_token.is_empty() {
                    &"<empty>"
                } else {
                    &"<redacted>"
                },
            )
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field(
                "refresh_token",
                if self.refresh_token.is_empty() {
                    &"<empty>"
                } else {
                    &"<redacted>"
                },
            )
            .field("user_id", &self.user_id)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct DeviceCodeFlow {
    http: Client,
    client_id: String,
    scope: String,
}

impl DeviceCodeFlow {
    pub fn new(client_id: impl Into<String>, scope: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            client_id: client_id.into(),
            scope: scope.into(),
        })
    }

    pub fn xbox_app_pc() -> Result<Self> {
        Self::new(consts::XBOX_APP_PC_CLIENT_ID, consts::XBOX_SCOPE)
    }

    /// Step 1: request a device code from Microsoft. Display
    /// `response.user_code` and `response.verification_uri` to the user.
    pub async fn request_device_code(&self) -> Result<DeviceCodeResponse> {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
            ("response_type", "device_code"),
        ];
        let resp = self
            .http
            .post(consts::DEVICE_CODE_URL)
            .form(&params)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::HttpStatus { status, body: text });
        }
        let parsed: DeviceCodeResponse = serde_json::from_str(&text)?;
        // BUG HUNTER M-deviceflow-uri: previously the response was
        // returned unchecked; the GUI then injected
        // `response.verification_uri` directly into an anchor's
        // `href`. A compromised endpoint or MITM yielding
        // `javascript:alert(...)` would execute in the webview.
        // Validate that the URI is https and points at a known
        // Microsoft sign-in host. Reject anything else with a clear
        // error rather than letting it propagate to the UI.
        if !is_trusted_microsoft_login_uri(&parsed.verification_uri) {
            return Err(Error::other(format!(
                "untrusted device-code verification_uri: `{}` (expected https://*.microsoft.com or https://login.live.com)",
                parsed.verification_uri
            )));
        }
        Ok(parsed)
    }

    /// Step 2: poll the token endpoint every `response.interval` seconds
    /// until the user approves, the code expires, or an unexpected error
    /// surfaces. Respects the backoff Microsoft sends with `slow_down`.
    pub async fn poll_token(&self, response: &DeviceCodeResponse) -> Result<MicrosoftOAuthToken> {
        let mut interval = response.interval.max(1);
        let deadline = std::time::Instant::now()
            + Duration::from_secs(response.expires_in.max(response.interval * 4));

        loop {
            if std::time::Instant::now() >= deadline {
                return Err(Error::other("device code expired before user approval"));
            }

            tokio::time::sleep(Duration::from_secs(interval)).await;

            let params = [
                ("client_id", self.client_id.as_str()),
                ("device_code", response.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ];
            // BUG HUNTER devicecode-transient-retry: a transient transport
            // error (DNS blip, connection reset, client timeout) used to
            // propagate via `?` and ABORT the whole device-code flow — even
            // though the user may have the verification page open and the
            // code is still valid for minutes. The deadline check at the
            // top of the loop bounds total wait, so on a transport error
            // just log and retry on the next tick instead of killing login.
            let resp = match self.http.post(consts::TOKEN_URL).form(&params).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        "device-code poll: transient network error, retrying"
                    );
                    continue;
                }
            };
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            if status.is_success() {
                return Ok(serde_json::from_str(&body)?);
            }

            // Error body looks like {"error":"authorization_pending", ...}
            let err: TokenError = serde_json::from_str(&body).unwrap_or_default();
            match err.error.as_str() {
                "authorization_pending" => {
                    tracing::debug!("authorization pending; will retry in {interval}s");
                }
                "slow_down" => {
                    interval = interval.saturating_add(5);
                    tracing::debug!(interval, "slow_down received; backing off");
                }
                "expired_token" | "code_expired" => {
                    return Err(Error::other("device code expired"));
                }
                "access_denied" => {
                    return Err(Error::other("user declined the sign-in request"));
                }
                _ => {
                    return Err(Error::HttpStatus { status, body });
                }
            }
        }
    }

    /// Convenience: run the full device-code flow end to end, calling
    /// `notify` once with the displayable prompt info before polling.
    pub async fn authenticate_interactively<F: FnOnce(&DeviceCodeResponse)>(
        &self,
        notify: F,
    ) -> Result<MicrosoftOAuthToken> {
        let code = self.request_device_code().await?;
        notify(&code);
        self.poll_token(&code).await
    }

    /// Exchange a `refresh_token` for a fresh `access_token`.
    pub async fn refresh(&self, refresh_token: &str) -> Result<MicrosoftOAuthToken> {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        let resp = self
            .http
            .post(consts::TOKEN_URL)
            .form(&params)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::HttpStatus { status, body });
        }
        Ok(serde_json::from_str(&body)?)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: String,
}

/// True when `uri` is `https://` and points at a known Microsoft
/// sign-in host. Used to gate `verification_uri` so a compromised
/// endpoint can't inject `javascript:` or attacker-controlled
/// hosts into the GUI's anchor href.
///
/// Trusted hosts (suffix match — covers
/// `microsoft.com`, `login.microsoft.com`, `login.live.com`,
/// `microsoft.com/link` etc.):
fn is_trusted_microsoft_login_uri(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    // Must be https — reject http://, javascript:, data:, file:.
    if !lower.starts_with("https://") {
        return false;
    }
    // Strip scheme and split into host + rest.
    let after_scheme = &lower["https://".len()..];
    let host = after_scheme
        .split(|c: char| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or("");
    // Disallow user-info segment (`user@host`) — Microsoft never
    // returns one, and accepting it lets a phisher craft
    // `https://microsoft.com@evil.example/`.
    if host.contains('@') {
        return false;
    }
    const TRUSTED_SUFFIXES: &[&str] = &["microsoft.com", "login.live.com", "live.com"];
    TRUSTED_SUFFIXES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

#[cfg(test)]
mod uri_tests {
    use super::is_trusted_microsoft_login_uri;
    #[test]
    fn accepts_real_microsoft_uris() {
        assert!(is_trusted_microsoft_login_uri("https://microsoft.com/link"));
        assert!(is_trusted_microsoft_login_uri(
            "https://login.microsoft.com/oauth2/v2.0/devicecode"
        ));
        assert!(is_trusted_microsoft_login_uri("https://login.live.com/"));
    }
    #[test]
    fn rejects_javascript_and_http() {
        assert!(!is_trusted_microsoft_login_uri("javascript:alert(1)"));
        assert!(!is_trusted_microsoft_login_uri("http://microsoft.com"));
        assert!(!is_trusted_microsoft_login_uri("about:blank"));
    }
    #[test]
    fn rejects_lookalikes_and_userinfo() {
        assert!(!is_trusted_microsoft_login_uri(
            "https://microsoft.com.evil.com/"
        ));
        assert!(!is_trusted_microsoft_login_uri(
            "https://microsoft.com@evil.com/"
        ));
        assert!(!is_trusted_microsoft_login_uri(
            "https://evilmicrosoft.com/"
        ));
    }
}
