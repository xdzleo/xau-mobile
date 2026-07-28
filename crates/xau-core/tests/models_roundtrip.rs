//! Wire-compat tests: verifies that sample JSON payloads captured from the
//! live Xbox Live endpoints (and matching what the C# project parses with
//! `Newtonsoft.Json`) round-trip cleanly through our `serde` types.
//!
//! The payloads are intentionally small but cover every field the rest of the
//! codebase actually reads.

use xau_core::models::{
    Achievement, AchievementsResponse, BasicProfile, GamePassProducts, GameStatsResponse,
    GameTitle, Profile, SlimAchievement, SlimAchievementsResponse, TitlesList,
    Xbox360AchievementResponse,
};
use xau_core::xbox::constants::ZERO_UID;

fn roundtrip<T>(raw: &str) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value: T = serde_json::from_str(raw).expect("deserialize");
    let s = serde_json::to_string(&value).expect("serialize");
    let _reparsed: T = serde_json::from_str(&s).expect("re-deserialize");
    value
}

#[test]
fn basic_profile_shape_matches_live_response() {
    let raw = r#"{
        "profileUsers": [{
            "id": "2533274800000001",
            "hostId": "2533274800000001",
            "settings": [{"id": "Gamertag", "value": "Cirno9Ball"}],
            "isSponsoredUser": "false"
        }]
    }"#;
    let v: BasicProfile = roundtrip(raw);
    assert_eq!(v.profile_users[0].id.as_deref(), Some("2533274800000001"));
    assert_eq!(
        v.profile_users[0].settings[0].value.as_deref(),
        Some("Cirno9Ball")
    );
}

#[test]
fn profile_decoration_parses() {
    // NOTE: peoplehub returns the *outer* Person keys as camelCase but the
    // nested `presenceDetails[]` objects use PascalCase — we verified this
    // against a live capture (see `tests/fixtures/profile_full.json`). The
    // PresenceDetail struct therefore has `rename_all = "PascalCase"`.
    let raw = r#"{
        "people": [{
            "xuid": "2533274800000001",
            "gamertag": "Cirno9Ball",
            "gamerScore": "123456",
            "xboxOneRep": "GoodPlayer",
            "isBroadcasting": false,
            "presenceDetails": [
                {"IsBroadcasting": false, "Device": "WindowsOneCore", "State": "Active", "TitleId": "1234567890", "IsPrimary": true, "IsGame": true}
            ],
            "detail": {
                "accountTier": "Gold", "isVerified": false, "tenure": "5",
                "watermarks": ["Launch2013"], "followerCount": 10, "followingCount": 20, "hasGamePass": true
            },
            "linkedAccounts": []
        }],
        "recommendationSummary": null
    }"#;
    let v: Profile = roundtrip(raw);
    let p = &v.people[0];
    assert_eq!(p.gamertag.as_deref(), Some("Cirno9Ball"));
    let d = p.detail.as_ref().unwrap();
    assert_eq!(d.tenure.as_deref(), Some("5"));
    assert_eq!(d.follower_count, 10);
    assert!(d.has_game_pass);
    assert_eq!(
        p.presence_details[0].title_id.as_deref(),
        Some("1234567890")
    );
    assert_eq!(
        p.presence_details[0].device.as_deref(),
        Some("WindowsOneCore")
    );
}

#[test]
fn achievements_response_parses() {
    let raw = r#"{
        "achievements": [{
            "id": "1",
            "serviceConfigId": "00000000-0000-0000-0000-6e2fa5b1c3d4",
            "name": "First blood",
            "progressState": "Achieved",
            "isSecret": false,
            "description": "Do the thing",
            "lockedDescription": "Pending",
            "platforms": ["Durango"],
            "mediaAssets": [{"name": "icon", "type": "Icon", "url": "https://example/icon.png"}],
            "titleAssociations": [{"id": "1234567890", "name": "TestGame"}],
            "rewards": [{"name": "", "value": "10", "type": "Gamerscore", "valueType": "Int"}],
            "progression": {"requirements": [], "timeUnlocked": "2024-04-01T00:00:00Z"}
        }]
    }"#;
    let v: AchievementsResponse = roundtrip(raw);
    let a: &Achievement = &v.achievements[0];
    assert_eq!(a.id, "1");
    assert_eq!(a.name, "First blood");
    assert_eq!(a.progress_state, "Achieved");
    assert!(a
        .progression
        .as_ref()
        .unwrap()
        .time_unlocked
        .as_deref()
        .unwrap_or("")
        .starts_with("2024"));
}

#[test]
fn achievement_defaults_use_zero_uid_for_missing_scid() {
    let raw = r#"{ "id": "1", "name": "foo", "isSecret": false }"#;
    let a: Achievement = serde_json::from_str(raw).unwrap();
    assert_eq!(a.service_config_id, ZERO_UID);
    assert_eq!(a.progress_state, "Null");
}

/// The slim bulk-scan model must read the SAME fields `build_plan`
/// uses — if Slim starts diverging from Achievement on those keys,
/// the parallel scan will silently produce wrong plan entries.
/// This test locks the contract.
#[test]
fn slim_achievement_covers_fields_plan_builder_reads() {
    let raw = r#"{
        "id": "ach_xyz",
        "serviceConfigId": "00000000-0000-0000-0000-6e2fa5b1c3d4",
        "name": "First blood",
        "progressState": "Achieved",
        "isSecret": false,
        "description": "Ignored by slim",
        "platforms": ["Durango"],
        "mediaAssets": [{"name": "icon", "type": "Icon", "url": "https://example/icon.png"}],
        "titleAssociations": [{"id": "1234567890", "name": "TestGame"}],
        "rewards": [
            {"name": "", "value": "10", "type": "Gamerscore", "valueType": "Int"},
            {"name": "", "value": "X", "type": "InAppFeature", "valueType": "Str"}
        ],
        "progression": {"requirements": [], "timeUnlocked": "2024-04-01T00:00:00Z"}
    }"#;
    let a: SlimAchievement = serde_json::from_str(raw).expect("slim deserialize");
    assert_eq!(a.id, "ach_xyz");
    assert_eq!(a.name, "First blood");
    assert_eq!(a.progress_state, "Achieved");
    assert_eq!(a.service_config_id, "00000000-0000-0000-0000-6e2fa5b1c3d4");
    assert_eq!(a.title_associations.len(), 1);
    assert_eq!(a.title_associations[0].id.as_deref(), Some("1234567890"));
    assert_eq!(a.title_associations[0].name.as_deref(), Some("TestGame"));
    let progression = a.progression.as_ref().expect("slim progression");
    assert_eq!(
        progression.time_unlocked.as_deref(),
        Some("2024-04-01T00:00:00Z")
    );
    // Gamerscore extraction invariant: builder iterates rewards and
    // picks the one with type=="Gamerscore". Slim must preserve that
    // reward alongside the others in source order.
    let gs = a
        .rewards
        .iter()
        .find(|r| r.ty.as_deref() == Some("Gamerscore"))
        .and_then(|r| r.value.as_deref())
        .and_then(|v| v.parse::<i32>().ok());
    assert_eq!(gs, Some(10));
}

/// Regression guard: SlimAchievementsResponse must parse a real-shaped
/// paginated response (with paging_info.continuation_token +
/// total_records) so the bulk scanner's page-planning math stays
/// compatible when we start pointing it at slim parsing.
#[test]
fn slim_achievements_response_parses_paging_info() {
    let raw = r#"{
        "achievements": [
            {"id": "a1", "name": "n1", "progressState": "Achieved",
             "titleAssociations": [{"id": "1", "name": "G1"}],
             "rewards": [], "progression": null},
            {"id": "a2", "name": "n2", "progressState": "Achieved",
             "titleAssociations": [{"id": "2", "name": "G2"}],
             "rewards": [], "progression": null}
        ],
        "pagingInfo": {"continuationToken": "2000", "totalRecords": 42000}
    }"#;
    let v: SlimAchievementsResponse = serde_json::from_str(raw).expect("slim resp");
    assert_eq!(v.achievements.len(), 2);
    assert_eq!(v.achievements[0].id, "a1");
    let paging = v.paging_info.as_ref().expect("paging");
    assert_eq!(paging.continuation_token.as_deref(), Some("2000"));
    assert_eq!(paging.total_records, Some(42_000));
}

/// Slim parse must be byte-for-byte compatible with the full Achievement
/// parse on the fields build_plan reads. If the Xbox server ever adds a
/// field, this test wouldn't notice — but it guards against accidental
/// divergence between our two models on KNOWN fields.
#[test]
fn slim_and_full_parse_agree_on_plan_builder_fields() {
    let raw = r#"{
        "id": "ach_1",
        "serviceConfigId": "scid_x",
        "name": "Beat the boss",
        "progressState": "Achieved",
        "isSecret": false,
        "titleAssociations": [{"id": "42", "name": "MyGame"}],
        "rewards": [{"value": "25", "type": "Gamerscore"}],
        "progression": {"timeUnlocked": "2025-06-01T12:34:56Z"}
    }"#;
    let full: Achievement = serde_json::from_str(raw).expect("full");
    let slim: SlimAchievement = serde_json::from_str(raw).expect("slim");
    assert_eq!(full.id, slim.id);
    assert_eq!(full.name, slim.name);
    assert_eq!(full.progress_state, slim.progress_state);
    assert_eq!(full.service_config_id, slim.service_config_id);
    let fta = &full.title_associations[0];
    let sta = &slim.title_associations[0];
    assert_eq!(fta.id, sta.id);
    assert_eq!(fta.name, sta.name);
    assert_eq!(
        full.progression
            .as_ref()
            .and_then(|p| p.time_unlocked.as_deref()),
        slim.progression
            .as_ref()
            .and_then(|p| p.time_unlocked.as_deref())
    );
    let full_gs = full
        .rewards
        .iter()
        .find(|r| r.ty.as_deref() == Some("Gamerscore"))
        .and_then(|r| r.value.as_deref());
    let slim_gs = slim
        .rewards
        .iter()
        .find(|r| r.ty.as_deref() == Some("Gamerscore"))
        .and_then(|r| r.value.as_deref());
    assert_eq!(full_gs, slim_gs);
}

#[test]
fn xbox360_achievements_parse() {
    let raw = r#"{
        "achievements": [{
            "id": 1, "titleId": 2147483648, "name": "Oldschool",
            "gamerscore": 25, "description": "Do", "lockedDescription": "Do again",
            "timeUnlocked": "2010-01-01T00:00:00Z", "isSecret": false
        }]
    }"#;
    let v: Xbox360AchievementResponse = roundtrip(raw);
    assert_eq!(v.achievements[0].title_id, 2_147_483_648);
}

#[test]
fn game_title_parses() {
    let raw = r#"{
        "xuid": "2533274800000001",
        "titles": [{
            "titleId": "1234567890",
            "name": "SampleGame",
            "type": "Game",
            "devices": ["PC", "Xbox One"],
            "displayImage": "https://example/img.png",
            "isBundle": false,
            "achievement": {
                "currentAchievements": 5,
                "totalAchievements": 20,
                "currentGamerscore": 100,
                "totalGamerscore": 1000,
                "progressPercentage": 25.0,
                "sourceVersion": 2
            }
        }]
    }"#;
    let v: GameTitle = roundtrip(raw);
    let t = &v.titles[0];
    assert_eq!(t.name, "SampleGame");
    let a = t.achievement.as_ref().unwrap();
    assert_eq!(a.current_gamerscore, 100);
}

#[test]
fn titles_list_parses() {
    let raw = r#"{
        "xuid": "2533274800000001",
        "titles": [{
            "titleId": "1234567890",
            "serviceConfigId": "00000000-0000-0000-0000-6e2fa5b1c3d4",
            "name": "SampleGame",
            "type": "Game",
            "devices": [],
            "titleHistory": {"lastTimePlayed": "2024-01-01T00:00:00Z", "visible": true, "canHide": true},
            "achievement": {
                "currentAchievements": 1, "totalAchievements": 10,
                "currentGamerscore": 25, "totalGamerscore": 250,
                "progressPercentage": 10.0, "sourceVersion": 2
            }
        }]
    }"#;
    let v: TitlesList = roundtrip(raw);
    assert_eq!(
        v.titles[0].service_config_id.as_deref(),
        Some("00000000-0000-0000-0000-6e2fa5b1c3d4")
    );
}

#[test]
fn gamepass_products_parse() {
    let raw = r#"{
        "products": {
            "ABCDEF12345": {
                "pcPlatformPreinstallable": true,
                "productTitle": "Some Game",
                "productDescription": "desc",
                "productDescriptionShort": "short",
                "packageFamilyName": "pkg",
                "productType": "Game",
                "xboxTitleId": "1234567890",
                "minimumUserAge": 0,
                "children": [],
                "categories": ["Action"],
                "isEAPlay": false,
                "storeId": "ABCDEF12345"
            }
        },
        "invalidIds": []
    }"#;
    let v: GamePassProducts = roundtrip(raw);
    assert_eq!(
        v.products["ABCDEF12345"].xbox_title_id.as_deref(),
        Some("1234567890")
    );
}

#[test]
fn game_stats_response_parses() {
    let raw = r#"{
        "groups": [],
        "statListsCollection": [{
            "arrangeByField": "xuid",
            "arrangeByFieldId": "2533274800000001",
            "stats": [{
                "xuid": "2533274800000001",
                "scid": "00000000-0000-0000-0000-6e2fa5b1c3d4",
                "titleid": "1234567890",
                "name": "MinutesPlayed",
                "type": "Integer",
                "value": "42"
            }]
        }]
    }"#;
    let v: GameStatsResponse = roundtrip(raw);
    assert_eq!(
        v.stat_lists_collection[0].stats[0].value.as_deref(),
        Some("42")
    );
}
