//! Game Pass member offers — list the perks an account has, and claim them.
//!
//! Reverse-engineered from the Xbox **Android** app by hooking BoringSSL
//! (`SSL_write`/`SSL_read` in `libssl.so`) with Frida on a rooted device
//! and decoding the HTTP/2 + HPACK stream offline. Two endpoints on
//! `profile.gamepass.com`:
//!
//! ```text
//! GET  /v2/offers            -> every offer, with its status
//! POST /v2/offers/{offerId}  -> claim one. Body is EMPTY.
//! ```
//!
//! The claim carries no body at all (`content-length: 0`): the offer to
//! settle is the one named in the path, exactly as `claimPoints` lets
//! the service decide what is owed.
//!
//! # What an offer looks like
//!
//! ```jsonc
//! // available — the reward is not minted yet
//! { "offerId": "5609A2AE…", "offerStatus": "available",
//!   "resourceType": "store-token", "resource": null,
//!   "claimedDate": null, "marketplaceId": null }
//!
//! // claimed — `resource` now holds the actual reward
//! { "offerId": "39FEA8F9…", "offerStatus": "claimed",
//!   "resourceType": "external-link",
//!   "resource": "https://discord.com/settings/connections?xgp_ref=…",
//!   "claimedDate": "2026-07-28T17:45:59Z", "marketplaceId": "BR" }
//! ```
//!
//! **`resource` is opaque.** Observed types are `external-link` (a
//! Discord promo URL) and `store-token`, and there is no reason to think
//! that list is closed — so nothing here parses it. It is carried
//! through as a string for the caller to display.
//!
//! # On the `signature` header
//!
//! The captured request carried one, from the same ECDSA scheme as the
//! device/SISU handshake. We do not send it, for the reason documented
//! in [`super::rewards`]: that signature belongs to the handshake, its
//! proof key is ephemeral and not retained past sign-in, and
//! XSTS-authenticated service calls accept its absence. If this endpoint
//! ever starts requiring it, the failure is a clean 401 on an otherwise
//! well-formed request — look here first.
//!
//! # Claiming is not reversible
//!
//! Unlike a points claim, settling an offer consumes it for that
//! account. Nothing in this module claims anything on its own; callers
//! must act on an explicit instruction.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::XboxClient;
use crate::error::{Error, Result};

const OFFERS_URL: &str = "https://profile.gamepass.com/v2/offers";

/// One Game Pass member offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Offer {
    pub offer_id: String,
    /// `available`, `claimed`, … as the service words it.
    pub offer_status: String,
    /// `external-link`, `store-token`, … Treated as opaque.
    pub resource_type: Option<String>,
    /// The reward itself. **Null until claimed** — that is the whole
    /// shape of this API: claiming is what mints the resource.
    pub resource: Option<String>,
    pub claimed_date: Option<String>,
    pub claim_type: Option<String>,
    pub marketplace_id: Option<String>,
}

impl Offer {
    /// Still claimable.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.offer_status.eq_ignore_ascii_case("available")
    }

    /// Already settled — `resource` should carry the reward.
    #[must_use]
    pub fn is_claimed(&self) -> bool {
        self.claimed_date.is_some() || self.offer_status.eq_ignore_ascii_case("claimed")
    }
}

impl XboxClient {
    /// Every offer on the signed-in account, claimed or not.
    ///
    /// # Errors
    /// Propagates transport failures and non-2xx responses.
    pub async fn get_offers(&self) -> Result<Vec<Offer>> {
        let headers = self.headers_with_active(&[("content-type", "application/json")])?;
        let raw: serde_json::Value = self
            .request_json(Method::GET, OFFERS_URL, headers, None)
            .await?;
        Ok(parse_offers(&raw))
    }

    /// Claim one offer, and return it with its freshly minted resource.
    ///
    /// **Irreversible**: this consumes the offer for this account. Call
    /// it only in response to an explicit user action.
    ///
    /// # Errors
    /// Propagates transport failures and non-2xx responses. Claiming an
    /// already-claimed offer is not an error — the service returns the
    /// existing resource, which is why this is safe to retry.
    pub async fn claim_offer(&self, offer_id: &str) -> Result<Offer> {
        if offer_id.trim().is_empty() {
            return Err(Error::invalid_argument("empty offer id"));
        }
        // Guard the path: an id with a slash would rewrite the URL and
        // POST somewhere nobody chose.
        if !offer_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(Error::invalid_argument(
                "offer id has unexpected characters",
            ));
        }

        let url = format!("{OFFERS_URL}/{offer_id}");
        let headers = self.headers_with_active(&[("content-type", "application/json")])?;
        // The captured request sends `content-length: 0` and no body at
        // all — not `{}`. Passing None keeps it that way.
        let raw: serde_json::Value = self.request_json(Method::POST, &url, headers, None).await?;
        Ok(serde_json::from_value(raw).unwrap_or_default())
    }
}

/// Accept both a bare array and an object wrapping one.
///
/// The captured response is a top-level array, but every other endpoint
/// in this crate wraps its list, and guessing wrong would silently yield
/// zero offers rather than an error.
fn parse_offers(v: &serde_json::Value) -> Vec<Offer> {
    let items = if v.is_array() {
        Some(v)
    } else {
        v.get("offers").or_else(|| v.get("items"))
    };
    items
        .and_then(|i| serde_json::from_value::<Vec<Offer>>(i.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape, from a live capture.
    #[test]
    fn parses_a_captured_offers_list() {
        let raw = serde_json::json!([
            { "offerId": "5609A2AE", "offerStatus": "available",
              "resourceType": "store-token", "resource": null,
              "claimedDate": null, "claimType": null, "marketplaceId": null },
            { "offerId": "39FEA8F9", "offerStatus": "claimed",
              "resourceType": "external-link",
              "resource": "https://discord.com/settings/connections?xgp_ref=X",
              "claimedDate": "2026-07-28T17:45:59Z", "claimType": null,
              "marketplaceId": "BR" }
        ]);
        let offers = parse_offers(&raw);
        assert_eq!(offers.len(), 2);
        assert!(offers[0].is_available() && !offers[0].is_claimed());
        assert!(offers[1].is_claimed() && !offers[1].is_available());
        // An available offer has no reward yet; that is the point.
        assert!(offers[0].resource.is_none());
        assert!(offers[1].resource.as_deref().unwrap().contains("xgp_ref"));
    }

    /// A wrapped list must not silently decode as empty.
    #[test]
    fn parses_a_wrapped_list_too() {
        let raw = serde_json::json!({ "offers": [
            { "offerId": "A", "offerStatus": "available" }
        ]});
        assert_eq!(parse_offers(&raw).len(), 1);
    }

    /// An unknown shape yields nothing rather than panicking.
    #[test]
    fn an_unexpected_shape_is_empty_not_a_panic() {
        assert!(parse_offers(&serde_json::json!({ "unexpected": 1 })).is_empty());
    }
}
