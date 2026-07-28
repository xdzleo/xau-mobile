use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::profile::Detail;

// These types all travel over the wire from `titlehub.xboxlive.com` and
// `catalog.gamepass.com`. Both endpoints emit camelCase JSON.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BasicAchievementDetails {
    pub current_achievements: i32,
    pub total_achievements: i32,
    pub current_gamerscore: i32,
    pub total_gamerscore: i32,
    pub progress_percentage: f64,
    pub source_version: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Stats {
    pub source_version: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GamePass {
    pub is_game_pass: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TitleHistory {
    pub last_time_played: Option<String>,
    pub visible: bool,
    pub can_hide: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XboxTitle {
    pub title_id: Option<String>,
    pub pfn: Option<String>,
    pub bing_id: Option<String>,
    pub windows_phone_product_id: Option<String>,
    pub name: String,
    pub r#type: Option<String>,
    pub devices: Vec<String>,
    pub display_image: Option<String>,
    pub media_item_type: Option<String>,
    pub modern_title_id: Option<String>,
    pub is_bundle: bool,
    pub achievement: Option<BasicAchievementDetails>,
    pub stats: Option<Stats>,
    pub game_pass: Option<GamePass>,
    pub xbox_live_tier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GameTitle {
    pub xuid: Option<String>,
    pub titles: Vec<XboxTitle>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Title {
    pub title_id: Option<String>,
    pub pfn: Option<String>,
    pub bing_id: Option<String>,
    pub service_config_id: Option<String>,
    pub windows_phone_product_id: Option<String>,
    pub name: Option<String>,
    pub r#type: Option<String>,
    pub devices: Vec<String>,
    pub display_image: Option<String>,
    pub media_item_type: Option<String>,
    pub modern_title_id: Option<String>,
    pub is_bundle: bool,
    pub achievement: Option<BasicAchievementDetails>,
    pub title_history: Option<TitleHistory>,
    pub detail: Option<Detail>,
    pub xbox_live_tier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TitlesList {
    pub xuid: Option<String>,
    pub titles: Vec<Title>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Image {
    #[serde(rename = "URI")]
    pub uri: Option<String>,
    pub height: i32,
    pub width: i32,
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Product {
    #[serde(rename = "pcPlatformPreinstallable")]
    pub pc_platform_preinstallable: bool,
    pub product_title: Option<String>,
    pub product_description: Option<String>,
    pub product_description_short: Option<String>,
    pub package_family_name: Option<String>,
    pub product_type: Option<String>,
    pub xbox_title_id: Option<String>,
    pub minimum_user_age: i32,
    pub children: Vec<Value>,
    pub categories: Vec<String>,
    pub image_box_art: Option<Image>,
    pub image_hero: Option<Image>,
    pub image_poster: Option<Image>,
    #[serde(rename = "isEAPlay")]
    pub is_ea_play: bool,
    pub store_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GamePassProducts {
    pub products: std::collections::BTreeMap<String, Product>,
    pub invalid_ids: Vec<Value>,
}

// ----- request bodies -----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameTitleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pfns: Option<String>,
    pub title_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GamepassProductsRequest {
    pub products: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameStat {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_id: Option<String>,
}

impl Default for GameStat {
    fn default() -> Self {
        Self {
            name: "MinutesPlayed".into(),
            title_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameStatsRequest {
    pub arrange_by_field: String,
    pub xuids: Vec<String>,
    pub stats: Vec<GameStat>,
}

impl Default for GameStatsRequest {
    fn default() -> Self {
        Self {
            arrange_by_field: "xuid".into(),
            xuids: Vec::new(),
            stats: Vec::new(),
        }
    }
}

// The userstats batch response uses lowercase `titleid`, mixed `type`, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatEntry {
    pub group_properties: std::collections::BTreeMap<String, Value>,
    pub xuid: Option<String>,
    pub scid: Option<String>,
    #[serde(rename = "titleid", alias = "titleId")]
    pub title_id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub value: Option<String>,
    pub properties: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatListCollection {
    pub arrange_by_field: Option<String>,
    pub arrange_by_field_id: Option<String>,
    pub stats: Vec<StatEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GameStatsResponse {
    pub groups: Vec<Value>,
    pub stat_lists_collection: Vec<StatListCollection>,
}
