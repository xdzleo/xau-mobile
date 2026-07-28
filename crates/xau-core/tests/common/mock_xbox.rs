//! Mock Xbox Live server for integration tests.
//!
//! # What it mocks
//!
//! The `achievements.xboxlive.com` endpoints that `XboxClient` hits
//! during plan build + replay. Pointed at the mock via
//! `XboxClientOptions::achievements_base_url_override`. Other Xbox
//! hosts (`profile.xboxlive.com`, `titlehub.xboxlive.com`) are NOT
//! intercepted — tests that need those today either use canned data
//! directly or defer until the corresponding base overrides land
//! (separate PR).
//!
//! # Routes handled
//!
//! - `GET /users/xuid({xuid})/achievements` (bulk-scan pagination)
//! - `POST /achievements/{scid}/update` (unlock)
//! - Graceful fallback on unmatched routes: returns 404 with a
//!   clear error body so tests fail fast instead of hanging on a
//!   stuck request.
//!
//! # Lifecycle
//!
//! ```ignore
//! let server = MockXbox::start(MockConfig {
//!     bulk_fixture: fixtures::tiny_plan_payload(5),
//!     ..Default::default()
//! }).await;
//! // ... point XboxClient at server.base_url() ...
//! server.shutdown().await; // clean teardown at end of test
//! ```
//!
//! The server binds 127.0.0.1:0 (OS-assigned port) so parallel tests
//! don't collide. Shutdown via `oneshot::Sender` — server stops
//! cleanly when `MockXbox` is dropped.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------
//  Public API
// ---------------------------------------------------------------------

/// Configuration for the mock. All fields optional; `Default` gives
/// an empty plan + no artificial latency + success-everywhere.
#[derive(Clone, Default)]
pub struct MockConfig {
    /// Full pre-rendered JSON body for the bulk achievements endpoint.
    /// Typically built via `fixtures::tiny_plan_payload(n)` (once we
    /// add fixtures — in this harness we accept a raw string).
    pub bulk_body_json: String,
    /// Artificial latency per request. Lets tests assert on
    /// timing-sensitive paths (e.g., stall detection).
    pub latency_per_request: Duration,
    /// Optional HTTP status to return from the unlock endpoint.
    /// `None` = 200 OK. `Some(400)` lets tests drive the runner's
    /// error-classification path.
    pub unlock_status: Option<u16>,
    /// When `Some`, the bulk endpoint returns this body for any request
    /// carrying `skipItems=<nonzero>` (the parallel-remainder pages),
    /// while `bulk_body_json` is used for page 1 (`skipItems=0` / absent).
    /// Lets a test simulate "page 1 full, every later page empty" — the
    /// shape that exercises the H4 sentinel-ceiling completion path.
    pub bulk_body_json_after_first_page: Option<String>,
}

/// Live handle to the mock server. Hold onto this for the duration
/// of the test; dropping it triggers graceful shutdown.
pub struct MockXbox {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Counts every request served. Tests use this to assert on
    /// expected call patterns (e.g., "scan issued exactly N page
    /// requests").
    pub request_count: Arc<AtomicUsize>,
    /// Counts unlock POSTs specifically. Replay tests assert on
    /// this independently of the counter above.
    pub unlock_count: Arc<AtomicUsize>,
}

impl MockXbox {
    /// Spin up the server. Returns once it's bound + accepting.
    pub async fn start(cfg: MockConfig) -> Self {
        let request_count = Arc::new(AtomicUsize::new(0));
        let unlock_count = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            bulk_body_json: Arc::new(cfg.bulk_body_json),
            bulk_body_after: cfg.bulk_body_json_after_first_page.map(Arc::new),
            latency: cfg.latency_per_request,
            unlock_status: cfg.unlock_status.unwrap_or(200),
            request_count: request_count.clone(),
            unlock_count: unlock_count.clone(),
        };
        let app = build_router(state);

        // Bind OS-assigned port so parallel tests don't collide.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock xbox");
        let addr = listener.local_addr().expect("local_addr");

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service())
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        // Give the task a moment to complete the bind loop. This is
        // usually instant; defensive microsleep avoids flaky tests
        // that hit before `serve` is ready.
        tokio::task::yield_now().await;

        Self {
            addr,
            shutdown: Some(tx),
            request_count,
            unlock_count,
        }
    }

    /// Base URL in the form `http://127.0.0.1:PORT`, ready to paste
    /// into `XboxClientOptions::achievements_base_url_override`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Explicit shutdown. Not strictly required (Drop handles it) but
    /// useful for tests that want to assert final counts BEFORE the
    /// server tears down.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // yield so the server's shutdown future can actually return.
        tokio::task::yield_now().await;
    }
}

impl Drop for MockXbox {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

// ---------------------------------------------------------------------
//  Internal state + handlers
// ---------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    bulk_body_json: Arc<String>,
    bulk_body_after: Option<Arc<String>>,
    latency: Duration,
    unlock_status: u16,
    request_count: Arc<AtomicUsize>,
    unlock_count: Arc<AtomicUsize>,
}

/// Parse the `skipItems=<n>` value out of a bulk-scan query string.
/// Returns `None` when the param is absent (page 1).
fn parse_skip_items(query: &str) -> Option<usize> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("skipItems="))
        .and_then(|v| v.parse::<usize>().ok())
}

fn build_router(state: MockState) -> Router {
    // Xbox Live URLs include literal parens in path segments
    // (`/users/xuid(2535...)/achievements`). Axum's path-params don't
    // natively parse parens, so we use a wildcard catch-all and
    // dispatch by inspecting the method + path. This is a test mock —
    // correctness matters more than router ergonomics.
    Router::new().fallback(dispatch).with_state(state)
}

async fn dispatch(State(s): State<MockState>, req: Request) -> impl IntoResponse {
    s.request_count.fetch_add(1, Ordering::SeqCst);
    if !s.latency.is_zero() {
        tokio::time::sleep(s.latency).await;
    }
    let path = req.uri().path();
    let method = req.method();

    // Bulk-scan: `GET /users/xuid(...)/achievements`
    // (with query params like `?maxItems=1000&skipItems=0`).
    // The `update` POST has the shape `/users/xuid(...)/achievements/{scid}/update`
    // so we MUST check for `/update` suffix to distinguish unlock from scan
    // — both live under the same prefix.
    if method == Method::GET && path.starts_with("/users/xuid(") && path.ends_with("/achievements")
    {
        // Page 1 has no `skipItems`; parallel-remainder pages carry
        // `skipItems=<nonzero>`. When the test configured an "after first
        // page" body, serve it for those remainder offsets so it can
        // simulate "page 1 full, all later pages empty".
        let skip = req.uri().query().and_then(parse_skip_items).unwrap_or(0);
        let body = if skip > 0 {
            match &s.bulk_body_after {
                Some(b) => (**b).clone(),
                None => (*s.bulk_body_json).clone(),
            }
        } else {
            (*s.bulk_body_json).clone()
        };
        return (StatusCode::OK, [("content-type", "application/json")], body);
    }
    if method == Method::POST && path.starts_with("/users/xuid(") && path.ends_with("/update") {
        s.unlock_count.fetch_add(1, Ordering::SeqCst);
        let status = StatusCode::from_u16(s.unlock_status).unwrap_or(StatusCode::OK);
        return (
            status,
            [("content-type", "application/json")],
            "{}".to_string(),
        );
    }
    // Unmatched — fail fast, tests that hit an unexpected endpoint
    // see a 404 with a clear message instead of hanging.
    (
        StatusCode::NOT_FOUND,
        [("content-type", "application/json")],
        format!(
            r#"{{"error":"mock_xbox: no route matched for {method} {path} — add a handler in tests/common/mock_xbox.rs"}}"#
        ),
    )
}

// ---------------------------------------------------------------------
//  Fixture builders — shared canned payloads for tests
// ---------------------------------------------------------------------

/// Build a minimal bulk-scan JSON body with N unlocked achievements
/// spread across `num_titles` titles. Deterministic: same args →
/// same bytes.
///
/// Designed to satisfy the `XboxClient`'s deserializer without
/// pretending to be 100% fidelity — the fields that matter for plan
/// building (`id`, `titleAssociations`, `progression.timeUnlocked`,
/// `rewards`) are populated; purely decorative fields (`isSecret`,
/// `description`, etc.) use fixed stubs.
#[must_use]
pub fn bulk_payload(num_achievements: usize, num_titles: usize) -> String {
    assert!(num_titles > 0, "num_titles must be positive");
    let mut body = String::with_capacity(num_achievements * 500);
    body.push_str(r#"{"achievements":["#);
    for i in 0..num_achievements {
        if i > 0 {
            body.push(',');
        }
        let title_idx = i % num_titles;
        // Deterministic timestamps: achievement 0 in 2024-01-01,
        // spreading forward by one day per entry. Keeps scheduler
        // tests predictable.
        let month = 1 + (i % 12);
        let day = 1 + (i % 28);
        body.push_str(&format!(
            concat!(
                r#"{{"id":"ach_{i}","serviceConfigId":"12345678-1234-1234-1234-000000000000","#,
                r#""name":"Achievement {i}","isSecret":false,"#,
                r#""description":"desc {i}","#,
                r#""platforms":["Scarlett"],"#,
                r#""titleAssociations":[{{"id":"{t:08}","name":"Title {t}"}}],"#,
                r#""rewards":[{{"value":"10","type":"Gamerscore"}}],"#,
                r#""progressState":"Achieved","#,
                r#""progression":{{"timeUnlocked":"2024-{month:02}-{day:02}T12:00:00Z"}}}}"#
            ),
            i = i,
            t = 1_000_000 + title_idx,
            month = month,
            day = day,
        ));
    }
    body.push_str(&format!(
        r#"],"pagingInfo":{{"totalRecords":{num_achievements}}}}}"#
    ));
    body
}

/// Same items as [`bulk_payload`] but with NO `pagingInfo.totalRecords`
/// — simulates the Xbox quirk (seen in the wild) where page 1 omits the
/// total. The bulk scan then adopts the H4 sentinel ceiling (1_000_000)
/// and must judge completeness by trailing-empty pages rather than by
/// reaching that fake total.
#[must_use]
pub fn bulk_payload_without_total(num_achievements: usize, num_titles: usize) -> String {
    let full = bulk_payload(num_achievements, num_titles);
    // `bulk_payload` ends with `],"pagingInfo":{...}}`; drop that suffix
    // so the response carries no `pagingInfo` object at all.
    match full.rfind(r#"],"pagingInfo""#) {
        Some(idx) => format!("{}]}}", &full[..idx]),
        None => full,
    }
}
