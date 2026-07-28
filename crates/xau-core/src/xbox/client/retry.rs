//! HTTP request machinery: retry loops, rate-limit handling, body
//! reads, JSON parsing. Split out of `xbox/client.rs` so the scan +
//! unlock + profile modules can focus on business logic while the
//! retry semantics live in one reviewable place.
//!
//! Public surface inside the crate: `request_bytes`, `request_json`,
//! `request_json_parallel`, `request_text`. All declared `pub(super)`
//! because sibling client modules (scan/unlock/titles/etc.) call
//! them but no external crate should.

use std::time::{Duration, Instant, SystemTime};

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::xbox::constants as c;

use super::{classify_endpoint, erased_json, is_transient_send_err, RateLimitInfo, XboxClient};

/// Parse an HTTP `Retry-After` header value into seconds-from-now.
///
/// RFC 7231 permits TWO forms:
///   - delta-seconds: `120`
///   - HTTP-date (IMF-fixdate): `Wed, 21 Oct 2015 07:28:00 GMT`
///
/// Previous code only tried the int form; when Xbox sends the date
/// form (observed on the stats endpoint during quota resets) we'd
/// fall through to the exponential backoff capped at 10s, even if
/// the server said "wait 5 minutes". See audit bug #5.
///
/// Returns `None` when neither form parses, so callers can fall
/// through to body-JSON or exponential backoff. Past dates clamp to
/// 0 (retry immediately) rather than returning `None` — the server
/// told us to wait "until <past time>", which means "wait 0".
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    // Form 1: integer seconds.
    if let Ok(s) = raw.trim().parse::<u64>() {
        return Some(s);
    }
    // Form 2: IMF-fixdate. Minimal hand-rolled parser — no new dep.
    let target = parse_imf_fixdate(raw.trim())?;
    let now = SystemTime::now();
    Some(target.duration_since(now).map(|d| d.as_secs()).unwrap_or(0))
}

/// Parse IMF-fixdate (RFC 7231 §7.1.1.1): `Sun, 06 Nov 1994 08:49:37 GMT`.
/// Exactly one allowed form — obsolete RFC 850 / asctime forms are not
/// supported (no Xbox endpoint observed emitting them).
fn parse_imf_fixdate(s: &str) -> Option<SystemTime> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT" — fixed width 29 chars.
    // Strict length check avoids matching weird inputs.
    if s.len() != 29 || !s.ends_with(" GMT") {
        return None;
    }
    // Split off the day-name prefix ("Sun, ") and trailing " GMT".
    let body = &s[5..s.len() - 4]; // "06 Nov 1994 08:49:37"
    let mut parts = body.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    // Validate day BEFORE the civil_from_days math. `day = 0` is
    // the classic underflow trap (`doy = ... + day - 1` wraps
    // u32::MAX); `day > 31` is always wrong by calendar. Caught by
    // `tests/prop_retry_after.rs::imf_day_zero_returns_none` and
    // audit R2 #13.
    if !(1..=31).contains(&day) {
        return None;
    }
    let mon: u32 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts.next()?.parse().ok()?;
    let time_str = parts.next()?;
    let mut time_parts = time_str.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let min: u32 = time_parts.next()?.parse().ok()?;
    let sec: u32 = time_parts.next()?.parse().ok()?;
    // Validate time-of-day ranges. HTTP-date permits 23:59:60 (leap
    // second) per RFC 7231 §7.1.1.1, so `sec` ceiling is 60 (inclusive).
    if hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Convert to Unix timestamp via the civil_from_days algorithm
    // (Howard Hinnant). Handles leap years correctly; no external dep.
    // `mon` is u32 (1..=12 from the match); we lift it to i32 only
    // for the single arithmetic step where mixing with `year` (i32)
    // is required, then collapse back.
    let mon_i = i32::try_from(mon).ok()?;
    let y: i32 = if mon <= 2 { year - 1 } else { year };
    let era: i32 = if y >= 0 { y } else { y - 399 } / 400;
    // (y - era * 400) is in 0..400 by construction; bounded cast
    // via try_from so the u32 conversion is provably safe.
    let yoe_i32 = y - era * 400;
    let yoe: u32 = u32::try_from(yoe_i32).ok().filter(|&v| v < 400)?;
    // mon ∈ 1..=12; branch picks 0..=11 for the Hinnant month-shift.
    let m_i32: i32 = if mon > 2 { mon_i - 3 } else { mon_i + 9 };
    let m: u32 = u32::try_from(m_i32).ok().filter(|&v| v < 12)?;
    let doy: u32 = (153 * m + 2) / 5 + day - 1;
    let doe: u32 = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days: i64 = i64::from(era) * 146_097 + i64::from(doe) - 719_468;
    let secs: i64 = days * 86400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);
    let secs_u64 = u64::try_from(secs).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_u64))
}

impl XboxClient {
    pub(super) async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
    ) -> Result<T> {
        let text = self.request_text(method, url, headers, body).await?;
        let value: T = serde_json::from_str(&text)?;
        Ok(value)
    }

    /// Like `request_json` but moves JSON deserialization onto tokio's
    /// blocking pool AND swaps serde_json for **simd-json** (AVX2 lanes
    /// on Windows x86_64). Use this for **large** responses (hundreds of
    /// KB or more) where the parse itself dwarfs network time.
    ///
    /// Why simd-json: serde_json tops out at ~200 MB/s on our payload
    /// shape; simd-json hits ~900 MB/s. On a SiegfriedX-sized profile
    /// (~130 MB total JSON across the bulk scan) that's a ~500ms net
    /// win after accounting for I/O overlap.
    ///
    /// Why spawn_blocking: parse is CPU-bound and dwarfs the RTT.
    /// Running it on the tokio runtime thread would starve concurrent
    /// I/O (like the GUI's IPC bridge that forwards progress events).
    /// The blocking pool (default 512 threads) absorbs it cleanly.
    ///
    /// The generic `T` must be `Send + 'static` so it can cross the
    /// spawn_blocking boundary. `DeserializeOwned` already implies
    /// owned data, so no extra work for the caller.
    pub(super) async fn request_json_parallel<T: DeserializeOwned + Send + 'static>(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
    ) -> Result<T> {
        // `request_bytes` skips UTF-8 validation (simd-json does its own
        // validation inline with parsing, so reqwest's `.text()` UTF-8
        // check would be redundant work on a multi-MB body).
        let bytes = self.request_bytes(method, url, headers, body).await?;
        let parsed = tokio::task::spawn_blocking(move || -> Result<T> {
            // simd-json mutates its input buffer in-place during parse
            // (that's how it achieves SIMD speeds — it writes tape
            // offsets over whitespace). The Vec<u8> we took ownership of
            // above is exactly the buffer it needs.
            let mut buf = bytes;
            let v: T = simd_json::serde::from_slice(&mut buf)
                .map_err(|e| Error::other(format!("simd-json parse: {e}")))?;
            Ok(v)
        })
        .await
        .map_err(|e| Error::other(format!("json parse task join: {e}")))??;
        Ok(parsed)
    }

    /// Like `request_text` but returns the raw response body as bytes,
    /// skipping reqwest's UTF-8 validation. Used by
    /// `request_json_parallel` because simd-json's SIMD lanes already
    /// cover UTF-8 validation during the parse pass — running reqwest's
    /// `.text()` (which internally calls `str::from_utf8`) first is
    /// dead work on a multi-MB body.
    ///
    /// Retry / rate-limit semantics are identical to `request_text` —
    /// the two share a common retry loop shape.
    pub(super) async fn request_bytes(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
    ) -> Result<Vec<u8>> {
        self.request_bytes_inner(method, url, headers, body, false)
            .await
    }

    // Wave-13 audit dead-code-B6: `request_bytes_active` had zero
    // call sites — the only references were doc-comments pointing
    // back at it. The text variant (`request_text_active`) covers
    // every existing mutating-write caller. Deleted; if a future
    // POST endpoint emerges whose response is parsed as raw bytes
    // and needs strict-identity, re-add by mirroring
    // `request_bytes` at line ~183 with `strict_active=true`.

    async fn request_bytes_inner(
        &self,
        method: Method,
        url: &str,
        mut headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
        strict_active: bool,
    ) -> Result<Vec<u8>> {
        // Lowered from 30s after the local rate-limit simulation
        // showed that with 5s throttle + 6 retries, a SiegfriedX-sized
        // scan of 126 pages could burn 90s waiting on backoff alone.
        // 10s is a better fast-fail bound; callers can re-drive the
        // scan via the `resume` path if we really need more retries.
        let max_retries: u32 = 4;
        // BUG HUNTER retry-after-cap (#8): the small backoff cap is
        // for our *exponential fallback*. Server-supplied `Retry-After`
        // (header or body `periodInSeconds`) needs a SEPARATE, larger
        // cap — Xbox legitimately sends 60-300s during real rate-limit
        // windows; clamping that to 10s burned the entire `max_retries`
        // budget in 40s and surfaced a 429 the user couldn't recover
        // from. Honor the server value up to `max_server_retry_after_secs`
        // (5 min). Past that the caller should `--resume` later.
        let max_backoff_secs: u64 = 10;
        let max_server_retry_after_secs: u64 = 300;
        // BUG HUNTER retry-deadline (wave-4 audit F2): without a
        // total-elapsed cap, a request that takes the unlucky path
        // of `Retry-After: 300` four times in a row could block for
        // 1200s (20 min). The user's Stop click cannot interrupt
        // those sleeps because they're not cancel-aware. Cap the
        // entire request lifetime at 180s — long enough to absorb
        // ONE realistic Xbox quota window (typically 60-90s) plus
        // one regular retry, but short enough that a frozen run
        // unsticks within 3 min if the user closes the window.
        // After the deadline, any further sleep is clipped to the
        // remaining budget and we surface a clean error so the
        // caller's `--resume` path can pick up where we left off.
        let request_deadline = Instant::now() + Duration::from_secs(180);
        let mut attempt: u32 = 0;
        // 401-rotation budget is SEPARATE from the transient-retry
        // budget. Capped at pool size so we fully exhaust rotation
        // before giving up, without eating the 429/5xx retry slots.
        // See audit bug #29. Only applies when `strict_active=false`.
        let mut rotation_attempts: u32 = 0;
        let max_rotation_attempts: u32 = self
            .inner
            .token_pool
            .as_ref()
            .map_or(0, |p| u32::try_from(p.snapshot().len()).unwrap_or(u32::MAX))
            .max(1);
        let endpoint = classify_endpoint(url, &method);
        loop {
            // Check the throttle gate before sending. If another in-
            // flight request just got a 429, this parks us until the
            // budget window clears — way more efficient than each
            // request burning its own 6-retry × 10s budget.
            super::wait_at_gate(
                &self.inner.throttle_gates,
                endpoint,
                self.inner.on_throttle.as_ref(),
            )
            .await;
            let mut req = self
                .inner
                .http
                .request(method.clone(), url)
                .headers(headers.clone());
            if let Some(b) = body {
                let payload = b.as_json_string()?;
                req = req
                    .header("content-type", c::header_values::ACCEPT)
                    .body(payload);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < max_retries && is_transient_send_err(&e) => {
                    attempt += 1;
                    // Earlier this was 300ms × 2^attempt — too aggressive.
                    // Raw curl against Xbox's achievements endpoint
                    // showed the "connection reset / HTTP 000" pattern
                    // is a **silent** rate-limit: Xbox drops the TCP
                    // conn without a status code. Retrying in 300ms
                    // just gets another drop. 2s × 2^attempt gives
                    // the server time to release the user from its
                    // penalty box. Sequence: 2s, 4s, 8s, 16s — cap 30s.
                    let wait_ms = (2000u64 << (attempt - 1).min(4)).min(30_000);
                    let wait = Duration::from_millis(wait_ms);
                    let remaining = request_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        tracing::debug!(
                            attempt,
                            ?e,
                            "request budget exhausted; surfacing transient send err"
                        );
                        return Err(e.into());
                    }
                    let actual_wait = wait.min(remaining);
                    tracing::debug!(attempt, ?actual_wait, ?e, "transient send error, retrying");
                    tokio::time::sleep(actual_wait).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                // Extract the xauth from the Authorization header we
                // just sent and quarantine it in the pool (no-op when
                // pool is None). If the pool has OTHER validated
                // tokens, retry with a different one instead of
                // bubbling up immediately.
                let bad_xauth = headers
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if let Some(bx) = &bad_xauth {
                    self.report_token_unauthorized(bx);
                }
                // Strict-active callers (mutating writes — unlock,
                // presence register/DELETE) bail NOW rather than
                // rotating to a sibling pool token. Rotating would
                // either mutate the sibling's profile (cross-account
                // leak) or get rejected by Xbox for body/auth xuid
                // mismatch. See `request_bytes_active` rustdoc +
                // audit bug BBB-Unlock#1.
                if strict_active {
                    tracing::warn!(
                        endpoint = ?endpoint,
                        attempt,
                        "Xbox Live 401 on mutating write — refusing pool rotation to avoid cross-account leak"
                    );
                    return Err(Error::Unauthorized);
                }
                // If pool rotated in a new token we can retry with it.
                // Uses a SEPARATE rotation_attempts counter (not
                // `attempt`) so the pool is exhausted before we give
                // up, without burning the 429/5xx retry slots.
                let next_xauth = self.pick_request_xauth();
                if bad_xauth.as_deref() != Some(next_xauth.as_str())
                    && rotation_attempts < max_rotation_attempts
                {
                    tracing::warn!(
                        endpoint = ?endpoint,
                        rotation_attempts,
                        "401 on one token — pool rotated to another, retrying"
                    );
                    // Rewrite the Authorization header to use the new token
                    // for this endpoint retry.
                    if let Ok(v) = reqwest::header::HeaderValue::from_str(&next_xauth) {
                        headers.insert(reqwest::header::AUTHORIZATION, v);
                    }
                    rotation_attempts += 1;
                    continue;
                }
                tracing::warn!(
                    endpoint = ?endpoint,
                    attempt,
                    "Xbox Live returned 401 Unauthorized — XAUTH expired or revoked. Re-run `xau login` / `xau refresh` to obtain a fresh token."
                );
                return Err(Error::Unauthorized);
            }
            if (status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE)
                && attempt < max_retries
            {
                let header_secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                let body_text = resp.text().await.unwrap_or_default();
                let rate_info = serde_json::from_str::<RateLimitInfo>(&body_text).ok();
                if let Some(ref info) = rate_info {
                    // Wave-6 audit info-leak F2: the
                    // `current=...` / `max=...` pair is per-tenant
                    // and account-fingerprintable when correlated
                    // with public Xbox tier policies. Combined with
                    // identity fields elsewhere in the same log it
                    // de-anonymises the user. Drop them from the
                    // default-level WARN line; keep only the
                    // operational signal (window + endpoint). Power
                    // users can re-enable the full breadcrumb at
                    // `RUST_LOG=debug`.
                    tracing::warn!(
                        %status,
                        attempt,
                        window_s = info.period_in_seconds,
                        endpoint = ?endpoint,
                        "Xbox Live rate-limit hit (limitType={:?})",
                        info.limit_type
                    );
                    tracing::debug!(
                        current = info.current_requests,
                        max = info.max_requests,
                        "rate-limit window state"
                    );
                }
                // Compute how long to park the whole gate. Priority:
                //   1. Explicit `Retry-After` header (server tells us)
                //   2. `periodInSeconds` from JSON body
                //   3. Exponential fallback
                // Server values are clamped at `max_server_retry_after_secs`
                // (5 min). Exponential fallback is clamped at the much
                // smaller `max_backoff_secs` so we still fail-fast when
                // the server gave us no hint.
                let body_secs = rate_info
                    .as_ref()
                    .and_then(|i| i.period_in_seconds)
                    .map(u64::from);
                let server_secs = header_secs.or(body_secs);
                let base_secs = match server_secs {
                    Some(s) => s.min(max_server_retry_after_secs).max(1),
                    None => (2u64 << u64::from(attempt).min(4)).min(max_backoff_secs),
                };
                // Raise the gate for this endpoint. All concurrent
                // in-flight requests will see it and park.
                let until = Instant::now() + Duration::from_secs(base_secs);
                {
                    let mut gates = self
                        .inner
                        .throttle_gates
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let prev = gates.get(&endpoint).copied();
                    if prev.map_or(true, |p| until > p) {
                        gates.insert(endpoint, until);
                    }
                }
                if let Some(cb) = self.inner.on_throttle.as_ref() {
                    cb(base_secs);
                }
                // Deadline guard (audit: deadline-not-enforced-on-429-gate).
                // Parking at the gate for `base_secs` — Xbox legitimately
                // sends Retry-After up to 300s — must not blow the 180s
                // per-request budget `request_deadline` promises. The gate
                // is already raised, so concurrent/subsequent requests
                // still back off correctly; if the backoff exceeds OUR
                // remaining budget, surface the 429 now for the caller's
                // `--resume` path, mirroring the send-error deadline clip.
                let remaining = request_deadline.saturating_duration_since(Instant::now());
                if Duration::from_secs(base_secs) >= remaining {
                    tracing::debug!(
                        attempt,
                        base_secs,
                        "throttle backoff exceeds request budget; surfacing 429 for --resume"
                    );
                    return Err(Error::HttpStatus {
                        status,
                        body: body_text,
                    });
                }
                attempt += 1;
                tracing::debug!(%status, attempt, wait_s = base_secs, ?endpoint, "throttled, gate raised");
                continue;
            }
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) if attempt < max_retries && (e.is_timeout() || e.is_body()) => {
                    attempt += 1;
                    let wait = Duration::from_millis(300u64 << (attempt - 1));
                    let remaining = request_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(e.into());
                    }
                    let actual_wait = wait.min(remaining);
                    tracing::debug!(attempt, ?actual_wait, ?e, "body read failed, retrying");
                    tokio::time::sleep(actual_wait).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            // Xbox Live returns **HTTP 304 Not Modified** from the
            // `/achievements/{scid}/update` endpoint when the caller
            // POSTs an achievement update that matches the server's
            // current state (i.e. the achievement was ALREADY unlocked).
            // Without this line, `is_success()` returns false for 304
            // and the runner marks the whole title batch as a "371
            // error". Treat 304 as idempotent success: body will be
            // empty, caller gets Ok(vec![]).
            if status == StatusCode::NOT_MODIFIED {
                return Ok(Vec::new());
            }
            if !status.is_success() {
                let body_str = String::from_utf8_lossy(&bytes).into_owned();
                return Err(Error::HttpStatus {
                    status,
                    body: body_str,
                });
            }
            return Ok(bytes.to_vec());
        }
    }

    pub(super) async fn request_text(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
    ) -> Result<String> {
        self.request_text_inner(method, url, headers, body, false)
            .await
    }

    /// Strict-identity variant of `request_text`: on 401 the pool is
    /// NOT rotated. See `request_bytes_active` for the rationale —
    /// identical invariants apply (mutating writes, audit bug
    /// BBB-Unlock#1).
    pub(super) async fn request_text_active(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
    ) -> Result<String> {
        self.request_text_inner(method, url, headers, body, true)
            .await
    }

    async fn request_text_inner(
        &self,
        method: Method,
        url: &str,
        mut headers: HeaderMap,
        body: Option<&(dyn erased_json::JsonBody + Send + Sync)>,
        strict_active: bool,
    ) -> Result<String> {
        // Rate-limit handling strategy:
        //
        // Xbox Live's achievements endpoint enforces 150 req / 300 sec per
        // user. Responses carry a structured JSON body telling us the
        // window state:
        //   {"currentRequests":1121,"maxRequests":150,"periodInSeconds":300,"limitType":"Rate"}
        //
        // Philosophy (per user request): go aggressive when it works, fail
        // FAST and CLEAN when it doesn't so the checkpointed plan can be
        // picked up later via `--resume` (after VPN change / cooldown).
        //
        // Budget: up to 4 attempts with min(Retry-After, 10s) each.
        // Total worst case ~40s; typical case <1s.
        let max_retries: u32 = 4;
        // BUG HUNTER retry-after-cap (#8): see request_bytes — we now
        // honor server-supplied Retry-After up to 5 min and only clamp
        // the exponential fallback to 10s.
        let max_backoff_secs: u64 = 10;
        let max_server_retry_after_secs: u64 = 300;
        let mut attempt: u32 = 0;
        // Same 180s deadline as `request_bytes_inner` — see that
        // function for the full rationale (BUG HUNTER retry-deadline,
        // wave-4 audit F2).
        let request_deadline = Instant::now() + Duration::from_secs(180);
        // See companion comment in `request_bytes`. Rotation budget is
        // separate from the transient-retry budget (audit bug #29).
        let mut rotation_attempts: u32 = 0;
        let max_rotation_attempts: u32 = self
            .inner
            .token_pool
            .as_ref()
            .map_or(0, |p| u32::try_from(p.snapshot().len()).unwrap_or(u32::MAX))
            .max(1);
        let endpoint = classify_endpoint(url, &method);
        loop {
            super::wait_at_gate(
                &self.inner.throttle_gates,
                endpoint,
                self.inner.on_throttle.as_ref(),
            )
            .await;
            let mut req = self
                .inner
                .http
                .request(method.clone(), url)
                .headers(headers.clone());
            if let Some(b) = body {
                let payload = b.as_json_string()?;
                req = req
                    .header("content-type", c::header_values::ACCEPT)
                    .body(payload);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < max_retries && is_transient_send_err(&e) => {
                    // Transient: timeout, connection reset (server closing
                    // the TCP connection mid-burst as soft rate-limit),
                    // TLS EOF, h1 body stall. Back off and try again.
                    attempt += 1;
                    // 2s × 2^attempt (2/4/8/16s, cap 30s) — MUST match the
                    // sibling request_bytes_inner backoff (audit: request-
                    // text-transient-backoff-too-fast). Xbox's silent
                    // TCP-drop soft rate-limit needs seconds to release the
                    // user; the old 300ms base burned all 4 retries in
                    // ~4.5s and just got dropped again. All mutating unlock
                    // POSTs + presence heartbeats + per-title reads go
                    // through here, so the fast cadence was the common path.
                    let wait = Duration::from_millis((2000u64 << (attempt - 1).min(4)).min(30_000));
                    let remaining = request_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(e.into());
                    }
                    let actual_wait = wait.min(remaining);
                    tracing::debug!(attempt, ?actual_wait, ?e, "transient send error, retrying");
                    tokio::time::sleep(actual_wait).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                // Extract the xauth from the Authorization header we
                // just sent and quarantine it in the pool (no-op when
                // pool is None). If the pool has OTHER validated
                // tokens, retry with a different one instead of
                // bubbling up immediately.
                let bad_xauth = headers
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if let Some(bx) = &bad_xauth {
                    self.report_token_unauthorized(bx);
                }
                // Strict-active callers (mutating writes — unlock,
                // presence register/DELETE) bail NOW rather than
                // rotating to a sibling pool token. Rotating would
                // either mutate the sibling's profile (cross-account
                // leak) or get rejected by Xbox for body/auth xuid
                // mismatch. See `request_bytes_active` rustdoc +
                // audit bug BBB-Unlock#1.
                if strict_active {
                    tracing::warn!(
                        endpoint = ?endpoint,
                        attempt,
                        "Xbox Live 401 on mutating write — refusing pool rotation to avoid cross-account leak"
                    );
                    return Err(Error::Unauthorized);
                }
                // If pool rotated in a new token we can retry with it.
                // Uses a SEPARATE rotation_attempts counter (not
                // `attempt`) so the pool is exhausted before we give
                // up, without burning the 429/5xx retry slots.
                let next_xauth = self.pick_request_xauth();
                if bad_xauth.as_deref() != Some(next_xauth.as_str())
                    && rotation_attempts < max_rotation_attempts
                {
                    tracing::warn!(
                        endpoint = ?endpoint,
                        rotation_attempts,
                        "401 on one token — pool rotated to another, retrying"
                    );
                    // Rewrite the Authorization header to use the new token
                    // for this endpoint retry.
                    if let Ok(v) = reqwest::header::HeaderValue::from_str(&next_xauth) {
                        headers.insert(reqwest::header::AUTHORIZATION, v);
                    }
                    rotation_attempts += 1;
                    continue;
                }
                tracing::warn!(
                    endpoint = ?endpoint,
                    attempt,
                    "Xbox Live returned 401 Unauthorized — XAUTH expired or revoked. Re-run `xau login` / `xau refresh` to obtain a fresh token."
                );
                return Err(Error::Unauthorized);
            }
            if (status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE)
                && attempt < max_retries
            {
                let header_secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                let body_text = resp.text().await.unwrap_or_default();
                let rate_info = serde_json::from_str::<RateLimitInfo>(&body_text).ok();
                if let Some(ref info) = rate_info {
                    // Wave-6 audit info-leak F2: the
                    // `current=...` / `max=...` pair is per-tenant
                    // and account-fingerprintable when correlated
                    // with public Xbox tier policies. Combined with
                    // identity fields elsewhere in the same log it
                    // de-anonymises the user. Drop them from the
                    // default-level WARN line; keep only the
                    // operational signal (window + endpoint). Power
                    // users can re-enable the full breadcrumb at
                    // `RUST_LOG=debug`.
                    tracing::warn!(
                        %status,
                        attempt,
                        window_s = info.period_in_seconds,
                        endpoint = ?endpoint,
                        "Xbox Live rate-limit hit (limitType={:?})",
                        info.limit_type
                    );
                    tracing::debug!(
                        current = info.current_requests,
                        max = info.max_requests,
                        "rate-limit window state"
                    );
                }
                let body_secs = rate_info
                    .as_ref()
                    .and_then(|i| i.period_in_seconds)
                    .map(u64::from);
                let server_secs = header_secs.or(body_secs);
                let base_secs = match server_secs {
                    Some(s) => s.min(max_server_retry_after_secs).max(1),
                    None => (2u64 << u64::from(attempt).min(4)).min(max_backoff_secs),
                };
                let until = Instant::now() + Duration::from_secs(base_secs);
                {
                    let mut gates = self
                        .inner
                        .throttle_gates
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let prev = gates.get(&endpoint).copied();
                    if prev.map_or(true, |p| until > p) {
                        gates.insert(endpoint, until);
                    }
                }
                if let Some(cb) = self.inner.on_throttle.as_ref() {
                    cb(base_secs);
                }
                // Deadline guard (audit: deadline-not-enforced-on-429-gate).
                // See the identical guard in request_bytes_inner: the gate
                // is raised for others, but this request must not sleep
                // past its 180s budget on a server Retry-After up to 300s.
                let remaining = request_deadline.saturating_duration_since(Instant::now());
                if Duration::from_secs(base_secs) >= remaining {
                    tracing::debug!(
                        attempt,
                        base_secs,
                        "throttle backoff exceeds request budget; surfacing 429 for --resume"
                    );
                    return Err(Error::HttpStatus {
                        status,
                        body: body_text,
                    });
                }
                attempt += 1;
                tracing::debug!(%status, attempt, wait_s = base_secs, ?endpoint, "throttled, gate raised");
                continue;
            }
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) if attempt < max_retries && (e.is_timeout() || e.is_body()) => {
                    attempt += 1;
                    let wait = Duration::from_millis(300u64 << (attempt - 1));
                    let remaining = request_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(e.into());
                    }
                    let actual_wait = wait.min(remaining);
                    tracing::debug!(attempt, ?actual_wait, ?e, "body read failed, retrying");
                    tokio::time::sleep(actual_wait).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            // See the matching comment in `request_bytes` — 304 is
            // idempotent success on `/achievements/{scid}/update`.
            if status == StatusCode::NOT_MODIFIED {
                return Ok(String::new());
            }
            if !status.is_success() {
                return Err(Error::HttpStatus { status, body: text });
            }
            return Ok(text);
        }
    }
}

#[cfg(test)]
mod parse_retry_after_tests {
    use super::*;
    #[test]
    fn int_seconds() {
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after("  0 "), Some(0));
    }
    #[test]
    fn http_date_future_returns_positive() {
        // Far future — the exact value is time-sensitive but must be > 1e9.
        let got = parse_retry_after("Sun, 06 Nov 2099 08:49:37 GMT").unwrap();
        assert!(got > 1_000_000_000);
    }
    #[test]
    fn http_date_past_clamps_to_zero() {
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(0));
    }
    #[test]
    fn invalid_returns_none() {
        assert_eq!(parse_retry_after("next Tuesday"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("Sun, 06 XYZ 1994 08:49:37 GMT"), None);
    }
}
