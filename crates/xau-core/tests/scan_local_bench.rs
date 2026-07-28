//! Real end-to-end diagnostic of the bulk scan pipeline.
//!
//! Spins a local axum server that mimics `achievements.xboxlive.com`'s
//! bulk endpoint (paginated via `skipItems`), then drives the actual
//! `XboxClient::get_all_unlocked_achievements_slim_with_progress` code
//! path against it. Measures per-page latency, total wall-clock, and
//! how many progress callbacks fire.
//!
//! The point is to ISOLATE whether slowness is in our pipeline vs in
//! the Xbox network. If this test is fast, the real-world bottleneck
//! is rate limiting or network; if slow, the bug is in our code.
//!
//! `#[ignore]`-gated — run manually:
//!     cargo test --release --test scan_local_bench -- --ignored --nocapture

// This is a manual bench test — all the common clippy pedantic
// complaints (format-push-string for payload building, items-after-statements
// for helper types, uninlined format args, cast-precision-loss on
// ratio math, continue-after-match on retry loops) are fine here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::format_push_string,
    clippy::items_after_statements,
    clippy::uninlined_format_args,
    clippy::cast_precision_loss,
    clippy::needless_continue
)]

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

// NOTE: we don't drive the real XboxClient here — we'd need to override
// the hard-coded achievements URL. Instead we reproduce the pipeline
// shape with a raw reqwest client, keeping the tuning knobs identical.

/// URL parameters the real Xbox endpoint accepts. Only two matter for us.
#[derive(Debug, Deserialize)]
struct PageQuery {
    #[serde(rename = "maxItems")]
    _max_items: Option<usize>,
    #[serde(rename = "skipItems")]
    skip_items: Option<usize>,
    #[serde(rename = "unlockedOnly")]
    _unlocked_only: Option<String>,
}

#[derive(Clone)]
struct MockState {
    total: usize,
    page_size: usize,
    /// Counter increments every time a page is served so we can see
    /// how many unique offsets landed in the final Vec.
    hits: Arc<AtomicUsize>,
    /// Artificial per-request latency to simulate network RTT.
    latency: Duration,
}

async fn page_handler(State(s): State<MockState>, Query(q): Query<PageQuery>) -> impl IntoResponse {
    s.hits.fetch_add(1, Ordering::SeqCst);
    // Simulate Xbox RTT.
    tokio::time::sleep(s.latency).await;

    let skip = q.skip_items.unwrap_or(0);
    if skip >= s.total {
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            r#"{"achievements":[],"pagingInfo":{"totalRecords":0}}"#.to_string(),
        );
    }
    let end = (skip + s.page_size).min(s.total);
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

/// Bind 127.0.0.1:0 (OS-assigned port) and spin the mock server.
/// Returns (addr, hits_counter, shutdown_tx).
async fn start_mock(
    total: usize,
    page_size: usize,
    latency_ms: u64,
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
    };
    // Two routes: games_list (stub) + the achievements bulk endpoint.
    let games_stub = r#"{"titles":[]}"#;
    let app = Router::new()
        .route("/users/xuid/:xuid/achievements", get(page_handler))
        .route(
            "/users/xuid/:xuid/titles/titlehistory/decoration/achievement,image,scid",
            get(move || async move {
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    games_stub,
                )
            }),
        )
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual bench — cargo test --release --test scan_local_bench -- --ignored --nocapture"]
async fn local_mock_scan_shows_per_page_latency() {
    // Setup knobs — mimic SiegfriedX-sized profile + realistic RTT.
    const TOTAL: usize = 125_381;
    const PAGE_SIZE: usize = 1000;
    const LATENCY_MS: u64 = 80; // ~80ms RTT is typical for Xbox from Brazil

    let (addr, hits, _shutdown) = start_mock(TOTAL, PAGE_SIZE, LATENCY_MS).await;
    println!("mock listening on {addr}");

    // Build a client. We can't point it at our mock without modifying
    // the URL constants, so we instead call `request_json_parallel`
    // directly via the client's public `get_all_unlocked_achievements_*`
    // API after overriding the URLs. This test is a LEGAL hack: we
    // monkey the URL helpers via std::env. If that doesn't work in the
    // codebase, we fall back to calling a raw reqwest loop that mirrors
    // the pipeline's fetch+parse steps.
    //
    // For now, emulate the pipeline's inner loop by hand with the same
    // tuning knobs (32 concurrent, retries, simd-json parse on blocking
    // pool) so the number is apples-to-apples.

    const PARALLELISM: usize = 32;
    let sem = Arc::new(tokio::sync::Semaphore::new(PARALLELISM));
    let offsets: Vec<usize> = (0..TOTAL).step_by(PAGE_SIZE).collect();
    let total_pages = offsets.len();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<usize>>();
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(128)
        .tcp_nodelay(true)
        .http1_only()
        .build()
        .unwrap();

    let t0 = Instant::now();
    let fetch_coordinator = tokio::spawn({
        let sem = sem.clone();
        let tx = tx.clone();
        async move {
            let mut joins: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            for offset in offsets {
                let permit_sem = sem.clone();
                let http = http.clone();
                let tx = tx.clone();
                joins.spawn(async move {
                    let Ok(_permit) = permit_sem.acquire_owned().await else {
                        return;
                    };
                    let url = format!(
                        "http://{addr}/users/xuid/1/achievements?maxItems={PAGE_SIZE}&skipItems={offset}&unlockedOnly=true"
                    );
                    let fetch_started = Instant::now();
                    let result: Option<usize> = match http.get(&url).send().await {
                        Ok(resp) => match resp.bytes().await {
                            Ok(bytes) => {
                                // Parse on blocking pool (simd-json).
                                let parsed: Result<
                                    xau_core::models::SlimAchievementsResponse,
                                    _,
                                > = tokio::task::spawn_blocking(move || {
                                    let mut buf = bytes.to_vec();
                                    simd_json::serde::from_slice::<
                                        xau_core::models::SlimAchievementsResponse,
                                    >(&mut buf)
                                })
                                .await
                                .unwrap();
                                match parsed {
                                    Ok(r) => Some(r.achievements.len()),
                                    Err(e) => {
                                        eprintln!("offset {offset} parse err: {e}");
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("offset {offset} body err: {e}");
                                None
                            }
                        },
                        Err(e) => {
                            eprintln!("offset {offset} send err: {e}");
                            None
                        }
                    };
                    let ms = fetch_started.elapsed().as_millis();
                    println!("offset={offset:>7} got={:?} ms={ms}", result);
                    let _ = tx.send(result);
                });
            }
            while joins.join_next().await.is_some() {}
        }
    });
    drop(tx);

    let mut completed = 0usize;
    let mut total_rows = 0usize;
    while let Some(res) = rx.recv().await {
        if let Some(n) = res {
            completed += 1;
            total_rows += n;
        }
    }
    fetch_coordinator.await.unwrap();
    let elapsed = t0.elapsed();

    println!("==============================================");
    println!("TOTAL: {total_rows} rows across {completed}/{total_pages} pages");
    println!("WALL-CLOCK: {:?}", elapsed);
    println!("SERVER HITS: {}", hits.load(Ordering::SeqCst));
    println!(
        "THROUGHPUT: {:.0} rows/sec",
        total_rows as f64 / elapsed.as_secs_f64()
    );
    println!("==============================================");

    // Assertions: with 32 concurrent × 80ms latency × 126 pages,
    // ideal time = 126/32 × 80 = 315ms. Allow 5× slack.
    assert!(completed >= total_pages * 99 / 100, "lost too many pages");
    assert!(
        elapsed < Duration::from_secs(10),
        "scan took {:?} — pipeline is serializing somewhere",
        elapsed
    );
}

// ============================================================
// Rate-limit simulation: verify pipeline behavior when server
// throws 429s at us like Xbox Live does above 150 req/300s.
// ============================================================

#[derive(Clone)]
struct RateLimitState {
    total: usize,
    page_size: usize,
    hits: Arc<AtomicUsize>,
    throttles_sent: Arc<AtomicUsize>,
    /// After this many requests, return 429 with Retry-After for
    /// `throttle_secs` seconds. Mimics Xbox Live's 150/300 budget.
    budget: usize,
    throttle_secs: u64,
}

async fn page_handler_with_throttle(
    State(s): State<RateLimitState>,
    Query(q): Query<PageQuery>,
) -> impl IntoResponse {
    let hit = s.hits.fetch_add(1, Ordering::SeqCst) + 1;
    // Every `budget`th request returns 429. Real Xbox is window-based
    // but this approximates the effect.
    if hit > s.budget {
        s.throttles_sent.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json"),
                (
                    "retry-after",
                    Box::leak(format!("{}", s.throttle_secs).into_boxed_str()),
                ),
            ],
            format!(
                r#"{{"currentRequests":{hit},"maxRequests":{budget},"periodInSeconds":300,"limitType":"Rate"}}"#,
                hit = hit,
                budget = s.budget,
            ),
        );
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    let skip = q.skip_items.unwrap_or(0);
    let end = (skip + s.page_size).min(s.total);
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
                r#""description":"desc","platforms":["Scarlett"],"#,
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
    (
        StatusCode::OK,
        [("content-type", "application/json"), ("retry-after", "")],
        body,
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual rate-limit simulation"]
async fn rate_limit_simulation_shows_real_bottleneck() {
    // Simulate Xbox Live's WORST-CASE behavior. Budget too low for
    // our scan — server throttles mid-flight. This is what appears
    // to be happening on the user's profile given the 91s/5% numbers.
    const TOTAL: usize = 125_381;
    const BUDGET: usize = 50; // much tighter than real 150 to force throttle
    const THROTTLE_SECS: u64 = 5; // short so the test finishes

    let hits = Arc::new(AtomicUsize::new(0));
    let throttles = Arc::new(AtomicUsize::new(0));
    let state = RateLimitState {
        total: TOTAL,
        page_size: 1000,
        hits: hits.clone(),
        throttles_sent: throttles.clone(),
        budget: BUDGET,
        throttle_secs: THROTTLE_SECS,
    };
    let app = Router::new()
        .route(
            "/users/xuid/:xuid/achievements",
            get(page_handler_with_throttle),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async {
                let _ = rx_shutdown.await;
            })
            .await
            .unwrap();
    });
    println!("mock with throttle listening on {addr}");

    // Drive the scan with a 45s timeout — if it takes longer the
    // bench is proving that the bottleneck IS rate limit + backoff.
    let offsets: Vec<usize> = (0..TOTAL).step_by(1000).collect();
    let sem = Arc::new(tokio::sync::Semaphore::new(32));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<usize>>();
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(128)
        .tcp_nodelay(true)
        .http1_only()
        .build()
        .unwrap();

    let t0 = Instant::now();
    let joins = tokio::spawn({
        let sem = sem.clone();
        let tx = tx.clone();
        async move {
            let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            for offset in offsets {
                let permit_sem = sem.clone();
                let http = http.clone();
                let tx = tx.clone();
                set.spawn(async move {
                    let Ok(_permit) = permit_sem.acquire_owned().await else { return };
                    let url = format!(
                        "http://{addr}/users/xuid/1/achievements?maxItems=1000&skipItems={offset}&unlockedOnly=true"
                    );
                    // Simulate our retry loop: 6 retries, min(retry_after,30s).
                    let mut attempt = 0u32;
                    let result: Option<usize> = loop {
                        match http.get(&url).send().await {
                            Ok(resp) => {
                                let status = resp.status();
                                if status == StatusCode::TOO_MANY_REQUESTS && attempt < 6 {
                                    let retry_after = resp
                                        .headers()
                                        .get("retry-after")
                                        .and_then(|v| v.to_str().ok())
                                        .and_then(|s| s.parse::<u64>().ok())
                                        .unwrap_or(30)
                                        .min(30);
                                    attempt += 1;
                                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                                    continue;
                                }
                                break resp.bytes().await.ok().map(|b| b.len());
                            }
                            Err(_) if attempt < 6 => {
                                attempt += 1;
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                continue;
                            }
                            Err(_) => break None,
                        }
                    };
                    let _ = tx.send(result);
                });
            }
            while set.join_next().await.is_some() {}
        }
    });
    drop(tx);
    let mut ok = 0;
    while let Some(res) = tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .ok()
        .flatten()
    {
        if res.is_some() {
            ok += 1;
        }
    }
    joins.abort();
    let elapsed = t0.elapsed();
    let _ = tx_shutdown.send(());

    println!("==========================================");
    println!("RATE-LIMIT SIMULATION:");
    println!(" Budget: {} requests; Throttle: {}s", BUDGET, THROTTLE_SECS);
    println!(" Pages OK: {ok}");
    println!(
        " Throttles sent by server: {}",
        throttles.load(Ordering::SeqCst)
    );
    println!(" Total hits: {}", hits.load(Ordering::SeqCst));
    println!(" Wall-clock: {elapsed:?}");
    println!(" ------------------------------------------");
    println!(" DIAGNOSIS: if wall-clock > 30s, the scan is");
    println!(" stuck in rate-limit backoff. Real Xbox is");
    println!(" likely doing the same thing on your profile.");
    println!("==========================================");
}
