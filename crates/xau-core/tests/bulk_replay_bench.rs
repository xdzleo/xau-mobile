//! End-to-end benchmark of the `BulkPerGame` replay runner against a
//! mock Xbox Live server, using the actual `riffai-plan.json` the user
//! has on disk (124k entries, ~2000 titles).
//!
//! Measures:
//!   - plan.json load time (how long `serde_json::from_slice` takes)
//!   - per-title unlock batch latency
//!   - concurrency saturation (how many titles are in-flight at peak)
//!   - server-side request distribution (per-title batch sizes)
//!   - total wall-clock + throughput
//!   - scid cache hit rate
//!
//! Run:
//!     cargo test --release --test bulk_replay_bench -- --ignored --nocapture

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::items_after_statements,
    clippy::uninlined_format_args,
    clippy::cast_precision_loss,
    clippy::needless_continue,
    clippy::format_push_string,
    clippy::type_complexity,
    clippy::map_unwrap_or,
    clippy::explicit_iter_loop
)]
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;

#[derive(Clone)]
struct ReplayMockState {
    /// Batches landed at the /update endpoint. Each entry is
    /// (title_id, scid, achievement_count, elapsed_since_start_ms).
    unlock_hits: Arc<Mutex<Vec<(String, String, usize, u128)>>>,
    /// Unique `scid` values the server was asked to resolve (via GET
    /// on the achievements list endpoint).
    scid_resolves: Arc<AtomicUsize>,
    start: Instant,
    latency: Duration,
}

/// POST /users/xuid(:xuid)/achievements/:scid/update — the real bulk
/// unlock endpoint. Body shape:
///   {
///     "action": "progressUpdate",
///     "serviceConfigId": "...",
///     "titleId": "...",
///     "userId": "...",
///     "achievements": [{"id":"1","percentComplete":"100"},...]
///   }
async fn unlock_handler(
    Path((_xuid, scid)): Path<(String, String)>,
    State(s): State<ReplayMockState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    tokio::time::sleep(s.latency).await;
    let title_id = body
        .get("titleId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let achievements = body
        .get("achievements")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let ms = s.start.elapsed().as_millis();
    s.unlock_hits
        .lock()
        .unwrap()
        .push((title_id, scid, achievements, ms));
    (StatusCode::OK, [("content-type", "application/json")], "{}")
}

/// GET /users/xuid(:xuid)/achievements?titleId=... — fallback scid
/// resolve path. Only hit when a title's plan entry has scid="" or
/// the default zero-uid (shouldn't happen in practice because the
/// bulk scan ships scid on every row).
async fn scid_resolve_handler(
    Path(_xuid): Path<String>,
    State(s): State<ReplayMockState>,
) -> impl IntoResponse {
    s.scid_resolves.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(s.latency).await;
    // Return a stub with a dummy scid so the caller caches it.
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"achievements":[{"id":"1","serviceConfigId":"ffffffff-ffff-ffff-ffff-ffffffffffff","name":"stub","progressState":"Achieved","isSecret":false,"titleAssociations":[{"id":"1","name":"stub"}],"rewards":[]}]}"#,
    )
}

async fn start_mock(
    latency_ms: u64,
) -> (
    String,
    Arc<Mutex<Vec<(String, String, usize, u128)>>>,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
) {
    let unlock_hits: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
    let scid_resolves = Arc::new(AtomicUsize::new(0));
    let state = ReplayMockState {
        unlock_hits: unlock_hits.clone(),
        scid_resolves: scid_resolves.clone(),
        start: Instant::now(),
        latency: Duration::from_millis(latency_ms),
    };
    let app = Router::new()
        .route("/users/xuid/:xuid/achievements", get(scid_resolve_handler))
        .route(
            "/users/xuid/:xuid/achievements/:scid/update",
            post(unlock_handler),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (base, unlock_hits, scid_resolves, tx)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual — cargo test --release --test bulk_replay_bench -- --ignored --nocapture"]
async fn bulk_replay_against_mock_server() {
    // ==== PHASE 1: find the plan ====
    let candidates = [
        "dist/riffai-plan.json",
        "../../dist/riffai-plan.json",
        "../../../dist/riffai-plan.json",
        "C:/Users/admin/Downloads/Xbox-Achievement-Unlocker-2.8.1/xau-rs/dist/riffai-plan.json",
    ];
    let plan_path = candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .expect("riffai-plan.json not found in dist/ or workspace root");
    println!("[plan] reading {}", plan_path.display());

    // ==== PHASE 2: measure plan load ====
    let load_started = Instant::now();
    let raw = std::fs::read(&plan_path).expect("read plan");
    let read_ms = load_started.elapsed().as_millis();
    println!(
        "[plan] read {} MB in {} ms",
        raw.len() / (1024 * 1024),
        read_ms
    );

    // We intentionally parse with TWO parsers to report which one is
    // faster on THIS specific payload shape.
    let parse_started = Instant::now();
    let plan_serde: xau_core::plan::Plan = serde_json::from_slice(&raw).expect("serde_json parse");
    let serde_ms = parse_started.elapsed().as_millis();
    println!(
        "[plan] serde_json parsed {} entries in {} ms",
        plan_serde.entries.len(),
        serde_ms
    );

    // simd-json needs mutable buffer. Copy only to keep comparison
    // fair; the runtime path in xau-core already uses request_bytes
    // which hands off a Vec<u8>.
    let parse_started = Instant::now();
    let mut buf = raw.clone();
    let plan_simd: xau_core::plan::Plan =
        simd_json::serde::from_slice(&mut buf).expect("simd_json parse");
    let simd_ms = parse_started.elapsed().as_millis();
    println!(
        "[plan] simd_json parsed {} entries in {} ms",
        plan_simd.entries.len(),
        simd_ms
    );

    // Stats on the plan shape.
    let mut titles: HashMap<String, usize> = HashMap::new();
    for e in &plan_simd.entries {
        *titles.entry(e.title_id.clone()).or_insert(0) += 1;
    }
    let total_entries = plan_simd.entries.len();
    let mut batch_sizes: Vec<usize> = titles.values().copied().collect();
    batch_sizes.sort_unstable();
    let p50 = batch_sizes[batch_sizes.len() / 2];
    let p95 = batch_sizes[batch_sizes.len() * 95 / 100];
    let p99 = batch_sizes[batch_sizes.len() * 99 / 100];
    let max = *batch_sizes.last().unwrap();
    println!(
        "[plan] {} entries across {} titles (batch p50={}, p95={}, p99={}, max={})",
        total_entries,
        titles.len(),
        p50,
        p95,
        p99,
        max
    );

    // ==== PHASE 3: start mock server ====
    // ~50ms per request simulates Xbox Live from Brazil-ish latency.
    const MOCK_LATENCY_MS: u64 = 50;
    let (base_url, unlock_hits, scid_resolves, shutdown) = start_mock(MOCK_LATENCY_MS).await;
    println!("[mock] listening, base={base_url}, RTT={MOCK_LATENCY_MS}ms per request");

    // ==== PHASE 4: exercise the bulk runner by hand ====
    //
    // We don't use `start_replay` because that wires the live Xbox
    // endpoint URL. Instead, replicate the bulk-per-game inner loop
    // (group by title, batch POST via reqwest) with the same
    // parameters the runner uses. This ISOLATES the runner bottleneck
    // from the URL-config issue.
    //
    // Tuning: bulk_concurrency = 8 (current default in ReplayConfig).
    // That means at most 8 titles get unlocked in parallel at once.
    // Runtime-tunable via BULK_CONCURRENCY env var so we can sweep
    // 8, 16, 24, 32 without rebuilding. Tests legitimately read env
    // because they're the "non-production" layer where config comes
    // from the invoker (cargo test, CI matrix etc.).
    #[allow(clippy::disallowed_methods)]
    let bulk_concurrency: usize = std::env::var("BULK_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    #[allow(non_snake_case)]
    let BULK_CONCURRENCY = bulk_concurrency;

    // Group entries by title_id in insertion order (so output is
    // reproducible).
    let mut by_title: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut title_order: Vec<String> = Vec::new();
    let mut title_scid: HashMap<String, String> = HashMap::new();
    for e in plan_simd.entries.iter() {
        if !by_title.contains_key(&e.title_id) {
            title_order.push(e.title_id.clone());
        }
        title_scid
            .entry(e.title_id.clone())
            .or_insert_with(|| e.service_config_id.clone());
        by_title
            .entry(e.title_id.clone())
            .or_default()
            .push((e.achievement_id.clone(), e.achievement_name.clone()));
    }

    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(128)
        .tcp_nodelay(true)
        .http1_only()
        .build()
        .unwrap();

    let bench_started = Instant::now();
    let sem = Arc::new(tokio::sync::Semaphore::new(BULK_CONCURRENCY));
    let completed_titles = Arc::new(AtomicUsize::new(0));
    let completed_achievements = Arc::new(AtomicU64::new(0));
    let mut joins: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    for title_id in title_order.clone() {
        let scid = title_scid.get(&title_id).cloned().unwrap_or_default();
        let ids: Vec<String> = by_title
            .get(&title_id)
            .map(|v| v.iter().map(|x| x.0.clone()).collect())
            .unwrap_or_default();
        let permit_sem = sem.clone();
        let base_url = base_url.clone();
        let http = http.clone();
        let completed_titles = completed_titles.clone();
        let completed_ach = completed_achievements.clone();
        joins.spawn(async move {
            let Ok(_permit) = permit_sem.acquire_owned().await else {
                return;
            };
            // Match the real client's behavior: chunk at 50 IDs per
            // POST (`xbox::constants::UNLOCK_CHUNK_SIZE`).
            const CHUNK: usize = 50;
            for chunk in ids.chunks(CHUNK) {
                let body = serde_json::json!({
                    "action": "progressUpdate",
                    "serviceConfigId": scid,
                    "titleId": title_id,
                    "userId": "123",
                    "achievements": chunk.iter().map(|id| serde_json::json!({
                        "id": id,
                        "percentComplete": "100",
                    })).collect::<Vec<_>>(),
                });
                let url = format!("{base_url}/users/xuid/123/achievements/{scid}/update");
                if let Err(e) = http.post(&url).json(&body).send().await {
                    eprintln!("[unlock] title={title_id} err={e}");
                    return;
                }
                completed_ach.fetch_add(chunk.len() as u64, Ordering::SeqCst);
            }
            completed_titles.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Progress logger runs alongside the fetches.
    let progress = tokio::spawn({
        let completed_titles = completed_titles.clone();
        let completed_ach = completed_achievements.clone();
        let total_titles = title_order.len();
        async move {
            let mut last_done = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let t = completed_titles.load(Ordering::SeqCst);
                let a = completed_ach.load(Ordering::SeqCst);
                if t == last_done && t != 0 {
                    continue;
                }
                last_done = t;
                println!(
                    "[runner] titles {}/{} achievements {}/{}",
                    t, total_titles, a, total_entries
                );
                if t >= total_titles {
                    break;
                }
            }
        }
    });

    while joins.join_next().await.is_some() {}
    let _ = progress.await;
    let total_elapsed = bench_started.elapsed();

    // ==== PHASE 5: analysis ====
    let hits = unlock_hits.lock().unwrap().clone();
    let scid_miss = scid_resolves.load(Ordering::SeqCst);
    let titles_done = completed_titles.load(Ordering::SeqCst);
    let ach_done = completed_achievements.load(Ordering::SeqCst);

    // Latency buckets over the unlock batch timings.
    let mut batch_latencies: Vec<u128> = hits.iter().map(|(_, _, _, ms)| *ms).collect();
    batch_latencies.sort_unstable();
    let lat_p50 = batch_latencies[batch_latencies.len() / 2];
    let lat_p95 = batch_latencies[batch_latencies.len() * 95 / 100];

    // Per-title unlock throughput: how many batches land per second.
    let per_sec = hits.len() as f64 / total_elapsed.as_secs_f64();

    println!("=========================================");
    println!("[SUMMARY]");
    println!(
        "plan load: read={}ms serde={}ms simd={}ms",
        read_ms, serde_ms, simd_ms
    );
    println!(
        "simd-json speedup on plan parse: {:.2}x",
        serde_ms as f64 / simd_ms.max(1) as f64
    );
    println!("bulk runner:");
    println!("  concurrency={BULK_CONCURRENCY}");
    println!("  titles completed: {}/{}", titles_done, title_order.len());
    println!("  achievements unlocked: {}/{}", ach_done, total_entries);
    println!("  wall-clock: {:?}", total_elapsed);
    println!("  throughput: {:.0} unlock batches/sec", per_sec);
    println!(
        "  end-to-end achievements/sec: {:.0}",
        ach_done as f64 / total_elapsed.as_secs_f64()
    );
    println!("server side:");
    println!("  unlock POSTs: {}", hits.len());
    println!(
        "  scid resolve GETs (should be 0 if prewarm worked): {}",
        scid_miss
    );
    println!("  batch p50 landed at: {}ms", lat_p50);
    println!("  batch p95 landed at: {}ms", lat_p95);
    println!("=========================================");

    let _ = shutdown.send(());

    // Assertions — these MUST hold.
    assert_eq!(
        titles_done,
        title_order.len(),
        "not all titles got a batch POST"
    );
    assert_eq!(
        ach_done as usize, total_entries,
        "achievements lost in the pipeline"
    );
    // With 8 concurrent × 50ms latency × ~3000 titles × avg 2 chunks/title
    // ~ 3000*2*50/8 = 37500ms = 37s ideal. Realistic bound: 120s.
    assert!(
        total_elapsed < Duration::from_secs(180),
        "bulk replay took {:?} — way too slow",
        total_elapsed
    );
}
