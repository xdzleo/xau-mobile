//! Sign in as a Windows PC does, so Xbox accepts the presence farm.
//!
//! Xbox only takes the multi-title `presence-heartbeat` from a token
//! whose device is a registered Windows device. The OAuth login
//! (`login_url` / `finish_login`) gets an anonymous device and every
//! heartbeat comes back 403, so the farm earned nothing on a phone.
//!
//! This is the sign-in the xodus / xodus-nexus Linux launcher uses,
//! through the `msa-device` crate vendored from it:
//!
//! 1. once per install, an MSA device account (`deviceaddcredential.srf`);
//! 2. Windows' own "inline" sign-in page in the WebView, which hands its
//!    host a DA token (`ServerData`), traded over RST2 for the user's
//!    long-lived STS token, bound to that device;
//! 3. whenever an xauth is needed, silent RST2 tickets for the Xbox app
//!    (device + user), a device token with `AuthMethod: RPS` and SISU.
//!
//! Live check (2026-10-22): that xauth's heartbeat was answered 200 and
//! the read-back showed all 8 farm titles on a `WindowsOneCore` device,
//! with no Xbox app running anywhere.
//!
//! Because the STS token mints fresh xauths on demand, these accounts
//! also survive the 16-hour xauth lifetime without signing in again.

use std::sync::Mutex;
use std::time::Duration;

use msa_device::{Device, LoginStep, PageMessage, StsToken, XBOX_APP_CLIENT_ID};
pub use msa_device::UserSession;
use xau_core::error::{Error, Result};
use xau_core::oauth::XboxSignedClient;

use crate::accounts::Store;
use crate::Session;

/// The JavaScript object the login page reports to. The host registers
/// it on the WebView under this name.
pub const BRIDGE: &str = "XauLogin";

/// The page lands here when the sign-in is done; the host then runs
/// `finish_script` to hand over `ServerData`.
pub const FINISH_URL_PREFIX: &str = "https://login.live.com/ppsecure/post.srf";

/// Everything the WebView needs to open the sign-in.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoginPage {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub user_agent: String,
    /// Run at the start of every document.
    pub init_script: String,
    /// Run once a page under `finish_url_prefix` has loaded.
    pub finish_script: String,
    pub finish_url_prefix: String,
    pub bridge: String,
}

/// What the host does with one message from the page.
pub enum PageReply {
    /// Nothing to do.
    Ignore,
    /// Run this script in the page.
    Eval(String),
    /// Load this URL in the same WebView (Microsoft wants one more page).
    Open(String),
    /// Signed in.
    Done(Box<Session>, UserSession),
}

/// The sign-in in progress. One at a time: the WebView is modal.
struct Pending {
    device_sts: StsToken,
    finish: bool,
    /// A finished sign-in is traded once; a second `ServerData` (a page
    /// reload) must not start a second exchange of a spent token.
    busy: bool,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn err(e: msa_device::Error) -> Error {
    Error::other(e.to_string())
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// This install's device account, registering it the first time.
///
/// # Errors
/// Fails when Microsoft refuses the registration, or when it cannot be
/// stored: a device that is not kept would orphan every sign-in made
/// through it.
async fn ensure_device(http: &reqwest::Client) -> Result<Device> {
    let mut store = Store::load();
    if let Some(d) = store.device.clone() {
        return Ok(d);
    }
    let device = msa_device::provision_device(http).await.map_err(err)?;
    store.device = Some(device.clone());
    store.save()?;
    Ok(device)
}

/// Prepare the sign-in page.
///
/// # Errors
/// Fails when the device cannot be registered or signed in.
pub async fn start() -> Result<LoginPage> {
    let http = http()?;
    let device = ensure_device(&http).await?;
    let device_sts = msa_device::authenticate_device(&http, &device).await.map_err(err)?;
    *pending() = Some(Pending { device_sts, finish: false, busy: false });

    let correlation = msa_device::new_correlation_id();
    Ok(LoginPage {
        url: msa_device::inline_login_url(XBOX_APP_CLIENT_ID),
        headers: msa_device::inline_login_headers(&correlation)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        user_agent: msa_device::INLINE_LOGIN_USER_AGENT.to_string(),
        init_script: msa_device::inline_login_init_script(BRIDGE),
        finish_script: msa_device::inline_login_finish_script(BRIDGE),
        finish_url_prefix: FINISH_URL_PREFIX.to_string(),
        bridge: BRIDGE.to_string(),
    })
}

/// Handle one message the page posted to the bridge.
///
/// # Errors
/// Fails when no sign-in is in progress, or when Microsoft or Xbox Live
/// refuse the exchange. The sign-in is over after an error.
pub async fn on_message(body: &str) -> Result<PageReply> {
    let da = match msa_device::parse_page_message(body) {
        PageMessage::ContextRequest(ctx) => {
            return Ok(PageReply::Eval(msa_device::host_context_reply_script(&ctx)))
        }
        PageMessage::Other => return Ok(PageReply::Ignore),
        PageMessage::Token(da) => da,
    };

    let (device_sts, finish) = {
        let mut guard = pending();
        let p = guard
            .as_mut()
            .ok_or_else(|| Error::other("nenhum login em andamento"))?;
        if p.busy {
            return Ok(PageReply::Ignore);
        }
        p.busy = true;
        (p.device_sts.clone(), p.finish)
    };

    let http = http()?;
    let step = match msa_device::exchange_login(&http, &device_sts, da, finish).await {
        Ok(s) => s,
        Err(e) => {
            *pending() = None;
            return Err(err(e));
        }
    };
    match step {
        LoginStep::Finalize(url) => {
            if let Some(p) = pending().as_mut() {
                p.finish = true;
                p.busy = false;
            }
            Ok(PageReply::Open(url))
        }
        LoginStep::Done(mut user) => {
            *pending() = None;
            let device = Store::load()
                .device
                .ok_or_else(|| Error::other("dispositivo não registrado"))?;
            let xauth = issue_xauth(&device, &mut user).await?;
            let session = crate::session_from_token(xauth).await?;
            Ok(PageReply::Done(Box::new(session), user))
        }
    }
}

/// Mint a fresh xauth from a stored Windows sign-in.
///
/// `user.sts` comes back renewed most of the time; the caller stores
/// `user` again afterwards.
///
/// # Errors
/// Fails when the device or the stored sign-in is no longer accepted
/// (the user then signs in again), or on transport failures.
pub async fn issue_xauth(device: &Device, user: &mut UserSession) -> Result<String> {
    let http = http()?;
    let device_sts = msa_device::authenticate_device(&http, device).await.map_err(err)?;
    let tickets = msa_device::xbox_tickets(&http, &device_sts, user, XBOX_APP_CLIENT_ID)
        .await
        .map_err(err)?;
    if let Some(sts) = tickets.renewed_sts {
        user.sts = sts;
    }

    let signed = XboxSignedClient::new()?;
    let dev = signed.request_device_token_rps(&tickets.device_rps).await?;
    let sisu = signed
        .sisu_auth_rps(&tickets.user_ticket, XBOX_APP_CLIENT_ID, &dev.token)
        .await?;
    sisu.xauth_header()
        .ok_or_else(|| Error::other("SISU sem AuthorizationToken"))
}

/// Rebuild a stored Windows account with a freshly issued xauth.
///
/// # Errors
/// As [`issue_xauth`], plus a token Xbox Live rejects.
pub async fn restore(device: &Device, mut user: UserSession) -> Result<(Session, UserSession)> {
    let xauth = issue_xauth(device, &mut user).await?;
    let session = crate::session_from_token(xauth).await?;
    Ok((session, user))
}

/// Abandon the sign-in in progress (the user closed the page).
pub fn cancel() {
    *pending() = None;
}

