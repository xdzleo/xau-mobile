//! End-to-end fixture tests — feeds REAL captured Xbox Live API responses
//! through every model the GUI consumes, asserting that *every field the UI
//! reads* is populated. Catches silent deserialization bugs that pass
//! `cargo test` but cause blank cards in the UI.
//!
//! The JSON files in `tests/fixtures/` are anonymised captures of real
//! Xbox Live responses: the identifiers in them are synthetic, and the
//! response shape is the ground truth for what Xbox Live actually sends.

use std::path::{Path, PathBuf};

use xau_core::models::{AchievementsResponse, BasicProfile, Gamepass, Profile, TitlesList};

fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is the crate root (crates/xau-core). Fixtures live
    // two levels up under the workspace.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate inside workspace")
        .join("tests/fixtures")
}

fn load(name: &str) -> String {
    let path = fixtures_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn basic_profile_parses_live_response() {
    let raw = load("basic_profile.json");
    let parsed: BasicProfile = serde_json::from_str(&raw).expect("parse basic_profile");
    let u = parsed
        .profile_users
        .first()
        .expect("at least one profile user");
    assert_eq!(u.id.as_deref(), Some("2500000000000001"));
    assert_eq!(u.host_id.as_deref(), Some("2500000000000001"));
    let gt = u
        .settings
        .iter()
        .find(|s| s.id.as_deref() == Some("Gamertag"))
        .and_then(|s| s.value.as_deref());
    assert_eq!(gt, Some("FixtureUser001"));
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn profile_full_parses_and_populates_every_field_the_ui_reads() {
    let raw = load("profile_full.json");
    let parsed: Profile = serde_json::from_str(&raw).expect("parse profile_full");
    let person = parsed.people.first().expect("at least one person");

    // Fields read by the Home page profile card:
    assert_eq!(person.gamertag.as_deref(), Some("FixtureUser001"));
    assert_eq!(person.xuid.as_deref(), Some("2500000000000001"));
    assert_eq!(person.gamer_score.as_deref(), Some("0"));
    assert_eq!(person.xbox_one_rep.as_deref(), Some("GoodPlayer"));
    assert!(
        person.display_pic_raw.is_some(),
        "displayPicRaw should populate (avatar image)"
    );
    assert!(person
        .display_pic_raw
        .as_deref()
        .unwrap()
        .starts_with("https://"));

    let detail = person.detail.as_ref().expect("detail present");
    assert_eq!(detail.account_tier.as_deref(), Some("Silver"));
    assert_eq!(detail.tenure.as_deref(), Some("0"));
    assert!(!detail.is_verified);
    assert_eq!(detail.follower_count, 0);
    assert_eq!(detail.following_count, 0);
    assert!(!detail.has_game_pass);

    // PresenceDetails: captured fixture shows PascalCase keys
    // (IsBroadcasting, Device, TitleId) — NOT camelCase. If this ever
    // becomes an empty default, the UI's "Currently Playing" row goes blank.
    let presence = person
        .presence_details
        .first()
        .expect("at least one presence detail");
    assert_eq!(
        presence.device.as_deref(),
        Some("WindowsOneCore"),
        "PresenceDetail.device must populate — Xbox returns PascalCase keys here"
    );
    assert_eq!(presence.title_id.as_deref(), Some("704208617"));
    assert_eq!(presence.state.as_deref(), Some("Active"));
    assert!(presence.is_primary);
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn gamepass_parses_live_response() {
    let raw = load("gamepass.json");
    let parsed: Gamepass = serde_json::from_str(&raw).expect("parse gamepass");
    // Xbox returns `gamePassMembership` (capital P). If the Rust field name
    // doesn't map to exactly that, the stat card shows a dash.
    assert_eq!(
        parsed.gamepass_membership.as_deref(),
        Some("None"),
        "gamePassMembership must populate — account in fixture is on 'None' tier"
    );
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn achievements_list_parses_live_response() {
    let raw = load("achievements_one.json");
    let parsed: AchievementsResponse = serde_json::from_str(&raw).expect("parse achievements");
    assert!(!parsed.achievements.is_empty());
    let a = &parsed.achievements[0];
    assert_eq!(a.id, "109");
    assert_eq!(a.service_config_id, "77290100-225e-4768-9373-98164430a9f8");
    assert_eq!(a.name, "Life Story");
    assert_eq!(a.progress_state, "NotStarted");

    // titleAssociations[].id is a NUMBER in the live response, not a string.
    let assoc = a.title_associations.first().expect("associations");
    assert!(
        assoc.id.is_some(),
        "titleAssociation.id must populate (Xbox returns it as a number)"
    );

    // rarity.currentPercentage is a NUMBER (0.26), not a string.
    let rarity = a.rarity.as_ref().expect("rarity present");
    assert!(
        rarity.current_percentage.is_some(),
        "rarity.currentPercentage must populate (Xbox returns it as a number)"
    );

    // rewards[].value is a string ("30"). Keep that contract.
    let gs_reward = a
        .rewards
        .iter()
        .find(|r| r.ty.as_deref() == Some("Gamerscore"));
    assert_eq!(gs_reward.and_then(|r| r.value.as_deref()), Some("30"));

    // isRevoked is a BOOLEAN in current API. The original C# expected a
    // string so we keep the contract flexible.
    // (No hard assertion — just verify parse succeeded for a real entry.)
    let _ = a.is_revoked.as_deref();

    // mediaAssets should have at least one Icon
    let icon = a
        .media_assets
        .iter()
        .find(|m| m.ty.as_deref() == Some("Icon"));
    assert!(
        icon.and_then(|m| m.url.as_deref()).is_some(),
        "Icon mediaAsset must have a URL"
    );
}

#[test]
#[ignore = "needs tests/fixtures/*.json, which this fork does not carry"]
fn titles_list_parses_live_response() {
    let raw = load("titles_list.json");
    let parsed: TitlesList = serde_json::from_str(&raw).expect("parse titles_list");
    // Fixture account has empty library — this test mostly guards against
    // schema drift (e.g. Microsoft adding a field we can't deserialize).
    assert_eq!(parsed.xuid.as_deref(), Some("2500000000000001"));
    assert_eq!(parsed.titles.len(), 0);
}
