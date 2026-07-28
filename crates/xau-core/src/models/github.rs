use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct EventsUpdateResponse {
    pub timestamp: i64,
    pub data_version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct VersionResponse {
    #[serde(rename = "DownloadURL")]
    pub download_url: Option<String>,
    pub latest_build_version: Option<String>,
}

/// Minimal subset of GitHub's `GET /repos/:owner/:repo/releases` response
/// used for update checks in the original tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubRelease {
    pub tag_name: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub html_url: String,
    pub assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub content_type: String,
}
