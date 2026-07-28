use serde::{Deserialize, Serialize};
use serde_json::Value;

// Xbox Live `profile.xboxlive.com` / `peoplehub.xboxlive.com` return
// camelCase JSON. Newtonsoft.Json on the .NET side is case-insensitive by
// default, which is why the C# classes use PascalCase but parse camelCase
// input transparently. Our serde types model the wire shape directly.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BasicProfile {
    pub profile_users: Vec<ProfileUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileUser {
    pub id: Option<String>,
    pub host_id: Option<String>,
    pub settings: Vec<ProfileSetting>,
    /// Xbox returns this as a **boolean** (not a string) on current
    /// builds, even though the C# source typed it as `string?`.
    /// `#[serde(default, deserialize_with)]` tolerates both shapes.
    #[serde(default, deserialize_with = "de_bool_or_string")]
    pub is_sponsored_user: Option<String>,
}

/// Accepts `true`, `false`, `"True"`, `"false"`, or `null` and normalises
/// into `Option<String>` ("true"/"false"). Used anywhere Xbox flips between
/// boolean and string representations for the same field across API builds.
fn de_bool_or_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        B(bool),
        S(String),
        Null,
    }
    match Either::deserialize(de)? {
        Either::B(b) => Ok(Some(b.to_string())),
        Either::S(s) => Ok(Some(s)),
        Either::Null => Ok(None),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileSetting {
    pub id: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub people: Vec<Person>,
    pub recommendation_summary: Option<Value>,
    pub friend_finder_state: Option<Value>,
    pub account_link_details: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Person {
    pub xuid: Option<String>,
    pub is_favorite: bool,
    pub is_following_caller: bool,
    pub is_followed_by_caller: bool,
    pub is_identity_shared: bool,
    pub added_date_time_utc: Option<String>,
    pub display_name: Option<String>,
    pub real_name: Option<String>,
    pub display_pic_raw: Option<String>,
    pub show_user_as_avatar: Option<String>,
    pub gamertag: Option<String>,
    pub gamer_score: Option<String>,
    pub modern_gamertag: Option<String>,
    pub modern_gamertag_suffix: Option<String>,
    pub unique_modern_gamertag: Option<String>,
    pub xbox_one_rep: Option<String>,
    pub presence_state: Option<String>,
    pub presence_text: Option<String>,
    pub is_broadcasting: bool,
    pub is_cloaked: Option<bool>,
    pub is_quarantined: bool,
    #[serde(rename = "isXbox360Gamerpic")]
    pub is_xbox_360_gamerpic: bool,
    pub last_seen_date_time_utc: Option<String>,
    pub multiplayer_summary: Option<MultiplayerSummary>,
    pub preferred_color: Option<PreferredColor>,
    pub presence_details: Vec<PresenceDetail>,
    pub detail: Option<Detail>,
    pub linked_accounts: Vec<LinkedAccount>,
    pub color_theme: Option<String>,
    pub preferred_flag: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MultiplayerSummary {
    pub joinable_activities: Vec<Value>,
    pub party_details: Vec<Value>,
    pub in_party: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PreferredColor {
    pub primary_color: Option<String>,
    pub secondary_color: Option<String>,
    pub tertiary_color: Option<String>,
}

/// **Case anomaly**: unlike the rest of the peoplehub response, Xbox Live
/// emits `PresenceDetail` keys as **PascalCase** (`IsBroadcasting`, `Device`,
/// `TitleId`, `IsPrimary`, etc.) rather than camelCase. Deserialising as
/// camelCase silently produces an empty default — and the UI's "currently
/// playing" row goes blank.
///
/// Verified against a live capture on 2026-04-21; see
/// `crates/xau-core/tests/fixtures/profile_full.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PresenceDetail {
    pub is_broadcasting: bool,
    pub device: Option<String>,
    pub presence_text: Option<String>,
    pub state: Option<String>,
    pub title_id: Option<String>,
    pub is_primary: bool,
    pub is_game: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Detail {
    pub account_tier: Option<String>,
    pub bio: Option<String>,
    pub is_verified: bool,
    pub location: Option<String>,
    pub tenure: Option<String>,
    pub watermarks: Vec<String>,
    pub blocked: bool,
    pub mute: bool,
    pub follower_count: i32,
    pub following_count: i32,
    pub has_game_pass: bool,
    pub genres: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LinkedAccount {
    pub network_name: Option<String>,
    pub display_name: Option<String>,
    pub show_on_profile: bool,
    pub is_family_friendly: bool,
    pub deeplink: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GamepassData {
    /// Alias covers both `gamePassMembership` (the current API field —
    /// capital P) and the legacy `gamepassMembership` spelling the C#
    /// source expected. Neither matches Rust's `rename_all = "camelCase"`
    /// from `gamepass_membership` alone, so we enumerate explicitly.
    #[serde(alias = "gamePassMembership", alias = "gamepassMembership")]
    pub gamepass_membership: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Gamepass {
    /// Xbox Live's current `xgrant.xboxlive.com/programInfo` response uses
    /// `gamePassMembership` (with capital P); older responses used
    /// `gamepassMembership`. We accept either — if neither is present we
    /// fall through to `data.gamepassMembership` or return `None`.
    #[serde(alias = "gamePassMembership", alias = "gamepassMembership")]
    pub gamepass_membership: Option<String>,
    pub data: Option<GamepassData>,
}
