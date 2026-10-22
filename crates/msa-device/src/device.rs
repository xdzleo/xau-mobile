//! The MSA device account: provisioning and RST2 authentication.
//!
//! Upstream `tokens/device.rs` does the same two steps through its
//! keychain-backed `TokenManager`; here the caller owns persistence, so
//! each step simply returns what it produced.

use crate::api::live;
use crate::hardware;
use crate::licensing::splicense::SPLicense;
use crate::licensing::utils::{generate_string, parse_bcrypt_rsa_private};
use crate::models::devicecredential::{Authentication, ClientInfo, DeviceAddRequest, DeviceInfo};
use crate::models::secrets::{Device, LegacyToken, Token};
use crate::models::soap::BodyContent;
use crate::Error;

/// Register a new MSA device account. Done once per install: the result
/// is the device's identity and must be stored, encrypted, for good.
pub async fn provision(client: &reqwest::Client) -> Result<Device, Error> {
    let username = format!("02{}", generate_string(14));
    let password = generate_string(20);
    let provision = DeviceAddRequest {
        client_info: ClientInfo::default(),
        authentication: Authentication::new(username.clone(), password.clone()),
        device_info: Some(DeviceInfo {
            id: "DeviceInfo".to_string(),
            components: hardware::probe_provision_components(),
            tpm_info: None,
        }),
    };

    let dev = live::login_device_credential(client, provision).await?;

    Ok(Device {
        username,
        password,
        puid: dev.puid,
        hwid: dev.hw_device_id,
        device_id: dev.license.binding.device_id.unwrap_or_default(),
        splicense: dev.license.splicense_block,
    })
}

/// Sign the device in: its STS token, the key every later RST2 request is
/// signed with. Lasts hours, not days; ask again when it expires.
pub async fn authenticate(client: &reqwest::Client, device: &Device) -> Result<LegacyToken, Error> {
    let sp_license = SPLicense::parse_base64(&device.splicense)
        .map_err(|e| Error::License(format!("{e:?}")))?;
    let clep_sign_state = sp_license
        .clep_sign_state
        .ok_or_else(|| Error::License("missing CLEP sign state".into()))?;
    let key = clep_sign_state.get_rsa_key();
    let private_key = parse_bcrypt_rsa_private(&key).map_err(|e| Error::License(e.to_string()))?;

    let resp = live::authenticate_device(client, device.username.clone(), private_key).await?;
    match resp.body.body {
        BodyContent::RequestSecurityTokenResponse(resp) => match Token::from(*resp) {
            Token::Legacy(token) => Ok(token),
            Token::Compact(_) => Err(Error::Unexpected("device STS token is not a legacy token".into())),
        },
        other => Err(Error::Unexpected(format!("device authentication: {other:?}"))),
    }
}
