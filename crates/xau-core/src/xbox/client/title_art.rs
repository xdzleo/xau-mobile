//! Box art for a list of titles, from titlehub in one request.
//!
//! ```text
//! POST https://titlehub.xboxlive.com/titles/batch/decoration/image
//!      {"titleIds": ["1144039928", ...], "pfns": null, "windowsPhoneProductIds": null}
//! ```
//!
//! Each title comes back with a `displayImage` on store-images, often as
//! plain `http://`. Android refuses cleartext by default, so the scheme is
//! raised to https (the host serves both), and a size is asked for so a
//! row of icons does not download eight full-size posters.
//!
//! Decoration only: a failure here costs the pictures, never the farm.

use std::collections::HashMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::XboxClient;
use crate::error::Result;
use crate::xbox::constants as c;

#[derive(Serialize)]
struct BatchRequest<'a> {
    #[serde(rename = "titleIds")]
    title_ids: &'a [String],
    pfns: Option<()>,
    #[serde(rename = "windowsPhoneProductIds")]
    windows_phone_product_ids: Option<()>,
}

#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    titles: Vec<TitleArt>,
}

#[derive(Deserialize)]
struct TitleArt {
    #[serde(rename = "titleId", default)]
    title_id: String,
    #[serde(rename = "displayImage", default)]
    display_image: Option<String>,
}

/// Square edge, in pixels, asked of store-images: a crisp icon on a
/// high-density screen without pulling the full poster.
const ICON_PX: u32 = 144;

impl XboxClient {
    /// `titleId -> image URL` for the titles titlehub knows. Titles it
    /// has no picture for are simply absent.
    ///
    /// # Errors
    /// Transport and Xbox-side failures.
    pub async fn get_title_images(&self, title_ids: &[String]) -> Result<HashMap<String, String>> {
        if title_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let url = format!("https://{}/titles/batch/decoration/image", c::hosts::TITLE_HUB);
        let headers = self.headers_with(&[
            (c::header_names::CONTRACT_VERSION, c::header_values::CONTRACT_VERSION_2),
            (c::header_names::ACCEPT_LANGUAGE, "pt-BR"),
        ])?;
        let body = BatchRequest {
            title_ids,
            pfns: None,
            windows_phone_product_ids: None,
        };
        let resp: BatchResponse = self
            .request_json(Method::POST, &url, headers, Some(&body))
            .await?;
        Ok(resp
            .titles
            .into_iter()
            .filter_map(|t| Some((t.title_id, icon_url(t.display_image.as_deref()?)?)))
            .collect())
    }
}

/// https, and sized for an icon. `None` for an empty or non-web URL.
fn icon_url(raw: &str) -> Option<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    if rest.is_empty() {
        return None;
    }
    let sep = if rest.contains('?') { '&' } else { '?' };
    Some(format!("https://{rest}{sep}w={ICON_PX}&h={ICON_PX}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_images_come_back_as_sized_https() {
        assert_eq!(
            icon_url("http://store-images.s-microsoft.com/image/apps.1.2").as_deref(),
            Some("https://store-images.s-microsoft.com/image/apps.1.2?w=144&h=144")
        );
        assert_eq!(
            icon_url("https://x.example/a?mode=scale").as_deref(),
            Some("https://x.example/a?mode=scale&w=144&h=144")
        );
    }

    #[test]
    fn anything_that_is_not_a_web_url_is_dropped() {
        assert!(icon_url("").is_none());
        assert!(icon_url("ms-appx://foo").is_none());
        assert!(icon_url("http://").is_none());
    }

    #[test]
    fn titles_without_a_picture_are_left_out() {
        let r: BatchResponse = serde_json::from_str(
            r#"{"titles":[{"titleId":"1","displayImage":"http://h/a"},{"titleId":"2"}]}"#,
        )
        .expect("parse");
        let got: HashMap<_, _> = r
            .titles
            .into_iter()
            .filter_map(|t| Some((t.title_id, icon_url(t.display_image.as_deref()?)?)))
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got["1"].starts_with("https://h/a?"));
    }
}
