//! URL-builder parity tests. These catch accidental divergence from the C#
//! constants file — every URL that ships to Xbox Live must stay byte-identical.

use xau_core::xbox::constants::{
    gamepass_membership_url, gamertag_search_url, heartbeat_url, profile_url,
    query_achievements_360_url, query_achievements_url, title_url, titles_url,
    update_achievements_url,
};

#[test]
fn profile_url_matches_csharp() {
    assert_eq!(
        profile_url("2533"),
        "https://peoplehub.xboxlive.com/users/me/people/xuids(2533)/decoration/detail,preferredColor,presenceDetail,multiplayerSummary"
    );
}

#[test]
fn title_url_matches_csharp() {
    assert_eq!(
        title_url("2533"),
        "https://titlehub.xboxlive.com/users/xuid(2533)/titles/batch/decoration/GamePass,Achievement,Stats"
    );
}

#[test]
fn titles_url_matches_csharp() {
    assert_eq!(
        titles_url("2533"),
        "https://titlehub.xboxlive.com/users/xuid(2533)/titles/titleHistory/decoration/Achievement,detail,scid?maxItems=10000"
    );
}

#[test]
fn gamepass_membership_url_matches_csharp() {
    assert_eq!(
        gamepass_membership_url("2533"),
        "https://xgrant.xboxlive.com/users/xuid(2533)/programInfo?filter=profile,activities,catalog"
    );
}

#[test]
fn gamertag_search_url_matches_csharp() {
    assert_eq!(
        gamertag_search_url("MyGT"),
        "https://profile.xboxlive.com/users/gt(MyGT)/profile/settings?settings=GameDisplayPicRaw,Gamerscore,Gamertag"
    );
}

#[test]
fn achievements_urls_match_csharp() {
    assert_eq!(
        query_achievements_url("2533", "111"),
        "https://achievements.xboxlive.com/users/xuid(2533)/achievements?titleId=111&maxItems=1000"
    );
    assert_eq!(
        query_achievements_360_url("2533", "111"),
        "https://achievements.xboxlive.com/users/xuid(2533)/titleachievements?titleId=111&maxItems=1000"
    );
    assert_eq!(
        update_achievements_url("2533", "scid1"),
        "https://achievements.xboxlive.com/users/xuid(2533)/achievements/scid1/update"
    );
}

#[test]
fn heartbeat_url_matches_csharp() {
    assert_eq!(
        heartbeat_url("2533"),
        "https://presence-heartbeat.xboxlive.com/users/xuid(2533)/devices/current/"
    );
}
