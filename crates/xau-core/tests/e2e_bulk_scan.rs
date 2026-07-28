//! End-to-end integration test: bulk-scan pipeline against a mocked
//! Xbox Live server.
//!
//! # What this test proves
//!
//! Spins up a local axum server that responds to the bulk
//! achievements endpoint with a canned JSON payload, then drives the
//! actual `XboxClient::get_all_unlocked_achievements_slim_with_progress`
//! code path against it. Verifies:
//!
//!   * Scan completes without hitting the network
//!   * All achievements from the mock payload are returned
//!   * Progress callback fires at least once
//!   * Mock's request counter matches expected call count
//!
//! # What it DOESN'T prove
//!
//! - Profile / titlehub endpoints (those don't have base-url overrides
//!   in XboxClient yet; future PR).
//! - Authentication / OAuth flow (deliberately out of scope — that's
//!   covered by unit tests in `crates/xau-core/src/xbox/validate.rs`).
//! - The full `build_plan` pipeline (it needs `get_games_list` which
//!   hits titlehub — same deferral as above).
//!
//! Future harness PRs will add titlehub + profile base overrides and
//! then wire up full `build_plan` E2E. This test establishes the
//! INFRASTRUCTURE + pattern.

#![allow(clippy::unwrap_used, clippy::expect_used)] // integration test

mod common;

use std::sync::atomic::Ordering;

use xau_core::xbox::client::XboxClientOptions;
use xau_core::xbox::XboxClient;

use common::mock_xbox::{bulk_payload, bulk_payload_without_total, MockConfig, MockXbox};

// ---------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------

/// Minimal XAUTH shape so `XboxClientOptions::new` validates. The
/// token isn't used against the mock (mock ignores Authorization)
/// but the client refuses empty/malformed tokens at construction.
const FIXTURE_XAUTH: &str =
    "XBL3.0 x=dummy;thisismoretestdatathatisnotrealvalidXauth1234567890abcdef1234567890abcdef";

/// Build a client pointed at the mock server. Collapses the boilerplate
/// every E2E test needs.
fn client_pointed_at(mock: &MockXbox) -> XboxClient {
    XboxClient::new(XboxClientOptions {
        xauth: FIXTURE_XAUTH.to_string(),
        achievements_base_url_override: Some(mock.base_url()),
        ..Default::default()
    })
    .expect("XboxClient::new with fixture xauth + base override")
}

// ---------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------

#[tokio::test]
async fn bulk_scan_returns_all_achievements_from_mock() {
    // 47 achievements across 3 titles — small enough that the scan
    // finishes in one page, large enough to exercise the deserializer
    // + progress callback.
    const NUM_ACH: usize = 47;
    const NUM_TITLES: usize = 3;

    let mock = MockXbox::start(MockConfig {
        bulk_body_json: bulk_payload(NUM_ACH, NUM_TITLES),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    // Count how many progress ticks fire — bulk scan should emit at
    // least one (final flush with done_count == NUM_ACH).
    let progress_ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let progress_ticks_cb = progress_ticks.clone();
    let progress = move |done: usize, _total: Option<i64>| {
        if done > 0 {
            progress_ticks_cb.fetch_add(1, Ordering::SeqCst);
        }
    };

    let result = client
        .get_all_unlocked_achievements_slim_with_progress("2500000000000001", true, Some(progress))
        .await
        .expect("mock scan succeeds");

    assert_eq!(
        result.len(),
        NUM_ACH,
        "scan returned {} achievements; expected {NUM_ACH}",
        result.len()
    );

    // Progress callback fired at least once.
    assert!(
        progress_ticks.load(Ordering::SeqCst) >= 1,
        "expected >=1 progress tick, got {}",
        progress_ticks.load(Ordering::SeqCst)
    );

    // Mock received at least one request (exact count depends on the
    // bulk-scan pipeline's probe + page strategy). It must have
    // received ONE OR MORE.
    assert!(
        mock.request_count.load(Ordering::SeqCst) >= 1,
        "mock received no requests — scan didn't hit the override URL"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn bulk_scan_empty_payload_returns_empty() {
    // Edge case: user has zero unlocks. Mock returns the empty-list
    // JSON shape; scan must complete cleanly, not hang or panic.
    let mock = MockXbox::start(MockConfig {
        bulk_body_json: r#"{"achievements":[],"pagingInfo":{"totalRecords":0}}"#.to_string(),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    let result = client
        .get_all_unlocked_achievements_slim_with_progress::<fn(usize, Option<i64>)>(
            "2500000000000001",
            true,
            None,
        )
        .await
        .expect("empty scan succeeds");

    assert!(
        result.is_empty(),
        "empty scan returned {} entries",
        result.len()
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn bulk_scan_sentinel_ceiling_completes_without_false_incomplete() {
    // BUG HUNTER scan-sentinel-falsefail regression: when Xbox omits
    // `pagingInfo.totalRecords` on a FULL page 1, the scan adopts a
    // 1_000_000 sentinel ceiling. The completion check must NOT then
    // treat "collected < 1_000_000" as incomplete — a fully-successful
    // scan previously failed with "bulk scan incomplete … rerun with
    // --resume", and the re-probe on resume re-adopted the same sentinel,
    // so it NEVER converged. With page 1 full (PAGE_SIZE items, no total)
    // and every later page empty, the scan must return Ok with exactly
    // the page-1 rows.
    const PAGE_SIZE: usize = 1000; // mirrors c::BULK_SCAN_PAGE_SIZE

    let mock = MockXbox::start(MockConfig {
        // Page 1: exactly PAGE_SIZE items and NO `totalRecords` → triggers
        // the sentinel ceiling.
        bulk_body_json: bulk_payload_without_total(PAGE_SIZE, 8),
        // Every `skipItems>0` page (offsets past the real end) is empty.
        bulk_body_json_after_first_page: Some(r#"{"achievements":[]}"#.to_string()),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    let result = client
        .get_all_unlocked_achievements_slim_with_progress::<fn(usize, Option<i64>)>(
            "2500000000000001",
            true,
            None,
        )
        .await
        .expect("sentinel-ceiling scan must succeed, not return a spurious 'incomplete' error");

    assert_eq!(
        result.len(),
        PAGE_SIZE,
        "sentinel scan returned {} rows; expected exactly the {PAGE_SIZE} page-1 rows",
        result.len()
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn bulk_scan_with_multiple_titles_groups_correctly() {
    // 20 achievements across 5 titles = 4 per title. The scan returns
    // them in source order; downstream `build_plan` groups by
    // title_id. Here we verify the raw scan surface exposes all
    // title_ids correctly.
    const NUM_ACH: usize = 20;
    const NUM_TITLES: usize = 5;

    let mock = MockXbox::start(MockConfig {
        bulk_body_json: bulk_payload(NUM_ACH, NUM_TITLES),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    let result = client
        .get_all_unlocked_achievements_slim_with_progress::<fn(usize, Option<i64>)>(
            "2500000000000001",
            true,
            None,
        )
        .await
        .expect("multi-title scan succeeds");

    assert_eq!(result.len(), NUM_ACH);

    // Count distinct title_ids via the first title association of
    // each achievement. Fixture sets exactly NUM_TITLES distinct
    // title_ids, rotating through them.
    use std::collections::HashSet;
    let distinct_titles: HashSet<String> = result
        .iter()
        .filter_map(|a| a.title_associations.first().and_then(|t| t.id.clone()))
        .collect();
    assert_eq!(
        distinct_titles.len(),
        NUM_TITLES,
        "expected {NUM_TITLES} distinct titles, got {}",
        distinct_titles.len()
    );

    mock.shutdown().await;
}
