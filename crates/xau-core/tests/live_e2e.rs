//! End-to-end integration test battery — requires:
//!   1. Xbox PC app running + signed in on this machine
//!   2. Network access to `*.xboxlive.com`
//!
//! Skipped by default. Run with:
//!
//!     cargo test -p xau-core --test live_e2e -- --ignored --nocapture
//!
//! Each feature the GUI exposes gets its own live test:
//!   - full_pipeline: memory → validate → get_profile (the Home page flow)
//!   - gamepass_membership: profile card Game Pass stat
//!   - games_list: the Games page
//!   - spoof_heartbeat: Misc / Farm pages (single-title presence)
//!   - spoof_heartbeat_bulk: Farm-mode multi-title heartbeat
//!   - stop_heartbeat: the spoof Stop button
//!   - achievements_read: the Achievements page (READ-ONLY — never unlocks)
//!   - dbox_search: Search page
//!   - github_release: Info page / update check

use xau_core::dbox::DBoxClient;
use xau_core::github::GithubClient;
use xau_core::memory;
use xau_core::xbox::client::XboxClientOptions;
use xau_core::xbox::{validate_candidates, XboxClient};

async fn live_client() -> (XboxClient, String) {
    let candidates = memory::extract_all_candidates(true).expect("memory scan");
    let tokens: Vec<String> = candidates.into_iter().map(|c| c.token).collect();
    let (token, profile) = validate_candidates(tokens, 0)
        .await
        .expect("validate")
        .expect("live token");
    let client = XboxClient::new(XboxClientOptions {
        xauth: token,
        ..Default::default()
    })
    .expect("client");
    let xuid = profile.xuid.expect("xuid");
    (client, xuid)
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn full_pipeline_from_memory_to_populated_profile() {
    eprintln!("[1/4] scanning memory for XAUTH candidates…");
    let candidates = memory::extract_all_candidates(true)
        .expect("memory scan works on Windows when Xbox app is running");
    eprintln!("      found {} unique candidate(s)", candidates.len());
    assert!(
        !candidates.is_empty(),
        "no XAUTH pattern in any process — is the Xbox PC app running and signed in?"
    );

    eprintln!("[2/4] validating candidates against Xbox Live (parallel)…");
    let tokens: Vec<String> = candidates.into_iter().map(|c| c.token).collect();
    let (token, profile) = validate_candidates(tokens, 0)
        .await
        .expect("validate call succeeds")
        .expect("at least one candidate accepted by Xbox Live");
    eprintln!(
        "      accepted by Xbox Live — gamertag={:?} xuid={:?}",
        profile.gamertag, profile.xuid
    );
    assert!(profile.xuid.is_some(), "validated profile has xuid");
    assert!(profile.gamertag.is_some(), "validated profile has gamertag");

    eprintln!("[3/4] building XboxClient + fetching full peoplehub profile…");
    let client = XboxClient::new(XboxClientOptions {
        xauth: token,
        ..Default::default()
    })
    .expect("XboxClient build");
    let xuid = profile.xuid.unwrap();
    let full = client.get_profile(&xuid).await.expect("get_profile 200 OK");
    eprintln!("      peoplehub returned {} person(s)", full.people.len());
    assert!(
        !full.people.is_empty(),
        "peoplehub response has at least one Person"
    );
    let person = &full.people[0];

    eprintln!("[4/4] asserting every stat the UI reads is populated…");
    // These are the fields the GUI's loadProfile() consumes.
    assert!(person.gamertag.is_some(), "gamertag");
    assert!(person.xuid.is_some(), "xuid");
    assert!(
        person.gamer_score.is_some(),
        "gamerScore populated (UI shows '—' if missing)"
    );
    assert!(person.xbox_one_rep.is_some(), "xboxOneRep populated");
    assert!(
        person.display_pic_raw.is_some(),
        "displayPicRaw populated (profile avatar)"
    );
    let detail = person.detail.as_ref().expect("detail object present");
    assert!(detail.account_tier.is_some(), "accountTier populated");
    assert!(detail.tenure.is_some(), "tenure populated");
    // follower_count / following_count are plain i32 — always populated.
    eprintln!(
        "      OK · gt={:?} gs={:?} rep={:?} tier={:?} followers={} tenure={:?}",
        person.gamertag.as_deref(),
        person.gamer_score.as_deref(),
        person.xbox_one_rep.as_deref(),
        detail.account_tier.as_deref(),
        detail.follower_count,
        detail.tenure.as_deref(),
    );
    eprintln!("\n✓ Full pipeline works end-to-end.");
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn gamepass_membership_round_trip() {
    let (client, xuid) = live_client().await;
    eprintln!("fetching gamepass membership for {xuid}…");
    let gp = client
        .get_gamepass_membership(&xuid)
        .await
        .expect("gamepass 200 OK");
    eprintln!(
        "      membership={:?}  data.membership={:?}",
        gp.gamepass_membership,
        gp.data
            .as_ref()
            .and_then(|d| d.gamepass_membership.as_deref())
    );
    // At least one of the two paths should be populated. The fixture capture
    // above showed "None" (a valid string value) in the top-level field.
    assert!(
        gp.gamepass_membership.is_some()
            || gp
                .data
                .as_ref()
                .and_then(|d| d.gamepass_membership.as_deref())
                .is_some(),
        "gamepassMembership must round-trip from the live response"
    );
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn games_list_round_trip() {
    let (client, xuid) = live_client().await;
    eprintln!("fetching games list for {xuid}…");
    let list = client.get_games_list(&xuid).await.expect("titlehub 200 OK");
    eprintln!("      titles={} (first 3 shown below)", list.titles.len());
    for t in list.titles.iter().take(3) {
        eprintln!(
            "        titleId={:?}  name={:?}  gs={:?}/{:?}",
            t.title_id,
            t.name,
            t.achievement.as_ref().map(|a| a.current_gamerscore),
            t.achievement.as_ref().map(|a| a.total_gamerscore)
        );
    }
    // The response is allowed to be empty for a fresh account, but the
    // xuid field should always round-trip.
    assert_eq!(list.xuid.as_deref(), Some(xuid.as_str()));
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn spoof_heartbeat_single_title() {
    let (client, xuid) = live_client().await;
    eprintln!("sending single-title heartbeat…");
    // 1144039928 = Halo: The Master Chief Collection. Public title IDs
    // are fine to spoof into temporarily; Xbox clears the presence after
    // ~10 minutes if no follow-up ping is sent.
    client
        .send_heartbeat(&xuid, "1144039928")
        .await
        .expect("heartbeat 200 OK");
    eprintln!("      heartbeat accepted");
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn spoof_heartbeat_bulk_eight_titles() {
    let (client, xuid) = live_client().await;
    eprintln!("sending bulk heartbeat for 8 titles (farm mode)…");
    let titles = vec![
        "1144039928".to_string(), // Halo MCC
        "1836984154".to_string(), // Forza Horizon 5
        "1144039928".to_string(), // (duplicate ok, Xbox dedups server-side)
        "1979501316".to_string(),
        "1220600561".to_string(),
        "980033725".to_string(),
        "1293331011".to_string(),
        "1710749065".to_string(),
    ];
    let sent = client
        .send_heartbeat_bulk(&xuid, &titles)
        .await
        .expect("bulk heartbeat 200 OK");
    eprintln!("      sent {sent} title(s) — matches request count");
    assert_eq!(sent, titles.len());
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn spoof_stop_heartbeat() {
    let (client, xuid) = live_client().await;
    eprintln!("stopping heartbeat for {xuid}…");
    client.stop_heartbeat(&xuid).await.expect("DELETE 200/204");
    eprintln!("      stopped");
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn achievements_read_public_title() {
    let (client, xuid) = live_client().await;
    // Halo MCC has a big public achievement list — good sanity test.
    eprintln!("reading achievements for Halo MCC (READ-ONLY, never unlocks)…");
    let resp = client
        .get_achievements_for_title(&xuid, "1144039928")
        .await
        .expect("achievements 200 OK");
    eprintln!(
        "      {} achievement(s) deserialized",
        resp.achievements.len()
    );
    if let Some(a) = resp.achievements.first() {
        eprintln!(
            "        id={} name={:?} progress={}",
            a.id, a.name, a.progress_state
        );
        assert!(!a.id.is_empty());
        assert!(!a.name.is_empty());
        assert!(!a.service_config_id.is_empty());
    }
}

#[tokio::test]
#[ignore = "requires live Xbox PC app + network"]
async fn resolve_service_config_id_caches() {
    let (client, xuid) = live_client().await;
    eprintln!("resolve scid for Halo MCC (first call hits Xbox)…");
    let t0 = std::time::Instant::now();
    let scid = client
        .resolve_service_config_id(&xuid, "1144039928")
        .await
        .expect("resolve");
    let t1 = t0.elapsed();
    eprintln!("      first: scid={scid} in {t1:?}");

    let t0 = std::time::Instant::now();
    let scid2 = client
        .resolve_service_config_id(&xuid, "1144039928")
        .await
        .expect("resolve cached");
    let t2 = t0.elapsed();
    eprintln!("      second (cached): scid={scid2} in {t2:?}");
    assert_eq!(scid, scid2);
    // Cached call should be near-instant (< 5ms); first call is a full
    // HTTP round trip. Tolerate network jitter.
    assert!(t2.as_millis() < 50, "cached lookup should be instant");
}

#[tokio::test]
#[ignore = "network only (no xbox memory needed)"]
async fn dbox_search_returns_results() {
    let c = DBoxClient::default();
    let v = c.search("Halo Infinite").await.expect("dbox 200 OK");
    eprintln!(
        "dbox.tools returned {} item(s)",
        v.get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    );
    assert!(
        v.get("items").and_then(|i| i.as_array()).is_some(),
        "response should have an `items` array"
    );
}

#[tokio::test]
#[ignore = "network only"]
async fn github_latest_release_reachable() {
    let c = GithubClient::default();
    let rel = c.get_latest_release().await.expect("github 200 OK");
    eprintln!("latest release: {rel:?}");
    // Response is optional (may be None if repo has no releases).
    // Just verify the HTTP call succeeds.
}
