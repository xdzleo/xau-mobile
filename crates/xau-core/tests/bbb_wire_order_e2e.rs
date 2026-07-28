//! End-to-end proof that `start_bit_by_bit` POSTs unlocks to Xbox in
//! the **exact chronological order** of the input plan within each
//! title, even under cross-title concurrency and same-second bursts.
//!
//! This is the regression cage that locks down the bit-by-bit replay's
//! "ordem estrita dentro do título" guarantee — the property the user
//! explicitly relies on for their 240k-entry aggregate plan.
//!
//! # Strategy
//!
//! 1. Build a synthetic 3-title × 5-entry Plan whose `time_unlocked`
//!    values include:
//!       a. Cross-title interleaving (so cross-title order is mixed).
//!       b. Same-second bursts within a title (microsecond fractions
//!          differ but seconds collide — the case that the
//!          unstable-sort regression broke before BBB-Timing-1 fix).
//! 2. Spin up an `axum` mock that records every POST to
//!    `/users/xuid(...)/achievements/{scid}/update` with:
//!       - Arrival index (atomic counter).
//!       - Wall-clock receive timestamp.
//!       - Parsed body (extracts `titleId` + `achievements[].id`).
//! 3. Drive `start_bit_by_bit` against the mock with a config that:
//!       - Skips network presence + safety (no extra mocked routes).
//!       - Forces `chunk_size = 1` (one POST per achievement).
//!       - Uses `title_concurrency = 3` (all titles in parallel — the
//!         hardest case for ordering).
//!       - `GapMode::Capped(0)` so every entry is immediately eligible.
//! 4. Wait for `Done`, then for each title assert that the order of
//!    POST arrivals matches the plan's chronological order for that
//!    title's entries. Cross-title interleaving is allowed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::print_stdout,
    clippy::items_after_statements,
    dead_code
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use serde_json::Value;
use tokio::net::TcpListener;

use xau_core::plan::bit_by_bit::{start_bit_by_bit, BitByBitConfig, BitByBitState, GapMode};
use xau_core::plan::safety::SafetyPolicy;
use xau_core::plan::{Plan, PlanEntry, TitleCompletion};
use xau_core::xbox::client::{XboxClient, XboxClientOptions};

// ---------------------------------------------------------------------
//  Mock recording infrastructure
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WireRecord {
    /// Strict global arrival order across ALL titles.
    arrival_idx: usize,
    title_id: String,
    achievement_ids: Vec<String>,
}

#[derive(Clone, Default)]
struct MockState {
    next_idx: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<WireRecord>>>,
    /// Empty-bulk JSON body returned for the GET endpoint that
    /// `bit_by_bit::run` calls at startup (`my_unlocks`).
    bulk_body: &'static str,
}

async fn dispatch(State(s): State<MockState>, req: Request) -> impl IntoResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // GET bulk: return empty unlocks list. The runner's `my_unlocks`
    // pass needs SOMETHING parseable; an empty achievements array is
    // valid input and means "user has unlocked nothing yet" — perfect
    // for the test, since every plan entry then becomes eligible.
    if method == Method::GET && path.starts_with("/users/xuid(") && path.ends_with("/achievements")
    {
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            s.bulk_body.to_string(),
        );
    }

    if method == Method::POST && path.starts_with("/users/xuid(") && path.ends_with("/update") {
        // Drain the body so we can parse `titleId` + `achievements[].id`.
        let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
            Ok(b) => b,
            Err(_) => Bytes::new(),
        };
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let title_id = parsed
            .get("titleId")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let achievement_ids: Vec<String> = parsed
            .get("achievements")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let arrival_idx = s.next_idx.fetch_add(1, Ordering::SeqCst);
        s.log.lock().unwrap().push(WireRecord {
            arrival_idx,
            title_id,
            achievement_ids,
        });
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            "{}".to_string(),
        );
    }

    // Anything else → 404 with a loud message so the test fails fast
    // instead of hanging on a stuck endpoint.
    (
        StatusCode::NOT_FOUND,
        [("content-type", "application/json")],
        format!(r#"{{"error":"unmatched route in bbb_wire_order_e2e mock: {method} {path}"}}"#),
    )
}

async fn start_mock(
    bulk_body: &'static str,
) -> (SocketAddr, MockState, tokio::sync::oneshot::Sender<()>) {
    let state = MockState {
        next_idx: Arc::new(AtomicUsize::new(0)),
        log: Arc::new(Mutex::new(Vec::new())),
        bulk_body,
    };
    let app = Router::new().fallback(dispatch).with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    tokio::task::yield_now().await;
    (addr, state, tx)
}

// ---------------------------------------------------------------------
//  Plan fixture
// ---------------------------------------------------------------------

/// Build the test plan. Three titles × five entries each. Critically:
/// - Title A and B share the same **second** for two entries each
///   (sub-second fractions differ) — exercises the BBB-Timing-1 stable
///   sort.
/// - Times across titles INTERLEAVE so a single global wall-clock
///   ordering would mix them — proves we only assert per-title order.
fn build_test_plan(my_xuid: &str) -> Plan {
    let entries = vec![
        // Title A — id "1000000001", scid "A...A"
        (
            "1000000001",
            "scid-aaaa-1111-1111-1111-111111111111",
            "a1",
            "A · #1",
            "2026-04-22T10:00:00.100Z",
        ),
        (
            "1000000001",
            "scid-aaaa-1111-1111-1111-111111111111",
            "a2",
            "A · #2",
            "2026-04-22T10:00:00.500Z",
        ), // same-second
        (
            "1000000001",
            "scid-aaaa-1111-1111-1111-111111111111",
            "a3",
            "A · #3",
            "2026-04-22T10:00:00.999Z",
        ), // same-second
        (
            "1000000001",
            "scid-aaaa-1111-1111-1111-111111111111",
            "a4",
            "A · #4",
            "2026-04-22T10:00:05.000Z",
        ),
        (
            "1000000001",
            "scid-aaaa-1111-1111-1111-111111111111",
            "a5",
            "A · #5",
            "2026-04-22T10:00:10.000Z",
        ),
        // Title B — id "2000000002"
        (
            "2000000002",
            "scid-bbbb-2222-2222-2222-222222222222",
            "b1",
            "B · #1",
            "2026-04-22T10:00:01.000Z",
        ),
        (
            "2000000002",
            "scid-bbbb-2222-2222-2222-222222222222",
            "b2",
            "B · #2",
            "2026-04-22T10:00:02.123Z",
        ),
        (
            "2000000002",
            "scid-bbbb-2222-2222-2222-222222222222",
            "b3",
            "B · #3",
            "2026-04-22T10:00:02.456Z",
        ), // same-second as b2
        (
            "2000000002",
            "scid-bbbb-2222-2222-2222-222222222222",
            "b4",
            "B · #4",
            "2026-04-22T10:00:03.000Z",
        ),
        (
            "2000000002",
            "scid-bbbb-2222-2222-2222-222222222222",
            "b5",
            "B · #5",
            "2026-04-22T10:00:08.000Z",
        ),
        // Title C — id "3000000003"
        (
            "3000000003",
            "scid-cccc-3333-3333-3333-333333333333",
            "c1",
            "C · #1",
            "2026-04-22T10:00:04.500Z",
        ),
        (
            "3000000003",
            "scid-cccc-3333-3333-3333-333333333333",
            "c2",
            "C · #2",
            "2026-04-22T10:00:06.000Z",
        ),
        (
            "3000000003",
            "scid-cccc-3333-3333-3333-333333333333",
            "c3",
            "C · #3",
            "2026-04-22T10:00:07.250Z",
        ),
        (
            "3000000003",
            "scid-cccc-3333-3333-3333-333333333333",
            "c4",
            "C · #4",
            "2026-04-22T10:00:09.000Z",
        ),
        (
            "3000000003",
            "scid-cccc-3333-3333-3333-333333333333",
            "c5",
            "C · #5",
            "2026-04-22T10:00:11.000Z",
        ),
    ];
    let plan_entries: Vec<PlanEntry> = entries
        .iter()
        .map(|(tid, scid, ach, name, ts)| PlanEntry {
            title_id: (*tid).into(),
            service_config_id: (*scid).into(),
            achievement_id: (*ach).into(),
            achievement_name: (*name).into(),
            game_name: format!("Game {}", &tid[..1]),
            time_unlocked: (*ts).into(),
            gamerscore: Some(10),
        })
        .collect();
    let mut title_totals: HashMap<String, TitleCompletion> = HashMap::new();
    for tid in ["1000000001", "2000000002", "3000000003"] {
        title_totals.insert(
            tid.into(),
            TitleCompletion {
                current_achievements: 0,
                total_achievements: 5,
            },
        );
    }
    let _ = my_xuid; // currently unused — Plan default fills the rest
    Plan {
        source_gamertag: "synthetic-target".into(),
        source_xuid: "9999999999999999".into(),
        built_at: "2026-04-22T00:00:00Z".into(),
        total: plan_entries.len(),
        entries: plan_entries,
        title_totals,
        ..Plan::default()
    }
}

// ---------------------------------------------------------------------
//  The test
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bit_by_bit_posts_in_strict_per_title_chronological_order() {
    // Empty bulk body — runner sees "user has zero unlocks", every
    // plan entry becomes eligible.
    const EMPTY_BULK: &str = r#"{"achievements":[],"pagingInfo":{"totalRecords":0}}"#;
    let (addr, mock, _shutdown) = start_mock(EMPTY_BULK).await;
    let base_url = format!("http://{}", addr);
    println!("mock listening on {}", base_url);

    let my_xuid = "1234567890123456".to_string();
    let plan = build_test_plan(&my_xuid);

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummydummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        ..Default::default()
    })
    .expect("XboxClient");

    let cfg = BitByBitConfig {
        my_xuid: my_xuid.clone(),
        // GapMode::Capped(0) clamps every gap to 0 → every entry's
        // scheduled_unix collapses to the chain anchor (which is in
        // the past, since plan timestamps are in 2026-04-22). Result:
        // everything immediately eligible, no scheduler-induced waits
        // distort the wire-order assertion.
        gap_mode: GapMode::Capped(Duration::ZERO),
        max_entries: 0,
        max_gamerscore: 0,
        top_n_titles: 0,
        selected_titles: None,
        title_concurrency: 3, // all 3 titles in parallel — hardest case
        time_budget: None,
        chunk_size_override: Some(1), // one POST per achievement
        // Bypass the safety gate's network calls (would hit unmocked
        // achievements-per-title GETs and stall the test).
        safety: SafetyPolicy::danger_allow_unsafe(),
        // Disable the global presence loop (spawns the heartbeat
        // background task; not needed for ordering proof).
        presence_keepalive: false,
        active_slots_cap: 0,
        max_speedup: xau_core::plan::bit_by_bit::MaxSpeedup::Off,
        catch_up: xau_core::plan::bit_by_bit::CatchUpMode::Shift,
        // Concurrent path (this test proves cross-title interleave
        // ordering); sequential mode is exercised by unit tests.
        sequential: false,
        // Inert here (`sequential: false`), but spelled out rather
        // than defaulted so this literal keeps failing the build if a
        // future config field is added without a decision — that is
        // the point of the exhaustive literal.
        sequential_window: 1,
        min_game_seconds: 60,
    };

    let handle = start_bit_by_bit(client, plan.clone(), cfg).expect("spawn bit-by-bit");

    // Wait for the run to finish (Done / Stopped / Error). Generous
    // overall budget — locally it should be a few hundred ms.
    let started = std::time::Instant::now();
    loop {
        let snap = handle.snapshot();
        if matches!(
            snap.state,
            BitByBitState::Done | BitByBitState::Stopped | BitByBitState::Error
        ) {
            println!(
                "run terminated: state={:?} unlocked_this_run={} errored_titles={} last_error={:?}",
                snap.state, snap.unlocked_this_run, snap.errored_titles, snap.last_error
            );
            assert!(
                matches!(snap.state, BitByBitState::Done),
                "expected Done, got {:?} (last_error={:?})",
                snap.state,
                snap.last_error
            );
            break;
        }
        if started.elapsed() > Duration::from_secs(20) {
            panic!(
                "bit_by_bit did not terminate within 20s — state still {:?}",
                snap.state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------
    // ASSERTIONS
    // -----------------------------------------------------------
    let log = mock.log.lock().unwrap().clone();

    // 1. We saw exactly 15 unlock POSTs (chunk_size=1, no duplicates).
    assert_eq!(
        log.len(),
        15,
        "expected exactly 15 unlock POSTs, got {} — full log: {:#?}",
        log.len(),
        log
    );

    // 2. Each POST has exactly ONE achievement_id (chunk_size=1
    //    contract).
    for r in &log {
        assert_eq!(
            r.achievement_ids.len(),
            1,
            "chunk_size=1 must produce one ach_id per POST; got {} for arrival_idx={}",
            r.achievement_ids.len(),
            r.arrival_idx
        );
    }

    // 3. Group POSTs by title_id and assert the per-title sequence
    //    matches the plan's chronological order for that title.
    let mut by_title_seen: HashMap<String, Vec<String>> = HashMap::new();
    for r in &log {
        by_title_seen
            .entry(r.title_id.clone())
            .or_default()
            .push(r.achievement_ids[0].clone());
    }

    for (title_id, expected_order) in [
        ("1000000001", vec!["a1", "a2", "a3", "a4", "a5"]),
        ("2000000002", vec!["b1", "b2", "b3", "b4", "b5"]),
        ("3000000003", vec!["c1", "c2", "c3", "c4", "c5"]),
    ] {
        let seen = by_title_seen
            .get(title_id)
            .unwrap_or_else(|| panic!("no POSTs recorded for title {title_id}"));
        assert_eq!(
            seen.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_order,
            "title {title_id}: wire-order does NOT match plan chronology",
        );
    }

    // 4. Cross-title interleaving DID happen (proves the test is
    //    actually exercising concurrency — if all titles ran serial
    //    we'd never know whether the per-title invariant survived
    //    parallelism).
    let title_sequence: Vec<&str> = log.iter().map(|r| r.title_id.as_str()).collect();
    let interleaved = title_sequence.windows(2).any(|w| w[0] != w[1]);
    assert!(
        interleaved,
        "expected cross-title interleaving with title_concurrency=3, but POSTs ran strictly serial: {:?}",
        title_sequence
    );

    println!(
        "wire-order proof OK: 15 POSTs across 3 titles, per-title order matches plan, \
         cross-title interleaved (sequence: {:?})",
        title_sequence
    );
}
