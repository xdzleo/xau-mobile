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
//!
//! # Hosts, and why this skips the retry loop
//!
//! `arc.msn.com` is on the common ad-block lists, so on a network with a
//! DNS filter it resolves to `0.0.0.0` / `127.0.0.1` and every connect
//! is refused. The shared request path treats a refused connect as
//! transient and retries it four times with backoff (2+4+8+16 s), so
//! three placements cost ~90 s — all spent before the offer list the
//! screen is waiting on is even requested. The same `v4/api/selection`
//! API is served by Windows Spotlight's host, `fd.api.iris.microsoft.com`,
//! with identical payloads, and is not on those lists. It is asked
//! first, `arc.msn.com` only if it fails, each with one short attempt:
//! a label is not worth a retry budget.

use std::collections::HashMap;
use std::time::Duration;

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

/// Hosts serving the same selection API, in the order they are asked.
/// See the module docs for why Spotlight's comes first.
const HOSTS: &[&str] = &["https://fd.api.iris.microsoft.com", "https://arc.msn.com"];

/// One attempt per host. The feed answers in about a second; a host that
/// takes longer is one the screen should not be waiting on.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);

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
        self.offer_names_from(HOSTS, market, locale).await
    }

    async fn offer_names_from(
        &self,
        hosts: &[&str],
        market: &str,
        locale: &str,
    ) -> Result<HashMap<String, OfferName>> {
        // Placements are independent, so they go out together: the
        // worst case is one placement's time, not the sum of three.
        let mut tasks = tokio::task::JoinSet::new();
        for placement in PLACEMENTS {
            let client = self.clone();
            let urls: Vec<String> = hosts
                .iter()
                .map(|host| selection_url(host, placement, market, locale))
                .collect();
            tasks.spawn(async move {
                for url in urls {
                    if let Some(v) = client.fetch_feed(&url).await {
                        return Some(v);
                    }
                }
                None
            });
        }
        let mut out = HashMap::new();
        while let Some(done) = tasks.join_next().await {
            if let Ok(Some(v)) = done {
                collect_names(&v, &mut out);
            }
        }
        Ok(out)
    }

    /// One unauthenticated GET, no retries. `None` on any failure.
    async fn fetch_feed(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self
            .inner
            .http
            .get(url)
            .timeout(ATTEMPT_TIMEOUT)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let text = resp.text().await.ok()?;
        serde_json::from_str(&text).ok()
    }
}

fn selection_url(host: &str, placement: &str, market: &str, locale: &str) -> String {
    format!(
        "{host}/v4/api/selection?placement={placement}\
         &country={market}&locale={locale}&fmt=json&nct=1\
         &app=XboxMobile&clr=Mobile&bcnt=60&ring=retail&device=android"
    )
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn client() -> XboxClient {
        XboxClient::new(super::super::XboxClientOptions::default()).unwrap()
    }

    /// A local port with nothing listening: what a DNS filter turns
    /// `arc.msn.com` into.
    fn refused_host() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    }

    /// Serve one canned feed response to every request.
    async fn feed_host(body: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The bug behind the stuck Vantagens screen: a blocked host went
    /// through the shared retry loop and held the offer list for ~90 s.
    /// It must now cost one refused connect per placement.
    #[tokio::test]
    async fn a_blocked_host_fails_fast() {
        let host = refused_host();
        let t = std::time::Instant::now();
        let names = client()
            .offer_names_from(&[host.as_str()], "BR", "pt-BR")
            .await
            .unwrap();
        assert!(names.is_empty());
        assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    }

    /// The first host blocked, the second answering: names still arrive.
    #[tokio::test]
    async fn falls_back_to_the_next_host() {
        let blocked = refused_host();
        let good = feed_host(
            r#"{"batchrsp":{"items":[{"item":"{\"ad\":{\"title\":\"Suas vantagens no Discord\",\"shortDescription\":\"Nitro\",\"offerId\":\"39FEA8F9C18B4222BF36A0D7BF5B28D7\"}}"}]}}"#,
        )
        .await;
        let names = client()
            .offer_names_from(&[blocked.as_str(), good.as_str()], "BR", "pt-BR")
            .await
            .unwrap();
        assert_eq!(
            names["39FEA8F9C18B4222BF36A0D7BF5B28D7"].title,
            "Suas vantagens no Discord"
        );
    }

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
