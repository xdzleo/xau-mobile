//! Microsoft OAuth + Xbox Live SISU sign-in.
//!
//! This is a Rust re-implementation of the vendored `XboxAuthNet` code in
//! `XAU/Util/XboxAuthNet/`. The flow, in order, is:
//!
//! 1. Use the Microsoft **device code flow** against `login.live.com` to get
//!    an access token + refresh token. A user-facing `user_code` is shown and
//!    the user confirms in a browser. This replaces the WPF `WebView2` widget
//!    from the original, making the port work headless / on non-Windows hosts.
//! 2. Generate an ephemeral EC P-256 key pair; export its public `x`/`y` as
//!    base64url for the `ProofKey` JWK.
//! 3. POST a signed `XboxDeviceTokenRequest` to
//!    `https://device.auth.xboxlive.com/device/authenticate` to exchange
//!    `ProofKey` for a `DeviceToken`.
//! 4. POST a signed `XboxSisuAuthRequest` to
//!    `https://sisu.xboxlive.com/authorize` with the Microsoft access token
//!    and the `DeviceToken`. The response includes an `AuthorizationToken`
//!    with `XuiClaims.uhs` (user hash) — the two fields concat'd produce the
//!    final `XBL3.0 x=<uhs>;<token>` header value.
//!
//! The signing algorithm is the Xbox-Live-specific scheme: policy version 1,
//! big-endian Windows timestamp, followed by the canonical request bytes
//! (`POST\0`, URI path+query, token, body). Identical to
//! `XboxRequestSigner.cs` in the vendored upstream.

pub mod code_flow;
pub mod device_code;
pub mod dpapi;
pub mod redact;
pub mod session;
pub mod signer;
pub mod sisu;
pub mod token_pool;

pub use code_flow::{classify_redirect, CodeFlow, RedirectOutcome};
pub use device_code::{DeviceCodeFlow, DeviceCodeResponse, MicrosoftOAuthToken};
pub use session::{AuthSession, XAUTH_AND_SESSION};
pub use signer::{ProofKey, XboxRequestSigner};
pub use sisu::{XboxAuthResponse, XboxAuthXuiClaims, XboxSignedClient, XboxSisuResponse};
pub use token_pool::{TokenEntry, TokenPool, TokenStatus};

/// Microsoft/Xbox constants shared across modules.
pub mod consts {
    pub const XBOX_APP_PC_CLIENT_ID: &str = "000000004424da1f";
    pub const XBOX_SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";
    pub const XBOX_LIVE_RELYING_PARTY: &str = "http://xboxlive.com";
    pub const XBOX_AUTH_RELYING_PARTY: &str = "http://auth.xboxlive.com";
    pub const XBOX_EVENTS_RELYING_PARTY: &str = "http://events.xboxlive.com";
    /// The Store's purchase page (`buynowui`) takes an XSTS of this party.
    pub const STORE_RELYING_PARTY: &str = "http://mp.microsoft.com/";
    pub const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";

    pub const DEVICE_CODE_URL: &str = "https://login.live.com/oauth20_connect.srf";
    /// Consent page for the authorization-code (embedded WebView) flow.
    pub const OAUTH_AUTHORIZE_URL: &str = "https://login.live.com/oauth20_authorize.srf";
    /// Where Microsoft sends the browser once consent completes. Nothing
    /// is actually served here — the WebView host watches for a
    /// navigation to this exact URL and reads `?code=` off it.
    pub const OAUTH_DESKTOP_REDIRECT: &str = "https://login.live.com/oauth20_desktop.srf";
    pub const TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
    pub const XBOX_DEVICE_AUTH_URL: &str = "https://device.auth.xboxlive.com/device/authenticate";
    pub const SISU_AUTH_URL: &str = "https://sisu.xboxlive.com/authorize";
}
