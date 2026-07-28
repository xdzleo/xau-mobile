//! End-to-end integration test: unlock path against mocked Xbox Live.
//!
//! Covers the REPLAY / bit-by-bit / nuclear code path — the POST that
//! actually credits an achievement on the user's profile. The mock
//! accepts or rejects the POST based on its config; the test asserts
//! on the client's behaviour: success count, error classification,
//! retry semantics.
//!
//! # Scope
//!
//! - `unlock_title_based_achievement` (and the `_chunked` bulk variant)
//! - Error classification on non-200 (400, 500, etc.)
//!
//! # Out of scope
//!
//! - Event-based unlock path (needs events bundle fixture + multi-host
//!   mock for OneCollector).
//! - Full replay runner end-to-end (needs progress plumbing +
//!   checkpoint state that's easier to test at the unit level).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;

use xau_core::xbox::client::XboxClientOptions;
use xau_core::xbox::XboxClient;

use common::mock_xbox::{MockConfig, MockXbox};

const FIXTURE_XAUTH: &str =
    "XBL3.0 x=dummy;thisismoretestdatathatisnotrealvalidXauth1234567890abcdef1234567890abcdef";

fn client_pointed_at(mock: &MockXbox) -> XboxClient {
    XboxClient::new(XboxClientOptions {
        xauth: FIXTURE_XAUTH.to_string(),
        achievements_base_url_override: Some(mock.base_url()),
        ..Default::default()
    })
    .expect("XboxClient::new")
}

// ---------------------------------------------------------------------
//  Success path
// ---------------------------------------------------------------------

#[tokio::test]
async fn unlock_single_achievement_succeeds_against_mock() {
    let mock = MockXbox::start(MockConfig::default()).await;
    let client = client_pointed_at(&mock);

    // Single-ID path: `unlock_title_based_achievement` wraps
    // `unlock_title_based_achievements_chunked` with a 1-item slice.
    // Either path sends one POST; mock counts it.
    client
        .unlock_title_based_achievement(
            "12345678-1234-1234-1234-000000000000", // SCID
            "12345678",                             // title_id
            "2500000000000001",                     // xuid
            "ach_42",                               // achievement_id
        )
        .await
        .expect("unlock succeeds against mock");

    assert_eq!(
        mock.unlock_count.load(Ordering::SeqCst),
        1,
        "expected exactly 1 POST to /update"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn unlock_batch_chunks_send_multiple_posts() {
    // Chunk size 1 = one POST per achievement. Batch of 3 →
    // 3 POSTs on the mock. Verifies the chunking loop.
    let mock = MockXbox::start(MockConfig::default()).await;
    let client = client_pointed_at(&mock);

    let achievement_ids = vec![
        "ach_1".to_string(),
        "ach_2".to_string(),
        "ach_3".to_string(),
    ];
    client
        .unlock_title_based_achievements_chunked(
            "12345678-1234-1234-1234-000000000000",
            "12345678",
            "2500000000000001",
            &achievement_ids,
            1, // chunk_size: force per-ID POST
        )
        .await
        .expect("chunked unlock succeeds");

    assert_eq!(
        mock.unlock_count.load(Ordering::SeqCst),
        3,
        "expected 3 POSTs (chunk_size=1 × 3 IDs)"
    );

    mock.shutdown().await;
}

// ---------------------------------------------------------------------
//  Error path
// ---------------------------------------------------------------------

#[tokio::test]
async fn unlock_returns_err_on_server_500() {
    // Mock returns 500 for every unlock. Client must propagate
    // the error instead of silently counting success.
    let mock = MockXbox::start(MockConfig {
        unlock_status: Some(500),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    let result = client
        .unlock_title_based_achievement(
            "12345678-1234-1234-1234-000000000000",
            "12345678",
            "2500000000000001",
            "ach_42",
        )
        .await;

    assert!(result.is_err(), "expected Err on 500, got {result:?}");

    mock.shutdown().await;
}

#[tokio::test]
async fn unlock_returns_err_on_server_400() {
    // 400 Bad Request — Xbox's actual response when the request shape
    // is wrong. The client's retry loop shouldn't retry 4xx.
    let mock = MockXbox::start(MockConfig {
        unlock_status: Some(400),
        ..Default::default()
    })
    .await;
    let client = client_pointed_at(&mock);

    let result = client
        .unlock_title_based_achievement(
            "12345678-1234-1234-1234-000000000000",
            "12345678",
            "2500000000000001",
            "ach_42",
        )
        .await;

    assert!(result.is_err(), "expected Err on 400, got {result:?}");
    // Client should stop after 1 attempt (no retry on 4xx); mock's
    // counter captures that.
    assert_eq!(
        mock.unlock_count.load(Ordering::SeqCst),
        1,
        "400 should not be retried — expected exactly 1 POST"
    );

    mock.shutdown().await;
}
