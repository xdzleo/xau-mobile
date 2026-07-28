//! Human names for Game Pass offers.
//!
//! `profile.gamepass.com/v2/offers` returns identity and status only —
//! `offerId`, `offerStatus`, `resourceType`. No title. The Xbox app gets
//! the words from a different service entirely: `arc.msn.com`, the
//! content platform that renders the cards, which returns each card with
//! its `offerId` alongside a `title` and `shortDescription`.
//!
//! Found by capturing the Android app with Frida (BoringSSL hook) and
//! decoding its HTTP/2 stream. The payloads nest JSON inside JSON
//! strings, hence the recursive walk below rather than a flat parse.
//!
//! ```text
//! GET https://arc.msn.com/v4/api/selection
//!         ?placement=88000271&country=BR&locale=pt-BR&fmt=json
//!         &app=XboxMobile&clr=Mobile&bcnt=60&ring=retail&device=android
//! ```
//!
//! This endpoint takes **no authentication** — it is a content feed, not
//! an account API. So a failure here costs a nice label and nothing
//! else, which is why every error path degrades to "no name" rather than
//! failing the offer list that the user actually came for.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::XboxClient;
use crate::error::Result;

/// What a card says about an offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OfferName {
    pub title: String,
    pub description: String,
}

/// Placements observed carrying member-offer cards. More than one is
/// queried because a single feed does not return every offer, and a
/// missing name is worse than an extra request.
const PLACEMENTS: &[&str] = &["88000271", "88000279", "88000620"];

impl XboxClient {
    /// Best-effort `offerId` → name map.
    ///
    /// Never fails the caller: a placement that errors or returns
    /// something unexpected contributes nothing and the rest still map.
    ///
    /// # Errors
    /// Only if the HTTP client itself cannot be used.
    pub async fn get_offer_names(
        &self,
        market: &str,
        locale: &str,
    ) -> Result<HashMap<String, OfferName>> {
        let mut out = HashMap::new();
        for placement in PLACEMENTS {
            let url = format!(
                "https://arc.msn.com/v4/api/selection?placement={placement}\
                 &country={market}&locale={locale}&fmt=json&nct=1\
                 &app=XboxMobile&clr=Mobile&bcnt=60&ring=retail&device=android"
            );
            // No auth headers: this is a public content feed.
            let Ok(v) = self
                .request_json::<serde_json::Value>(
                    reqwest::Method::GET,
                    &url,
                    reqwest::header::HeaderMap::new(),
                    None,
                )
                .await
            else {
                continue;
            };
            collect_names(&v, &mut out);
        }
        Ok(out)
    }
}

/// Walk any structure for objects carrying an `offerId`, keeping the
/// title found on that object.
///
/// Recursive, and it re-parses strings that look like embedded JSON:
/// `arc.msn` wraps card payloads inside JSON string values, so a flat
/// walk finds the ids and none of the titles.
fn collect_names(v: &serde_json::Value, out: &mut HashMap<String, OfferName>) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(id) = map.get("offerId").and_then(serde_json::Value::as_str) {
                if id.len() == 32 {
                    let pick = |k: &str| {
                        map.get(k)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    let title = {
                        let t = pick("title");
                        if t.is_empty() {
                            pick("subTitle")
                        } else {
                            t
                        }
                    };
                    let description = {
                        let d = pick("shortDescription");
                        if d.is_empty() {
                            pick("description")
                        } else {
                            d
                        }
                    };
                    if !title.is_empty() {
                        out.entry(id.to_string())
                            .or_insert(OfferName { title, description });
                    }
                }
            }
            for val in map.values() {
                collect_names(val, out);
                if let Some(s) = val.as_str() {
                    if s.contains("offerId") {
                        if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                            collect_names(&inner, out);
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                collect_names(it, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real nesting: a card object under `ad`, carrying both.
    #[test]
    fn reads_a_card_shaped_like_the_captured_one() {
        let v = serde_json::json!({
            "items": [{ "ad": {
                "title": "Discord Nitro",
                "shortDescription": "3 meses grátis do Nitro",
                "offerId": "6870AAC98E444CD69D948BD07D2D33B6"
            }}]
        });
        let mut m = HashMap::new();
        collect_names(&v, &mut m);
        assert_eq!(m["6870AAC98E444CD69D948BD07D2D33B6"].title, "Discord Nitro");
    }

    /// arc.msn embeds card JSON inside a string value; a flat walk finds
    /// the id and loses the title, which is the bug this guards.
    #[test]
    fn reads_a_card_encoded_inside_a_string() {
        let inner = r#"{"ad":{"title":"skate.","shortDescription":"Supercharge Pack","offerId":"5609A2AE78614695B55634C5D4F2963D"}}"#;
        let v = serde_json::json!({ "payload": inner });
        let mut m = HashMap::new();
        collect_names(&v, &mut m);
        assert_eq!(m["5609A2AE78614695B55634C5D4F2963D"].title, "skate.");
    }

    /// An id with no title contributes nothing rather than an empty row.
    #[test]
    fn an_untitled_offer_is_skipped() {
        let v = serde_json::json!({ "offerId": "A".repeat(32) });
        let mut m = HashMap::new();
        collect_names(&v, &mut m);
        assert!(m.is_empty());
    }
}
