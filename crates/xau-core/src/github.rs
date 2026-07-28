//! Thin GitHub client used only for update checks — verbatim port of
//! `XAU/Networking/GithubRestApi.cs`.

use std::time::Duration;

use reqwest::{header, Client};

use crate::error::Result;
use crate::models::{EventsUpdateResponse, GithubRelease, VersionResponse};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:108.0) Gecko/20100101 Firefox/108.0";

/// Same resource URLs the C# version polls. Kept private because these are
/// implementation details of the "Fumo-Unlockers" repo.
mod urls {
    pub const DEV_VERSION: &str =
        "https://raw.githubusercontent.com/Fumo-Unlockers/Xbox-Achievement-Unlocker/Pre-Release/info.json";
    pub const RELEASES: &str =
        "https://api.github.com/repos/Fumo-Unlockers/Xbox-Achievement-unlocker/releases";
    pub const EVENTS_META: &str =
        "https://raw.githubusercontent.com/Fumo-Unlockers/Xbox-Achievement-Unlocker/Events-Data/meta.json";
}

#[derive(Debug, Clone)]
pub struct GithubClient {
    http: Client,
}

impl Default for GithubClient {
    fn default() -> Self {
        Self::new().expect("building default reqwest client must not fail")
    }
}

impl GithubClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(Duration::from_secs(15))
            .default_headers({
                let mut h = header::HeaderMap::new();
                h.insert(header::USER_AGENT, header::HeaderValue::from_static(USER_AGENT));
                h.insert(
                    header::ACCEPT,
                    header::HeaderValue::from_static(
                        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
                    ),
                );
                h
            })
            .build()?;
        Ok(Self { http })
    }

    pub async fn get_dev_tool_version(&self) -> Result<VersionResponse> {
        let text = self
            .http
            .get(urls::DEV_VERSION)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Returns the latest (most recent) published release.
    pub async fn get_latest_release(&self) -> Result<Option<GithubRelease>> {
        let text = self
            .http
            .get(urls::RELEASES)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let releases: Vec<GithubRelease> = serde_json::from_str(&text)?;
        Ok(releases.into_iter().next())
    }

    pub async fn get_all_releases(&self) -> Result<Vec<GithubRelease>> {
        let text = self
            .http
            .get(urls::RELEASES)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn check_for_event_updates(&self) -> Result<EventsUpdateResponse> {
        let text = self
            .http
            .get(urls::EVENTS_META)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(serde_json::from_str(&text)?)
    }
}
