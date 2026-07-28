use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::xbox::constants::ZERO_UID;

/// Deserializer that accepts any of `null`, `bool`, `i64`, `f64`, or
/// `String` and normalises into `Option<String>`. The Xbox Live
/// achievements endpoint is not internally consistent about types: for
/// example `rarity.currentPercentage` is a number (0.26), `isRevoked` is a
/// boolean, and `titleAssociation.id` is a number — but the original C#
/// models typed them as strings. We accept any scalar.
fn de_scalar_or_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Any {
        Null,
        Bool(bool),
        Int(i64),
        Uint(u64),
        Float(f64),
        Str(String),
    }
    Ok(match Any::deserialize(de)? {
        Any::Null => None,
        Any::Bool(b) => Some(b.to_string()),
        Any::Int(i) => Some(i.to_string()),
        Any::Uint(u) => Some(u.to_string()),
        Any::Float(f) => Some(f.to_string()),
        Any::Str(s) => Some(s),
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rarity {
    pub current_category: Option<String>,
    /// Xbox returns this as a number (e.g. `0.26`); older C# typed it as
    /// string. Accept either.
    #[serde(default, deserialize_with = "de_scalar_or_string")]
    pub current_percentage: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaAsset {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TitleAssociation {
    pub name: Option<String>,
    /// `id` comes back as a **number** (e.g. `1144039928`) for every title
    /// Xbox tracks internally, even though the C# model typed it as
    /// `string?`. Accept either so deserialization doesn't blow up.
    #[serde(default, deserialize_with = "de_scalar_or_string")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AchievementRewards {
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub media_asset: Option<MediaAsset>,
    pub value_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AchievementRequirement {
    pub id: Option<String>,
    pub current: Option<String>,
    pub target: Option<String>,
    pub operation_type: Option<String>,
    pub value_type: Option<String>,
    pub rule_participation_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AchievementProgression {
    pub requirements: Vec<AchievementRequirement>,
    pub time_unlocked: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TimeWindow {
    pub start_date: String,
    pub end_date: String,
}

/// Xbox One / Series achievement — matches the C# `OneCoreAchievementResponse`.
///
/// Uses camelCase for the JSON serialization exactly like the upstream Xbox
/// Live endpoint `achievements.xboxlive.com`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Achievement {
    pub rarity: Option<Rarity>,
    pub gamerscore: Option<Value>,
    pub id: String,
    #[serde(default = "default_scid")]
    pub service_config_id: String,
    pub name: String,
    pub title_associations: Vec<TitleAssociation>,
    #[serde(default = "default_progress_state")]
    pub progress_state: String,
    pub progression: Option<AchievementProgression>,
    pub media_assets: Vec<MediaAsset>,
    pub platforms: Vec<String>,
    pub is_secret: bool,
    pub description: Option<String>,
    pub locked_description: Option<String>,
    pub product_id: Option<String>,
    pub achievement_type: Option<String>,
    pub participation_type: Option<String>,
    pub time_window: Option<TimeWindow>,
    pub rewards: Vec<AchievementRewards>,
    pub estimated_time: Option<String>,
    pub deeplink: Option<String>,
    /// Xbox returns this as a **boolean**; the C# source typed it as a
    /// nullable string. Accept either.
    #[serde(default, deserialize_with = "de_scalar_or_string")]
    pub is_revoked: Option<String>,
}

fn default_scid() -> String {
    ZERO_UID.to_string()
}

fn default_progress_state() -> String {
    "Null".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PagingInfo {
    /// Cursor for the next page, or `None` when the list is exhausted.
    /// Xbox returns this as a stringified integer representing the
    /// 0-based offset the next page should start at.
    pub continuation_token: Option<String>,
    pub total_records: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AchievementsResponse {
    pub achievements: Vec<Achievement>,
    /// Present when the server paginates. Callers that care about ALL
    /// achievements (e.g. `get_all_achievements_for_title`) iterate while
    /// `continuation_token` is `Some`.
    pub paging_info: Option<PagingInfo>,
}

// --------------------------------------------------------------------
// SLIM MODELS for bulk-profile scan
// --------------------------------------------------------------------
//
// The full `Achievement` struct above carries ~18 fields so the shape
// matches Xbox Live's response exactly. For the bulk-profile scan
// (`XboxClient::get_all_unlocked_achievements_*`) the plan builder only
// reads 7 of them. When a SiegfriedX-sized profile returns ~126k rows,
// paying the allocation + clone cost for the other 11 fields translates
// into real wall-clock time (dozens of seconds of pure CPU + GC churn
// on the tokio runtime thread, which in turn freezes the GUI progress
// bar).
//
// `SlimAchievement` matches the wire shape 1:1 but only deserializes
// the fields `plan::builder::build_plan` consumes. `#[serde(default)]`
// on the struct plus individual fields means unknown/extra JSON keys
// are silently ignored — the Xbox server can add fields without us
// blowing up.
//
// All three slim types are intentionally **owned** (no borrows) so they
// can cross the `spawn_blocking` boundary.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlimTitleAssociation {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de_scalar_or_string")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlimProgression {
    pub time_unlocked: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlimReward {
    pub value: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SlimAchievement {
    pub id: String,
    #[serde(default = "default_scid")]
    pub service_config_id: String,
    pub name: String,
    pub title_associations: Vec<SlimTitleAssociation>,
    #[serde(default = "default_progress_state")]
    pub progress_state: String,
    pub progression: Option<SlimProgression>,
    pub rewards: Vec<SlimReward>,
    // ---- unlock-safety fields -----------------------------------
    // Small, usually absent — the slim model stays slim in the
    // common case. Included here so the builder's plan-build-time
    // safety gate (`plan::safety::classify_achievement`) can work
    // directly off the bulk scan response without having to re-fetch
    // the full `Achievement` record per title.
    #[serde(default, deserialize_with = "de_scalar_or_string")]
    pub is_revoked: Option<String>,
    pub time_window: Option<TimeWindow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlimAchievementsResponse {
    pub achievements: Vec<SlimAchievement>,
    pub paging_info: Option<PagingInfo>,
}

/// Xbox 360 achievement — different endpoint and schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Xbox360Achievement {
    pub id: i64,
    pub title_id: i64,
    pub name: String,
    pub gamerscore: i32,
    pub description: String,
    pub locked_description: String,
    pub time_unlocked: String,
    pub is_secret: bool,
    pub rarity: Option<Rarity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Xbox360AchievementResponse {
    pub achievements: Vec<Xbox360Achievement>,
}

// ----- request bodies ---------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementsArrayEntry {
    pub id: String,
    #[serde(default = "default_percent")]
    pub percent_complete: String,
}

fn default_percent() -> String {
    "100".into()
}

impl AchievementsArrayEntry {
    pub fn completed(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            percent_complete: "100".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UnlockTitleBasedAchievementRequest {
    pub action: String,
    #[serde(rename = "serviceConfigId")]
    pub service_config_id: String,
    #[serde(rename = "titleId")]
    pub title_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    pub achievements: Vec<AchievementsArrayEntry>,
}

impl UnlockTitleBasedAchievementRequest {
    pub fn new(
        service_config_id: impl Into<String>,
        title_id: impl Into<String>,
        user_id: impl Into<String>,
        achievements: Vec<AchievementsArrayEntry>,
    ) -> Self {
        Self {
            action: "progressUpdate".into(),
            service_config_id: service_config_id.into(),
            title_id: title_id.into(),
            user_id: user_id.into(),
            achievements,
        }
    }
}
