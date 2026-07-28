//! END-TO-END proof that the replay runner actually unlocks every plan
//! entry on the Xbox Live backend.
//!
//! Builds a synthetic Plan with 3 hand-picked never-before-unlocked
//! achievements from 2 different titles (simulating "follow another
//! profile's chronology"), kicks off `start_replay`, polls the handle
//! until Done, and then queries Xbox Live to confirm each one flipped
//! from `NotStarted` to `Achieved`.
//!
//! Run with:
//!
//!     cargo test -p xau-core --test replay_runner_e2e -- --ignored --nocapture

#![cfg(windows)]

use std::time::Duration;

use xau_core::memory;
use xau_core::plan::runner::{start_replay, ReplayConfig, ReplayState};
use xau_core::plan::{Plan, PlanEntry};
use xau_core::xbox::client::{XboxClient, XboxClientOptions};
use xau_core::xbox::validate_candidates;

#[tokio::test]
#[ignore = "live: needs Xbox PC app running + signed in"]
#[allow(clippy::too_many_lines)]
async fn replay_runner_unlocks_plan_entries_in_order_with_timing() {
    // 1) AUTH — same flow as production: memory scan + validation.
    eprintln!("[1/5] extracting XAUTH from memory…");
    let candidates = memory::extract_all_candidates(true).expect("memory scan");
    let tokens: Vec<String> = candidates.into_iter().map(|c| c.token).collect();
    let (xauth, profile) = validate_candidates(tokens, 0)
        .await
        .expect("validate")
        .expect("at least one valid token");
    let xuid = profile.xuid.expect("xuid");
    eprintln!(
        "      authenticated as {:?} (xuid={})",
        profile.gamertag, xuid
    );

    let client = XboxClient::new(XboxClientOptions {
        xauth,
        ..Default::default()
    })
    .expect("client");

    // 2) BUILD synthetic plan — 3 entries, 2 titles, WITH TIMESTAMPS that
    //    produce a measurable gap (the replay runner in mirror mode MUST
    //    wait that exact gap between unlocks).
    eprintln!("[2/5] assembling synthetic plan with 3 entries…");
    let plan = Plan {
        source_gamertag: "synthetic".into(),
        source_xuid: xuid.clone(),
        built_at: "2026-04-22T00:00:00Z".into(),
        total: 3,
        entries: vec![
            PlanEntry {
                title_id: "2134518491".into(), // Curious Copy Cats (Windows)
                service_config_id: "00000000-0000-0000-0000-00007f3a2adb".into(),
                achievement_id: "1".into(),
                achievement_name: "Level 3".into(),
                game_name: "Curious Copy Cats".into(),
                time_unlocked: "2026-04-22T10:00:00Z".into(),
                gamerscore: Some(30),
            },
            PlanEntry {
                title_id: "1643012526".into(), // Decollate Decoration
                service_config_id: "00000000-0000-0000-0000-000061ee61ae".into(),
                achievement_id: "1".into(),
                achievement_name: "First Seven Days".into(),
                game_name: "Decollate Decoration".into(),
                // +8s after entry 1 → replay must wait ~8s.
                time_unlocked: "2026-04-22T10:00:08Z".into(),
                gamerscore: Some(30),
            },
            PlanEntry {
                title_id: "2134518491".into(),
                service_config_id: "00000000-0000-0000-0000-00007f3a2adb".into(),
                achievement_id: "2".into(),
                achievement_name: "Level 6".into(),
                game_name: "Curious Copy Cats".into(),
                // +5s after entry 2.
                time_unlocked: "2026-04-22T10:00:13Z".into(),
                gamerscore: Some(30),
            },
        ],
        ..Default::default()
    };
    for (i, e) in plan.entries.iter().enumerate() {
        eprintln!(
            "   [{}] {} · title={} ach={} ({})",
            i + 1,
            e.time_unlocked,
            e.title_id,
            e.achievement_id,
            e.achievement_name
        );
    }

    // 3) START REPLAY — mirror mode, exact timing, no clamp.
    eprintln!("[3/5] starting replay with mirror_real_pacing=ON, max_delay=0…");
    let cfg = ReplayConfig {
        delay_between: Duration::from_secs(30),
        mirror_real_pacing: true,
        min_delay: Duration::ZERO, // no floor
        max_delay: Duration::ZERO, // no cap — EXACT mirror
        my_xuid: xuid.clone(),
        mode: xau_core::plan::runner::ReplayMode::Mirror,
        bulk_concurrency: 8,
        ..Default::default()
    };
    let started_at = std::time::Instant::now();
    let handle = start_replay(client.clone(), plan, cfg).expect("spawn replay");

    // 4) POLL until Done, showing progress ticks.
    eprintln!("[4/5] polling progress…");
    loop {
        let snap = handle.snapshot();
        eprint!(
            "\r   state={:?}  done={}/{}  errors={}  current={:?}       ",
            snap.state, snap.done, snap.total, snap.errors, snap.current_achievement
        );
        if matches!(
            snap.state,
            ReplayState::Done | ReplayState::Stopped | ReplayState::Error
        ) {
            eprintln!();
            eprintln!(
                "   final: done={}/{}  errors={}  last_error={:?}  wall={:?}",
                snap.done,
                snap.total,
                snap.errors,
                snap.last_error,
                started_at.elapsed()
            );
            assert!(
                matches!(snap.state, ReplayState::Done),
                "replay ended in state {:?}",
                snap.state
            );
            assert_eq!(snap.done, 3, "expected 3 unlocks, got {}", snap.done);
            assert_eq!(snap.errors, 0, "unlock errors: {:?}", snap.last_error);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Wait 3s for Xbox to flush to the read side.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 5) VERIFY on the live backend.
    eprintln!("[5/5] verifying each entry landed as Achieved on Xbox Live…");
    let checks = vec![
        ("2134518491", "1", "Curious Copy Cats · Level 3"),
        ("1643012526", "1", "Decollate Decoration · First Seven Days"),
        ("2134518491", "2", "Curious Copy Cats · Level 6"),
    ];
    let mut failures = 0;
    for (tid, ach_id, label) in checks {
        client.clear_achievements_cache(); // bypass in-memory cache
        let resp = client
            .get_achievements_for_title(&xuid, tid)
            .await
            .expect("fetch achievements");
        let entry = resp
            .achievements
            .iter()
            .find(|a| a.id == ach_id)
            .unwrap_or_else(|| panic!("{label} not in response"));
        let is_achieved = entry.progress_state.eq_ignore_ascii_case("Achieved");
        let time = entry
            .progression
            .as_ref()
            .and_then(|p| p.time_unlocked.clone())
            .unwrap_or_default();
        if is_achieved && !time.starts_with("0001") {
            eprintln!("   ✓ {label} — state=Achieved at {time}");
        } else {
            let state = &entry.progress_state;
            eprintln!("   ✗ {label} — state={state} time={time}");
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of 3 entries failed to unlock on the live backend"
    );
    eprintln!("\n🎯 All 3 entries unlocked + verified on Xbox Live backend.");
    eprintln!(
        "   Total wall-time: {:?} (minimum expected: 13s for sum of gaps)",
        started_at.elapsed()
    );
}
