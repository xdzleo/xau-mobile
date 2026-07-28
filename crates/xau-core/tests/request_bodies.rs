//! Verifies that JSON bodies sent to Xbox Live match the original C# shape.

use xau_core::models::{
    AchievementsArrayEntry, GameStatsRequest, GameTitleRequest, GamepassProductsRequest,
    HeartbeatRequest, UnlockTitleBasedAchievementRequest,
};
use xau_core::xbox::constants::ZERO_UID;

#[test]
fn unlock_request_shape_is_byte_for_byte_compatible() {
    let req = UnlockTitleBasedAchievementRequest::new(
        ZERO_UID,
        "1234567890",
        "2533274800000001",
        vec![AchievementsArrayEntry::completed("1")],
    );
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["action"], "progressUpdate");
    assert_eq!(json["serviceConfigId"], ZERO_UID);
    assert_eq!(json["titleId"], "1234567890");
    assert_eq!(json["userId"], "2533274800000001");
    assert_eq!(json["achievements"][0]["id"], "1");
    assert_eq!(json["achievements"][0]["percentComplete"], "100");
}

#[test]
fn heartbeat_request_matches_csharp_defaults() {
    let req = HeartbeatRequest::for_title("1234567890");
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["titles"][0]["id"], "1234567890");
    assert_eq!(json["titles"][0]["state"], "active");
    assert_eq!(json["titles"][0]["sandbox"], "RETAIL");
    assert_eq!(json["titles"][0]["expiration"], 600);
}

#[test]
fn heartbeat_bulk_serializes_every_title() {
    let ids = [
        "1111111111".to_string(),
        "2222222222".to_string(),
        "3333333333".to_string(),
        "4444444444".to_string(),
        "5555555555".to_string(),
        "6666666666".to_string(),
        "7777777777".to_string(),
        "8888888888".to_string(),
    ];
    let req = HeartbeatRequest::for_titles(ids.iter().cloned());
    let json = serde_json::to_value(&req).unwrap();
    let titles = json["titles"].as_array().expect("array");
    assert_eq!(titles.len(), 8);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(titles[i]["id"], *id);
        assert_eq!(titles[i]["state"], "active");
        assert_eq!(titles[i]["sandbox"], "RETAIL");
        assert_eq!(titles[i]["expiration"], 600);
    }
}

#[test]
fn game_title_request_serializes_pascal_case() {
    let req = GameTitleRequest {
        pfns: None,
        title_ids: vec!["1234567890".into()],
    };
    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("Pfns").is_none());
    assert_eq!(json["TitleIds"][0], "1234567890");
}

#[test]
fn game_stats_request_has_default_fields() {
    let req = GameStatsRequest {
        xuids: vec!["2533".into()],
        stats: vec![xau_core::models::GameStat {
            name: "MinutesPlayed".into(),
            title_id: Some("1234567890".into()),
        }],
        ..Default::default()
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["ArrangeByField"], "xuid");
    assert_eq!(json["Xuids"][0], "2533");
    assert_eq!(json["Stats"][0]["Name"], "MinutesPlayed");
}

#[test]
fn gamepass_products_request_shape() {
    let req = GamepassProductsRequest {
        products: vec!["ABCDEF12345".into()],
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["Products"][0], "ABCDEF12345");
}
