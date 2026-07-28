//! Microsoft OAuth **authorization-code** flow, for signing in inside an
//! embedded browser instead of the device-code hand-off.
//!
//! The device-code flow ([`super::device_code`]) is the headless path:
//! we print a short `user_code`, the user opens a browser somewhere and
//! types it. It works everywhere, but it makes the user leave the app
//! and copy a code by hand.
//!
//! This module is the other half — the flow a WebView can drive end to
//! end:
//!
//! 1. [`CodeFlow::authorize_url`] builds the `login.live.com` consent
//!    URL. The host navigates a WebView there.
//! 2. The user signs in. Microsoft redirects to
//!    [`consts::OAUTH_DESKTOP_REDIRECT`] with `?code=…` (or `?error=…`).
//! 3. The host detects that navigation and calls
//!    [`CodeFlow::extract_code`] on the URL.
//! 4. [`CodeFlow::exchange_code`] trades the code for the same
//!    [`MicrosoftOAuthToken`] the device-code flow returns — so
//!    everything downstream (SISU, `AuthSession`, refresh) is shared and
//!    neither path can drift from the other.
//!
//! Same `client_id` and `scope` as the device-code flow: to Xbox Live
//! the two are indistinguishable once step 4 completes.
//!
//! Ports the shape of upstream's `CodeFlowLiveApiClient` (Fumo-Unlockers
//! XAU), whose WebView2 login this enables us to match.

use std::time::Duration;

use reqwest::Client;

use crate::error::{Error, Result};
use crate::oauth::consts;
use crate::oauth::device_code::MicrosoftOAuthToken;

/// Authorization-code flow against `login.live.com`.
pub struct CodeFlow {
    http: Client,
    client_id: String,
    scope: String,
}

impl CodeFlow {
    /// Build a flow for an arbitrary client/scope pair.
    ///
    /// # Errors
    /// Fails when the HTTP client cannot be constructed.
    pub fn new(client_id: impl Into<String>, scope: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(Error::from)?;
        Ok(Self {
            http,
            client_id: client_id.into(),
            scope: scope.into(),
        })
    }

    /// The Xbox PC app client — identical identity to
    /// [`super::device_code::DeviceCodeFlow::xbox_app_pc`].
    ///
    /// # Errors
    /// Fails when the HTTP client cannot be constructed.
    pub fn xbox_app_pc() -> Result<Self> {
        Self::new(consts::XBOX_APP_PC_CLIENT_ID, consts::XBOX_SCOPE)
    }

    /// The URL to point a WebView at.
    ///
    /// `prompt=select_account` is deliberate: without it Microsoft
    /// silently reuses whatever account the embedded browser already has
    /// a cookie for, which makes "log in as someone else" impossible
    /// without clearing the WebView's storage. Replicating a target's
    /// achievements means signing in as a *specific* account, so the
    /// picker must always appear.
    #[must_use]
    pub fn authorize_url(&self) -> String {
        let q = [
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", consts::OAUTH_DESKTOP_REDIRECT),
            ("scope", self.scope.as_str()),
            ("response_mode", "query"),
            ("prompt", "select_account"),
        ];
        let query = q
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{}", consts::OAUTH_AUTHORIZE_URL, query)
    }

    /// Trade an authorization code for tokens.
    ///
    /// # Errors
    /// Returns [`Error::HttpStatus`] when Microsoft rejects the code
    /// (already used, expired, or issued for a different `redirect_uri`).
    pub async fn exchange_code(&self, code: &str) -> Result<MicrosoftOAuthToken> {
        if code.trim().is_empty() {
            return Err(Error::invalid_argument("empty authorization code"));
        }
        let params = [
            ("client_id", self.client_id.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", consts::OAUTH_DESKTOP_REDIRECT),
            ("scope", self.scope.as_str()),
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

/// Outcome of inspecting a URL the embedded browser navigated to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectOutcome {
    /// Not the redirect target — keep browsing (the user is still on a
    /// Microsoft sign-in / MFA / consent page).
    Pending,
    /// Sign-in completed; here is the authorization code.
    Code(String),
    /// Microsoft reported a failure, or the user cancelled.
    Failed(String),
}

/// Classify a URL the WebView navigated to.
///
/// Only URLs on the exact redirect endpoint are considered terminal —
/// matching on a bare `?code=` substring would fire on unrelated
/// Microsoft pages that happen to carry such a parameter, and would
/// let a page reached mid-flow hand us a code we then exchange.
#[must_use]
pub fn classify_redirect(url: &str) -> RedirectOutcome {
    let trimmed = url.trim();
    // Split off the query before comparing, so extra params can't
    // smuggle a different path in.
    let (path, query) = match trimmed.split_once('?') {
        Some((p, q)) => (p, q),
        None => (trimmed, ""),
    };
    if !path.eq_ignore_ascii_case(consts::OAUTH_DESKTOP_REDIRECT) {
        return RedirectOutcome::Pending;
    }
    let mut code: Option<String> = None;
    let mut error: Option<String> = None;
    let mut error_desc: Option<String> = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "code" => code = Some(urldecode(v)),
            "error" => error = Some(urldecode(v)),
            "error_description" => error_desc = Some(urldecode(v)),
            _ => {}
        }
    }
    if let Some(e) = error {
        let detail = error_desc.unwrap_or_default();
        return RedirectOutcome::Failed(if detail.is_empty() {
            e
        } else {
            format!("{e}: {detail}")
        });
    }
    match code {
        Some(c) if !c.is_empty() => RedirectOutcome::Code(c),
        // Landed on the redirect with neither code nor error — treat as
        // a cancel rather than hanging the login window forever.
        _ => RedirectOutcome::Failed("sign-in was cancelled".into()),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_every_required_parameter() {
        let url = CodeFlow::xbox_app_pc().expect("flow").authorize_url();
        assert!(url.starts_with(consts::OAUTH_AUTHORIZE_URL));
        assert!(url.contains(&format!("client_id={}", consts::XBOX_APP_PC_CLIENT_ID)));
        assert!(url.contains("response_type=code"));
        // The scope contains ':' which MUST be percent-encoded or
        // login.live.com rejects the request.
        assert!(
            url.contains("scope=service%3A%3Auser.auth.xboxlive.com%3A%3AMBI_SSL"),
            "scope not percent-encoded: {url}"
        );
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Flogin.live.com%2Foauth20_desktop.srf"),
            "redirect_uri not percent-encoded: {url}"
        );
        assert!(
            url.contains("prompt=select_account"),
            "without the account picker a second account can never sign in"
        );
    }

    #[test]
    fn a_normal_sign_in_page_is_not_treated_as_the_redirect() {
        assert_eq!(
            classify_redirect("https://login.live.com/ppsecure/post.srf?wa=wsignin1.0"),
            RedirectOutcome::Pending
        );
        assert_eq!(
            classify_redirect("https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize"),
            RedirectOutcome::Pending
        );
    }

    #[test]
    fn the_redirect_yields_the_code() {
        let got = classify_redirect(
            "https://login.live.com/oauth20_desktop.srf?code=M.C123_BAY.2.U.abc-def&lc=1033",
        );
        assert_eq!(got, RedirectOutcome::Code("M.C123_BAY.2.U.abc-def".into()));
    }

    /// A page that merely carries `?code=` must NOT end the flow — the
    /// terminal condition is the redirect endpoint itself.
    #[test]
    fn a_foreign_page_carrying_a_code_parameter_is_ignored() {
        assert_eq!(
            classify_redirect("https://evil.example/oauth20_desktop.srf?code=attacker"),
            RedirectOutcome::Pending
        );
        assert_eq!(
            classify_redirect("https://login.live.com/err.srf?code=12345"),
            RedirectOutcome::Pending
        );
    }

    #[test]
    fn an_error_redirect_surfaces_the_reason() {
        let got = classify_redirect(
            "https://login.live.com/oauth20_desktop.srf?error=access_denied&error_description=User+cancelled",
        );
        assert_eq!(
            got,
            RedirectOutcome::Failed("access_denied: User cancelled".into())
        );
    }

    #[test]
    fn landing_on_the_redirect_with_nothing_is_a_cancel_not_a_hang() {
        match classify_redirect("https://login.live.com/oauth20_desktop.srf") {
            RedirectOutcome::Failed(msg) => assert!(msg.contains("cancel")),
            other => panic!("expected a cancel, got {other:?}"),
        }
    }

    #[test]
    fn percent_escapes_decode_in_the_code() {
        let got = classify_redirect("https://login.live.com/oauth20_desktop.srf?code=a%2Bb%2Fc%3D");
        assert_eq!(got, RedirectOutcome::Code("a+b/c=".into()));
    }
}
