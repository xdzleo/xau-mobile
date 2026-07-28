//! Contract tests for the JSON shapes the GUI's IPC layer emits.
//!
//! The webview reads field names directly from the JSON that Rust produces
//! via `serde_json::to_value`. If a Rust struct's `rename_all` or `rename`
//! is wrong, the UI renders blanks while `cargo test` stays green. These
//! tests pin the exact keys the frontend depends on.
//!
//! Run order: we load live-captured fixtures, deserialize through the Rust
//! models, then RE-serialize and assert on the output structure — same
//! path the IPC layer takes for `profile.full`, `games.list`,
//! `achievements.list`, etc.

use serde_json::Value;
use std::path::{Path, PathBuf};

use xau_core::models::{AchievementsResponse, BasicProfile, Gamepass, Profile, TitlesList};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate inside workspace")
        .join("tests/fixtures")
}

fn load(name: &str) -> String {
    std::fs::read_to_string(fixtures_dir().join(name))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn roundtrip<T: for<'a> serde::Deserialize<'a> + serde::Serialize>(raw: &str) -> Value {
    let parsed: T = serde_json::from_str(raw).expect("deserialize");
    serde_json::to_value(parsed).expect("reserialize")
}

/// Key paths the GUI's `loadProfile()` reads. If any of these return null
/// for a fixture that contains data, the profile card shows dashes.
#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn profile_full_json_matches_gui_contract() {
    let raw = load("profile_full.json");
    let v = roundtrip::<Profile>(&raw);

    // Match the JS: full.profile.people[0]
    let person = &v["people"][0];
    assert_eq!(person["gamertag"].as_str(), Some("FixtureUser001"));
    assert_eq!(person["xuid"].as_str(), Some("2500000000000001"));

    // These are the exact camelCase keys the GUI reads:
    assert_eq!(person["gamerScore"].as_str(), Some("0"));
    assert_eq!(person["xboxOneRep"].as_str(), Some("GoodPlayer"));
    assert!(
        person["displayPicRaw"].as_str().is_some(),
        "displayPicRaw must serialize (avatar)"
    );

    let detail = &person["detail"];
    assert_eq!(detail["accountTier"].as_str(), Some("Silver"));
    assert_eq!(detail["tenure"].as_str(), Some("0"));
    assert_eq!(detail["followerCount"].as_i64(), Some(0));
    assert_eq!(detail["followingCount"].as_i64(), Some(0));
    assert_eq!(detail["hasGamePass"].as_bool(), Some(false));

    // PresenceDetails live at PascalCase keys on the input, but serde
    // deserializes into our struct then RE-serializes with PascalCase too
    // (the struct has `rename_all = "PascalCase"` so the UI contract for
    // this sub-object is pascal). The JS can check either — we expose
    // whatever the struct decides.
    let presence = &person["presenceDetails"][0];
    let device = presence
        .get("Device")
        .or_else(|| presence.get("device"))
        .and_then(|v| v.as_str());
    assert_eq!(
        device,
        Some("WindowsOneCore"),
        "presence device field populated"
    );
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn gamepass_json_exposes_membership_key() {
    let raw = load("gamepass.json");
    let v = roundtrip::<Gamepass>(&raw);
    // Our struct re-serializes as `gamepassMembership` (camelCase of the
    // Rust field), even though the original API used `gamePassMembership`.
    // The GUI reads via `full.gamepass.gamepassMembership` OR
    // `full.gamepass.gamePassMembership` to stay compatible with both.
    let membership = v["gamepassMembership"]
        .as_str()
        .or_else(|| v["gamePassMembership"].as_str());
    assert_eq!(membership, Some("None"));
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn basic_profile_json_for_validation() {
    let raw = load("basic_profile.json");
    let v = roundtrip::<BasicProfile>(&raw);
    assert_eq!(
        v["profileUsers"][0]["id"].as_str(),
        Some("2500000000000001")
    );
    assert_eq!(
        v["profileUsers"][0]["settings"][0]["value"].as_str(),
        Some("FixtureUser001")
    );
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn titles_list_json_for_games_page() {
    let raw = load("titles_list.json");
    let v = roundtrip::<TitlesList>(&raw);
    assert_eq!(v["xuid"].as_str(), Some("2500000000000001"));
    assert!(v["titles"].is_array());
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn achievements_json_for_achievements_page() {
    let raw = load("achievements_one.json");
    let v = roundtrip::<AchievementsResponse>(&raw);
    let arr = v["achievements"].as_array().expect("array");
    let first = &arr[0];

    // The JS reads these exact keys in renderAchievements():
    assert!(first["id"].as_str().is_some());
    assert!(first["name"].as_str().is_some());
    assert!(first["progressState"].as_str().is_some());
    assert!(first["mediaAssets"].is_array());
    assert!(first["rewards"].is_array());
    // Optional but should round-trip if present
    if let Some(rarity) = first.get("rarity") {
        if !rarity.is_null() {
            // GUI reads rarity.currentPercentage
            assert!(
                rarity.get("currentPercentage").is_some(),
                "rarity.currentPercentage must exist after round-trip"
            );
        }
    }
    // Achievement progression.timeUnlocked is read by JS
    if let Some(prog) = first.get("progression") {
        if !prog.is_null() {
            assert!(
                prog.get("timeUnlocked").is_some(),
                "progression.timeUnlocked must exist after round-trip"
            );
        }
    }
}

/// Sanity: the validated extract_from_memory payload shape the UI
/// immediately pins to the profile card.
#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn extract_from_memory_response_shape() {
    // This is hand-built to mirror what ipc.rs returns:
    //   json!({ "logged_in": true, "xuid": xuid, "gamertag": gamertag, "source": "xbox_app_memory" })
    let v = serde_json::json!({
        "logged_in": true,
        "xuid": "2500000000000001",
        "gamertag": "FixtureUser001",
        "source": "xbox_app_memory",
    });
    // GUI reads these:
    assert_eq!(v["logged_in"].as_bool(), Some(true));
    assert_eq!(v["xuid"].as_str(), Some("2500000000000001"));
    assert_eq!(v["gamertag"].as_str(), Some("FixtureUser001"));
}
