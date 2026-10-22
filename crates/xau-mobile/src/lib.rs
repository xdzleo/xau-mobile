//! Everything the Android app can do. Three operations, nothing else:
//! sign in, read/claim Rewards, run the presence farm.
//!
//! Reached from Kotlin through `xau-jni`; see `ARCHITECTURE.md`.
//!
//! # Why this still depends on `xau-core`
//!
//! Signing in is not just an OAuth dance. After Microsoft returns an
//! access token, Xbox Live requires a **device token** and a **SISU**
//! authorisation, and both are rejected unless each request carries a
//! `Signature` header built from an undocumented byte layout: policy
//! version big-endian, NUL, Windows-epoch timestamp big-endian, NUL,
//! `"POST\0"`, path+query, NUL, token, NUL, body, NUL — ECDSA P-256 over
//! exactly that. One byte wrong and Xbox refuses without explaining.
//!
//! That code exists and works in `xau_core::oauth::signer`. Rewriting it
//! for the phone would put the one part of this app that fails silently
//! at risk, for no gain — so the phone uses it. The modules this never
//! calls (plan engine, bulk scanner, memory scanner) are simply not
//! reachable, and the linker drops them.

pub mod accounts;
pub mod offers_csv;
pub mod store;
pub mod windows_login;

use std::time::Duration;

use xau_core::error::{Error, Result};
use xau_core::oauth::{
    classify_redirect, AuthSession, CodeFlow, RedirectOutcome, XboxSignedClient,
};
use xau_core::xbox::client::offers::Offer;
use xau_core::xbox::client::rewards::{ClaimResult, RewardsInfo};
use xau_core::xbox::farm::{Farm, FarmConfig, FIXED_CONSOLE_TITLES};
// Re-exported: the JNI bridge holds the handle to stop the farm and to
// poll snapshots for the progress the Compose screen draws.
use xau_core::xbox::client::{XboxClient, XboxClientOptions};
pub use xau_core::xbox::farm::FarmHandle;

/// How long one farm run broadcasts presence: up to 15 minutes, then it
/// ends on its own (one cycle, see `start_farm`).
pub const FARM_CYCLE: Duration = Duration::from_secs(15 * 60);

/// A signed-in session: the Xbox client plus the xuid every call needs.
pub struct Session {
    client: XboxClient,
    xuid: String,
    gamertag: String,
    xauth: String,
    email: Option<String>,
}

impl Session {
    #[must_use]
    pub fn gamertag(&self) -> &str {
        &self.gamertag
    }

    /// The Xbox Live token behind this session.
    ///
    /// The multi-account store keeps one of these per xuid so a launch
    /// can rebuild every session without sending the user back through
    /// sign-in. Treat it as a credential: it is the whole account.
    #[must_use]
    pub fn xauth(&self) -> &str {
        &self.xauth
    }

    /// The address seen during sign-in, when there was one. Restored
    /// sessions have none: nobody typed anything.
    #[must_use]
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// Avatar URL and gamerscore.
    ///
    /// Fetched separately from sign-in because a slow or missing avatar
    /// must never hold up authentication — the account is usable the
    /// moment the token validates, picture or not.
    ///
    /// # Errors
    /// Propagates transport failures.
    pub async fn profile_display(&self) -> Result<(String, i64)> {
        self.client.get_display_bits(&self.xuid).await
    }

    #[must_use]
    pub fn xuid(&self) -> &str {
        &self.xuid
    }
}

// ---------------------------------------------------------------- login

/// The URL the Android WebView should load to sign in.
///
/// # Errors
/// Fails only if the HTTP client cannot be constructed.
pub fn login_url() -> Result<String> {
    Ok(CodeFlow::xbox_app_pc()?.authorize_url())
}

/// Classify a URL the WebView navigated to.
///
/// `Ok(None)` while the user is still on Microsoft's pages, `Ok(Some)`
/// with the authorization code once sign-in completes, `Err` when it was
/// refused or cancelled.
///
/// The host must call this on EVERY navigation. Only the exact redirect
/// endpoint is terminal — matching on a bare `?code=` would let any page
/// reached mid-flow hand over a code to exchange.
///
/// # Errors
/// Returns the failure reason when sign-in was refused or cancelled.
pub fn on_navigate(url: &str) -> Result<Option<String>> {
    sniff_email(url);
    match classify_redirect(url) {
        RedirectOutcome::Pending => Ok(None),
        RedirectOutcome::Code(c) => Ok(Some(c)),
        RedirectOutcome::Failed(reason) => Err(Error::other(reason)),
    }
}

/// The address the user typed on Microsoft's page, if it went past.
///
/// The Xbox scope this app authenticates with returns no email and no
/// id_token — deliberately, because widening it would change the token
/// the SISU handshake is signed against. But the sign-in itself happens
/// in a WebView whose every navigation is already classified here, and
/// Microsoft carries the username in the query string between steps.
/// So it is read from something already passing through, rather than by
/// asking for more access.
///
/// **Only the address.** The password is never in a URL and is never
/// looked for. This never leaves the device.
static SEEN_EMAIL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Microsoft has used several names for it across flow revisions;
/// accepting all of them costs nothing and avoids silently breaking
/// when the login page is next reworked.
const EMAIL_PARAMS: &[&str] = &["login_hint", "username", "login", "email", "loginfmt"];

fn sniff_email(url: &str) {
    let Some((_, query)) = url.split_once('?') else {
        return;
    };
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if !EMAIL_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)) {
            continue;
        }
        let decoded = urldecode_min(v);
        // A bare "@" test is enough: anything without one is not an
        // address, and anything with one is what the user typed.
        if decoded.contains('@') && decoded.len() <= 254 {
            *SEEN_EMAIL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(decoded);
            return;
        }
    }
}

/// Take whatever the sign-in saw, clearing it so the next account does
/// not inherit the previous one's address.
fn take_seen_email() -> Option<String> {
    SEEN_EMAIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn urldecode_min(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Trade the authorization code for a live session.
///
/// # Errors
/// Fails when the code is stale or reused, or when Xbox Live rejects the
/// device-token or SISU step.
pub async fn finish_login(code: &str) -> Result<Session> {
    let ms = CodeFlow::xbox_app_pc()?.exchange_code(code).await?;
    let signed = XboxSignedClient::new()?;
    let (xauth, _) = signed
        .acquire_xauth(
            &ms.access_token,
            xau_core::oauth::consts::XBOX_LIVE_RELYING_PARTY,
        )
        .await?;

    // Persist so the app does not re-prompt on every launch. Best effort:
    // a phone that refuses the write should still finish signing in.
    let mut sess = AuthSession::from_oauth(&ms);
    sess.xauth = Some(xauth.clone());
    if let Err(e) = sess.save() {
        tracing::warn!(%e, "could not persist session");
    }
    session_from_token(xauth).await
}

/// Rebuild a session from storage, for a launch where the user is
/// already signed in.
///
/// # Errors
/// Fails when nothing is stored, or the stored token is dead.
pub async fn restore_session() -> Result<Session> {
    let xauth = AuthSession::load()?
        .and_then(|s| s.xauth)
        .ok_or_else(|| Error::other("no stored session"))?;
    session_from_token(xauth).await
}

/// Rebuild a session from a token the multi-account store kept.
///
/// Every account the phone holds comes back through here on launch, so
/// one dead token surfaces as one row that needs re-authenticating
/// rather than as a failure that takes the whole list down.
///
/// # Errors
/// Fails when Xbox Live no longer accepts the token.
pub async fn session_from_token(xauth: String) -> Result<Session> {
    // Validate BEFORE building the client: a dead token then surfaces as
    // "sign in again" instead of as a confusing failure buried in the
    // rewards screen. `validate_token` also hands back the xuid, which
    // every rewards and presence call needs.
    let profile = xau_core::xbox::validate::validate_token(&xauth)
        .await?
        .ok_or_else(|| Error::other("token rejected by Xbox Live — sign in again"))?;
    let xuid = profile
        .xuid
        .clone()
        .ok_or_else(|| Error::other("validated token carries no xuid"))?;
    let client = XboxClient::new(XboxClientOptions {
        xauth: xauth.clone(),
        timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    })?;
    Ok(Session {
        gamertag: profile.gamertag.clone().unwrap_or_default(),
        client,
        xuid,
        xauth,
        email: take_seen_email(),
    })
}

/// Forget the stored session.
///
/// # Errors
/// Propagates filesystem failures.
pub fn sign_out() -> Result<()> {
    AuthSession::delete()
}

// -------------------------------------------------------------- rewards

/// Balance and everything currently claimable.
///
/// # Errors
/// Propagates transport and Xbox-side failures.
pub async fn rewards(session: &Session) -> Result<RewardsInfo> {
    session.client.get_rewards_info(&session.xuid).await
}

/// Claim every outstanding activity.
///
/// # Errors
/// Propagates transport and Xbox-side failures.
pub async fn claim(session: &Session) -> Result<ClaimResult> {
    session.client.claim_rewards_points(&session.xuid).await
}

// --------------------------------------------------------------- offers

/// Every Game Pass member offer on this account, claimed or not.
///
/// # Errors
/// Propagates transport and Xbox-side failures.
pub async fn offers(session: &Session) -> Result<Vec<Offer>> {
    session.client.get_offers().await
}

/// Human names for offers, best effort.
///
/// # Errors
/// Only if the HTTP client cannot be used; a content feed that fails
/// yields an empty map rather than an error.
pub async fn offer_names(
    session: &Session,
) -> Result<std::collections::HashMap<String, xau_core::xbox::client::offer_names::OfferName>> {
    session.client.get_offer_names("BR", "pt-BR").await
}

/// Claim one offer and return it with its minted resource.
///
/// **Irreversible**: this consumes the offer for that account. The
/// caller must be acting on an explicit user instruction, never on a
/// timer or a sweep the user did not ask for.
///
/// # Errors
/// Propagates transport and Xbox-side failures.
pub async fn claim_offer(session: &Session, offer_id: &str) -> Result<Offer> {
    session.client.claim_offer(offer_id).await
}

// ----------------------------------------------------------------- farm

/// Titles the farm broadcasts, as `(id, name)`.
#[must_use]
pub fn farm_titles() -> Vec<(&'static str, &'static str)> {
    FIXED_CONSOLE_TITLES
        .iter()
        .map(|(id, name, _)| (*id, *name))
        .collect()
}

/// Box art for the farm's titles, `titleId -> image URL`.
///
/// Decoration: any failure is an empty map, never an error.
pub async fn farm_title_art(session: &Session) -> std::collections::HashMap<String, String> {
    let ids: Vec<String> = FIXED_CONSOLE_TITLES
        .iter()
        .map(|(id, _, _)| (*id).to_string())
        .collect();
    session
        .client
        .get_title_images(&ids)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(%e, "farm title art unavailable");
            std::collections::HashMap::new()
        })
}

/// Broadcast presence across the 8 fixed console titles for up to
/// [`FARM_CYCLE`] (15 minutes), then stop on its own. Each ping's outcome
/// (accepted / denied / rejected / not sent) is in the snapshot's
/// `presence`, so the screen can say when Xbox refuses it.
///
/// # Errors
/// Fails if the title list does not satisfy the presence cap.
pub fn start_farm(session: &Session) -> Result<FarmHandle> {
    let cfg = FarmConfig {
        title_ids: FIXED_CONSOLE_TITLES
            .iter()
            .map(|(id, _, _)| (*id).to_string())
            .collect(),
        cycle_duration: FARM_CYCLE,
        max_cycles: Some(1),
        ..Default::default()
    };
    cfg.validate()?;
    // `start` also spawns the loop task; the JoinHandle is dropped
    // deliberately — the farm is stopped through the handle, and holding
    // the join would only matter if we awaited it, which no caller does.
    let (handle, _task) = Farm::start(session.client.clone(), session.xuid.clone(), cfg)?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The farm preset must survive re-export; shipping an empty list
    /// would look fine and broadcast nothing.
    #[test]
    fn the_farm_preset_is_intact() {
        let t = farm_titles();
        assert_eq!(t.len(), xau_core::xbox::farm::FARM_MAX_TITLES);
        assert!(t.iter().all(|(id, n)| !id.is_empty() && !n.is_empty()));
    }

    /// The login hinges on treating only the real redirect as terminal,
    /// so pin it here too rather than trusting the core not to drift.
    #[test]
    fn only_the_real_redirect_ends_the_login() {
        assert!(on_navigate("https://login.live.com/ppsecure/post.srf")
            .expect("pending")
            .is_none());
        assert!(
            on_navigate("https://evil.example/oauth20_desktop.srf?code=x")
                .expect("pending")
                .is_none()
        );
        assert_eq!(
            on_navigate("https://login.live.com/oauth20_desktop.srf?code=abc123")
                .expect("ok")
                .as_deref(),
            Some("abc123")
        );
        assert!(
            on_navigate("https://login.live.com/oauth20_desktop.srf?error=access_denied").is_err()
        );
    }

    /// The mission is "play 8 titles for 15 minutes"; the farm runs one
    /// 15-minute cycle (see `start_farm`'s `max_cycles`) and ends.
    #[test]
    fn the_farm_runs_for_fifteen_minutes() {
        assert_eq!(FARM_CYCLE.as_secs(), 15 * 60);
    }
}
