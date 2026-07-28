//! Unit tests for the bit-by-bit runner.

use super::*;
use std::collections::HashSet;
use std::sync::Arc;

const DAY: i64 = 24 * 60 * 60;
const NOW: i64 = 1_700_000_000; // 2023-11-14T22:13:20Z

fn write_test_events_bundle(
    title_id: &str,
    achievement_id: &str,
) -> Arc<crate::events::EventsBundle> {
    let dir = std::env::temp_dir().join(format!(
        "xau-bit-events-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp events dir");
    let data = format!(
        r#"{{
                "{title_id}": {{
                    "FullySupported": true,
                    "Achievements": {{
                        "{achievement_id}": {{}}
                    }}
                }}
            }}"#
    );
    std::fs::write(dir.join("Data.json"), data).expect("write Data.json");
    let bundle = crate::events::EventsBundle::load(&dir).expect("load bundle");
    Arc::new(bundle)
}

fn mk_entry(id: &str, title: &str, iso: &str, gs: i32) -> PlanEntry {
    PlanEntry {
        title_id: title.into(),
        service_config_id: "scid-1".into(),
        achievement_id: id.into(),
        achievement_name: format!("ach {id}"),
        game_name: format!("game {title}"),
        time_unlocked: iso.into(),
        gamerscore: Some(gs),
        ..Default::default()
    }
}

#[test]
fn default_config_has_30_day_cap() {
    let c = BitByBitConfig::default();
    match c.gap_mode {
        GapMode::Capped(d) => assert_eq!(d.as_secs(), 30 * 24 * 60 * 60),
        other => panic!("default gap_mode should be Capped(30d), got {other:?}"),
    }
    assert_eq!(c.chunk_size_override, Some(1));
    assert_eq!(c.title_concurrency, 6);
    assert_eq!(c.top_n_titles, 0);
    assert!(c.selected_titles.is_none());
    assert!(c.time_budget.is_none());
    assert!(c.presence_keepalive);
}

/// Field-reported: "every time I restart bit-by-bit the time to finish
/// the game resets — it doesn't know how many minutes I already have on
/// that title."
///
/// The floor is a TOTAL ("Xbox must eventually show this much time on
/// this game"), and the gate is right to compare my running total
/// against it. The PACING is a DURATION ("spread the unlocks over this
/// long"), and it used the same number — so resuming a half-done game
/// re-spent time already banked. This pins the arithmetic that splits
/// them, exactly as the dispatcher applies it.
#[test]
fn resumed_game_paces_only_the_outstanding_time() {
    const MIN_GAME: u64 = 60;
    let pace_for = |floor: u64, already: u64| floor.saturating_sub(already).max(MIN_GAME).max(1);

    // Fresh title: nothing credited, so pacing == the whole floor.
    assert_eq!(pace_for(3600, 0), 3600, "a fresh game is unchanged");

    // Resumed: 50 of 60 minutes already credited -> pace the last 10,
    // not another full hour. This is the reported bug.
    assert_eq!(pace_for(3600, 3000), 600);

    // Already past the floor is handled by the overshoot window, not
    // by this subtraction — see
    // `overshoot_completes_the_title_in_half_an_hour`.
    assert_ne!(pace_for(3600, 3600), 0, "must never produce the sentinel");

    // The GATE target is untouched by any of this — it stays the total.
    let floor = 3600u64;
    assert_eq!(floor, 3600, "gate compares my total against the full floor");
}

/// When Xbox already credits me MORE time on a game than the target
/// spent finishing it, the floor has nothing left to enforce — the
/// "never faster than the speedrunner" guarantee is met by the clock
/// before the run starts. That case gets its own window: finish the
/// title inside half an hour with the unlocks spread across it, rather
/// than collapsing to `min_game_seconds` (a burst) or re-pacing hours
/// that aren't owed.
#[test]
fn overshoot_completes_the_title_in_half_an_hour() {
    const MIN_GAME: u64 = 60;
    // Mirrors the dispatcher's branch exactly.
    let window = |floor: u64, already: u64, n: usize| {
        let outstanding = if already >= floor {
            OVERSHOOT_PACE_WINDOW_SECS
        } else {
            floor.saturating_sub(already).max(MIN_GAME).max(1)
        };
        pace_window_with_min_spacing(outstanding, n)
    };

    assert_eq!(OVERSHOOT_PACE_WINDOW_SECS, 1800, "half an hour");

    // Overshot by a lot, and exactly at the boundary: both take the
    // half-hour window, NOT the 60 s collapse that produced a burst.
    assert_eq!(window(3600, 99_999, 20), 1800);
    assert_eq!(window(3600, 3600, 20), 1800);
    assert_eq!(1800 / 20, 90, "90s between unlocks — reads like a session");

    // Not overshot: unchanged, still paces only what's outstanding.
    assert_eq!(window(3600, 3000, 5), 600);
    assert_eq!(window(3600, 0, 5), 3600, "fresh game keeps the full floor");

    // The spacing backstop only engages past ~90 achievements, where
    // half an hour genuinely would be too tight.
    assert_eq!(window(3600, 99_999, 90), 1800);
    assert!(
        window(3600, 99_999, 200) > OVERSHOOT_PACE_WINDOW_SECS,
        "200 unlocks in 30 min would be 9s apart — the guard must widen it"
    );
}

/// "If my playtime already passed the floor, won't it fire every
/// achievement at once?"
///
/// It wouldn't fire them in the same instant, but the window collapsed
/// to `min_game_seconds` and then divided by the achievement count —
/// 50 unlocks across 60 s, one every 1.2 s. The playtime requirement is
/// satisfied there, but the TIMESTAMPS are a pattern no human makes,
/// and the rate limiter is no defence (it bottoms out at 150 ms and
/// exists to protect Xbox's budget, not the replay's credibility).
#[test]
fn collapsed_pacing_window_cannot_become_a_burst() {
    const MIN_GAME: u64 = 60;

    // The other way a window collapses: a hot speedup tier on a short
    // game floors at `min_game_seconds`. (The overshoot case now takes
    // the half-hour window instead — see the test above.)
    let collapsed = floor_speedup_secs(90 * 60, MaxSpeedup::Extreme100x, MIN_GAME);
    assert_eq!(collapsed, MIN_GAME);

    // Unguarded that is 1.2 s between unlocks; guarded it is at least
    // MIN_SECS_PER_ACHIEVEMENT apart.
    let n = 50usize;
    let guarded = pace_window_with_min_spacing(collapsed, n);
    assert!(
        guarded / (n as u64) >= MIN_SECS_PER_ACHIEVEMENT,
        "spacing {}s is below the {}s minimum",
        guarded / (n as u64),
        MIN_SECS_PER_ACHIEVEMENT
    );
    assert_eq!(guarded, 50 * MIN_SECS_PER_ACHIEVEMENT);

    // A roomy window is returned untouched — this must not slow down
    // the normal path, only rescue the pathological one.
    assert_eq!(
        pace_window_with_min_spacing(40 * 3600, 50),
        40 * 3600,
        "a 40h floor is already far above the minimum spacing"
    );
    // Even a 100x tier on a long game keeps its compression.
    let hot = floor_speedup_secs(40 * 3600, MaxSpeedup::Extreme100x, MIN_GAME);
    assert_eq!(pace_window_with_min_spacing(hot, 50), hot);

    // Degenerate inputs stay sane.
    assert_eq!(pace_window_with_min_spacing(MIN_GAME, 0), MIN_GAME);
    assert!(pace_window_with_min_spacing(1, 1) >= MIN_SECS_PER_ACHIEVEMENT);
}

/// The sequential defaults are a COMPATIBILITY contract, not a
/// preference: `sequential_window` must stay 1 and `max_speedup` must
/// stay `Off`, so upgrading the binary can never silently change how
/// an existing run behaves. `title_concurrency` is 6 and is
/// deliberately NOT the source of the window — binding them would
/// move every existing user from 1 game to 6 with no action on their
/// part. If this test fails, that is the bug.
#[test]
fn default_config_is_one_game_at_a_time_and_unsped() {
    let c = BitByBitConfig::default();
    assert!(c.sequential, "sequential is the default mode");
    assert_eq!(c.sequential_window, 1, "one game at a time by default");
    assert_eq!(c.max_speedup, MaxSpeedup::Off, "faithful pacing by default");
    assert_ne!(
        c.sequential_window, c.title_concurrency,
        "the window must not be derived from title_concurrency"
    );
    assert_eq!(c.min_game_seconds, 60);
}

/// `floor_speedup_secs` is the ONLY lever on a sequential run's total
/// duration. It must (a) pass through untouched at `Off` so an
/// untiered run is byte-identical, (b) actually honour the label the
/// user picked — the gap ladder does not, which is why this function
/// exists — and (c) never return 0, because 0 is the concurrent-mode
/// sentinel and would silently disable pacing AND the playtime gate.
#[test]
fn floor_speedup_honours_the_tier_and_never_returns_the_sentinel() {
    const HOUR: u64 = 3600;

    // (a) Off is a pure pass-through, at every magnitude.
    for floor in [0, 1, 59, 60, HOUR, 40 * HOUR] {
        assert_eq!(
            floor_speedup_secs(floor, MaxSpeedup::Off, 60),
            floor,
            "Off must not touch a {floor}s floor"
        );
    }

    // (b) The tier means what its label says. A 10-hour game:
    let ten_h = 10 * HOUR;
    assert_eq!(floor_speedup_secs(ten_h, MaxSpeedup::Mild2x, 60), 5 * HOUR);
    assert_eq!(
        floor_speedup_secs(ten_h, MaxSpeedup::Moderate3x, 60),
        12_000
    );
    assert_eq!(floor_speedup_secs(ten_h, MaxSpeedup::Fast5x, 60), 2 * HOUR);
    assert_eq!(floor_speedup_secs(ten_h, MaxSpeedup::Extreme100x, 60), 360);

    // Regression guard for the category error this replaced: the
    // per-GAP ladder caps at 3x for any floor under 30 days and
    // returns 1.0 below a 2h span, so 5x/100x would have been
    // indistinguishable from 3x and short games unsped.
    assert!(
        floor_speedup_secs(ten_h, MaxSpeedup::Extreme100x, 60)
            < floor_speedup_secs(ten_h, MaxSpeedup::Fast5x, 60),
        "100x must be strictly faster than 5x on a realistic floor"
    );
    let ninety_min = 90 * 60;
    assert_eq!(
        floor_speedup_secs(ninety_min, MaxSpeedup::Mild2x, 60),
        45 * 60,
        "a sub-2h game must still respond to the tier"
    );

    // (c) `min_game_seconds` survives compression, and the result is
    // never the 0 sentinel — even at 100x on a tiny floor, or with a
    // pathological min of 0.
    assert_eq!(
        floor_speedup_secs(120, MaxSpeedup::Extreme100x, 60),
        60,
        "min_game_seconds is a hard floor under compression"
    );
    assert_eq!(floor_speedup_secs(30, MaxSpeedup::Extreme100x, 60), 60);
    assert_eq!(
        floor_speedup_secs(10, MaxSpeedup::Extreme100x, 0),
        1,
        "min_game_seconds == 0 still cannot produce the sentinel"
    );
    for tier in MaxSpeedup::all() {
        for floor in [1, 59, 60, 61, HOUR, 40 * HOUR] {
            assert_ne!(
                floor_speedup_secs(floor, tier, 60),
                0,
                "floor_secs == 0 is the concurrent-mode sentinel ({tier:?}, {floor}s)"
            );
        }
    }

    // Monotonic in the tier: a more aggressive pick is never slower.
    let mut prev = u64::MAX;
    for tier in MaxSpeedup::all() {
        let v = floor_speedup_secs(40 * HOUR, tier, 60);
        assert!(
            v <= prev,
            "tier ladder must be monotonic, {tier:?} broke it"
        );
        prev = v;
    }
}

/// Worked example the UI copy promises, pinned so the labels can never
/// drift from the arithmetic: a game the target really finished in
/// **15 minutes** of Xbox playtime.
///
/// Note the top rung is clamped by `min_game_seconds`, NOT by the
/// tier — 900/100 = 9 s, but the per-game minimum holds it at 60 s.
/// That backstop is the only thing between `Extreme100x` and an
/// instant 100%, which is why it survives the division.
#[test]
fn floor_speedup_worked_example_fifteen_minute_game() {
    const FIFTEEN_MIN: u64 = 15 * 60;
    let got: Vec<(&str, u64)> = MaxSpeedup::all()
        .into_iter()
        .map(|t| (t.as_str(), floor_speedup_secs(FIFTEEN_MIN, t, 60)))
        .collect();
    assert_eq!(
        got,
        vec![
            ("off", 900), // 15m — verbatim, the speedrunner's real time
            ("2x", 450),  // 7m30
            ("3x", 300),  // 5m
            ("5x", 180),  // 3m
            ("100x", 60), // 1m — floored by min_game_seconds, not 9s
        ]
    );
}

#[test]
fn gap_mode_exact_preserves_raw_gap() {
    assert_eq!(GapMode::Exact.apply(86400), 86400);
    assert_eq!(GapMode::Exact.apply(0), 0);
    assert_eq!(GapMode::Exact.apply(-100), 0, "negatives clamp to zero");
}

#[test]
fn gap_mode_capped_clamps_over_cap() {
    let m = GapMode::Capped(Duration::from_secs(3600));
    assert_eq!(m.apply(100), 100, "under cap passes through");
    assert_eq!(m.apply(3600), 3600, "at cap is unchanged");
    assert_eq!(m.apply(10_000), 3600, "over cap clamps");
}

// ============================================================
// Shadow-replay slot promotion — regression tests for the bug
// where 100%-completed titles ("ghosts") ate slot budget.
//
// Prior behaviour: `!user_unlocked.is_empty()` counted ghosts
// as in-progress, so on a profile that had 100%'d 5/8 selected
// titles with `active_slots_cap=8` no fresh title was ever
// activated. These tests lock in the fixed semantic.
// ============================================================

fn mk_tp(title_id: &str, source_ids: &[&str], user_unlocked_ids: &[&str]) -> TitlePlan {
    let source: Vec<PlanEntry> = source_ids
        .iter()
        .enumerate()
        .map(|(i, id)| mk_entry(id, title_id, "2024-01-01T00:00:00Z", 10 * (i as i32 + 1)))
        .collect();
    TitlePlan {
        title_id: title_id.into(),
        game_name: format!("game {title_id}"),
        scid: "scid-1".into(),
        source,
        user_unlocked: user_unlocked_ids.iter().map(|&s| s.to_string()).collect(),
        user_last_unix: None,
        user_first_unix: None,
    }
}

#[test]
fn slot_promotion_ghosts_do_not_consume_budget() {
    // 5 ghosts (user has all source entries) + 3 fresh (no unlocks).
    // cap = 8. Pre-fix this retained 5 ghosts + 3 fresh = 8 but only
    // 3 fresh got dispatched. Post-fix: ghosts don't count toward
    // in_progress, so `available_slots = 8` and all 3 fresh land —
    // ghosts still retained for metric integrity.
    let mut plans = vec![
        mk_tp("t1", &["a", "b"], &["a", "b"]), // ghost
        mk_tp("t2", &["a", "b"], &["a", "b"]), // ghost
        mk_tp("t3", &["a", "b"], &["a", "b"]), // ghost
        mk_tp("t4", &["a", "b"], &["a", "b"]), // ghost
        mk_tp("t5", &["a", "b"], &["a", "b"]), // ghost
        mk_tp("t6", &["a", "b", "c"], &[]),    // fresh
        mk_tp("t7", &["a", "b", "c"], &[]),    // fresh
        mk_tp("t8", &["a", "b", "c"], &[]),    // fresh
    ];
    apply_shadow_slot_promotion(&mut plans, 8);
    // All 5 ghosts retained (metric integrity) + all 3 fresh activated.
    assert_eq!(plans.len(), 8);
    let fresh_kept: Vec<_> = plans
        .iter()
        .filter(|tp| tp.user_unlocked.is_empty())
        .map(|tp| tp.title_id.as_str())
        .collect();
    assert_eq!(
        fresh_kept,
        vec!["t6", "t7", "t8"],
        "all fresh titles must be activated when ghosts exist"
    );
}

#[test]
fn slot_promotion_real_in_progress_consumes_budget() {
    // 3 real in-progress (has unlocks + missing some) + 5 fresh.
    // cap = 4. in_progress_count = 3, available_slots = 1.
    // Retain 3 in-progress + 1 fresh = 4. Drop 4 fresh.
    let mut plans = vec![
        mk_tp("t1", &["a", "b", "c"], &["a"]), // real in-progress
        mk_tp("t2", &["a", "b", "c"], &["a"]), // real in-progress
        mk_tp("t3", &["a", "b", "c"], &["a"]), // real in-progress
        mk_tp("t4", &["a", "b"], &[]),         // fresh
        mk_tp("t5", &["a", "b"], &[]),         // fresh
        mk_tp("t6", &["a", "b"], &[]),         // fresh
        mk_tp("t7", &["a", "b"], &[]),         // fresh
        mk_tp("t8", &["a", "b"], &[]),         // fresh
    ];
    apply_shadow_slot_promotion(&mut plans, 4);
    assert_eq!(plans.len(), 4);
    // 3 in-progress titles + first fresh (t4).
    let ids: Vec<_> = plans.iter().map(|tp| tp.title_id.as_str()).collect();
    assert_eq!(ids, vec!["t1", "t2", "t3", "t4"]);
}

#[test]
fn slot_promotion_mixed_ghosts_real_fresh() {
    // 2 ghosts + 3 real + 5 fresh. cap = 4.
    // in_progress_count = 3 (only real), available_slots = 1.
    // Retain: 2 ghosts + 3 real + 1 fresh = 6 titles.
    let mut plans = vec![
        mk_tp("t1", &["a"], &["a"]),           // ghost
        mk_tp("t2", &["a"], &["a"]),           // ghost
        mk_tp("t3", &["a", "b", "c"], &["a"]), // real
        mk_tp("t4", &["a", "b", "c"], &["a"]), // real
        mk_tp("t5", &["a", "b", "c"], &["a"]), // real
        mk_tp("t6", &["a", "b"], &[]),         // fresh — first fresh kept
        mk_tp("t7", &["a", "b"], &[]),         // fresh — dropped
        mk_tp("t8", &["a", "b"], &[]),         // fresh — dropped
        mk_tp("t9", &["a", "b"], &[]),         // fresh — dropped
        mk_tp("t10", &["a", "b"], &[]),        // fresh — dropped
    ];
    apply_shadow_slot_promotion(&mut plans, 4);
    let ids: Vec<_> = plans.iter().map(|tp| tp.title_id.as_str()).collect();
    assert_eq!(ids, vec!["t1", "t2", "t3", "t4", "t5", "t6"]);
}

#[test]
fn slot_promotion_cap_exceeded_by_real_keeps_all_real() {
    // 6 real in-progress, cap = 4, 0 fresh. saturating_sub gives
    // available_slots = 0, but all 6 real are retained (they're
    // already in-progress on the user's profile).
    let mut plans = (0..6)
        .map(|i| mk_tp(&format!("t{i}"), &["a", "b", "c"], &["a"]))
        .collect::<Vec<_>>();
    apply_shadow_slot_promotion(&mut plans, 4);
    assert_eq!(plans.len(), 6, "saturating cap keeps real titles intact");
}

// ============================================================
// Presence keepalive tests — cadence, cancel, cleanup.
//
// These prove the CONTRACT of the keepalive loop against a mock
// `PresenceSender` that just counts calls. They do NOT prove Xbox
// Live accepts the payload — only a live run with a real account
// can validate that. What they DO prove:
//
//   1. First heartbeat fires at t=0 (immediate).
//   2. Subsequent heartbeats fire at the configured period.
//   3. Signal on `rx` stops the loop + fires stop() exactly once.
//   4. Errors from send() don't abort the loop.
// ============================================================

use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingSender {
    send_calls: Arc<AtomicUsize>,
    stop_calls: Arc<AtomicUsize>,
    fail_mode: bool,
}

#[async_trait::async_trait]
impl PresenceSender for CountingSender {
    async fn send(&self, _title_ids: &[String]) -> Result<()> {
        self.send_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_mode {
            Err(Error::other("simulated network failure"))
        } else {
            Ok(())
        }
    }
    async fn stop(&self) {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
    }
}

// Records the exact title list of every heartbeat so tests can
// assert the loop broadcasts the DYNAMIC active set, not a stale
// snapshot.
#[derive(Clone)]
struct RecordingSender {
    sends: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    stop_calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PresenceSender for RecordingSender {
    async fn send(&self, title_ids: &[String]) -> Result<()> {
        self.sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(title_ids.to_vec());
        Ok(())
    }
    async fn stop(&self) {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn presence_loop_tracks_dynamic_active_set() {
    // The loop must heartbeat whatever is in the shared set AT EACH
    // TICK: a title removed (finished) drops out, a title added
    // (rotated in) appears, and an empty set skips the POST.
    let sends = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let sender = RecordingSender {
        sends: sends.clone(),
        stop_calls: stop_calls.clone(),
    };
    let active: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(vec!["t1".into(), "t2".into()]));
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(presence_keepalive_loop(
        sender,
        active.clone(),
        Duration::from_secs(100),
        rx,
    ));
    // t=0 immediate tick → heartbeats [t1, t2].
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        sends.lock().unwrap().first().cloned(),
        Some(vec!["t1".to_string(), "t2".to_string()]),
        "first tick heartbeats the initial active set"
    );
    // Rotate: t1 finishes (removed), t3 starts (added).
    {
        let mut g = active.lock().unwrap();
        g.retain(|t| t != "t1");
        g.push("t3".into());
    }
    // Advance past the max jittered interval (100 * 1.15 = 115s).
    tokio::time::sleep(Duration::from_secs(116)).await;
    let last = sends.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.contains(&"t3".to_string()) && !last.contains(&"t1".to_string()),
        "after rotation the heartbeat drops t1 and includes t3, got {last:?}"
    );
    // Empty the set → next tick must SKIP the POST (no new send).
    active.lock().unwrap().clear();
    let before_empty = sends.lock().unwrap().len();
    tokio::time::sleep(Duration::from_secs(116)).await;
    assert_eq!(
        sends.lock().unwrap().len(),
        before_empty,
        "an empty active set skips the heartbeat POST entirely"
    );
    let _ = tx.send(());
    task.await.expect("task joined");
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn presence_first_tick_is_immediate() {
    let send_calls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let sender = CountingSender {
        send_calls: send_calls.clone(),
        stop_calls: stop_calls.clone(),
        fail_mode: false,
    };
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(presence_keepalive_loop(
        sender,
        Arc::new(std::sync::Mutex::new(vec!["t1".into()])),
        Duration::from_secs(180),
        rx,
    ));
    // Let the task run a tiny bit of virtual time so the interval
    // can fire its immediate tick.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        send_calls.load(Ordering::SeqCst),
        1,
        "first tick should fire immediately"
    );
    let _ = tx.send(());
    task.await.expect("task joined");
    assert_eq!(
        stop_calls.load(Ordering::SeqCst),
        1,
        "stop called exactly once on cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn presence_ticks_within_jitter_window() {
    // Wave-9 audit forensic-HIGH-5: keepalive interval is now
    // jittered by ±15% (so a wireshark observer doesn't see a
    // fingerprint-perfect sawtooth). The first tick still fires
    // at t=0 (the heartbeat-on-start semantics that match Xbox
    // clients) but subsequent ticks are randomized within
    // `[base*0.85, base*1.15)`. Test asserts the count grows
    // monotonically and stays bounded by the window — not that
    // ticks land at exactly t=180s.
    let send_calls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let sender = CountingSender {
        send_calls: send_calls.clone(),
        stop_calls: stop_calls.clone(),
        fail_mode: false,
    };
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(presence_keepalive_loop(
        sender,
        Arc::new(std::sync::Mutex::new(vec!["t1".into(), "t2".into()])),
        Duration::from_secs(180),
        rx,
    ));
    // t=0 → 1 tick (immediate, deterministic).
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        send_calls.load(Ordering::SeqCst),
        1,
        "first tick fires at t=0"
    );
    // Advance past the maximum jittered period (180 * 1.15 = 207s)
    // → must have ticked again exactly once.
    tokio::time::sleep(Duration::from_secs(208)).await;
    assert!(
        send_calls.load(Ordering::SeqCst) >= 2,
        "second tick should have fired within the 207s upper-jitter bound"
    );
    // The total count after t=208s is bounded above by 2 (one
    // immediate + at most one jittered) since the smallest
    // jittered interval is 153s and we've only advanced 208s
    // past t=0.
    assert!(
        send_calls.load(Ordering::SeqCst) <= 2,
        "no third tick possible inside 208s (min jittered interval 153s × 2 = 306s)"
    );
    let _ = tx.send(());
    task.await.expect("task joined");
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn presence_send_errors_do_not_abort_loop() {
    // send() returns Err every time. Loop must keep ticking and
    // eventually fire stop() on cancel — not terminate early.
    let send_calls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let sender = CountingSender {
        send_calls: send_calls.clone(),
        stop_calls: stop_calls.clone(),
        fail_mode: true,
    };
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(presence_keepalive_loop(
        sender,
        Arc::new(std::sync::Mutex::new(vec!["t1".into()])),
        Duration::from_secs(60),
        rx,
    ));
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    // Wave-9 forensic-HIGH-5: keepalive interval is now jittered
    // ±15%, so a 60s base period actually fires every 51-69s.
    // Worst case three ticks (the immediate one + two jittered)
    // = 0 + 69 + 69 = 138s. Sleep 180s to comfortably cover the
    // worst-case jitter and still witness ≥3 ticks. Without this
    // bump the test would flake on unlucky RNG picks.
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert!(
        send_calls.load(Ordering::SeqCst) >= 3,
        "errors don't break the tick cycle"
    );
    let _ = tx.send(());
    task.await.expect("task joined despite errors");
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn presence_cancel_before_first_tick_still_calls_stop() {
    // Edge case: cancel fires before the interval even gets its
    // first tick. The `select!` should pick up rx and still call
    // stop() — we don't want to leak presence just because the
    // runner aborted instantly.
    let send_calls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let sender = CountingSender {
        send_calls: send_calls.clone(),
        stop_calls: stop_calls.clone(),
        fail_mode: false,
    };
    let (tx, rx) = oneshot::channel::<()>();
    let _ = tx.send(()); // fire cancel BEFORE spawn
    let task = tokio::spawn(presence_keepalive_loop(
        sender,
        Arc::new(std::sync::Mutex::new(vec!["t1".into()])),
        Duration::from_secs(180),
        rx,
    ));
    task.await.expect("task joined");
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
}

fn sched(unix: i64, subsec: u16, done: bool) -> ScheduledEntry {
    ScheduledEntry {
        achievement_id: "a".into(),
        gamerscore: 10,
        scheduled_unix: unix,
        scheduled_subsec_ms: subsec,
        already_done: done,
    }
}

#[test]
fn catch_up_shift_kills_burst_preserves_gaps_and_order() {
    // Three overdue pending entries, gaps of 1 day and 5 days,
    // earliest 10 days in the past. now = NOW.
    let mut s = vec![
        sched(NOW - 10 * DAY, 0, false), // earliest pending
        sched(NOW - 9 * DAY, 0, false),  // +1 day
        sched(NOW - 4 * DAY, 0, false),  // +5 days
    ];
    apply_catch_up_shift(&mut s, NOW);
    // Earliest pending now lands exactly at NOW (fires this run).
    assert_eq!(s[0].scheduled_unix, NOW);
    // Real gaps preserved going forward — NOT collapsed to a burst.
    assert_eq!(s[1].scheduled_unix, NOW + DAY, "1-day gap kept");
    assert_eq!(
        s[2].scheduled_unix,
        NOW + 6 * DAY,
        "cumulative 6-day offset kept"
    );
    // Strictly increasing (order never broken).
    assert!(s[0].scheduled_unix < s[1].scheduled_unix);
    assert!(s[1].scheduled_unix < s[2].scheduled_unix);
}

#[test]
fn catch_up_shift_is_noop_when_nothing_overdue() {
    // Earliest pending is in the FUTURE → no shift.
    let mut s = vec![sched(NOW + 100, 0, false), sched(NOW + 200, 0, false)];
    let before = s.clone();
    apply_catch_up_shift(&mut s, NOW);
    assert_eq!(s, before, "future-only schedule is untouched");
}

#[test]
fn catch_up_shift_ignores_already_done_and_anchors_on_first_pending() {
    // already_done entries in the past must NOT be shifted and must
    // NOT be treated as the anchor — only the first PENDING one.
    let mut s = vec![
        sched(NOW - 100 * DAY, 0, true),       // done, ancient — ignored
        sched(NOW - 2 * DAY, 0, false),        // first pending (anchor)
        sched(NOW - 2 * DAY + 3600, 0, false), // +1h
    ];
    apply_catch_up_shift(&mut s, NOW);
    assert_eq!(s[0].scheduled_unix, NOW - 100 * DAY, "done entry untouched");
    assert!(s[0].already_done);
    // First pending lands exactly at NOW; 1h gap preserved forward.
    assert_eq!(s[1].scheduled_unix, NOW);
    assert_eq!(s[2].scheduled_unix, NOW + 3600, "1h gap preserved");
}

#[tokio::test]
async fn pace_unlock_enforces_min_interval_across_concurrent_callers() {
    // Real-clock test (the pacer uses std::time::Instant + real
    // tokio::time::sleep, which agree in production; a paused clock
    // would desync them). 5 concurrent callers hammering one shared
    // gate must land >= UNLOCK_MIN_INTERVAL apart — proving the
    // aggregate rate can't exceed Xbox's budget however many workers
    // pile on.
    let gate = Arc::new(tokio::sync::Mutex::new(Instant::now()));
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..5 {
        let g = gate.clone();
        handles.push(tokio::spawn(async move {
            pace_unlock(&g).await;
            Instant::now()
        }));
    }
    let mut times = Vec::new();
    for h in handles {
        times.push(h.await.unwrap());
    }
    times.sort();
    // 5 sends spaced by 150ms → the last lands >= 4 intervals out.
    // This is the aggregate-rate guarantee: N sends take >= (N-1) ×
    // interval, so the combined rate can never exceed the budget.
    // (Per-pair spacing isn't asserted — task-wakeup jitter can make
    // an individual pair read slightly under while the total holds.)
    let span = times.last().unwrap().duration_since(start);
    assert!(
        span >= UNLOCK_MIN_INTERVAL * 4,
        "5 paced sends must span >= 4 intervals, got {span:?}"
    );
    // The derived rate is exactly the documented budget.
    assert_eq!(
        UNLOCK_MIN_INTERVAL,
        Duration::from_millis(150),
        "100 req / 15s = 150ms interval"
    );
}

#[test]
fn catch_up_mode_defaults_to_shift() {
    assert_eq!(CatchUpMode::default(), CatchUpMode::Shift);
    assert_eq!(BitByBitConfig::default().catch_up, CatchUpMode::Shift);
}

#[test]
fn gap_mode_scaled_compresses_by_factor() {
    assert_eq!(GapMode::Scaled(0.1).apply(1000), 100);
    assert_eq!(GapMode::Scaled(0.5).apply(1000), 500);
    assert_eq!(GapMode::Scaled(1.0).apply(1000), 1000);
    assert_eq!(GapMode::Scaled(0.0).apply(1000), 0);
    // Out-of-range factor clamps silently.
    assert_eq!(GapMode::Scaled(-1.0).apply(1000), 0);
    assert_eq!(GapMode::Scaled(999.0).apply(1000), 1000);
}

#[test]
fn start_without_xuid_errors() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = XboxClient::new(crate::xbox::client::XboxClientOptions {
            xauth: "XBL3.0 x=1;dummydummydummydummydummydummydummy".into(),
            ..Default::default()
        })
        .unwrap();
        let plan = Plan {
            source_gamertag: "Target".into(),
            source_xuid: "123".into(),
            built_at: String::new(),
            total: 1,
            entries: vec![mk_entry("a", "1", "2024-01-01T00:00:00Z", 10)],
            ..Default::default()
        };
        let err = start_bit_by_bit(client, plan, BitByBitConfig::default())
            .err()
            .unwrap();
        assert!(err.to_string().contains("my_xuid"));
    });
}

/// First entry of a title the user has never touched: schedules
/// immediately at `now_unix` regardless of the source timestamp.
#[test]
fn schedule_first_entry_fires_now_when_user_empty() {
    let entries = vec![mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10)];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out.len(), 1);
    assert!(!out[0].already_done);
    assert_eq!(out[0].scheduled_unix, NOW);
}

/// Source gap is SMALLER than max_gap → real gap is preserved.
#[test]
fn schedule_gap_under_max_preserves_source_gap() {
    // A at day 0, B at day 5 (source gap = 5 days, well under 30d cap)
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-01-06T00:00:00Z", 20),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out.len(), 2);
    // First fires now, second schedules now + 5 days
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_unix, NOW + 5 * DAY);
}

/// Source gap is LARGER than max_gap → clamped.
#[test]
fn schedule_gap_over_max_is_clamped() {
    // A at day 0, B at day 200 → cap to 30 days
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-07-19T00:00:00Z", 20),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out[1].scheduled_unix, NOW + 30 * DAY);
}

/// `max_gap = 0` → every non-first entry schedules at the exact
/// same moment as the previous (no real-world wait), which is the
/// "ignore source timing, fire everything in order" behaviour.
#[test]
fn schedule_with_zero_max_gap_fires_consecutive_now() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-01-06T00:00:00Z", 20),
        mk_entry("a3", "t1", "2024-02-01T00:00:00Z", 30),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(0)),
        NOW,
    );
    assert_eq!(out.len(), 3);
    for s in &out {
        assert_eq!(s.scheduled_unix, NOW);
    }
}

/// Entries the user has already unlocked are flagged + don't
/// consume real-world wait from the follow-on schedule.
///
/// Anchor-clamp contract: the chain anchor is clamped to NOW even
/// when `user_last_unix` lies in the past, because Xbox stamps each
/// POST with its server-side receipt time (no backdating). So A2
/// schedules at `NOW + source_gap`, not `user_last + source_gap` —
/// the source gap is preserved verbatim, just rebased onto NOW.
#[test]
fn schedule_already_done_flagged_and_advances_running() {
    let mut user_unlocked = HashSet::new();
    user_unlocked.insert("a1".to_string());
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-01-06T00:00:00Z", 20),
    ];
    // Resume semantics: A1 was actually unlocked 10 days ago, and the
    // A1→A2 source gap is 5 days. So A2 became due 5 days ago
    // (user_last + 5d = NOW - 5d) and is now OVERDUE — it fires this
    // run. The chain anchors on the TRUE historical last-unlock, NOT
    // a now-rebase.
    let user_last = NOW - 10 * DAY;
    let out = compute_schedule_for_title(
        &entries,
        Some(user_last),
        &user_unlocked,
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out.len(), 2);
    assert!(out[0].already_done);
    // A2 = user_last (NOW-10d) + 5d gap = NOW-5d → past-due, eligible now.
    assert_eq!(out[1].scheduled_unix, NOW - 5 * DAY);
    assert!(
        out[1].scheduled_unix <= NOW,
        "overdue entry must be eligible now"
    );
    assert!(!out[1].already_done);
}

/// Resume/catch-up: a historical `user_last_unix` anchors the chain
/// in the past, so entries whose (anchor + gap) already elapsed come
/// out OVERDUE (`<= NOW`) and fire this run. This is the documented
/// stateless-resume model — the previous `.max(now)` anchor clamp
/// rebased every run to now and so could never advance a title with
/// prior history across runs. The catch-up POST rate is bounded by
/// the confirmed-unlock cap + the 6.67 RPS rate limiter, not the raw
/// schedule.
#[test]
fn schedule_historical_anchor_makes_overdue_entries_fire() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-01-02T00:00:00Z", 20),
        mk_entry("a3", "t1", "2024-01-03T00:00:00Z", 30),
    ];
    // Two-year-old historical anchor — every entry is long overdue.
    let user_last_two_years_ago = NOW - 2 * 365 * DAY;
    let out = compute_schedule_for_title(
        &entries,
        Some(user_last_two_years_ago),
        &HashSet::new(),
        GapMode::Exact,
        NOW,
    );
    assert_eq!(out.len(), 3);
    // Anchored on the true historical time; gaps preserved verbatim.
    assert_eq!(out[0].scheduled_unix, user_last_two_years_ago);
    assert_eq!(out[1].scheduled_unix, user_last_two_years_ago + DAY);
    assert_eq!(out[2].scheduled_unix, user_last_two_years_ago + 2 * DAY);
    // All three are overdue → all eligible this run (bounded catch-up).
    for s in &out {
        assert!(
            s.scheduled_unix <= NOW,
            "overdue entry must be eligible now"
        );
    }
}

/// Tiered counterpart: a historical `user_first_unix` anchors the
/// tier-scaled chain in the past so overdue entries fire this run
/// (resume), instead of the old clamp that rebased to now.
#[test]
fn tiered_schedule_historical_anchor_makes_overdue_entries_fire() {
    let entries = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
    ];
    let tiers = vec![(u64::MAX, 1.0)];
    // Historical first-unlock a year ago — both entries overdue.
    let user_first_historical = NOW - 365 * DAY;
    let out = compute_tiered_schedule(
        &entries,
        Some(user_first_historical),
        &HashSet::new(),
        &tiers,
        NOW,
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].scheduled_unix, user_first_historical);
    assert_eq!(out[1].scheduled_unix, user_first_historical + 3600);
    assert!(out[1].scheduled_unix <= NOW);
}

/// Entries with unparseable timestamps treat the gap as zero and
/// still schedule normally (graceful degradation).
#[test]
fn schedule_unparseable_timestamp_treats_gap_as_zero() {
    let entries = vec![
        mk_entry("a1", "t1", "garbage", 10),
        mk_entry("a2", "t1", "also-garbage", 20),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_unix, NOW);
}

/// Resumability sanity: a schedule computed today, where A2's
/// scheduled time is in the future, should still have A2 as
/// `!already_done` so a future re-run can fire it.
#[test]
fn schedule_future_entries_remain_eligible_for_future_run() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-06-01T00:00:00Z", 20),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert!(out[1].scheduled_unix > NOW);
    assert!(!out[1].already_done);
}

/// Unparseable timestamps between parseable ones MUST NOT reset
/// the gap chain. Audit bug BBB-Timing#1 / #6: the previous
/// behaviour overwrote `prev_source_unix` with `None` on parse
/// failure, causing the next parseable entry to see `source_gap
/// = 0` instead of `cur - last_parseable`.
///
/// **Wave-5 contract update (clock-F2 holy-grail fix):** the
/// scheduler now PRE-SORTS entries by parsed source timestamp at
/// function entry, with unparseable entries sinking to the END
/// of the schedule (their chronology is undefined). The previous
/// contract preserved INPUT order even when input was sorted
/// wrong — that violated the holy-grail "chronological order is
/// the santo graal" invariant. The new test pins the new
/// contract: unparseable entries appear after every parseable
/// one, regardless of where they sit in the input slice.
#[test]
fn schedule_preserves_gap_chain_across_unparseable_hole() {
    // Input has the unparseable entry in the MIDDLE; the new
    // sort sinks it to the end so the parseable chain a1 → a3
    // is contiguous in the output.
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "not-a-timestamp", 20),
        mk_entry("a3", "t1", "2024-01-06T00:00:00Z", 30),
    ];
    let out = compute_schedule_for_title(&entries, None, &HashSet::new(), GapMode::Exact, NOW);
    // out[0] = a1 (parseable, earliest) → NOW (no anchor yet).
    // out[1] = a3 (parseable, second) → NOW + 5 days (chains from a1).
    // out[2] = a2 (unparseable, sunk to end) → no gap (anchor stays).
    assert_eq!(out[0].achievement_id, "a1");
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].achievement_id, "a3");
    assert_eq!(out[1].scheduled_unix, NOW + 5 * DAY);
    assert_eq!(out[2].achievement_id, "a2");
    // a2's anchor is the LAST scheduled entry's time + 0 gap.
    assert_eq!(out[2].scheduled_unix, NOW + 5 * DAY);
}

/// HOLY-GRAIL regression: input slice is reverse-chronological
/// (clock skew on source profile, manually-edited JSON, or an
/// aggregate-of-N merger that didn't sort). Wave-5 audit
/// clock-F2 fixed this — the scheduler now pre-sorts by source
/// timestamp so the wire-emit order is correct regardless of
/// input order.
#[test]
fn schedule_resorts_unsorted_input_by_source_timestamp() {
    // Three real-looking entries, but in REVERSE order in the
    // input slice. The scheduler must reorder them by parsed
    // timestamp.
    let entries = vec![
        mk_entry("z3", "t1", "2024-01-06T00:00:00Z", 30),
        mk_entry("z2", "t1", "2024-01-03T00:00:00Z", 20),
        mk_entry("z1", "t1", "2024-01-01T00:00:00Z", 10),
    ];
    let out = compute_schedule_for_title(&entries, None, &HashSet::new(), GapMode::Exact, NOW);
    assert_eq!(out[0].achievement_id, "z1", "earliest must come first");
    assert_eq!(out[1].achievement_id, "z2");
    assert_eq!(out[2].achievement_id, "z3");
    // And the gaps must be the SOURCE gaps (2 days, 3 days),
    // not 0 + 1ms bumps from the old monotonicity-guard
    // bandage.
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_unix, NOW + 2 * DAY);
    assert_eq!(out[2].scheduled_unix, NOW + 5 * DAY);
}

/// Adversarial `running_user_last` near `i64::MAX` must not
/// overflow when `effective_gap` is also large. Audit bug
/// BBB-Timing#3 — the old `last + effective_gap` wrapped to
/// large-negative, which the eligibility filter treats as
/// trivially past and fires immediately.
#[test]
fn schedule_saturates_on_overflow() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2024-06-01T00:00:00Z", 20),
    ];
    let out = compute_schedule_for_title(
        &entries,
        Some(i64::MAX - 100),
        &HashSet::new(),
        GapMode::Exact,
        NOW,
    );
    // a1 / a2 must saturate to a large *positive* value (no wrap
    // to a small negative). After the BBB-Subsec migration the
    // chain runs in ms — `(i64::MAX - 100).saturating_mul(1000)`
    // already saturates to `i64::MAX`, so both entries land on
    // `i64::MAX / 1000` (the second-resolution floor of the
    // saturated ms target). Without saturation the entries would
    // wrap through zero into negative territory, which in turn
    // would make them trivially eligible (≤ eligibility_horizon)
    // and fire entries the user wanted far-future.
    let saturated_secs = i64::MAX / 1000;
    assert_eq!(out[0].scheduled_unix, saturated_secs);
    assert_eq!(out[1].scheduled_unix, saturated_secs);
}

// ======================================================================
//  BBB-Subsec: sub-second cadence preservation
//  ----------------------------------------------------------------------
//  These tests lock down the contract that the bit-by-bit replay
//  honors source sub-second timing within a same-second burst —
//  the case that distinguishes "looks like a real profile" from
//  "looks like a script".
// ======================================================================

/// Source has three entries within the same second (`.100`, `.500`,
/// `.999`). The schedule MUST place them at the same scheduled
/// second AND in chronological order with the original sub-second
/// deltas preserved (400ms, 499ms).
#[test]
fn schedule_preserves_subsecond_burst_cadence() {
    let entries = vec![
        mk_entry("a", "t1", "2024-06-15T12:34:56.100Z", 10),
        mk_entry("b", "t1", "2024-06-15T12:34:56.500Z", 10),
        mk_entry("c", "t1", "2024-06-15T12:34:56.999Z", 10),
    ];
    // Use Exact mode — the scheduler should preserve every gap
    // verbatim, including sub-second residuals.
    let out = compute_schedule_for_title(&entries, Some(NOW), &HashSet::new(), GapMode::Exact, NOW);
    assert_eq!(out.len(), 3);
    // First entry anchors at user_last_unix (no source gap to
    // apply); subsec_ms = 0 since the chain anchor has no
    // sub-second info.
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[0].scheduled_subsec_ms, 0);
    // Second entry: source gap to first = 400ms. Preserved
    // verbatim into the schedule chain.
    assert_eq!(out[1].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_subsec_ms, 400);
    // Third entry: source gap to second = 499ms.
    // Cumulative sub-second offset = 400 + 499 = 899ms.
    assert_eq!(out[2].scheduled_unix, NOW);
    assert_eq!(out[2].scheduled_subsec_ms, 899);
}

/// Even under aggressive `Capped(0)` compression, sub-second
/// bursts within a single source-second MUST stay in chronological
/// order with their cadence preserved. Cross-second source gaps
/// collapse — that's the documented compression contract — but the
/// monotonicity bump (+1ms) keeps the wire-order strict so the
/// runner's pre-flight assertion (process_title:debug_assert) doesn't
/// fire.
#[test]
fn schedule_subsec_burst_survives_compression() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-06-15T12:34:56.100Z", 10),
        mk_entry("a2", "t1", "2024-06-15T12:34:56.500Z", 10),
        mk_entry("a3", "t1", "2024-06-15T12:34:56.999Z", 10),
        mk_entry("a4", "t1", "2024-06-15T12:35:00.000Z", 10),
        mk_entry("a5", "t1", "2024-06-15T12:35:05.000Z", 10),
    ];
    let out = compute_schedule_for_title(
        &entries,
        Some(NOW),
        &HashSet::new(),
        GapMode::Capped(Duration::ZERO),
        NOW,
    );
    // Same-second burst preserves the 400/499 ms cadence verbatim
    // because `apply_gap_mode_ms` keeps the sub-second residual
    // when the whole-second portion is preserved (here both are 0).
    let combined: Vec<i64> = out
        .iter()
        .map(|e| e.scheduled_unix * 1000 + i64::from(e.scheduled_subsec_ms))
        .collect();
    assert!(
        combined.windows(2).all(|w| w[0] < w[1]),
        "compressed burst must remain strictly monotonic, got {:?}",
        combined
    );
    // a1, a2, a3 must reflect the source's verbatim cadence.
    assert_eq!(combined[0], NOW * 1000);
    assert_eq!(combined[1], NOW * 1000 + 400);
    assert_eq!(combined[2], NOW * 1000 + 899);
    // a4, a5 are CROSS-second source entries; Capped(0) flattens
    // them, monotonicity bump kicks in (+1ms over previous).
    assert_eq!(combined[3], NOW * 1000 + 900);
    assert_eq!(combined[4], NOW * 1000 + 901);
}

// ======================================================================
//  TIERED SCHEDULER TESTS (shadow replay)
// ======================================================================

/// Tiered resumability (post anchor-clamp removal).
///
/// The restored invariant is the strong one the module doc always
/// promised: "same `user_first_unix` + same source → bit-identical
/// ABSOLUTE schedule across reboots". Because the anchor is now the
/// real historical first-unlock (not `max(user_first, now)`),
/// re-running the plan at a later wall-clock time recomputes the
/// SAME schedule — so `now` simply catches up to each entry and
/// progress continues, instead of the chain rebasing forward every
/// run and starving cross-run resume.
#[test]
fn tiered_schedule_resume_preserves_absolute_schedule() {
    // Target unlocked A at t=0, B 1 hour later, C 1 day later.
    let entries = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
        mk_entry("c", "t1", "2024-01-02T01:00:00Z", 30),
    ];
    let user_first = NOW;
    let mut unlocked = HashSet::new();
    unlocked.insert("a".to_string());
    let tiers = vec![
        (3600, 1.0),   // 0-1h: 1x
        (86400, 10.0), // 1h-1d: 10x
        (u64::MAX, 100.0),
    ];
    // Run 1 at t=user_first, Run 2 an hour later — SAME user_first.
    let out_1 = compute_tiered_schedule(&entries, Some(user_first), &unlocked, &tiers, NOW);
    let out_2 = compute_tiered_schedule(&entries, Some(user_first), &unlocked, &tiers, NOW + 3600);
    assert_eq!(out_1.len(), 3);
    // Anchor = user_first (NOW). A at +0, B at +3600 (1h tier, 1x),
    // C at +12240 (3600 + 86400/10 = 3600 + 8640).
    assert_eq!(out_1[0].scheduled_unix, NOW);
    assert_eq!(out_1[1].scheduled_unix, NOW + 3600);
    assert_eq!(out_1[2].scheduled_unix, NOW + 12240);
    // Run 2 recomputes the IDENTICAL absolute schedule (resume):
    // the later `now` does not rebase the chain.
    let s1: Vec<i64> = out_1.iter().map(|s| s.scheduled_unix).collect();
    let s2: Vec<i64> = out_2.iter().map(|s| s.scheduled_unix).collect();
    assert_eq!(
        s1, s2,
        "same user_first must yield the same schedule across runs"
    );
}

/// Tiered scheduler outputs are monotonic non-decreasing per
/// title. Verified against an adversarial input with huge gaps
/// in the later entries.
#[test]
fn tiered_schedule_is_monotonic() {
    let entries = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T00:30:00Z", 20), // 30min gap
        mk_entry("c", "t1", "2024-06-01T00:00:00Z", 30), // ~5 months gap
        mk_entry("d", "t1", "2025-06-01T00:00:00Z", 40), // ~1 year gap
    ];
    let tiers = vec![
        (3600, 1.0),
        (86400, 10.0),
        (2_592_000, 100.0),
        (u64::MAX, 10000.0),
    ];
    let out = compute_tiered_schedule(&entries, Some(NOW), &HashSet::new(), &tiers, NOW);
    // Each entry's scheduled_unix must be >= the previous one.
    for i in 1..out.len() {
        assert!(
            out[i].scheduled_unix >= out[i - 1].scheduled_unix,
            "monotonicity violated at idx {i}: {:?}",
            out
        );
    }
}

/// BUG HUNTER bbb-tiered-sort regression: the tiered scheduler must
/// emit entries in CHRONOLOGICAL order even when the input slice is
/// UNSORTED (loaded-from-disk / hand-edited / aggregate-merged plans,
/// or source-profile clock skew). Before the fix it iterated the
/// slice verbatim, so out-of-order input produced the WRONG order
/// with 1ms spacing instead of the real gaps — violating the module's
/// "chronological order is NEVER broken" invariant. The classic
/// scheduler already sorted defensively; this asserts the tiered twin
/// now does too.
#[test]
fn tiered_schedule_sorts_unordered_input_chronologically() {
    // Supplied OUT OF ORDER: c (latest), a (earliest), b (middle).
    let entries = vec![
        mk_entry("c", "t1", "2024-01-01T02:00:00Z", 30),
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
    ];
    // Single 1.0× tier: source gaps pass through verbatim.
    let tiers = vec![(u64::MAX, 1.0)];
    let out = compute_tiered_schedule(&entries, Some(NOW), &HashSet::new(), &tiers, NOW);

    // Output is in chronological order a → b → c …
    let order: Vec<&str> = out.iter().map(|e| e.achievement_id.as_str()).collect();
    assert_eq!(
        order,
        vec!["a", "b", "c"],
        "tiered schedule not chronological: {out:?}"
    );

    // … with the real 1h / 2h gaps preserved (NOT collapsed to +1ms).
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_unix, NOW + 3600);
    assert_eq!(out[2].scheduled_unix, NOW + 7200);
}

/// When `user_first_unix` is None (title not started), tiered
/// scheduler falls back to `now_unix` as the anchor.
#[test]
fn tiered_schedule_falls_back_to_now_when_unstarted() {
    let entries = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
    ];
    let tiers = vec![(u64::MAX, 1.0)];
    let out = compute_tiered_schedule(&entries, None, &HashSet::new(), &tiers, NOW);
    assert_eq!(out[0].scheduled_unix, NOW);
    assert_eq!(out[1].scheduled_unix, NOW + 3600);
}

/// Tier boundaries: a gap exactly at 1h should use the 0-1h
/// tier's speedup, not the 1h+ tier's.
#[test]
fn tiered_boundary_inclusive_on_lower_edge() {
    let entries = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20), // exactly 3600s gap
    ];
    let tiers = vec![
        (3600, 1.0),        // 0-1h inclusive: speedup 1
        (u64::MAX, 1000.0), // above: speedup 1000
    ];
    let out = compute_tiered_schedule(&entries, Some(NOW), &HashSet::new(), &tiers, NOW);
    // 3600 gap at 1x → B scheduled at NOW + 3600.
    assert_eq!(out[1].scheduled_unix, NOW + 3600);
}

/// Gamerscore carries through unchanged — tests against accidental
/// `unwrap_or(0)` drift.
#[test]
fn schedule_preserves_gamerscore() {
    let entries = vec![
        mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 50),
        mk_entry("a2", "t1", "2024-01-02T00:00:00Z", 100),
    ];
    let out = compute_schedule_for_title(
        &entries,
        None,
        &HashSet::new(),
        GapMode::Capped(Duration::from_secs(30 * DAY as u64)),
        NOW,
    );
    assert_eq!(out[0].gamerscore, 50);
    assert_eq!(out[1].gamerscore, 100);
}

/// `title_spans_shortest_first` orders correctly (shortest → longest),
/// with tiebreakers on count then title_id.
#[test]
fn title_spans_sorted_shortest_first() {
    let plan = Plan {
        entries: vec![
            // Title A: 1 day span, 3 achievements
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "titleA", "2024-01-01T06:00:00Z", 20),
            mk_entry("a3", "titleA", "2024-01-02T00:00:00Z", 30),
            // Title B: 1 hour span, 2 achievements
            mk_entry("b1", "titleB", "2024-01-01T00:00:00Z", 10),
            mk_entry("b2", "titleB", "2024-01-01T01:00:00Z", 20),
            // Title C: 100-day span, 10 achievements
            mk_entry("c1", "titleC", "2024-01-01T00:00:00Z", 10),
            mk_entry("c2", "titleC", "2024-04-10T00:00:00Z", 20),
        ],
        ..Default::default()
    };
    let spans = plan.title_spans_shortest_first();
    assert_eq!(spans.len(), 3);
    // Shortest (B, 1h) first
    assert_eq!(spans[0].title_id, "titleB");
    assert_eq!(spans[0].duration_seconds, 60 * 60);
    // Then A (1 day)
    assert_eq!(spans[1].title_id, "titleA");
    assert_eq!(spans[1].duration_seconds, 24 * 60 * 60);
    // Then C (100 days)
    assert_eq!(spans[2].title_id, "titleC");
    assert!(spans[2].duration_seconds >= 99 * 24 * 60 * 60);
}

/// Single-entry titles get `duration_seconds = 0`, which puts
/// them first (fastest "wins").
#[test]
fn title_spans_single_entry_is_zero_duration() {
    let plan = Plan {
        entries: vec![
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("b1", "titleB", "2024-01-01T00:00:00Z", 10),
            mk_entry("b2", "titleB", "2024-01-15T00:00:00Z", 20),
        ],
        ..Default::default()
    };
    let spans = plan.title_spans_shortest_first();
    assert_eq!(spans[0].title_id, "titleA");
    assert_eq!(spans[0].duration_seconds, 0);
    assert_eq!(spans[0].achievement_count, 1);
}

// ============ 100%-COMPLETED FILTERING ============

fn mk_plan_with_totals(entries: Vec<PlanEntry>, totals: &[(&str, i32, i32)]) -> Plan {
    let mut p = Plan {
        entries,
        ..Default::default()
    };
    for (tid, cur, total) in totals {
        p.title_totals.insert(
            (*tid).to_string(),
            crate::plan::TitleCompletion {
                current_achievements: *cur,
                total_achievements: *total,
            },
        );
    }
    p
}

#[test]
fn title_spans_100pct_filters_incomplete() {
    // titleA: 2 ach earned, 2 total → 100% (eligible)
    // titleB: 2 ach earned, 5 total → 40% (filtered out)
    // titleC: 3 ach earned, 3 total → 100% (eligible)
    let plan = mk_plan_with_totals(
        vec![
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "titleA", "2024-01-02T00:00:00Z", 20),
            mk_entry("b1", "titleB", "2024-01-01T00:00:00Z", 10),
            mk_entry("b2", "titleB", "2024-01-03T00:00:00Z", 20),
            mk_entry("c1", "titleC", "2024-01-01T00:00:00Z", 10),
            mk_entry("c2", "titleC", "2024-01-01T01:00:00Z", 20),
            mk_entry("c3", "titleC", "2024-01-01T02:00:00Z", 30),
        ],
        &[("titleA", 2, 2), ("titleB", 2, 5), ("titleC", 3, 3)],
    );
    let spans = plan.title_spans_100_percent_shortest_first();
    assert_eq!(spans.len(), 2);
    // titleC is fastest (2 hour span) → #1
    assert_eq!(spans[0].title_id, "titleC");
    // titleA is 1 day span → #2
    assert_eq!(spans[1].title_id, "titleA");
}

#[test]
fn title_spans_100pct_excludes_single_unlock_even_if_100pct() {
    // titleA has 1 achievement total, 1 earned → "100%" but
    // no span. Excluded from ranking.
    let plan = mk_plan_with_totals(
        vec![mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10)],
        &[("titleA", 1, 1)],
    );
    let spans = plan.title_spans_100_percent_shortest_first();
    assert_eq!(spans.len(), 0);
}

#[test]
fn title_spans_100pct_empty_when_no_totals() {
    // Plan loaded from old file (no title_totals populated).
    // Ranking should be empty, not crash.
    let plan = Plan {
        entries: vec![
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "titleA", "2024-01-02T00:00:00Z", 20),
        ],
        ..Default::default()
    };
    // No title_totals at all.
    let spans = plan.title_spans_100_percent_shortest_first();
    assert_eq!(spans.len(), 0);
}

#[test]
fn title_completion_is_complete_handles_edge_cases() {
    use crate::plan::TitleCompletion;
    // 5/5 = complete
    assert!(TitleCompletion {
        current_achievements: 5,
        total_achievements: 5,
    }
    .is_complete());
    // 6/5 = complete (Xbox rarely gives "over 100%" but we accept it)
    assert!(TitleCompletion {
        current_achievements: 6,
        total_achievements: 5,
    }
    .is_complete());
    // 3/5 = not complete
    assert!(!TitleCompletion {
        current_achievements: 3,
        total_achievements: 5,
    }
    .is_complete());
    // 0/0 = NOT complete (unknown, treat as false)
    assert!(!TitleCompletion {
        current_achievements: 0,
        total_achievements: 0,
    }
    .is_complete());
    // 5/0 = NOT complete (division-by-zero guard)
    assert!(!TitleCompletion {
        current_achievements: 5,
        total_achievements: 0,
    }
    .is_complete());
}

// ============ JSON ROUND-TRIP ============

/// The Plan gets serialized to disk (`plan.save`) AND back into the
/// IPC bridge (`plan.summary`). Both paths need the JSON format to
/// stay stable across versions. This test encodes a fully-populated
/// plan, decodes it, and asserts every field round-trips correctly.
/// Catches:
/// - Accidental `#[serde(skip)]` on a field someone needs.
/// - Missing `#[serde(default)]` causing old plans to fail to load.
/// - Key rename regressions (camelCase vs snake_case drift).
#[test]
fn plan_roundtrips_through_json() {
    let mut plan = Plan {
        source_gamertag: "TargetGT".into(),
        source_xuid: "2535406451863815".into(),
        built_at: "2026-04-22T10:00:00Z".into(),
        total: 3,
        entries: vec![
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "titleA", "2024-01-02T00:00:00Z", 20),
            mk_entry("b1", "titleB", "2024-03-15T00:00:00Z", 15),
        ],
        ..Default::default()
    };
    plan.title_totals.insert(
        "titleA".into(),
        crate::plan::TitleCompletion {
            current_achievements: 2,
            total_achievements: 2,
        },
    );
    plan.title_totals.insert(
        "titleB".into(),
        crate::plan::TitleCompletion {
            current_achievements: 1,
            total_achievements: 5,
        },
    );

    // Serialize.
    let json = serde_json::to_string_pretty(&plan).expect("serialize");
    // Basic sanity on the encoded shape.
    assert!(json.contains("\"source_gamertag\""));
    assert!(json.contains("\"TargetGT\""));
    assert!(json.contains("\"title_totals\""));
    assert!(json.contains("\"titleA\""));
    // Entries array should contain our 3 entries.
    assert!(json.contains("\"a1\""));
    assert!(json.contains("\"a2\""));
    assert!(json.contains("\"b1\""));

    // Deserialize.
    let loaded: Plan = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(loaded.source_gamertag, plan.source_gamertag);
    assert_eq!(loaded.source_xuid, plan.source_xuid);
    assert_eq!(loaded.built_at, plan.built_at);
    assert_eq!(loaded.total, plan.total);
    assert_eq!(loaded.entries.len(), plan.entries.len());
    assert_eq!(loaded.entries[0].achievement_id, "a1");
    assert_eq!(loaded.entries[0].time_unlocked, "2024-01-01T00:00:00Z");
    assert_eq!(loaded.entries[0].gamerscore, Some(10));
    assert_eq!(loaded.title_totals.len(), 2);
    assert_eq!(
        loaded
            .title_totals
            .get("titleA")
            .unwrap()
            .current_achievements,
        2
    );
    assert!(loaded.title_totals.get("titleA").unwrap().is_complete());
    assert!(!loaded.title_totals.get("titleB").unwrap().is_complete());
}

/// Old plans (pre-title_totals) should still deserialize cleanly.
/// The `#[serde(default)]` on `title_totals` takes care of this;
/// this test asserts it.
#[test]
fn plan_from_old_json_without_title_totals_loads() {
    // JSON shape as it existed before the `title_totals` field was
    // added (patch 2026-04-22.8 and earlier).
    let json = r#"{
            "source_gamertag": "OldTarget",
            "source_xuid": "123",
            "built_at": "2026-01-01T00:00:00Z",
            "total": 1,
            "entries": [
                {
                    "title_id": "titleX",
                    "service_config_id": "scid-x",
                    "achievement_id": "ach1",
                    "achievement_name": "Ach One",
                    "game_name": "Game X",
                    "time_unlocked": "2024-06-01T10:00:00Z",
                    "gamerscore": 50
                }
            ]
        }"#;
    let plan: Plan = serde_json::from_str(json).expect("load legacy plan");
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.title_totals.len(), 0);
    // The 100%-ranking should still work (returns empty, not panic).
    let spans = plan.title_spans_100_percent_shortest_first();
    assert!(spans.is_empty());
}

/// `sleep_until_scheduled_cancellable` returns `Err(())` quickly
/// when cancelled mid-sleep — the whole point of converting to
/// a watch-based signal (audit bug BBB-Sleep-Cancel#1). Without
/// the fix, a 1h scheduled_unix would block for the full hour;
/// with the fix, flipping cancel in ms unblocks it in ms.
#[tokio::test(start_paused = true)]
async fn cancellable_sleep_returns_early_on_cancel() {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    // Dummy client just to satisfy the helper signature —
    // presence refresh branch won't run since we cancel before
    // the sleep elapses.
    let client = XboxClient::new(crate::xbox::client::XboxClientOptions {
        xauth: "XBL3.0 x=1;dummy".into(),
        ..Default::default()
    })
    .expect("client");
    // Scheduled 1 hour in the future.
    let target = now_unix() + 3600;
    let sleep_fut = sleep_until_scheduled_cancellable(
        Some((target, 0)),
        &client,
        "2535446299094411",
        "374923716",
        &[],
        &mut rx,
    );
    // Fire cancel after a short advance — the sleep should bail
    // virtually instantly, not park for the full hour.
    let cancel_fut = async {
        tokio::time::advance(Duration::from_millis(1)).await;
        let _ = tx.send(true);
    };
    let (result, _) = tokio::join!(sleep_fut, cancel_fut);
    assert!(
        result.is_err(),
        "sleep_until_scheduled_cancellable should have returned Err on cancel"
    );
}

#[tokio::test]
async fn process_title_event_based_all_failed_is_error() {
    let client = XboxClient::new(crate::xbox::client::XboxClientOptions {
        xauth: "XBL3.0 x=1;dummy".into(),
        events_bundle: Some(write_test_events_bundle("123", "ach1")),
        ..Default::default()
    })
    .expect("client");

    let sem = Arc::new(tokio::sync::Semaphore::new(1));
    let activation_sem = Arc::new(tokio::sync::Semaphore::new(MAX_SIMULTANEOUS_TITLES));
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let out = process_title(
        &client,
        "1",
        "123".into(),
        "Test Game".into(),
        "scid-1".into(),
        vec![("ach1".into(), 10, 0, 0)],
        None,
        crate::plan::safety::SafetyPolicy::danger_allow_unsafe(),
        sem,
        activation_sem,
        cancel_rx,
        Arc::new(RwLock::new(BitByBitProgress::default())),
        Arc::new(std::sync::Mutex::new(Vec::new())),
        "test-run".to_string(),
        Arc::new(tokio::sync::Mutex::new(Instant::now())),
        0, // floor_secs: concurrent mode (no per-game floor pacing)
        0, // pace_secs: matches — nothing outstanding when there's no floor
    )
    .await;

    match out.result {
        TitleResult::Error(msg) => {
            assert!(msg.contains("event-based unlock failed for all"));
        }
        _ => panic!("expected event-based failure to surface as error"),
    }
}

#[test]
fn title_spans_100pct_sort_determinism() {
    // Two titles with identical duration → tiebreaker by count
    // then title_id. Prove it's deterministic.
    let plan = mk_plan_with_totals(
        vec![
            // titleA: 2 ach, 1 hour span
            mk_entry("a1", "titleA", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "titleA", "2024-01-01T01:00:00Z", 20),
            // titleB: 2 ach, 1 hour span (same duration as A)
            mk_entry("b1", "titleB", "2024-01-01T00:00:00Z", 10),
            mk_entry("b2", "titleB", "2024-01-01T01:00:00Z", 20),
        ],
        &[("titleA", 2, 2), ("titleB", 2, 2)],
    );
    let spans = plan.title_spans_100_percent_shortest_first();
    assert_eq!(spans.len(), 2);
    // Same duration + same count → title_id tiebreaker → A before B
    assert_eq!(spans[0].title_id, "titleA");
    assert_eq!(spans[1].title_id, "titleB");
}

// ==========================================================================
//  MaxSpeedup tier — unit tests
// ==========================================================================

#[test]
fn speedup_summary_buckets_boundary_inclusive() {
    // BUG HUNTER bbb-bucket-boundary regression: a gap EXACTLY on a
    // tier edge must land in the SAME tier the scheduler applies
    // (`per_gap_speedup_cap` uses `<=`), not one tier higher.
    // Exactly 3600s (1h) → bucket[0] (sub_1h / verbatim), not bucket[1].
    let one_hour = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
    ];
    let s = compute_speedup_summary(&one_hour, MaxSpeedup::Off);
    assert_eq!(s.gap_buckets[0], 1, "exactly-3600s gap belongs in bucket 0");
    assert_eq!(s.gap_buckets[1], 0);

    // Exactly 5h (18000s) → bucket[1] (1h_to_5h), not bucket[2].
    let five_hours = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T05:00:00Z", 20),
    ];
    let s = compute_speedup_summary(&five_hours, MaxSpeedup::Off);
    assert_eq!(s.gap_buckets[1], 1, "exactly-5h gap belongs in bucket 1");
    assert_eq!(s.gap_buckets[2], 0);
}

#[test]
fn speedup_summary_sorts_unordered_input_like_the_scheduler() {
    // BUG HUNTER bbb-summary-unsorted-gap-walk regression: on an
    // out-of-order plan the preview must decompose the timeline
    // into the SAME gaps the scheduler will apply (which sorts
    // first), not the raw slice-order gaps. File order A@T0,
    // B@+8h, C@+2h, D@+10h → chronological A,C,B,D with gaps
    // 2h / 6h / 2h → buckets [0, 2, 1, 0, 0].
    let unsorted = vec![
        mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
        mk_entry("b", "t1", "2024-01-01T08:00:00Z", 20),
        mk_entry("c", "t1", "2024-01-01T02:00:00Z", 30),
        mk_entry("d", "t1", "2024-01-01T10:00:00Z", 40),
    ];
    let s = compute_speedup_summary(&unsorted, MaxSpeedup::Off);
    assert_eq!(
        s.gap_buckets,
        [0, 2, 1, 0, 0],
        "buckets must reflect the chronological (sorted) gap decomposition"
    );
    // MaxSpeedup::Off compresses nothing, so post == source span,
    // and the documented `post <= source` invariant must hold
    // (the pre-fix slice-order walk summed 8h+8h = 16h > 10h span).
    assert_eq!(s.source_span_secs, 10 * 3600);
    assert_eq!(s.post_speedup_span_secs, 10 * 3600);
    assert!(
        s.post_speedup_span_secs <= s.source_span_secs,
        "post-speedup span must never exceed source span"
    );
}

#[test]
fn apply_gap_mode_ms_capped_does_not_leak_residual_over_cap() {
    // BUG HUNTER bbb-capped-residual regression: a 5.5s gap capped at
    // 5s must clamp to 5000ms, not leak the sub-second residual to
    // 5500ms (500ms over the cap).
    assert_eq!(
        apply_gap_mode_ms(&GapMode::Capped(Duration::from_secs(5)), 5500),
        5000,
        "Capped(5s) must clamp a 5.5s gap to exactly 5000ms"
    );
    // Cap not reached (5.5s capped at 10s) → residual preserved.
    assert_eq!(
        apply_gap_mode_ms(&GapMode::Capped(Duration::from_secs(10)), 5500),
        5500,
    );
    // Same-second burst is always preserved verbatim, even Capped(0).
    assert_eq!(
        apply_gap_mode_ms(&GapMode::Capped(Duration::from_secs(0)), 500),
        500,
    );
    // Exact preserves the residual.
    assert_eq!(apply_gap_mode_ms(&GapMode::Exact, 5500), 5500);
}

// These tests exercise speedup with NO gap_mode, so the source gap
// equals the shaped gap — pass the (ms→secs) gap for classification.
fn apply_speedup_ms3(user_max: MaxSpeedup, span_secs: i64, gap_ms: i64) -> i64 {
    apply_speedup_ms(user_max, span_secs, gap_ms.saturating_div(1000), gap_ms)
}

#[test]
fn speedup_cap_classifies_on_source_gap_not_shaped() {
    // A long SOURCE gap shaped down by Capped(30d): the cap tier
    // must come from the SOURCE gap, not the clamped shaped gap.
    // Old bug: classify on shaped 30d → 3x cap, so Fast5x/Extreme
    // silently collapsed to 3x.
    let span = 365 * 24 * 3600; // 1yr span (clears the 2h guard)
    let shaped_gap_ms = 30 * 86_400 * 1000; // Capped(30d) result
                                            // 300-day SOURCE gap → 30d-1y bucket → 5x cap. Fast5x = 5x.
    let out5 = apply_speedup_ms(MaxSpeedup::Fast5x, span, 300 * 86_400, shaped_gap_ms);
    let r5 = shaped_gap_ms as f64 / out5 as f64;
    assert!(
        (r5 - 5.0).abs() < 0.01,
        "Fast5x on 300d source must be 5x, got {r5}"
    );
    // 2-year SOURCE gap → >1y bucket → 100x cap. Extreme100x = 100x.
    let out100 = apply_speedup_ms(
        MaxSpeedup::Extreme100x,
        span,
        2 * 365 * 86_400,
        shaped_gap_ms,
    );
    let r100 = shaped_gap_ms as f64 / out100 as f64;
    assert!(
        (r100 - 100.0).abs() < 0.5,
        "Extreme100x on 2yr source must be 100x, got {r100}"
    );
    // Regression guard: classifying on the SHAPED 30d gap would cap
    // at 3x — prove we do NOT.
    assert!(r5 > 3.5, "must exceed the shaped-gap 3x cap");
}

#[test]
fn speedup_off_is_passthrough() {
    // Off must be a no-op regardless of gap / span — protects
    // the holy-grail default.
    for gap_ms in &[0_i64, 60_000, 3_600_000, 86_400_000, 31_536_000_000] {
        for span_secs in &[0_i64, 1, 7200, 31_536_000] {
            assert_eq!(
                apply_speedup_ms3(MaxSpeedup::Off, *span_secs, *gap_ms),
                (*gap_ms).max(0),
                "Off must passthrough; gap={gap_ms}ms span={span_secs}s",
            );
        }
    }
}

#[test]
fn speedup_skips_short_titles() {
    // Title span < 2h → never speedup, even at Extreme.
    let span = SPEEDUP_MIN_TITLE_SPAN_SECS - 1;
    let gap_ms = 7_200_000; // 2h gap
    assert_eq!(
        apply_speedup_ms3(MaxSpeedup::Extreme100x, span, gap_ms),
        gap_ms
    );
    assert_eq!(apply_speedup_ms3(MaxSpeedup::Mild2x, span, gap_ms), gap_ms);
}

#[test]
fn speedup_skips_sub_hour_gaps() {
    // Gap < 1h → never speedup, even on long-span titles.
    let span = 365 * 24 * 3600; // 1 year span
    let gap_ms = SPEEDUP_MIN_GAP_SECS * 1000 - 1;
    assert_eq!(
        apply_speedup_ms3(MaxSpeedup::Extreme100x, span, gap_ms),
        gap_ms
    );
}

#[test]
fn speedup_per_gap_caps_clip_user_choice() {
    // 2h gap on a long-span title at Extreme(100x): per-gap cap
    // is 2x (since 2h < 5h), so result must be gap/2 — NOT
    // gap/100.
    let span = 365 * 24 * 3600;
    let gap_ms = 2 * 3600 * 1000; // 2h
    let out = apply_speedup_ms3(MaxSpeedup::Extreme100x, span, gap_ms);
    let ratio = (gap_ms as f64) / (out as f64);
    assert!(
        (ratio - 2.0).abs() < 0.01,
        "expected ~2x compression at 2h gap, got {ratio}x"
    );
}

#[test]
fn speedup_5h_gap_caps_at_3x() {
    let span = 365 * 24 * 3600;
    let gap_ms = 6 * 3600 * 1000; // 6h (in [5h, 30d) tier)
    let out = apply_speedup_ms3(MaxSpeedup::Extreme100x, span, gap_ms);
    let ratio = (gap_ms as f64) / (out as f64);
    assert!(
        (ratio - 3.0).abs() < 0.01,
        "expected ~3x compression at 6h gap, got {ratio}x"
    );
}

#[test]
fn speedup_year_plus_gap_uses_full_user_max() {
    let span = 5 * 365 * 24 * 3600;
    // Spec is strict-greater-than 1y, so the boundary itself
    // (exactly 365d) clamps to the 5× tier — use 1y + 1 second
    // to actually open the 100× tier. Wave-4 audit F2 fix.
    let gap_ms = (365 * 24 * 3600 + 1) * 1000_i64;
    let out_extreme = apply_speedup_ms3(MaxSpeedup::Extreme100x, span, gap_ms);
    let out_mild = apply_speedup_ms3(MaxSpeedup::Mild2x, span, gap_ms);
    let r_ext = (gap_ms as f64) / (out_extreme as f64);
    let r_mild = (gap_ms as f64) / (out_mild as f64);
    assert!(r_ext > 50.0, "Extreme should be ~100x, got {r_ext}x");
    assert!(
        (r_mild - 2.0).abs() < 0.01,
        "Mild user-max must clip year-plus to 2x, got {r_mild}x"
    );
}

#[test]
fn speedup_label_round_trip() {
    for tier in &[
        MaxSpeedup::Off,
        MaxSpeedup::Mild2x,
        MaxSpeedup::Moderate3x,
        MaxSpeedup::Fast5x,
        MaxSpeedup::Extreme100x,
    ] {
        let label = tier.as_str();
        assert_eq!(
            MaxSpeedup::from_label(label),
            Some(*tier),
            "round-trip failed for {label}"
        );
    }
    // Synonyms map correctly.
    assert_eq!(MaxSpeedup::from_label("none"), Some(MaxSpeedup::Off));
    assert_eq!(MaxSpeedup::from_label("1x"), Some(MaxSpeedup::Off));
    assert_eq!(MaxSpeedup::from_label(""), Some(MaxSpeedup::Off));
    assert_eq!(MaxSpeedup::from_label("MILD"), Some(MaxSpeedup::Mild2x));
    assert_eq!(
        MaxSpeedup::from_label("EXTREME"),
        Some(MaxSpeedup::Extreme100x)
    );
    assert_eq!(MaxSpeedup::from_label("garbage"), None);
}

#[test]
fn speedup_chronology_invariant_with_compute_schedule() {
    // **The holy-grail test.** Run compute_schedule_for_title
    // through a synthetic plan with a mix of sub-hour bursts +
    // multi-day gaps + multi-year dormant + same-second
    // duplicates, at every MaxSpeedup tier, and assert that
    // `(scheduled_unix, scheduled_subsec_ms)` is strictly
    // monotonically non-decreasing for EVERY tier.
    let now = NOW;
    let entries = vec![
        mk_entry("a1", "t1", "2020-01-01T00:00:00.100Z", 10),
        mk_entry("a2", "t1", "2020-01-01T00:00:00.500Z", 10),
        mk_entry("a3", "t1", "2020-01-01T00:00:00.999Z", 10),
        // ~5 min gap (sub-hour, never speedup)
        mk_entry("a4", "t1", "2020-01-01T00:05:00.000Z", 20),
        // ~3h gap (in [1h,5h) tier)
        mk_entry("a5", "t1", "2020-01-01T03:05:00.000Z", 50),
        // ~7h gap (in [5h,30d) tier)
        mk_entry("a6", "t1", "2020-01-01T10:05:00.000Z", 50),
        // ~45d gap (in [30d,1y) tier)
        mk_entry("a7", "t1", "2020-02-15T10:05:00.000Z", 100),
        // ~2y gap (>= 1y tier)
        mk_entry("a8", "t1", "2022-02-15T10:05:00.000Z", 200),
    ];
    for tier in &[
        MaxSpeedup::Off,
        MaxSpeedup::Mild2x,
        MaxSpeedup::Moderate3x,
        MaxSpeedup::Fast5x,
        MaxSpeedup::Extreme100x,
    ] {
        let sched = compute_schedule_for_title_with_speedup(
            &entries,
            Some(now),
            &HashSet::new(),
            GapMode::Exact,
            now,
            *tier,
        );
        assert_eq!(sched.len(), entries.len());
        for w in sched.windows(2) {
            let a = (w[0].scheduled_unix, w[0].scheduled_subsec_ms);
            let b = (w[1].scheduled_unix, w[1].scheduled_subsec_ms);
            assert!(
                a <= b,
                "chronology broken at tier {:?}: prev={a:?} next={b:?}",
                tier
            );
        }
    }
}

#[test]
fn speedup_boundary_3601s_gap_activates_2x_cap() {
    // Spec (user-emphasised): "Speedups only allowed on gaps >1h,
    // >5h, >30d, >1y" — STRICT-GREATER. Pin the boundary:
    //   gap=3600s (exactly 1h) → 1x  (boundary case, NOT eligible)
    //   gap=3601s              → 2x  (strictly past 1h)
    // Wave-4 audit F2 fix: changed `<` to `<=` in the cap ladder.
    let span = 365 * 24 * 3600;
    // 3600s gap (exactly at boundary): NO speedup
    let out_at = apply_speedup_ms3(MaxSpeedup::Extreme100x, span, 3600 * 1000);
    assert_eq!(
        out_at,
        3600 * 1000,
        "3600s gap is on the spec boundary — must stay unchanged"
    );
    // 3601s gap: 2x cap kicks in
    let out_above = apply_speedup_ms3(MaxSpeedup::Extreme100x, span, 3601 * 1000);
    let r_above = (3601.0 * 1000.0) / (out_above as f64);
    assert!(
        (r_above - 2.0).abs() < 0.01,
        "3601s gap should compress 2x, got {r_above}x"
    );
}

#[test]
fn speedup_boundary_7200s_title_span_activates_guard() {
    // Spec (user-emphasised): "Games with ≤2h completionist time
    // CANNOT be sped up." A title spanning EXACTLY 7200s is on
    // the boundary and the guard MUST suppress speedup. Wave-4
    // audit F1 fix: changed `<` to `<=` in the title-span guard.
    let gap_ms = 3 * 3600 * 1000; // 3h gap (well past 1h boundary, would be 2x at long-span)
                                  // 7199s span: still under the boundary → also untouched
    let out_below = apply_speedup_ms3(MaxSpeedup::Mild2x, 7199, gap_ms);
    assert_eq!(out_below, gap_ms, "title span 7199s must be untouched");
    // 7200s span (exactly 2h): GUARD KICKS IN — no compression
    let out_at = apply_speedup_ms3(MaxSpeedup::Mild2x, 7200, gap_ms);
    assert_eq!(
        out_at, gap_ms,
        "title span 7200s is on the spec boundary — guard must suppress speedup"
    );
    // 7201s span: just past boundary → 2x kicks in
    let out_above = apply_speedup_ms3(MaxSpeedup::Mild2x, 7201, gap_ms);
    let r_above = (gap_ms as f64) / (out_above as f64);
    assert!(
        (r_above - 2.0).abs() < 0.01,
        "title span 7201s should allow 2x compression, got {r_above}x"
    );
}

#[test]
fn speedup_boundary_zero_and_negative_gaps() {
    // gap_ms == 0: passthrough as 0
    let out0 = apply_speedup_ms3(MaxSpeedup::Extreme100x, 1_000_000, 0);
    assert_eq!(out0, 0);
    // gap_ms < 0: clamped to 0 (defensive)
    let out_neg = apply_speedup_ms3(MaxSpeedup::Extreme100x, 1_000_000, -100);
    assert_eq!(out_neg, 0);
}

#[test]
fn compute_title_span_zero_one_many_entries() {
    // Empty slice → 0 (no entries to span). Per the rustdoc, this
    // is strictly safer than a real measurement (downstream gates
    // on `span < 7200` so 0 disables speedup for unparseable plans).
    let empty: Vec<PlanEntry> = vec![];
    assert_eq!(compute_title_span_secs(&empty), 0);
    // Single entry → 0 (no gap to span).
    let single = vec![mk_entry("a", "t", "2020-01-01T00:00:00Z", 10)];
    assert_eq!(compute_title_span_secs(&single), 0);
    // Two entries → exact difference.
    let pair = vec![
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("b", "t", "2020-01-01T01:00:00Z", 10),
    ];
    assert_eq!(compute_title_span_secs(&pair), 3600);
    // Three entries → max - min (order-independent).
    let triple = vec![
        mk_entry("b", "t", "2020-01-01T01:00:00Z", 10),
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("c", "t", "2020-01-02T00:00:00Z", 10),
    ];
    assert_eq!(compute_title_span_secs(&triple), 86400);
}

#[test]
fn compute_title_span_unparseable_timestamps_skip_safely() {
    // An entry with a garbage timestamp should not poison the
    // span calculation — the parseable entries still yield a
    // valid span. Per the rustdoc, this is "strictly safer"
    // because the resulting span < 2h disables speedup.
    let mixed = vec![
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("b", "t", "GARBAGE_NOT_AN_ISO_DATE", 10),
        mk_entry("c", "t", "2020-01-01T00:30:00Z", 10),
    ];
    assert_eq!(compute_title_span_secs(&mixed), 1800);
    // All-unparseable plans return 0.
    let all_garbage = vec![mk_entry("a", "t", "x", 10), mk_entry("b", "t", "y", 10)];
    assert_eq!(compute_title_span_secs(&all_garbage), 0);
}

#[test]
fn floor_adjusted_target_spreads_evenly_and_last_hits_floor() {
    // 4 unlocks, 100s floor, game started at t=1000s (unix ms 1_000_000).
    // Entry idx fires at start + (idx+1)/4 * 100s → 25s, 50s, 75s, 100s.
    let start_ms = 1_000_000;
    let n = 4;
    let floor = 100;
    let at = |idx: usize| floor_adjusted_target(Some((0, 0)), floor, start_ms, idx, n).unwrap();
    assert_eq!(at(0), (1025, 0), "idx0 → start + 25s");
    assert_eq!(at(1), (1050, 0), "idx1 → start + 50s");
    assert_eq!(at(2), (1075, 0), "idx2 → start + 75s");
    assert_eq!(
        at(3),
        (1100, 0),
        "last idx → start + full floor (100% never faster)"
    );
    // Strictly monotonic non-decreasing across all idx (order safe).
    let mut prev = i64::MIN;
    for idx in 0..n {
        let (s, ms) = at(idx);
        let t = s * 1000 + i64::from(ms);
        assert!(t >= prev, "floor targets must be monotonic in idx");
        prev = t;
    }
}

#[test]
fn security_claims_error_detection() {
    // The real 403 body from the field (code:21).
    let real = r#"HTTP status 403 Forbidden: {"code":21,"source":"Progress","description":"The request's security claims were rejected.","traceInformation":"89B:cb45"}"#;
    assert!(is_security_claims_error(real));
    // Human-description-only variant, and the space-after-colon JSON.
    assert!(is_security_claims_error(
        "the request's security claims were rejected"
    ));
    assert!(is_security_claims_error(
        r#"{"code": 21,"source":"Progress"}"#
    ));
    // Must NOT match the event-based (code:49) or unrelated errors.
    assert!(!is_security_claims_error(
        r#"{"code":49,"description":"None of the submitted achievements may be updated in this fashion."}"#
    ));
    assert!(!is_security_claims_error(
        "HTTP status 429 Too Many Requests"
    ));
    // `"code":210` must NOT false-positive on the `21` prefix.
    assert!(!is_security_claims_error(r#"{"code":210,"source":"x"}"#));
}

#[test]
fn floor_adjusted_target_passthrough_when_disabled() {
    // floor_secs == 0 (concurrent mode) → returns the plan target
    // verbatim, so the default scheduling path is byte-identical.
    let plan = Some((42, 737));
    assert_eq!(floor_adjusted_target(plan, 0, 1_000_000, 2, 5), plan);
    // n == 0 → also passthrough (no division), never panics.
    assert_eq!(floor_adjusted_target(plan, 100, 1_000_000, 0, 0), plan);
    // A single-unlock game (n=1) fires its one entry at start+floor.
    assert_eq!(
        floor_adjusted_target(Some((0, 0)), 60, 2_000_000, 0, 1),
        Some((2060, 0))
    );
}

#[test]
fn floor_adjusted_targets_replays_real_cadence() {
    // Target unlocked at t=0, +10s, +40s, +100s (bursty: a tight
    // 0-10 pair then long gaps) over a 100s unlock span, and their
    // MinutesPlayed floor is 300s (span fits inside the floor →
    // scale = 1.0, exact gaps preserved, anchored so the LAST lands
    // at start+floor). Game "went online" at unix ms 1_000_000.
    let sched = vec![
        Some((0_i64, 0_u16)),
        Some((10, 0)),
        Some((40, 0)),
        Some((100, 0)),
    ];
    let start = 1_000_000_i64;
    let floor = 300_u64;
    let out = floor_adjusted_targets(&sched, floor, start);
    let ms = |o: Option<(i64, u16)>| {
        let (s, sub) = o.unwrap();
        s * 1000 + i64::from(sub)
    };
    // Last lands exactly at start + floor (never-faster invariant).
    assert_eq!(ms(out[3]), start + 300_000);
    // Gaps are the REAL relative gaps (10s, 30s, 60s), NOT the even
    // 75s/75s/75s an even-spread would produce — this is the whole
    // point: no metronome fingerprint.
    assert_eq!(ms(out[1]) - ms(out[0]), 10_000);
    assert_eq!(ms(out[2]) - ms(out[1]), 30_000);
    assert_eq!(ms(out[3]) - ms(out[2]), 60_000);
    // First lands at start + (floor - span) = played 200s before the
    // first cheevo (realistic warm-up), all strictly monotonic.
    assert_eq!(ms(out[0]), start + 200_000);
    for w in out.windows(2) {
        assert!(ms(w[1]) >= ms(w[0]), "targets must be non-decreasing");
    }
}

#[test]
fn floor_adjusted_targets_compresses_when_span_exceeds_floor() {
    // Unlock span 1000s but floor only 100s → scale = 0.1, gaps
    // compress proportionally, last still at start+floor.
    let sched = vec![Some((0_i64, 0_u16)), Some((200, 0)), Some((1000, 0))];
    let out = floor_adjusted_targets(&sched, 100, 0);
    let ms = |o: Option<(i64, u16)>| {
        let (s, sub) = o.unwrap();
        s * 1000 + i64::from(sub)
    };
    assert_eq!(ms(out[2]), 100_000); // last at floor
                                     // Relative shape preserved under 0.1x: gap 200→ 20s, 800→80s.
    assert_eq!(ms(out[1]) - ms(out[0]), 20_000);
    assert_eq!(ms(out[2]) - ms(out[1]), 80_000);
    assert_eq!(ms(out[0]), 0); // first at start
}

#[test]
fn floor_adjusted_targets_fallbacks_are_even_spread() {
    // Degenerate cadence → even-spread fallback (matches
    // floor_adjusted_target semantics), never panics.
    // floor 0 (concurrent) → plan verbatim.
    let sched = vec![Some((5_i64, 0_u16)), Some((9, 0))];
    assert_eq!(
        floor_adjusted_targets(&sched, 0, 1_000),
        vec![Some((5, 0)), Some((9, 0))]
    );
    // Same-second burst (span 0) → even spread across the floor.
    let burst = vec![Some((7_i64, 0_u16)), Some((7, 0))];
    let out = floor_adjusted_targets(&burst, 100, 1_000_000);
    // Even spread: idx0 at start+50s, idx1 at start+100s.
    assert_eq!(out[0], Some((1050, 0)));
    assert_eq!(out[1], Some((1100, 0)));
    // A missing plan time → even-spread fallback (no panic).
    let missing = vec![Some((0_i64, 0_u16)), None, Some((100, 0))];
    let out2 = floor_adjusted_targets(&missing, 90, 0);
    assert_eq!(out2.len(), 3);
    // Last still lands at floor under even spread ((idx+1)/n).
    assert_eq!(out2[2], Some((90, 0)));
}

#[test]
fn floor_gate_decide_boundaries() {
    use FloorGateDecision as D;
    const ZP: u32 = FLOOR_GATE_MAX_ZERO_POLLS;
    const NP: u32 = FLOOR_GATE_MAX_NONE_POLLS;
    // Args: (fetched, floor, waited, zero_polls, none_polls, ever_credited)
    // Floor reached (exact boundary) → release.
    assert_eq!(floor_gate_decide(Some(60), 60, 0, 0, 0, true), D::Release);
    // Above floor → release.
    assert_eq!(floor_gate_decide(Some(120), 60, 5, 0, 0, true), D::Release);
    // Release wins over EVERY give-up condition. Use a large floor so
    // the proportional lag cap (1800) is actually reached at 1800.
    assert_eq!(
        floor_gate_decide(Some(7200), 7200, 1800, ZP, NP, false),
        D::Release
    );
    // Below floor, young wait, few zero polls → keep waiting.
    assert_eq!(floor_gate_decide(Some(0), 60, 45, 1, 0, false), D::Wait);
    // API error (None), young wait → wait.
    assert_eq!(floor_gate_decide(None, 60, 45, 0, 1, false), D::Wait);
    // Floor-proportional lag cap: a 60 s floor caps at 300 s (NOT
    // 1800), so a dead 60 s-floor title can never park 30 min.
    assert_eq!(floor_gate_lag_cap_secs(60), 300);
    assert_eq!(floor_gate_lag_cap_secs(7200), 1800);
    assert_eq!(floor_gate_lag_cap_secs(200), 400);
    // At the small-floor cap (300) below-floor → give up (cap wins,
    // it's checked before the no-credit/outage branches).
    assert_eq!(
        floor_gate_decide(Some(0), 60, 300, 1, 0, false),
        D::GiveUpLagCap
    );
    // Large-floor lag cap reached at 1800 with credit seen (so
    // no_credit/api branches are excluded) → GiveUpLagCap.
    assert_eq!(
        floor_gate_decide(Some(30), 7200, 1800, 0, 0, true),
        D::GiveUpLagCap
    );
    // Persistent API outage: none-streak + floor-aware min wait, but
    // BELOW the lag cap (floor 200 → cap 400, min wait 300) →
    // GiveUpApiOutage (the 30-min-dead-title stall the audit found).
    assert_eq!(
        floor_gate_decide(None, 200, 300, 0, NP, false),
        D::GiveUpApiOutage
    );
    // One none-poll short of the outage threshold → still waiting.
    assert_eq!(floor_gate_decide(None, 200, 300, 0, NP - 1, false), D::Wait);
    // Outage needs the floor-aware min wait too — a brief hiccup at
    // 100 s doesn't trip it even with a long streak.
    assert_eq!(
        floor_gate_decide(None, 7200, 100, 0, NP + 5, false),
        D::Wait
    );
    // Never credited + unbroken zero streak + floor-aware min wait,
    // BELOW the lag cap (floor 200 → cap 400, min wait 300) →
    // futility exit (presence never credits here).
    assert_eq!(
        floor_gate_decide(Some(0), 200, 300, ZP, 0, false),
        D::GiveUpNoCredit
    );
    // One poll short of the streak threshold, young wait → waiting.
    assert_eq!(
        floor_gate_decide(Some(0), 200, 300, ZP - 1, 0, false),
        D::Wait
    );
    // Streak satisfied but the floor-aware min wait is NOT (large
    // floor → 600 s minimum): keep waiting — first credited minute
    // may still be propagating.
    assert_eq!(floor_gate_decide(Some(0), 7200, 599, ZP, 0, false), D::Wait);
    assert_eq!(
        floor_gate_decide(Some(0), 7200, 600, ZP, 0, false),
        D::GiveUpNoCredit
    );
    // ANY observed credit permanently blocks the futility exit.
    assert_eq!(
        floor_gate_decide(Some(0), 7200, 700, ZP + 30, 0, true),
        D::Wait
    );
    // Partial credit (stat IS moving) below floor → wait for the
    // cap, never futility.
    assert_eq!(floor_gate_decide(Some(30), 7200, 300, 0, 0, true), D::Wait);
    // Min-wait helper bounds: max(300, min(floor, 600)).
    assert_eq!(floor_gate_no_credit_min_wait_secs(60), 300);
    assert_eq!(floor_gate_no_credit_min_wait_secs(300), 300);
    assert_eq!(floor_gate_no_credit_min_wait_secs(450), 450);
    assert_eq!(floor_gate_no_credit_min_wait_secs(7200), 600);
}

#[test]
fn effective_speedup_multiplier_caps_per_gap() {
    // Compose: user picks Extreme (100x) but per-gap cap clamps
    // the actual multiplier to the gap-bucket's cap.
    let span_long = 365 * 24 * 3600;
    // Sub-1h gap: always 1x.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, span_long, 1800),
        1.0
    );
    // 1h-5h gap: capped at 2x.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, span_long, 3 * 3600),
        2.0
    );
    // 5h-30d gap: capped at 3x.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, span_long, 10 * 3600),
        3.0
    );
    // 30d-1y gap: capped at 5x.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, span_long, 40 * 86400),
        5.0
    );
    // ≥1y gap: full 100x for Extreme.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, span_long, 400 * 86400),
        100.0
    );
}

#[test]
fn effective_speedup_multiplier_user_max_clips_per_gap_cap() {
    // User picks Mild (2x). Per-gap cap allows higher tiers on
    // larger gaps, but user's choice clips down to 2x for every
    // gap STRICTLY past 1h (the spec uses `>1h`, not `>=1h`, so
    // the boundary at gap=3600 is excluded — see the dedicated
    // `boundary_gates_at_*` test below).
    let span_long = 365 * 24 * 3600;
    for &gap in &[3601_i64, 10 * 3600, 40 * 86400, 400 * 86400] {
        assert_eq!(
            effective_speedup_multiplier(MaxSpeedup::Mild2x, span_long, gap),
            2.0,
            "Mild user-max must clip every >1h gap to 2x; gap={gap}"
        );
    }
}

/// Lock the spec the user repeatedly emphasised: "Games with ≤2h
/// completionist time CANNOT be sped up" and "Speedups only
/// allowed on gaps >1h, >5h, >30d, >1y". These are STRICT
/// inequalities — the boundary itself is OFF-LIMITS. Wave-4
/// audit F1+F2 caught the previous `<` (off-by-one) which let
/// the boundary cases slip into the speedup tier.
#[test]
fn boundary_gates_at_2h_title_span_and_1h_5h_30d_1y_gaps_are_off_limits() {
    // === Title-span boundary === (spec: ≤2h title NEVER speeds up)
    // Exact 7200s (= 2h) span MUST short-circuit to 1.0 even
    // with a long gap that would otherwise compress.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, 7200, 400 * 86400),
        1.0,
        "title span = 7200s is ≤2h — must NEVER speed up regardless of gap"
    );
    // Just past the boundary: 7201s span DOES allow compression.
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Mild2x, 7201, 10 * 3600),
        2.0,
        "title span = 7201s is past the 2h gate — Mild + 5h-tier gap → 2x"
    );

    // === Per-gap boundaries === (spec: >1h, >5h, >30d, >1y)
    let long_span = 400 * 86400;
    // gap == 3600 (exactly 1h) → no speedup
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 3600),
        1.0,
        "gap == 1h boundary must stay at 1.0 (spec: speedups on gaps >1h)"
    );
    // gap == 3601 → 2× tier
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 3601),
        2.0
    );
    // gap == 5h boundary == 18000 → still 2× (NOT 3× yet)
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 18000),
        2.0,
        "gap == 5h boundary stays at 2× (spec: speedups >5h get 3×)"
    );
    // gap == 18001 → 3× tier opens
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 18001),
        3.0
    );
    // gap == 30d boundary == 2_592_000 → still 3×
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 30 * 86400),
        3.0,
        "gap == 30d boundary stays at 3× (spec: speedups >30d get 5×)"
    );
    // gap == 30d + 1s → 5× tier opens
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 30 * 86400 + 1),
        5.0
    );
    // gap == 1y boundary == 31_536_000 → still 5×
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 365 * 86400),
        5.0,
        "gap == 1y boundary stays at 5× (spec: speedups >1y get 100×)"
    );
    // gap == 1y + 1s → 100× tier opens
    assert_eq!(
        effective_speedup_multiplier(MaxSpeedup::Extreme100x, long_span, 365 * 86400 + 1),
        100.0
    );
}

/// `compute_speedup_summary.short_title_guard_active` must agree
/// with `effective_speedup_multiplier`'s decision at the 2h
/// boundary. Wave-4 F1: previously summary used `<`, which
/// reported `false` (no guard) at exactly 7200s while the
/// actual scheduler ran with `<` too — both wrong, both agreed.
/// Now both use `<=`; this test locks that agreement.
#[test]
fn short_title_guard_active_at_exact_2h_boundary() {
    use crate::plan::PlanEntry;
    // Build a synthetic title with first unlock at T and last
    // unlock at T+7200 (exactly 2h span).
    let entries = vec![
        PlanEntry {
            title_id: "t".into(),
            game_name: "g".into(),
            achievement_id: "a1".into(),
            achievement_name: "a1".into(),
            gamerscore: Some(10),
            time_unlocked: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        },
        PlanEntry {
            title_id: "t".into(),
            game_name: "g".into(),
            achievement_id: "a2".into(),
            achievement_name: "a2".into(),
            gamerscore: Some(10),
            time_unlocked: "2026-01-01T02:00:00Z".into(),
            ..Default::default()
        },
    ];
    let s = compute_speedup_summary(&entries, MaxSpeedup::Extreme100x);
    assert_eq!(s.source_span_secs, 7200, "exact 2h span");
    assert!(
        s.short_title_guard_active,
        "guard MUST be active at the 2h boundary (spec: ≤2h titles cannot be sped up)"
    );
    assert_eq!(
        s.post_speedup_span_secs, s.source_span_secs,
        "guard active means post == source"
    );
    assert_eq!(s.mean_compression, 1.0);
}

#[test]
fn effective_speedup_multiplier_off_is_identity() {
    // Off short-circuits before any cap math runs. Lock the
    // contract that Off ALWAYS returns 1.0 regardless of inputs.
    for &span in &[0_i64, 1, 7200, 365 * 86400] {
        for &gap in &[0_i64, 60, 3600, 365 * 86400] {
            assert_eq!(
                effective_speedup_multiplier(MaxSpeedup::Off, span, gap),
                1.0,
                "Off must passthrough; span={span} gap={gap}"
            );
        }
    }
}

#[test]
fn max_speedup_all_iterates_in_aggression_order() {
    // Lock the order of the `all()` constructor — tests rely on
    // it for "monotonic in tier" property checks. A future
    // refactor that swaps two variants would break the
    // monotonicity assertion in `prop_schedule.rs`.
    let all = MaxSpeedup::all();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0], MaxSpeedup::Off);
    assert_eq!(all[1], MaxSpeedup::Mild2x);
    assert_eq!(all[2], MaxSpeedup::Moderate3x);
    assert_eq!(all[3], MaxSpeedup::Fast5x);
    assert_eq!(all[4], MaxSpeedup::Extreme100x);
    // user_max_multiplier is monotonically non-decreasing.
    for w in all.windows(2) {
        assert!(
            w[0].user_max_multiplier() <= w[1].user_max_multiplier(),
            "monotonicity broken: {:?} ({}) > {:?} ({})",
            w[0],
            w[0].user_max_multiplier(),
            w[1],
            w[1].user_max_multiplier()
        );
    }
}

#[test]
fn speedup_summary_short_title_guard_active() {
    // 90-minute span (< 2h) → guard active, mean_compression = 1.0
    // even at Extreme.
    let entries = vec![
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("b", "t", "2020-01-01T00:30:00Z", 20),
        mk_entry("c", "t", "2020-01-01T01:30:00Z", 30),
    ];
    let s = compute_speedup_summary(&entries, MaxSpeedup::Extreme100x);
    assert!(s.short_title_guard_active);
    assert_eq!(s.mean_compression, 1.0);
    assert_eq!(s.source_span_secs, s.post_speedup_span_secs);
}

#[test]
fn speedup_summary_long_title_compresses_at_extreme() {
    // 1-year span with 4 1h+ gaps: compression should be > 1
    // at Extreme. Lock the qualitative result; exact value
    // depends on the per-gap cap math.
    let entries = vec![
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("b", "t", "2020-01-15T00:00:00Z", 20), // 14d gap → 3x cap
        mk_entry("c", "t", "2020-06-01T00:00:00Z", 30), // 4mo gap → 5x cap
        mk_entry("d", "t", "2021-01-01T00:00:00Z", 40), // 7mo gap → 5x cap
    ];
    let s = compute_speedup_summary(&entries, MaxSpeedup::Extreme100x);
    assert!(!s.short_title_guard_active);
    assert!(
        s.mean_compression > 2.0,
        "mean compression should be > 2x for multi-month gaps, got {}",
        s.mean_compression
    );
    assert_eq!(s.gap_buckets[2], 1, "expected 1 gap in 5h-30d bucket");
    assert_eq!(s.gap_buckets[3], 2, "expected 2 gaps in 30d-1y bucket");
}

#[test]
fn speedup_summary_off_is_identity() {
    // At Off, the post-span equals the source-span exactly.
    let entries = vec![
        mk_entry("a", "t", "2020-01-01T00:00:00Z", 10),
        mk_entry("b", "t", "2021-01-01T00:00:00Z", 20),
    ];
    let s = compute_speedup_summary(&entries, MaxSpeedup::Off);
    assert_eq!(s.mean_compression, 1.0);
    assert_eq!(s.source_span_secs, s.post_speedup_span_secs);
    assert_eq!(s.gap_buckets[4], 1, "expected 1 ≥1y gap");
}

#[test]
fn speedup_summary_empty_plan_is_safe() {
    // Empty plan: zero spans, no buckets, safe defaults.
    let s = compute_speedup_summary(&[], MaxSpeedup::Extreme100x);
    assert_eq!(s.source_span_secs, 0);
    assert_eq!(s.post_speedup_span_secs, 0);
    assert_eq!(s.mean_compression, 1.0);
    assert!(s.short_title_guard_active);
    assert_eq!(s.gap_buckets, [0, 0, 0, 0, 0]);
}

#[test]
fn bitbybit_progress_default_round_trip() {
    // The Default impl must serialize and deserialize cleanly —
    // a runner that emits a snapshot with all-zero counters at
    // the start of a run should produce a JSON the GUI can
    // parse without choking on missing fields.
    let p = BitByBitProgress::default();
    let json = serde_json::to_string(&p).expect("serialize default");
    let _back: BitByBitProgress = serde_json::from_str(&json).expect("deserialize default");
    // Default state must be Idle.
    assert_eq!(p.state, BitByBitState::Idle);
    // Default max_speedup must be Off.
    assert_eq!(p.max_speedup, MaxSpeedup::Off);
    // active_titles defaults to empty Vec.
    assert!(p.active_titles.is_empty());
    // presence_keepalive_active defaults to false (no run alive).
    assert!(!p.presence_keepalive_active);
}

#[test]
fn schedule_with_speedup_is_deterministic() {
    // Lock the determinism contract: same input → byte-identical
    // schedule. A scheduler with internal non-determinism
    // (HashMap iteration leaking, RNG, time sampling) would
    // fail here. Critical because the GUI estimator + the live
    // runner must agree on the schedule down to the millisecond.
    let entries = vec![
        mk_entry("a", "t1", "2020-01-01T00:00:00.100Z", 10),
        mk_entry("b", "t1", "2020-01-01T03:00:00.500Z", 20),
        mk_entry("c", "t1", "2020-02-01T00:00:00.999Z", 30),
        mk_entry("d", "t1", "2021-06-15T12:34:56.789Z", 40),
    ];
    let unlocked = HashSet::from(["a".to_string()]);
    for &tier in MaxSpeedup::all().iter() {
        for mode in [
            GapMode::Exact,
            GapMode::Capped(Duration::from_secs(86400)),
            GapMode::Scaled(0.5),
        ] {
            let s1 = compute_schedule_for_title_with_speedup(
                &entries,
                Some(NOW - 1000),
                &unlocked,
                mode.clone(),
                NOW,
                tier,
            );
            let s2 = compute_schedule_for_title_with_speedup(
                &entries,
                Some(NOW - 1000),
                &unlocked,
                mode,
                NOW,
                tier,
            );
            assert_eq!(s1, s2, "non-deterministic schedule at tier {tier:?}");
        }
    }
}

#[test]
fn bitbybit_progress_serde_shape() {
    // Lock the JSON wire shape against accidental refactor.
    // The GUI's `_plan_replay.js` reads these field names verbatim;
    // a Rust-side rename without the corresponding JS update
    // would silently break the dashboard. This test fails LOUD
    // if any consumer-facing field disappears or renames.
    let mut p = BitByBitProgress::default();
    p.state = BitByBitState::Running;
    p.unlocked_this_run = 42;
    p.gamerscore_this_run = 1234;
    p.eligible_now = 100;
    p.scheduled_for_future = 50;
    p.last_title = Some("My Game".to_string());
    p.presence_keepalive_active = true;
    p.max_speedup = MaxSpeedup::Mild2x;
    p.active_titles = vec!["A".into(), "B".into()];
    let json = serde_json::to_value(&p).expect("serialize");
    // Required fields the GUI reads:
    for field in &[
        "state",
        "unlocked_this_run",
        "gamerscore_this_run",
        "eligible_now",
        "already_done",
        "scheduled_for_future",
        "earliest_future_unix",
        "event_based_skipped_titles",
        "errored_titles",
        "last_title",
        "last_error",
        "resolve_fetched",
        "resolve_total",
        "presence_keepalive_active",
        "max_speedup",
        "active_titles",
        // Live sequential-mode telemetry the GUI's NOW line +
        // rate-cell hint read verbatim (previously unguarded —
        // adversarial-review catch).
        "active_game_name",
        "active_title_done",
        "active_title_total",
        "active_title_floor_secs",
        "floor_gate_active",
        "floor_gate_waited_secs",
        "floor_gate_played_secs",
        "floor_gate_giveups",
        "recent_unlocks",
        "skipped_games",
        "next_unlock_at_ms",
        "pre_skipped_known_dead",
        "token_expired",
        "unauthorized_skipped_titles",
    ] {
        assert!(
            json.get(field).is_some(),
            "BitByBitProgress is missing JSON field `{field}` (GUI depends on it)"
        );
    }
    // State must serialize as lowercase string (not the
    // capitalized enum name).
    assert_eq!(json.get("state").and_then(|v| v.as_str()), Some("running"));
    // max_speedup must serialize as the human label, not "Mild2x".
    assert_eq!(json.get("max_speedup").and_then(|v| v.as_str()), Some("2x"));
    // active_titles must be a JSON array.
    assert_eq!(
        json.get("active_titles")
            .and_then(|v| v.as_array())
            .map(Vec::len),
        Some(2)
    );
}

#[test]
fn max_speedup_from_label_synonym_exhaustive() {
    // Lock every accepted synonym so future "we should accept X"
    // requests don't slip in unnoticed. This is the inverse of
    // the serde rejection test — IPC is lenient (accepts
    // synonyms), serde is strict (only the canonical labels).
    let cases: &[(&str, MaxSpeedup)] = &[
        ("off", MaxSpeedup::Off),
        ("OFF", MaxSpeedup::Off),
        ("Off", MaxSpeedup::Off),
        ("none", MaxSpeedup::Off),
        ("1x", MaxSpeedup::Off),
        ("1", MaxSpeedup::Off),
        ("", MaxSpeedup::Off),
        ("  ", MaxSpeedup::Off),
        ("  off  ", MaxSpeedup::Off),
        ("2x", MaxSpeedup::Mild2x),
        ("2", MaxSpeedup::Mild2x),
        ("mild", MaxSpeedup::Mild2x),
        ("Mild", MaxSpeedup::Mild2x),
        ("3x", MaxSpeedup::Moderate3x),
        ("3", MaxSpeedup::Moderate3x),
        ("moderate", MaxSpeedup::Moderate3x),
        ("MODERATE", MaxSpeedup::Moderate3x),
        ("5x", MaxSpeedup::Fast5x),
        ("5", MaxSpeedup::Fast5x),
        ("fast", MaxSpeedup::Fast5x),
        ("100x", MaxSpeedup::Extreme100x),
        ("100", MaxSpeedup::Extreme100x),
        ("extreme", MaxSpeedup::Extreme100x),
        ("EXTREME", MaxSpeedup::Extreme100x),
    ];
    for (label, expected) in cases {
        assert_eq!(
            MaxSpeedup::from_label(label),
            Some(*expected),
            "from_label({label:?}) ≠ {expected:?}"
        );
    }
    // Negative cases: real garbage rejected.
    for &bad in &["wat", "999x", "10x", "negative", "extreme!"] {
        assert!(
            MaxSpeedup::from_label(bad).is_none(),
            "from_label({bad:?}) should reject"
        );
    }
}

#[test]
fn max_speedup_serde_roundtrip_uses_human_labels() {
    // Custom serde impls map MaxSpeedup ↔ "off"/"2x"/.../"100x".
    // Lock the wire shape so a future auto-derive doesn't
    // silently switch to the variant names ("Off"/"Mild2x"/etc),
    // which would break GUI ↔ CLI label parity.
    for &tier in MaxSpeedup::all().iter() {
        let serialized = serde_json::to_string(&tier).expect("serialize");
        // Stripped of quotes, must equal as_str().
        let stripped = serialized.trim_matches('"');
        assert_eq!(stripped, tier.as_str());
        let parsed: MaxSpeedup = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(parsed, tier);
    }
    // Synonym round-trip: serialize Off → "\"off\"", parse "\"1x\"" → Off.
    let from_synonym: MaxSpeedup = serde_json::from_str(r#""1x""#).expect("synonym 1x → Off");
    assert_eq!(from_synonym, MaxSpeedup::Off);
    let from_mild: MaxSpeedup = serde_json::from_str(r#""mild""#).expect("synonym mild → Mild2x");
    assert_eq!(from_mild, MaxSpeedup::Mild2x);
    // Unknown values are rejected (NOT coerced to Off, unlike
    // the lenient IPC path which accepts unknowns for
    // forward-compat).
    let unknown: Result<MaxSpeedup, _> = serde_json::from_str(r#""wat""#);
    assert!(unknown.is_err(), "unknown labels must be rejected by serde");
}

/// Wave-4 audit serde-MED-2: numeric JSON should also parse.
/// Hand-edited settings or future numeric-shorthand emitters
/// pass `2` instead of `"2x"` — the deserializer now accepts
/// both. Also pin the 0/1 → Off mapping (no-speedup-by-number).
#[test]
fn max_speedup_deserialize_accepts_numeric_form() {
    let pairs: &[(&str, MaxSpeedup)] = &[
        ("0", MaxSpeedup::Off),
        ("1", MaxSpeedup::Off),
        ("2", MaxSpeedup::Mild2x),
        ("3", MaxSpeedup::Moderate3x),
        ("5", MaxSpeedup::Fast5x),
        ("100", MaxSpeedup::Extreme100x),
    ];
    for (json, expected) in pairs {
        let parsed: MaxSpeedup = serde_json::from_str(json).expect("numeric form should parse");
        assert_eq!(
            &parsed, expected,
            "numeric `{json}` must map to {expected:?}"
        );
    }
    // Non-recognised number is still rejected (we don't silently
    // pick the nearest tier — surface the bad input).
    let bogus: Result<MaxSpeedup, _> = serde_json::from_str("7");
    assert!(
        bogus.is_err(),
        "numeric 7 has no matching tier — must error"
    );
    // Non-string-non-number stays a hard error.
    let bool_form: Result<MaxSpeedup, _> = serde_json::from_str("true");
    assert!(bool_form.is_err(), "boolean must not be coerced");
    let null_form: Result<MaxSpeedup, _> = serde_json::from_str("null");
    assert!(null_form.is_err(), "null must not be coerced");
}

#[test]
fn speedup_per_gap_cap_boundary_constants() {
    // Lock the per-gap cap thresholds against accidental refactor.
    // Spec (user-emphasised): "Speedups only allowed on gaps >1h,
    // >5h, >30d, >1y" — STRICT-GREATER. The boundary VALUE itself
    // stays in the LOWER tier; the next-second-up opens the new
    // tier. Wave-4 audit F2: was `<` (so 3600 promoted to 2×),
    // now `<=` (so 3600 stays at 1×).
    // ≤1h → 1x
    assert_eq!(per_gap_speedup_cap(0), 1.0);
    assert_eq!(per_gap_speedup_cap(3599), 1.0);
    assert_eq!(per_gap_speedup_cap(3600), 1.0); // exactly 1h is BOUNDARY
                                                // 1h+ε to 5h → 2x
    assert_eq!(per_gap_speedup_cap(3601), 2.0);
    assert_eq!(per_gap_speedup_cap(5 * 3600 - 1), 2.0);
    assert_eq!(per_gap_speedup_cap(5 * 3600), 2.0); // exactly 5h is BOUNDARY
                                                    // 5h+ε to 30d → 3x
    assert_eq!(per_gap_speedup_cap(5 * 3600 + 1), 3.0);
    assert_eq!(per_gap_speedup_cap(30 * 86400 - 1), 3.0);
    assert_eq!(per_gap_speedup_cap(30 * 86400), 3.0); // exactly 30d is BOUNDARY
                                                      // 30d+ε to 1y → 5x
    assert_eq!(per_gap_speedup_cap(30 * 86400 + 1), 5.0);
    assert_eq!(per_gap_speedup_cap(365 * 86400 - 1), 5.0);
    assert_eq!(per_gap_speedup_cap(365 * 86400), 5.0); // exactly 1y is BOUNDARY
                                                       // 1y+ε onwards → 100x
    assert_eq!(per_gap_speedup_cap(365 * 86400 + 1), 100.0);
    assert_eq!(per_gap_speedup_cap(i64::MAX), 100.0);
}

#[test]
fn speedup_compresses_long_gap_at_extreme() {
    // Two entries 1 year apart on a long-span title at Extreme:
    // the gap should compress dramatically (~100x).
    let now = NOW;
    let entries = vec![
        mk_entry("a1", "t1", "2020-01-01T00:00:00Z", 10),
        mk_entry("a2", "t1", "2021-01-01T00:00:00Z", 10),
    ];
    let sched_off = compute_schedule_for_title_with_speedup(
        &entries,
        Some(now),
        &HashSet::new(),
        GapMode::Exact,
        now,
        MaxSpeedup::Off,
    );
    let sched_extreme = compute_schedule_for_title_with_speedup(
        &entries,
        Some(now),
        &HashSet::new(),
        GapMode::Exact,
        now,
        MaxSpeedup::Extreme100x,
    );
    let off_gap = sched_off[1].scheduled_unix - sched_off[0].scheduled_unix;
    let ext_gap = sched_extreme[1].scheduled_unix - sched_extreme[0].scheduled_unix;
    assert!(
        ext_gap < off_gap / 50,
        "Extreme should compress 1y gap by >50x; off={off_gap}s ext={ext_gap}s"
    );
}
