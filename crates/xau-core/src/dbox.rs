//! Port of `XAU/Networking/DBoxRestAPI.cs` — searches title IDs by game name
//! against https://dbox.tools.

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use url::Url;

use crate::error::{Error, Result};

const SEARCH_BASE: &str = "https://dbox.tools/api/title_ids/";

#[derive(Debug, Clone)]
pub struct DBoxClient {
    http: Client,
}

impl Default for DBoxClient {
    fn default() -> Self {
        Self::new().expect("building default reqwest client must not fail")
    }
}

impl DBoxClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { http })
    }

    /// `GET https://dbox.tools/api/title_ids/?name=<text>&limit=100&offset=0`
    ///
    /// The upstream API returns each item's `title_id` as an **uppercase
    /// hexadecimal string** (e.g. `"79C6D2A0"` for Halo Infinite). Every
    /// other Xbox Live endpoint we talk to — achievements.xboxlive.com,
    /// titlehub, the spoof heartbeat — expects the **decimal** form
    /// (`2043073696`). So pasting the raw dbox result into our "spoof"
    /// or "unlock" inputs silently fails.
    ///
    /// This method normalises each item in place: `title_id` becomes the
    /// decimal string, and the original hex is preserved as
    /// `title_id_hex` so power users can still see it.
    pub async fn search(&self, text: &str) -> Result<Value> {
        if text.trim().is_empty() {
            return Err(Error::invalid_argument("search text must not be empty"));
        }

        let mut url = Url::parse(SEARCH_BASE)?;
        url.query_pairs_mut()
            .append_pair("name", text)
            .append_pair("limit", "100")
            .append_pair("offset", "0");

        let resp = self
            .http
            .get(url)
            .header("accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::HttpStatus { status, body: text });
        }
        let mut v: Value = serde_json::from_str(&text)?;
        normalise_title_ids(&mut v);
        Ok(v)
    }
}

/// Walk the JSON returned by dbox.tools and convert every `title_id`
/// field from hex to decimal. Preserves the original as `title_id_hex`.
fn normalise_title_ids(v: &mut Value) {
    let Some(items) = v.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let Some(tid_val) = obj.get("title_id").cloned() else {
            continue;
        };
        let Some(hex) = tid_val.as_str() else {
            continue;
        };
        // Decimal? Skip — already in the right form.
        if hex.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        match u32::from_str_radix(hex.trim_start_matches("0x"), 16) {
            Ok(decimal) => {
                obj.insert(
                    "title_id_hex".to_string(),
                    Value::String(hex.to_ascii_uppercase()),
                );
                obj.insert("title_id".to_string(), Value::String(decimal.to_string()));
            }
            Err(_) => {
                tracing::warn!(%hex, "dbox returned un-parseable title_id, leaving as-is");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_title_ids_get_decoded() {
        let mut v: Value = serde_json::from_str(
            r#"{
            "items": [
                {"title_id": "79C6D2A0", "name": "Halo Infinite"},
                {"title_id": "442736B2", "name": "Halo MCC"}
            ],
            "count": 2
        }"#,
        )
        .unwrap();
        normalise_title_ids(&mut v);
        let items = v["items"].as_array().unwrap();
        // 0x79C6D2A0 = 2043073184, 0x442736B2 = 1143420594.
        assert_eq!(items[0]["title_id"], "2043073184");
        assert_eq!(items[0]["title_id_hex"], "79C6D2A0");
        assert_eq!(items[1]["title_id"], "1143420594");
        assert_eq!(items[1]["title_id_hex"], "442736B2");
    }

    #[test]
    fn already_decimal_title_ids_are_left_alone() {
        let mut v: Value =
            serde_json::from_str(r#"{"items": [{"title_id": "1144039928", "name": "Halo MCC"}]}"#)
                .unwrap();
        normalise_title_ids(&mut v);
        let items = v["items"].as_array().unwrap();
        assert_eq!(items[0]["title_id"], "1144039928");
        // No hex added — already decimal.
        assert!(items[0].get("title_id_hex").is_none());
    }
}
