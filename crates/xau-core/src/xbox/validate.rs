//! XAUTH validator — pings
//! `profile.xboxlive.com/users/me/profile/settings?settings=Gamertag`
//! with each candidate and reports which one Xbox Live accepts.
//!
//! Multiple tokens usually coexist in memory (partial writes, expired
//! sessions, cached events-relying-party tokens) and only one currently
//! authenticates. Validating every candidate against the Xbox profile
//! endpoint is the only reliable way to pick the live one.
//!
//! This matches the approach of the sister `rewards_farming` Python scanner.

use std::time::Duration;

use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidatedProfile {
    pub gamertag: Option<String>,
    pub xuid: Option<String>,
}

fn build_client() -> Result<Client> {
    Ok(Client::builder()
        .gzip(true)
        // Pool aggressive enough that N parallel validations share TLS
        // handshakes to profile.xboxlive.com.
        .pool_max_idle_per_host(16)
        .timeout(Duration::from_secs(6))
        .build()?)
}

/// `GET profile.xboxlive.com/users/me/profile/settings?settings=Gamertag`
/// with the given token. Returns `Some(ValidatedProfile)` on HTTP 200,
/// `None` on 401/403, retries 429/5xx up to 5 times with backoff.
///
/// Without retry, a single bbb storm against profile.xboxlive.com left
/// the endpoint 429-throttled for ~10min and EVERY subsequent CLI call
/// (`xau plan-from`, `xau profile`, `xau spoof`) failed with "validate
/// token returned transient/non-auth HTTP 429" — even though the token
/// itself was perfectly valid. The retry path lets the budget window
/// refill (Xbox's gt() endpoint is 10 reqs / 15s) so callers don't
/// have to manually wait 10min between operations.
pub async fn validate_token(token: &str) -> Result<Option<ValidatedProfile>> {
    let client = build_client()?;
    validate_with_client(&client, token).await
}

async fn validate_with_client(client: &Client, token: &str) -> Result<Option<ValidatedProfile>> {
    // Up to 5 attempts; backoff doubles 5s -> 10s -> 20s -> 40s -> 80s
    // (max 155s total) to ride out a typical 429 storm without
    // surfacing an error to callers.
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt: u32 = 0;
    let resp = loop {
        attempt += 1;
        let r = client
            .get("https://profile.xboxlive.com/users/me/profile/settings?settings=Gamertag")
            .header("Authorization", token)
            .header("x-xbl-contract-version", "2")
            .send()
            .await?;
        let status = r.status();
        // Honor server's Retry-After when present; fall back to
        // exponential backoff otherwise.
        let should_retry = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
        if !should_retry || attempt >= MAX_ATTEMPTS {
            break r;
        }
        let server_hint = r
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        let backoff = Duration::from_secs(5u64 << (attempt - 1).min(4));
        let wait = server_hint.unwrap_or(backoff).min(Duration::from_secs(60));
        tracing::debug!(
            %status,
            attempt,
            ?wait,
            "validate: transient response, backing off"
        );
        tokio::time::sleep(wait).await;
    };
    let status = resp.status();
    if !status.is_success() {
        tracing::debug!(%status, "validate: reject");
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Ok(None);
        }
        return Err(Error::other(format!(
            "validate token returned transient/non-auth HTTP {status} (after {attempt} attempts)"
        )));
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(?e, "validate: json parse failed on 200 body");
            return Err(Error::other(format!(
                "validate token JSON parse failed on 200 response: {e}"
            )));
        }
    };
    let user = body.get("profileUsers").and_then(|u| u.get(0));
    let xuid = user
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let gamertag = user
        .and_then(|u| u.get("settings"))
        .and_then(|s| s.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|x| x.get("id").and_then(|i| i.as_str()) == Some("Gamertag"))
                .and_then(|x| x.get("value"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    // Wave-6 audit info-leak F1: previously logged `?gamertag, ?xuid`
    // at info level — every default-install run dropped the user's
    // identity into the log file forever. A forum helper looking at
    // a screenshot or pasted log can correlate xuid with public Xbox
    // profile data. Move identity to debug; the bare "live token
    // accepted" message is sufficient for default-level operational
    // signal. Use `tracing::debug!` for the still-useful identity
    // breadcrumb so power users can `RUST_LOG=debug` if they need
    // it.
    tracing::info!("validate: live token accepted");
    tracing::debug!(
        ?gamertag,
        ?xuid,
        "validate: identity bound to accepted token"
    );
    Ok(Some(ValidatedProfile { gamertag, xuid }))
}

/// Validate candidates in order and return the FIRST one Xbox Live accepts.
///
/// **Order matters here** — callers should pass candidates sorted by
/// in-memory frequency descending (the default from
/// [`memory::extract_all_candidates`]). The highest-frequency token
/// correlates strongly with the *most-recently-written* session, so picking
/// the first valid one in that order handles account-switching correctly:
/// when the user signs out of account A and into account B inside the
/// Xbox app, B's token is written many times (via UI refresh) while A's
/// residue may still be valid but appears less often. Purely parallel
/// "first-past-the-post" validation would race between them and sometimes
/// pick A.
///
/// Set `limit = 0` to try every candidate the iterator yields.
pub async fn validate_candidates<I>(
    candidates: I,
    limit: usize,
) -> Result<Option<(String, ValidatedProfile)>>
where
    I: IntoIterator<Item = String>,
{
    let tokens: Vec<String> = if limit == 0 {
        candidates.into_iter().collect()
    } else {
        candidates.into_iter().take(limit).collect()
    };
    if tokens.is_empty() {
        return Ok(None);
    }

    tracing::info!(
        count = tokens.len(),
        "validating xauth candidates (in frequency order)"
    );

    // Small batches probed in parallel preserve the order bias while still
    // amortising HTTP latency. With batch size 3, the first 3 (top by
    // frequency) get tried together — the first valid one wins; if all 3
    // fail we move to the next batch.
    let client = build_client()?;
    let batch_size = 3;
    for (i, batch) in tokens.chunks(batch_size).enumerate() {
        tracing::debug!(batch = i, size = batch.len(), "validating batch");

        // Spawn this batch's probes in parallel. We await ALL of them and
        // then pick the first-in-input-order that returned Some — this
        // keeps frequency ranking stable even though requests finish out of
        // order.
        let mut futures = Vec::with_capacity(batch.len());
        for token in batch {
            let client = client.clone();
            let token = token.clone();
            futures.push(tokio::spawn(async move {
                let result = validate_with_client(&client, &token).await;
                (token, result)
            }));
        }
        let mut results = Vec::with_capacity(futures.len());
        for fut in futures {
            match fut.await {
                Ok((token, Ok(profile))) => results.push(Ok(profile.map(|p| (token, p)))),
                Ok((_token, Err(e))) => results.push(Err(e)),
                Err(e) => results.push(Err(Error::other(format!(
                    "validate candidate worker failed: {e}"
                )))),
            }
        }
        // Return the first one accepted, preserving input order (= frequency rank).
        // A transient error on the next highest-ranked candidate is not equivalent
        // to rejection; do not skip it and accidentally select an older account.
        for result in results {
            match result {
                Ok(Some(r)) => return Ok(Some(r)),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests intentionally skip network — they just verify the public
    // surface compiles and handles empty input.

    #[tokio::test]
    async fn empty_candidates_returns_none() {
        let r = validate_candidates::<Vec<String>>(Vec::new(), 0)
            .await
            .unwrap();
        assert!(r.is_none());
    }
}
