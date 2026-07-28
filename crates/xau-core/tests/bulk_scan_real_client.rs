//! End-to-end test of the REAL `XboxClient::get_all_unlocked_achievements_slim_with_progress`
//! pipeline against a local axum mock that mimics
//! `achievements.xboxlive.com`. This catches bugs `scan_local_bench.rs`
//! can't because that test uses its own raw reqwest loop — here we
//! drive the actual production code path, URL override and all.
//!
//! Exercises:
//! - PROBE → PARALLEL ALL pipeline
//! - Progress callback firing for every page
//! - No-stall behaviour (<10s wall-clock for 126k mock records)
//! - Recovery pass when some offsets transiently fail
//! - Heartbeat emission via the watch channel

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::uninlined_format_args,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::format_push_string,
    clippy::print_stdout,
    dead_code
)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::net::TcpListener;

use xau_core::xbox::client::{XboxClient, XboxClientOptions};

#[derive(Debug, Deserialize)]
struct PageQuery {
    #[serde(rename = "maxItems")]
    max_items: Option<usize>,
    #[serde(rename = "skipItems")]
    skip_items: Option<usize>,
    #[serde(rename = "unlockedOnly")]
    _unlocked_only: Option<String>,
}

#[derive(Clone)]
struct MockState {
    total: usize,
    page_size: usize,
    hits: Arc<AtomicUsize>,
    /// Per-request latency — simulates Xbox RTT + body streaming.
    latency: Duration,
    /// When non-zero, the first N hits fail with 503 to exercise the
    /// retry path. Resets to 0 after N failures.
    first_n_fail: Arc<AtomicUsize>,
    /// Offsets that should fail permanently to exercise the
    /// incomplete-scan path after recovery is exhausted.
    always_fail_skips: Arc<HashSet<usize>>,
    /// Offsets that SHOULD NEVER RESPOND — the request handler
    /// will hang indefinitely. Simulates Xbox's silent-stream-drop
    /// behavior on mega-profiles. Drives the stall-detector tests.
    stall_skips: Arc<HashSet<usize>>,
}

async fn achievements_handler(
    State(s): State<MockState>,
    Query(q): Query<PageQuery>,
) -> impl IntoResponse {
    let hit = s.hits.fetch_add(1, Ordering::SeqCst);
    let skip = q.skip_items.unwrap_or(0);
    // Optional transient failure to exercise recovery.
    let remaining_fails = s.first_n_fail.load(Ordering::SeqCst);
    if remaining_fails > 0 {
        s.first_n_fail.fetch_sub(1, Ordering::SeqCst);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            r#"{"error":"503 mock transient"}"#.to_string(),
        );
    }
    if s.always_fail_skips.contains(&skip) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "application/json")],
            format!(r#"{{"error":"500 mock permanent for skip {skip}"}}"#),
        );
    }
    // Permanent stall — never send a response. The client's
    // `PER_PAGE_REQUEST_TIMEOUT` (90s) kills the request, retry
    // happens, kills again, marks as failed. At scan level, the
    // stall detector degrades parallelism then eventually aborts.
    //
    // Use a 5-minute sleep — longer than any real request timeout,
    // shorter than infinite so the test harness doesn't leak
    // tasks if the test framework tries to clean up abruptly.
    if s.stall_skips.contains(&skip) {
        tokio::time::sleep(Duration::from_secs(300)).await;
        return (
            StatusCode::GATEWAY_TIMEOUT,
            [("content-type", "application/json")],
            r#"{"error":"mock stall timeout"}"#.to_string(),
        );
    }
    // Latency simulation — unique per hit so parallel + sequential
    // patterns are distinguishable in the logs.
    tokio::time::sleep(s.latency).await;
    let _ = hit;

    // Use the requested `max_items` — if omitted (shouldn't be) default
    // to page_size. This way the probe (maxItems=1) gets 1 item.
    let requested = q.max_items.unwrap_or(s.page_size);
    if skip >= s.total {
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            format!(
                r#"{{"achievements":[],"pagingInfo":{{"totalRecords":{}}}}}"#,
                s.total
            ),
        );
    }
    let end = (skip + requested).min(s.total);
    let mut body = String::with_capacity((end - skip) * 600);
    body.push_str(r#"{"achievements":["#);
    for i in skip..end {
        if i > skip {
            body.push(',');
        }
        body.push_str(&format!(
            concat!(
                r#"{{"id":"ach_{i}","serviceConfigId":"scid_{sc}","name":"Ach {i}","#,
                r#""progressState":"Achieved","isSecret":false,"#,
                r#""description":"description {i}","#,
                r#""platforms":["Scarlett"],"#,
                r#""titleAssociations":[{{"id":"{t}","name":"Game {t}"}}],"#,
                r#""rewards":[{{"value":"10","type":"Gamerscore"}}],"#,
                r#""progression":{{"timeUnlocked":"2024-{mo:02}-15T12:00:00Z"}}}}"#
            ),
            i = i,
            sc = i % 32,
            t = 1_000_000 + (i % 200),
            mo = 1 + (i % 12),
        ));
    }
    body.push_str(&format!(
        r#"],"pagingInfo":{{"totalRecords":{}}}}}"#,
        s.total
    ));
    (StatusCode::OK, [("content-type", "application/json")], body)
}

async fn start_mock(
    total: usize,
    page_size: usize,
    latency_ms: u64,
    first_n_fail: usize,
    always_fail_skips: &[usize],
) -> (
    SocketAddr,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
) {
    start_mock_ext(
        total,
        page_size,
        latency_ms,
        first_n_fail,
        always_fail_skips,
        &[],
    )
    .await
}

/// Extended mock — adds `stall_skips` (offsets that never respond,
/// used to drive stall-detector tests).
async fn start_mock_ext(
    total: usize,
    page_size: usize,
    latency_ms: u64,
    first_n_fail: usize,
    always_fail_skips: &[usize],
    stall_skips: &[usize],
) -> (
    SocketAddr,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
) {
    let hits = Arc::new(AtomicUsize::new(0));
    let state = MockState {
        total,
        page_size,
        hits: hits.clone(),
        latency: Duration::from_millis(latency_ms),
        first_n_fail: Arc::new(AtomicUsize::new(first_n_fail)),
        always_fail_skips: Arc::new(always_fail_skips.iter().copied().collect()),
        stall_skips: Arc::new(stall_skips.iter().copied().collect()),
    };
    let app = Router::new()
        .route("/users/xuid(:xuid)/achievements", get(achievements_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, hits, tx)
}

/// Verifies the happy path: 125k records, no failures, probe + parallel
/// pipeline completes in a few seconds. Catches the "stall at 1000"
/// class of bugs by asserting wall-clock AND assertions on every page
/// landing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn full_scan_via_real_client_happy_path() {
    const TOTAL: usize = 125_381;
    // Match the client's live page size so `expected_hits` stays
    // honest. Previously hardcoded 10_000 — when the production
    // constant dropped to 1000 (Xbox's timeout-safe page size), this
    // test went red without the numbers telling us why.
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;
    const LATENCY_MS: u64 = 50; // 50ms RTT simulation

    let (addr, hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, LATENCY_MS, 0, &[]).await;
    let base_url = format!("http://{}", addr);
    println!("mock listening on {}", base_url);

    // HTTP (not HTTPS) base URL — the override accepts both schemes.
    // Configure the client to use our mock.
    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummydummydummydummy".into(),
        achievements_base_url_override: Some(base_url.clone()),
        ..Default::default()
    })
    .unwrap();

    // Progress tracking: collect every callback so we can assert
    // monotonic progress + heartbeat spacing.
    #[derive(Debug, Clone)]
    struct ProgressEvent {
        done: usize,
        total: Option<i64>,
        at: Instant,
    }
    let events = Arc::new(std::sync::Mutex::new(Vec::<ProgressEvent>::new()));
    let events_cl = events.clone();

    let t0 = Instant::now();
    let result = client
        .get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            Some(move |done: usize, total: Option<i64>| {
                events_cl.lock().unwrap().push(ProgressEvent {
                    done,
                    total,
                    at: Instant::now(),
                });
            }),
        )
        .await
        .expect("scan should succeed");
    let wall = t0.elapsed();

    let events_vec = events.lock().unwrap().clone();
    println!("=== FULL SCAN VIA REAL CLIENT (happy path) ===");
    println!("collected: {} / expected {}", result.len(), TOTAL);
    println!("server hits: {}", hits.load(Ordering::SeqCst));
    println!("wall-clock: {:?}", wall);
    println!("progress events: {}", events_vec.len());
    if events_vec.len() >= 2 {
        println!("  first: done={}", events_vec[0].done);
        println!(
            "  last:  done={} (total={:?})",
            events_vec.last().unwrap().done,
            events_vec.last().unwrap().total
        );
    }
    println!("===============================================");

    // --- ASSERTIONS ---

    // 1. All records collected.
    assert_eq!(
        result.len(),
        TOTAL,
        "all records should be collected, not 1000 or partial"
    );

    // 2. Reasonable wall-clock. Mock latency 50ms × 13 pages parallel
    // ≈ 200-500ms. Cap at 30s to catch "stalled retry-loop" bugs.
    assert!(
        wall < Duration::from_secs(30),
        "scan took {:?} — regression suggests a stall bug returned",
        wall
    );

    // 3. Server hits = ceil(TOTAL / PAGE_SIZE). The old architecture
    //    fired a separate `maxItems=1` probe just to learn
    //    `totalRecords`, then N pages for the data — `1 + ceil(N/PS)`
    //    hits total. Live testing against SiegfriedX showed that
    //    probe was slower than fetching page 1 at full page size
    //    (the server-side index scan dominates response time;
    //    maxItems=1 vs maxItems=1000 takes ~the same wall clock).
    //    The new `bulk_pages_inner` skips the probe and uses page 1
    //    for both totalRecords AND data — so the total hit count
    //    drops by one.
    let expected_hits = TOTAL.div_ceil(PAGE_SIZE);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        expected_hits,
        "should be ceil(total/page_size) fetches (page 1 + parallel remainder)"
    );

    // 4. Progress callback fired multiple times (probe + updates +
    // heartbeats + final flush).
    assert!(
        events_vec.len() >= 2,
        "progress should fire at least twice (probe + final)"
    );

    // 5. Progress is monotonic — done never decreases.
    let mut prev = 0usize;
    for e in &events_vec {
        assert!(
            e.done >= prev,
            "progress regressed: prev={}, this={}",
            prev,
            e.done
        );
        prev = e.done;
    }

    // 6. Final progress reaches 100%.
    assert_eq!(
        events_vec.last().unwrap().done,
        TOTAL,
        "final progress should equal total"
    );

    // 7. Progress reports totalRecords from early on (probe gives it).
    let first_with_total = events_vec.iter().find(|e| e.total.is_some_and(|t| t > 0));
    assert!(
        first_with_total.is_some(),
        "at least one early event should carry total_records"
    );
}

/// Verifies recovery: inject 2 transient 503s at the start. The probe
/// absorbs them via request_bytes's retry loop (max_retries=4, so 2
/// fails + 1 success leaves headroom). Scan still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn full_scan_survives_transient_503s() {
    const TOTAL: usize = 30_000;
    // Match the client's live page size so `expected_hits` stays
    // honest. Previously hardcoded 10_000 — when the production
    // constant dropped to 1000 (Xbox's timeout-safe page size), this
    // test went red without the numbers telling us why.
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    // 2 failures — well under the probe's 4-retry budget.
    let (addr, _hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, 20, 2, &[]).await;
    let base_url = format!("http://{}", addr);

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        ..Default::default()
    })
    .unwrap();

    let t0 = Instant::now();
    let result = client
        .get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        )
        .await
        .expect("scan should succeed even with transient 503s");
    let wall = t0.elapsed();

    println!("=== SCAN WITH 5 TRANSIENT 503s ===");
    println!("collected: {} / expected {}", result.len(), TOTAL);
    println!("wall-clock: {:?}", wall);
    println!("===================================");

    // Still collected everything despite the 503s (retries handle it).
    assert_eq!(result.len(), TOTAL);
    // And didn't hang forever.
    assert!(
        wall < Duration::from_secs(60),
        "took {:?} — retry loop should back off fast enough",
        wall
    );
}

/// Verifies tiny profile fast path: if the probe shows <= PAGE_SIZE
/// records, we don't fire unnecessary offsets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_profile_does_not_fire_parallel_fetches() {
    const TOTAL: usize = 42;
    // Match the client's live page size so `expected_hits` stays
    // honest. Previously hardcoded 10_000 — when the production
    // constant dropped to 1000 (Xbox's timeout-safe page size), this
    // test went red without the numbers telling us why.
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    let (addr, hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, 10, 0, &[]).await;
    let base_url = format!("http://{}", addr);

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        ..Default::default()
    })
    .unwrap();

    let result = client
        .get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        )
        .await
        .expect("tiny scan should succeed");

    // Post probe-removal: page 1 covers [0, PAGE_SIZE) and also
    // carries `totalRecords`. For a 42-achievement profile that's
    // ONE hit total — no parallel remainder needed.
    let hit_count = hits.load(Ordering::SeqCst);
    assert_eq!(result.len(), TOTAL);
    assert!(
        hit_count <= 2,
        "tiny profile should fire at most 1 hit (page 1 covers everything), got {} hits",
        hit_count
    );
}

/// Verifies that a permanently bad page does NOT come back as a silent
/// partial success. The client should error once recovery is exhausted.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn full_scan_errors_when_page_never_recovers() {
    const TOTAL: usize = 25_000;
    // Match the client's live page size so `expected_hits` stays
    // honest. Previously hardcoded 10_000 — when the production
    // constant dropped to 1000 (Xbox's timeout-safe page size), this
    // test went red without the numbers telling us why.
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    let (addr, _hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, 10, 0, &[10_000]).await;
    let base_url = format!("http://{}", addr);

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        ..Default::default()
    })
    .unwrap();

    let err = client
        .get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        )
        .await
        .expect_err("incomplete scan must surface as Err");

    let msg = err.to_string();
    assert!(
        msg.contains("bulk scan incomplete"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("missing offsets"),
        "error should explain the recovery failure: {msg}"
    );
}

// ============================================================
//  MEGA-PROFILE RESILIENCE TESTS
//
//  Goal: prove that the scan architecture handles the
//  siegfriedx-class failure modes WITHOUT needing to hit real
//  Xbox. Every real-world pain point (silent stream drops,
//  staggered timeouts, penalty-box behavior) is reproduced
//  deterministically here so regressions are caught at
//  `cargo test`, not in production.
// ============================================================

/// Stress scenario 1: mega-profile (50k records) with some pages
/// that take a WHILE but eventually respond. Verifies the scan
/// doesn't false-positive the stall detector on slow-but-live
/// responses — per-request timeout (90s) gives each page a real
/// chance to complete.
///
/// `#[ignore]` — this test drives a mock through ~5000 paginated
/// requests with 10ms simulated latency each, which is wall-clock
/// bound (~50 s minimum) and dominates the default `cargo test`
/// time. Run explicitly with `cargo test -- --include-ignored` to
/// exercise the full stall-resilience suite. See also the sibling
/// `mega_profile_*_stall_*` tests, ignored for the same reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "wall-clock-bound mega-profile test; run with --include-ignored"]
async fn mega_profile_with_slow_pages_still_completes() {
    const TOTAL: usize = 50_000;
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    // 10ms latency is enough to decorrelate request order without
    // making the test take forever. Real Xbox latency is 1-45s per
    // request on mega-profiles, but that'd make this a 25-min
    // integration test — way past `cargo test` usefulness.
    let (addr, _hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, 10, 0, &[]).await;
    let base_url = format!("http://{addr}");

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        ..Default::default()
    })
    .unwrap();

    let result = client
        .get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        )
        .await
        .expect("mega-profile happy-path scan must complete");
    assert_eq!(result.len(), TOTAL);
}

/// Stress scenario 2: mega-profile where a FEW specific offsets
/// never respond. Verifies:
/// - The scan keeps landing the pages that DO respond
/// - Stall detector degrades parallelism (doesn't instantly abort)
/// - Eventually either the stalled offsets complete the recovery
///   pass, or the scan surfaces a clean "stalled — use --resume"
///   error (not a silent hang).
///
/// `#[ignore]` — waits up to 8 minutes for the stall detector to
/// degrade + retry (per-request timeout is 90 s × 2 retries ×
/// degraded-parallelism batches). Run with `--include-ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "wall-clock-bound stall-detector test; run with --include-ignored"]
async fn mega_profile_partial_stall_degrades_then_recovers_or_aborts_cleanly() {
    // Keep TOTAL small enough that non-stall pages finish in <1s
    // but large enough that partial stalls exercise the drain
    // loop, worker retry, and phase-4 recovery paths. 15 pages
    // with 2 stalled covers all three phases without turning this
    // into a 4-minute test.
    const TOTAL: usize = 15_000;
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    // Stall 2 specific offsets. The handlers never respond — the
    // client's 3s timeout + inner retries + outer retries burn
    // ~90s per stalled offset, then phase 4 burns ~20s each
    // trying to recover. Total: ~130s worst case.
    let stall_offsets = [5_000, 10_000];
    let (addr, _hits, _shutdown) =
        start_mock_ext(TOTAL, PAGE_SIZE, 5, 0, &[], &stall_offsets).await;
    let base_url = format!("http://{addr}");

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        // Short client timeout so the test finishes in <2 min
        // even with stalled streams.
        timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .unwrap();

    // Deadline: matches worst-case budget for the partial-stall
    // scenario (5 offsets hanging, 3s client timeout × inner 5
    // retries × MAX_PAGE_RETRIES=3 outer × 8-parallel pool, plus
    // 2s-per-offset phase-4 recovery sequential). Measured
    // empirically at ~3min on modest hardware; 8min gives 2.5×
    // headroom without turning the test into a nap.
    let scan_deadline = Duration::from_secs(480);
    let outcome = tokio::time::timeout(
        scan_deadline,
        client.get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        ),
    )
    .await
    .expect("test harness timeout — scan hung past 3min (stall detector broken)");

    // Two legitimate outcomes: full-completes if the non-stall
    // pages land fast enough AND the stalled offsets somehow
    // bypass (not expected in this setup but possible), or
    // surfaces an Err with a clear message that the stall
    // detector / recovery pass couldn't finish. Hanging past the
    // 3-min deadline would fail ABOVE at `.expect(...)`.
    match outcome {
        Ok(items) => {
            // Possible on very fast systems where the client timeout
            // fired fast enough for the stall offsets to reach the
            // recovery pass — but NOT if every stall offset stays
            // stuck. Accept this only if items.len() == TOTAL.
            assert_eq!(
                items.len(),
                TOTAL,
                "if scan returned Ok, result must be complete"
            );
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("stall")
                    || msg.contains("incomplete")
                    || msg.contains("missing offsets")
                    || msg.contains("timed out")
                    || msg.contains("--resume")
                    || msg.contains("timeout"),
                "scan Err must be a clean stall/incomplete message, got: {msg}"
            );
        }
    }
}

/// Stress scenario 3: WORST CASE — every non-page-1 offset stalls.
/// The scan should:
/// - Successfully land page 1 (no stall on offset=0)
/// - Notice that ALL parallel fetches are stuck
/// - Degrade parallelism progressively
/// - Give up cleanly and return the partial from page 1
///
/// Critically: MUST NOT hang the test harness indefinitely.
///
/// `#[ignore]` — waits for the stall detector's full degradation +
/// give-up cycle (~7 minutes wall clock). Run with
/// `--include-ignored` to exercise.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "wall-clock-bound stall-detector test; run with --include-ignored"]
async fn mega_profile_total_stall_aborts_cleanly() {
    // Smaller TOTAL keeps the test fast. The behavior we're
    // verifying (stall detector degrades parallelism then aborts)
    // is independent of the number of offsets — all that matters
    // is that AT LEAST a few offsets try to run and all of them
    // stall.
    const TOTAL: usize = 10_000;
    const PAGE_SIZE: usize = xau_core::xbox::constants::BULK_SCAN_PAGE_SIZE;

    // Every offset except page 1 stalls. Page 1 lands (via URL
    // without skipItems, handled as skip=0).
    let mut stall_offsets: Vec<usize> = (PAGE_SIZE..TOTAL).step_by(PAGE_SIZE).collect();
    // Also stall every subsequent retry attempt on those offsets —
    // the mock doesn't distinguish attempts but offsets stay in
    // the set until the test ends.
    stall_offsets.sort_unstable();
    let (addr, _hits, _shutdown) =
        start_mock_ext(TOTAL, PAGE_SIZE, 5, 0, &[], &stall_offsets).await;
    let base_url = format!("http://{addr}");

    let client = XboxClient::new(XboxClientOptions {
        xauth: "XBL3.0 x=1;dummydummydummy".into(),
        achievements_base_url_override: Some(base_url),
        timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .unwrap();

    // Realistic budget computed from scan internals:
    // - 9 offsets all stalling; 8 parallel → 2 waves
    // - Per offset: inner retries (~45s) × MAX_PAGE_RETRIES=2 outer ≈ 90s
    // - Phase 2: 2 waves × ~90s = ~180s
    // - Phase 4 budget: 90s cap
    // - Total worst case: ~270s (~4m30s)
    // 7-min deadline gives ~2× headroom without masking truly hung scans.
    let scan_deadline = Duration::from_secs(420);
    let outcome = tokio::time::timeout(
        scan_deadline,
        client.get_all_unlocked_achievements_slim_with_progress(
            "2535406451863815",
            true,
            None::<Box<dyn FnMut(usize, Option<i64>) + Send>>,
        ),
    )
    .await
    .expect("stall detector failed to abort — test hung past 4min");

    // Every offset (except page 1) stalled — result MUST be Err.
    // If it somehow came back Ok, that's a mock bug or a
    // correctness regression.
    let err = outcome.expect_err("every offset stalled, but scan returned Ok?");
    let msg = err.to_string();
    assert!(
        msg.contains("stall")
            || msg.contains("incomplete")
            || msg.contains("missing offsets")
            || msg.contains("timeout")
            || msg.contains("--resume"),
        "total-stall error must carry a clean message, got: {msg}"
    );
}
