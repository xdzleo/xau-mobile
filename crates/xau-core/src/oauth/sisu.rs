//! Xbox Live SISU: signed device-token + SISU authorization.
//!
//! Port of `XboxAuthNet/XboxLive/{XboxSignedClient,AbstractXboxSignedAuthRequest,XboxDeviceTokenRequest,XboxSisuAuthRequest}.cs`.
//!
//! Every request here carries a `Signature` header produced by
//! [`XboxRequestSigner::sign_request`](super::signer::XboxRequestSigner::sign_request),
//! and the body is serialized with the ephemeral EC P-256 `ProofKey`.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::oauth::consts;
use crate::oauth::signer::{ProofKey, XboxRequestSigner};

/// Standard Xbox Live auth-token response envelope.
///
/// `Debug` is hand-written (NOT derived): `token` is a live XSTS/XBL
/// bearer and `display_claims` carries identity (`uhs`/`xid`). A
/// derived `Debug` would spray both into any `tracing::…(?resp)` or
/// `{:?}` log. Audit: sisu-response-types-derive-debug-leak.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct XboxAuthResponse {
    #[serde(default, rename = "IssueInstant")]
    pub issue_instant: Option<String>,
    #[serde(default, rename = "NotAfter")]
    pub not_after: Option<String>,
    #[serde(default, rename = "Token")]
    pub token: String,
    #[serde(default, rename = "DisplayClaims")]
    pub display_claims: Option<Value>,
}

impl std::fmt::Debug for XboxAuthResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XboxAuthResponse")
            .field("issue_instant", &self.issue_instant)
            .field("not_after", &self.not_after)
            .field("token", &crate::oauth::redact::mask_token(&self.token))
            .field(
                "display_claims",
                &self
                    .display_claims
                    .as_ref()
                    .map(|_| "<identity claims redacted>"),
            )
            .finish()
    }
}

impl XboxAuthResponse {
    /// Extracts the user-hash (`uhs`) claim. The Xbox API returns
    /// `{"xui": [{ "uhs": "..." , ...}]}` under `DisplayClaims`.
    pub fn user_hash(&self) -> Option<String> {
        let claims = self.display_claims.as_ref()?;
        claims
            .get("xui")
            .and_then(|x| x.get(0))
            .and_then(|x| x.get("uhs"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    pub fn xui_claims(&self) -> Option<XboxAuthXuiClaims> {
        let raw = self.display_claims.as_ref()?.get("xui")?.get(0)?.clone();
        serde_json::from_value(raw).ok()
    }
}

/// Convenience wrapper around a parsed `xui[0]` entry. Mirrors
/// `XboxAuthXuiClaims.cs` in the vendored source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XboxAuthXuiClaims {
    #[serde(default)]
    pub gtg: Option<String>,
    #[serde(default)]
    pub mgt: Option<String>,
    #[serde(default)]
    pub umg: Option<String>,
    #[serde(default)]
    pub mgs: Option<String>,
    #[serde(default)]
    pub xid: Option<String>,
    #[serde(default)]
    pub uhs: Option<String>,
    #[serde(default)]
    pub agg: Option<String>,
    #[serde(default)]
    pub usr: Option<String>,
    #[serde(default)]
    pub utr: Option<String>,
    #[serde(default)]
    pub prv: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XboxSisuResponse {
    #[serde(default, rename = "DeviceToken")]
    pub device_token: Option<String>,
    #[serde(default, rename = "TitleToken")]
    pub title_token: Option<XboxAuthResponse>,
    #[serde(default, rename = "UserToken")]
    pub user_token: Option<XboxAuthResponse>,
    #[serde(default, rename = "AuthorizationToken")]
    pub authorization_token: Option<XboxAuthResponse>,
    #[serde(default, rename = "WebPage")]
    pub web_page: Option<String>,
    #[serde(default, rename = "Sandbox")]
    pub sandbox: Option<String>,
}

impl XboxSisuResponse {
    /// Builds an `XBL3.0 x=<uhs>;<token>` string from the `AuthorizationToken`
    /// field. Returns `None` if the expected fields are missing.
    pub fn xauth_header(&self) -> Option<String> {
        let auth = self.authorization_token.as_ref()?;
        let uhs = auth.user_hash()?;
        Some(format!("XBL3.0 x={};{}", uhs, auth.token))
    }
}

/// High-level signed client for Xbox Live token exchanges.
#[derive(Debug, Clone)]
pub struct XboxSignedClient {
    http: Client,
    signer: XboxRequestSigner,
}

impl XboxSignedClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            signer: XboxRequestSigner::generate(),
        })
    }

    pub fn with_signer(signer: XboxRequestSigner) -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, signer })
    }

    /// `POST https://device.auth.xboxlive.com/device/authenticate`
    pub async fn request_device_token(
        &self,
        device_type: &str,
        device_version: &str,
    ) -> Result<XboxAuthResponse> {
        let body = DeviceTokenRequestBody {
            properties: DeviceTokenProperties {
                auth_method: "ProofOfPossession",
                id: brace_uuid(),
                device_type: device_type.to_string(),
                serial_number: brace_uuid(),
                version: device_version.to_string(),
                proof_key: self.signer.proof_key().clone(),
            },
            relying_party: consts::XBOX_AUTH_RELYING_PARTY,
            token_type: "JWT",
        };
        self.signed_post(consts::XBOX_DEVICE_AUTH_URL, "", &body)
            .await
    }

    /// `POST https://sisu.xboxlive.com/authorize`
    pub async fn sisu_auth(
        &self,
        access_token: &str,
        client_id: &str,
        device_token: &str,
        relying_party: &str,
    ) -> Result<XboxSisuResponse> {
        let body = SisuAuthRequestBody {
            access_token: format!("t={access_token}"),
            app_id: client_id.to_string(),
            device_token: device_token.to_string(),
            sandbox: "RETAIL",
            use_modern_gamertag: true,
            site_name: "user.auth.xboxlive.com",
            relying_party: relying_party.to_string(),
            proof_key: self.signer.proof_key().clone(),
        };
        self.signed_post(consts::SISU_AUTH_URL, "", &body).await
    }

    /// `POST https://device.auth.xboxlive.com/device/authenticate` with
    /// `AuthMethod: RPS`: the device token of a registered Windows device,
    /// from the device ticket of its MSA device account (see the
    /// `msa-device` crate). Same request as xal's `get_device_token_rps`.
    pub async fn request_device_token_rps(&self, rps_ticket: &str) -> Result<XboxAuthResponse> {
        let body = RpsDeviceTokenRequestBody {
            relying_party: consts::XBOX_AUTH_RELYING_PARTY,
            token_type: "JWT",
            properties: RpsDeviceTokenProperties {
                auth_method: "RPS",
                rps_ticket: rps_ticket.to_string(),
                site_name: "user.auth.xboxlive.com",
                version: "10.0.22621",
                proof_key: self.signer.proof_key().clone(),
            },
        };
        self.signed_post(consts::XBOX_DEVICE_AUTH_URL, "", &body)
            .await
    }

    /// `POST https://sisu.xboxlive.com/authorize` for an RST2 user ticket,
    /// which goes in as-is (no `t=` prefix). Same request as xal's
    /// `sisu_authorize_rps`; an RPS device token is refused with a plain
    /// MSA access token (401), it needs this device-bound ticket.
    pub async fn sisu_auth_rps(
        &self,
        user_ticket: &str,
        client_id: &str,
        device_token: &str,
    ) -> Result<XboxSisuResponse> {
        self.sisu_auth_rps_for(user_ticket, client_id, device_token, None)
            .await
    }

    /// As [`Self::sisu_auth_rps`], with the `AuthorizationToken` issued
    /// for `relying_party` instead of Xbox Live.
    pub async fn sisu_auth_rps_for(
        &self,
        user_ticket: &str,
        client_id: &str,
        device_token: &str,
        relying_party: Option<&str>,
    ) -> Result<XboxSisuResponse> {
        let body = RpsSisuRequestBody {
            access_token: user_ticket.to_string(),
            app_id: client_id.to_string(),
            device_token: device_token.to_string(),
            sandbox: "RETAIL",
            site_name: "user.auth.xboxlive.com",
            relying_party: relying_party.map(str::to_string),
            proof_key: self.signer.proof_key().clone(),
        };
        self.signed_post(consts::SISU_AUTH_URL, "", &body).await
    }

    /// `POST https://xsts.auth.xboxlive.com/xsts/authorize`: trade the
    /// user/device/title tokens of a SISU response for an XSTS token of
    /// another relying party, as `XBL3.0 x=<uhs>;<token>`.
    pub async fn xsts_authorize(
        &self,
        sisu: &XboxSisuResponse,
        relying_party: &str,
    ) -> Result<String> {
        let user = sisu
            .user_token
            .as_ref()
            .ok_or_else(|| Error::other("SISU sem UserToken"))?;
        // XSTS refuses a `null` token, so the optional ones are left out.
        let mut props = serde_json::json!({ "SandboxId": "RETAIL", "UserTokens": [user.token] });
        if let Some(d) = &sisu.device_token {
            props["DeviceToken"] = d.as_str().into();
        }
        if let Some(t) = &sisu.title_token {
            props["TitleToken"] = t.token.as_str().into();
        }
        let body = serde_json::json!({
            "Properties": props,
            "RelyingParty": relying_party,
            "TokenType": "JWT",
        });
        let xsts: XboxAuthResponse = self.signed_post(consts::XSTS_AUTH_URL, "", &body).await?;
        let uhs = xsts
            .user_hash()
            .ok_or_else(|| Error::other("XSTS sem uhs"))?;
        Ok(format!("XBL3.0 x={uhs};{}", xsts.token))
    }

    /// Runs the full device → SISU flow for the given Microsoft access token
    /// and returns a ready-to-use `XBL3.0 x=…;…` header value along with the
    /// full SISU response (for callers that want the events-token path).
    pub async fn acquire_xauth(
        &self,
        access_token: &str,
        relying_party: &str,
    ) -> Result<(String, XboxSisuResponse)> {
        let device = self.request_device_token("Win32", "0.0.0").await?;
        let dt = if device.token.is_empty() {
            return Err(Error::other("empty device token"));
        } else {
            device.token
        };
        let sisu = self
            .sisu_auth(
                access_token,
                consts::XBOX_APP_PC_CLIENT_ID,
                &dt,
                relying_party,
            )
            .await?;
        let xauth = sisu
            .xauth_header()
            .ok_or_else(|| Error::other("SISU response missing AuthorizationToken / uhs"))?;
        Ok((xauth, sisu))
    }

    async fn signed_post<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
        body: &B,
    ) -> Result<R> {
        let body_str = serde_json::to_string(body)?;
        let signature = self.signer.sign_request(url, token, &body_str)?;
        let resp = self
            .http
            .post(url)
            .header("x-xbl-contract-version", "1")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("Signature", signature)
            .body(body_str)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::HttpStatus { status, body: text });
        }
        Ok(serde_json::from_str(&text)?)
    }
}

fn brace_uuid() -> String {
    // UUIDv4, brace-wrapped like the C# `"{" + Guid.NewGuid() + "}"`.
    let mut bytes = [0u8; 16];
    getrandom(&mut bytes);
    // Set version (4) and variant (RFC 4122).
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{{{:08x}-{:04x}-{:04x}-{:04x}-{:012x}}}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        (u64::from(bytes[10]) << 40)
            | (u64::from(bytes[11]) << 32)
            | (u64::from(bytes[12]) << 24)
            | (u64::from(bytes[13]) << 16)
            | (u64::from(bytes[14]) << 8)
            | u64::from(bytes[15])
    )
}

fn getrandom(buf: &mut [u8]) {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(buf);
}

#[derive(Debug, Serialize)]
struct DeviceTokenRequestBody {
    #[serde(rename = "Properties")]
    properties: DeviceTokenProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
}

#[derive(Debug, Serialize)]
struct DeviceTokenProperties {
    #[serde(rename = "AuthMethod")]
    auth_method: &'static str,
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "DeviceType")]
    device_type: String,
    #[serde(rename = "SerialNumber")]
    serial_number: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "ProofKey")]
    proof_key: ProofKey,
}

#[derive(Debug, Serialize)]
struct SisuAuthRequestBody {
    #[serde(rename = "AccessToken")]
    access_token: String,
    #[serde(rename = "AppId")]
    app_id: String,
    #[serde(rename = "DeviceToken")]
    device_token: String,
    #[serde(rename = "Sandbox")]
    sandbox: &'static str,
    #[serde(rename = "UseModernGamertag")]
    use_modern_gamertag: bool,
    #[serde(rename = "SiteName")]
    site_name: &'static str,
    #[serde(rename = "RelyingParty")]
    relying_party: String,
    #[serde(rename = "ProofKey")]
    proof_key: ProofKey,
}

#[derive(Debug, Serialize)]
struct RpsDeviceTokenRequestBody {
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
    #[serde(rename = "Properties")]
    properties: RpsDeviceTokenProperties,
}

#[derive(Debug, Serialize)]
struct RpsDeviceTokenProperties {
    #[serde(rename = "AuthMethod")]
    auth_method: &'static str,
    #[serde(rename = "RpsTicket")]
    rps_ticket: String,
    #[serde(rename = "SiteName")]
    site_name: &'static str,
    #[serde(rename = "Version")]
    version: &'static str,
    #[serde(rename = "ProofKey")]
    proof_key: ProofKey,
}

#[derive(Debug, Serialize)]
struct RpsSisuRequestBody {
    #[serde(rename = "AccessToken")]
    access_token: String,
    #[serde(rename = "AppId")]
    app_id: String,
    #[serde(rename = "DeviceToken")]
    device_token: String,
    #[serde(rename = "Sandbox")]
    sandbox: &'static str,
    #[serde(rename = "SiteName")]
    site_name: &'static str,
    /// Absent: the Xbox Live token. Set: a token for that party.
    #[serde(rename = "RelyingParty", skip_serializing_if = "Option::is_none")]
    relying_party: Option<String>,
    #[serde(rename = "ProofKey")]
    proof_key: ProofKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sisu_response_and_builds_xauth() {
        let raw = json!({
            "DeviceToken": "DEV",
            "TitleToken": {"Token": "TT", "DisplayClaims": {}},
            "UserToken": {"Token": "UT", "DisplayClaims": {}},
            "AuthorizationToken": {
                "Token": "ABC",
                "DisplayClaims": {"xui": [{"uhs": "USER_HASH", "gtg": "MyGT"}]}
            },
            "Sandbox": "RETAIL"
        })
        .to_string();
        let parsed: XboxSisuResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed.xauth_header().as_deref(),
            Some("XBL3.0 x=USER_HASH;ABC")
        );
        let claims = parsed
            .authorization_token
            .as_ref()
            .unwrap()
            .xui_claims()
            .unwrap();
        assert_eq!(claims.uhs.as_deref(), Some("USER_HASH"));
        assert_eq!(claims.gtg.as_deref(), Some("MyGT"));
    }

    #[test]
    fn brace_uuid_has_canonical_shape() {
        let s = brace_uuid();
        assert!(s.starts_with('{') && s.ends_with('}'));
        assert_eq!(s.len(), 38);
        // version 4 nibble in position 15 (index inside braces)
        let inner = &s[1..s.len() - 1];
        assert_eq!(inner.chars().nth(14).unwrap(), '4');
        let variant = inner.chars().nth(19).unwrap();
        assert!(matches!(variant, '8' | '9' | 'a' | 'b'));
    }
}
