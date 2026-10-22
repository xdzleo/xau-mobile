//! The user side: inline login, then silent tickets for the Xbox app.
//!
//! Mirrors xodus-cli `commands/login.rs` (the inline login exchange) and
//! `xodus::auth::do_sisu` (the silent device + user tickets), without the
//! keychain: every token comes in as an argument and goes out as a value.

use crate::api::live::{self, rst::RSTError};
use crate::models::live::{DAProperty, ExchangeUserTokenOutcome};
use crate::models::secrets::{LegacyToken, Token};
use crate::models::soap;

pub use crate::device::{authenticate as authenticate_device, provision as provision_device};
pub use crate::models::live::HostBridgeMessage;
pub use crate::models::secrets::{Device, LegacyToken as StsToken};

/// The Xbox app for Windows. Its title (704208617) is the one whose token
/// `presence-heartbeat` accepts for any list of titles.
pub const XBOX_APP_CLIENT_ID: &str = "000000004424da1f";

/// What Windows' token broker calls itself when it asks for a ticket.
pub const HOSTING_APP: &str = "{28C08266-F973-4AE6-FFE4-409B249F138F}";

/// The key the long-lived user STS token is filed under.
pub const PASSPORT_STS: &str = "http://Passport.NET/STS";

const USER_AUTH_SCOPE: &str = "scope=service::user.auth.xboxlive.com::MBI_SSL&api-version=2.0";
const LOGIN_MARKET: &str = "en-US";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("RST2: {0}")]
    Rst(#[from] RSTError),
    #[error("device license: {0}")]
    License(String),
    #[error("{0}")]
    Unexpected(String),
}

// ------------------------------------------------------------ inline login

/// The page the login WebView opens: Windows' own MSA sign-in, hosted.
pub fn inline_login_url(client_id: &str) -> String {
    format!(
        "https://login.live.com/ppsecure/InlineLogin.srf?id=80604&scid=3&mkt={LOGIN_MARKET}&Platform=Windows10&clientid={client_id}&hosted=1"
    )
}

/// Headers that make login.live.com serve the CloudExperienceHost flow,
/// which ends by handing the page's host a DA token (`ServerData`).
pub fn inline_login_headers(correlation_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("cxh-capabilities", CXH_CAPABILITIES.to_string()),
        ("cxh-correlationId", correlation_id.to_string()),
        ("cxh-msaBinaryVersion", "55".to_string()),
        ("cxh-identityClientBinaryVersion", "3".to_string()),
        (
            "cxh-osVersionInfo",
            r#"{"platformId":2,"majorVersion":10,"minorVersion":0,"buildNumber":26100}"#.to_string(),
        ),
        ("cxh-platform", "CloudExperienceHost.Platform.DESKTOP".to_string()),
        ("cxh-protocol", "TokenBroker".to_string()),
        ("cxh-source", "TokenBroker".to_string()),
        ("hostApp", "CloudExperienceHost".to_string()),
    ]
}

const CXH_CAPABILITIES: &str =
    r#"{"PrivatePropertyBag":1,"PasswordlessConnect":1,"PreferAssociate":1,"ChromelessUI":0}"#;

/// A fresh `cxh-correlationId` for one sign-in.
pub fn new_correlation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The user agent the login WebView must present (Windows' web host).
pub const INLINE_LOGIN_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; MSAppHost/3.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.102 Safari/537.36 Edge/18.26100";

/// Script run at document start: the page reports to `window.external`.
/// `bridge` is the name of the host object that receives the messages.
pub fn inline_login_init_script(bridge: &str) -> String {
    format!("window.external = {{ notify: function (m) {{ {bridge}.postMessage(m); }} }};")
}

/// Script run once the page lands on `post.srf`: hand over `ServerData`.
pub fn inline_login_finish_script(bridge: &str) -> String {
    format!("{bridge}.postMessage(JSON.stringify(ServerData));")
}

/// The page asks its host for context during the flow; this is the answer
/// the Windows host gives (and xodus-cli replays).
pub fn host_context_reply_script(context: &str) -> String {
    format!(
        r#"window["CloudExperienceHost.Bridge.dispatchMessage"](JSON.stringify({{"type": "callback", "value": {{ "name": "CloudExperienceHost.getContext", "args": ["CloudExperienceHost", "TokenBroker", "TokenBroker", "{{\"PrivatePropertyBag\":1,\"PasswordlessConnect\":1,\"PreferAssociate\":1,\"ChromelessUI\":0}}"], "context": "{context}"}}}}))"#
    )
}

/// What one message from the login page means.
pub enum PageMessage {
    /// The sign-in finished: exchange this with [`exchange_login`].
    Token(DAProperty),
    /// The page wants the host context: run [`host_context_reply_script`].
    ContextRequest(String),
    Other,
}

pub fn parse_page_message(body: &str) -> PageMessage {
    if let Ok(da) = serde_json::from_str::<DAProperty>(body) {
        return PageMessage::Token(da);
    }
    match serde_json::from_str::<HostBridgeMessage>(body) {
        Ok(m) => match m.get_context_invoke() {
            Some(ctx) => PageMessage::ContextRequest(ctx.to_string()),
            None => PageMessage::Other,
        },
        Err(_) => PageMessage::Other,
    }
}

/// The signed-in user, as far as this protocol is concerned. `sts` is the
/// long-lived credential: keep it (encrypted) to get tickets silently.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserSession {
    pub username: String,
    pub puid: String,
    pub sts: LegacyToken,
}

pub enum LoginStep {
    /// Microsoft wants one more page (consent, proofs): open this URL in
    /// the same WebView and call [`exchange_login`] again with `finish`.
    Finalize(String),
    Done(UserSession),
}

/// Trade the DA token the login page produced for user tokens.
///
/// `finish` is false on the first page and true after a [`LoginStep::Finalize`]
/// page, exactly as xodus-cli asks for the STS scope only on the last leg.
pub async fn exchange_login(
    client: &reqwest::Client,
    device_sts: &LegacyToken,
    da: DAProperty,
    finish: bool,
) -> Result<LoginStep, Error> {
    let user_token = LegacyToken {
        key_name: None,
        token: da.da_token,
        binary_secret: None,
        tpm_key: None,
        lifetime: soap::Timestamp {
            id: None,
            created: da.da_start_time,
            expires: da.da_expires,
        },
    };
    let mut scopes = vec![(USER_AUTH_SCOPE.to_string(), Some(soap::PolicyReference::token_broker()))];
    if finish {
        scopes.push(("http://Passport.NET/tb".to_string(), None));
    }

    let outcome = live::exchange_user_token(
        client,
        user_token,
        da.username.clone(),
        device_sts.clone(),
        Some(da.sts_inline_flow_token),
        None,
        XBOX_APP_CLIENT_ID.to_string(),
        &scopes,
    )
    .await?;

    match outcome {
        ExchangeUserTokenOutcome::Fault(pp) => match pp.and_then(|pp| pp.inline_auth_url) {
            Some(url) => Ok(LoginStep::Finalize(url)),
            None => Err(Error::Unexpected("sign-in refused without a follow-up page".into())),
        },
        ExchangeUserTokenOutcome::Issued(body) => {
            let sts = issued_tokens(body)
                .into_iter()
                .find_map(|(address, token)| match token {
                    Token::Legacy(t) if address == PASSPORT_STS => Some(t),
                    _ => None,
                })
                .ok_or_else(|| Error::Unexpected("sign-in issued no STS token".into()))?;
            Ok(LoginStep::Done(UserSession { username: da.username, puid: da.puid, sts }))
        }
    }
}

/// Every issued token with the address xodus files it under.
fn issued_tokens(body: soap::BodyContent) -> Vec<(String, Token)> {
    let list = match body {
        soap::BodyContent::RequestSecurityTokenResponseCollection(c) => c.security_tokens,
        soap::BodyContent::RequestSecurityTokenResponse(t) => vec![*t],
        _ => Vec::new(),
    };
    list.into_iter()
        .map(|t| {
            let address = t.applies_to.endpoint_reference.address.clone();
            let token = Token::from(t);
            let address = match &token {
                Token::Legacy(l) => l.key_name.clone().unwrap_or(address),
                Token::Compact(_) => address,
            };
            (address, token)
        })
        .collect()
}

// ---------------------------------------------------------- silent tickets

/// What SISU needs, issued silently from the stored credentials.
pub struct XboxTickets {
    /// `DeviceRps`: the device's ticket for `device.auth.xboxlive.com`
    /// with `AuthMethod: RPS`.
    pub device_rps: String,
    /// The user's ticket for SISU's `AccessToken`.
    pub user_ticket: String,
    /// The STS token comes back renewed; store it in place of the old one.
    pub renewed_sts: Option<LegacyToken>,
}

/// `xodus::auth::do_sisu` up to the SISU call: both tickets for `client_id`.
pub async fn xbox_tickets(
    client: &reqwest::Client,
    device_sts: &LegacyToken,
    user: &UserSession,
    client_id: &str,
) -> Result<XboxTickets, Error> {
    let device = live::exchange_device_token(
        client,
        device_sts.clone(),
        HOSTING_APP.to_string(),
        USER_AUTH_SCOPE.to_owned(),
        Some(soap::PolicyReference::token_broker()),
    )
    .await?;
    let Token::Compact(device_rps) = Token::from(device) else {
        return Err(Error::Unexpected("device ticket is not compact".into()));
    };

    let scope = "xboxlive.signin";
    let outcome = live::exchange_user_token(
        client,
        user.sts.clone(),
        "USERNAME".to_string(),
        device_sts.clone(),
        None,
        Some("Silent".to_string()),
        client_id.to_string(),
        &[
            (
                format!("scope={scope}&api-version=2.0&clientid={client_id}"),
                Some(soap::PolicyReference::token_broker()),
            ),
            ("http://Passport.NET/tb".to_string(), None),
        ],
    )
    .await?;
    let ExchangeUserTokenOutcome::Issued(body) = outcome else {
        // A silent request cannot show a page: the stored sign-in is no
        // longer enough and the user has to sign in again.
        return Err(Error::Unexpected("the stored sign-in needs to be renewed".into()));
    };

    let mut tokens = issued_tokens(body);
    let renewed_sts = tokens.iter().find_map(|(address, token)| match token {
        Token::Legacy(t) if address == PASSPORT_STS => Some(t.clone()),
        _ => None,
    });
    if tokens.is_empty() {
        return Err(Error::Unexpected("no user ticket issued".into()));
    }
    let Token::Compact(user_ticket) = tokens.swap_remove(0).1 else {
        return Err(Error::Unexpected("user ticket is not compact".into()));
    };

    Ok(XboxTickets { device_rps, user_ticket, renewed_sts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_data_is_the_token() {
        let body = r#"{"sDAToken":"t","sDASessionKey":"k","sDAStartTime":"a","sDAExpires":"b",
            "sSTSInlineFlowToken":"f","sSigninName":"me@example.com","K":"p","other":1}"#;
        match parse_page_message(body) {
            PageMessage::Token(da) => {
                assert_eq!(da.username, "me@example.com");
                assert_eq!(da.sts_inline_flow_token, "f");
            }
            _ => panic!("ServerData must be read as the token"),
        }
    }

    #[test]
    fn a_context_request_gets_the_host_reply() {
        let body = r#"{"type":"invoke","value":{"name":"CloudExperienceHost.getContext","context":"c42"}}"#;
        let PageMessage::ContextRequest(ctx) = parse_page_message(body) else {
            panic!("getContext must be answered");
        };
        assert_eq!(ctx, "c42");
        let script = host_context_reply_script(&ctx);
        assert!(script.contains(r#""context": "c42""#));
        assert!(script.starts_with(r#"window["CloudExperienceHost.Bridge.dispatchMessage"]"#));
    }

    #[test]
    fn anything_else_is_ignored() {
        assert!(matches!(parse_page_message("not json"), PageMessage::Other));
        assert!(matches!(
            parse_page_message(r#"{"type":"invoke","value":{"name":"Other"}}"#),
            PageMessage::Other
        ));
    }

    #[test]
    fn the_page_is_windows_inline_login_for_the_xbox_app() {
        let url = inline_login_url(XBOX_APP_CLIENT_ID);
        assert!(url.starts_with("https://login.live.com/ppsecure/InlineLogin.srf?"));
        assert!(url.contains("clientid=000000004424da1f"));
        let headers = inline_login_headers("id");
        assert!(headers.iter().any(|(k, v)| *k == "cxh-protocol" && v == "TokenBroker"));
        assert!(headers.iter().any(|(k, v)| *k == "cxh-correlationId" && v == "id"));
        assert!(inline_login_init_script("B").contains("window.external"));
        assert!(inline_login_finish_script("B").contains("ServerData"));
    }
}
