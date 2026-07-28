//! POST a rendered telemetry event body to `v20.events.data.microsoft.com`.
//!
//! Implements the exact header set the C# original emits
//! (`SetDefaultEventBasedHeaders`) plus the `tickets` auth line that
//! carries the user's events token. The Xbox telemetry backend matches
//! the event against the achievement's progress stat and grants the
//! achievement server-side.

use reqwest::Client;
use std::sync::OnceLock;
use std::time::Duration;

use crate::error::{Error, Result};

use super::bundle::EventsBundle;

/// API key pair pinned in the C# client — ties the POST to the Xbox
/// one-collector tenant. Do not rotate unless Microsoft's backend changes.
const API_KEY: &str = "0890af88a9ed4cc886a14f5e174a2827-9de66c5e-f867-43a8-a7b8-e0ddd481cca4-7548,95c1f21d6cb047a09e7b423c1cb2222e-9965f07b-54fa-498e-9727-9e8d24dec39e-7027";
const CLIENT_VERSION: &str =
    "EUTC-Windows-C++-no-10.0.22621.3296.amd64fre.ni_release.220506-1250-no";
const TELEMETRY_URL: &str = "https://v20.events.data.microsoft.com/OneCollector/1.0/";

/// Shared telemetry HTTP client. Built lazily on first use and reused
/// for every subsequent `unlock` call across the whole process.
///
/// Before this was a `OnceLock`, each `unlock()` built a fresh
/// `reqwest::Client` with a fresh TLS stack. In bulk replay (e.g.
/// 80 event-based titles × ~30 achievements = 2400 POSTs) that meant
/// 2400 TLS handshakes and 2400 connection-pool teardowns. Shared
/// client means ~one handshake then keep-alive reuse, saving
/// several minutes of wall-time on a big profile.
///
/// SCHANNEL NOTE: we still need `native-tls` (not rustls) because
/// OneCollector requests a mid-response TLS renegotiation that rustls
/// refuses by design.
fn shared_telemetry_client() -> Result<&'static Client> {
    // Old implementation cached `std::result::Result<Client, String>`
    // in a `OnceLock`. That meant a transient `native_tls` init
    // failure on the FIRST call was sticky for the whole process
    // lifetime — every subsequent events unlock returned the same
    // cloned error, even after TLS/native_tls had recovered.
    //
    // New: cache ONLY successful clients (`OnceLock<Client>`). On
    // init failure, the `OnceLock` stays empty and the next caller
    // retries. Sound because `OnceLock::get_or_init` can only
    // observe the final closure-returned value; if we only call
    // `get_or_init` after a successful build attempt, the cell
    // never sees the error path. Implementation: try the build
    // outside `get_or_init`; on success, stash via `get_or_init`
    // (which races harmlessly with other success writes because
    // all writes would be equivalent clients — but OnceLock only
    // keeps the first).
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let builder = Client::builder()
        .timeout(Duration::from_secs(15))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(32);
    // Desktop pins native-tls (schannel on Windows) because OneCollector
    // was observed to be picky about the TLS stack. Android has no
    // native-tls backend in this dependency set — `use_native_tls` does
    // not even exist there — so it takes reqwest's default. Selecting the
    // backend per target rather than gating the whole module keeps
    // `XboxClient`, which holds an events bundle, compiling on both.
    #[cfg(not(target_os = "android"))]
    let builder = builder.use_native_tls();
    let built = builder
        .build()
        .map_err(|e| Error::other(format!("events http client init: {e}")))?;
    Ok(CLIENT.get_or_init(|| built))
}

impl EventsBundle {
    /// Render + POST a telemetry event that unlocks `achievement_id` on
    /// `title_id`. The `xauth` header is derived from the full XBL3.0
    /// token with the user-hash segment blanked (`XBL3.0 x=-;<jwt>`) to
    /// match the C# client — OneCollector accepts this form and skips
    /// the hash check. The `events_token` is a separate short-lived
    /// credential the user pastes into settings (see
    /// `OpenableLinks.EventsDocumentationUrl` in the upstream C# project
    /// for instructions).
    pub async fn unlock(
        &self,
        title_id: &str,
        achievement_id: &str,
        xuid: &str,
        xauth: &str,
        events_token: &str,
    ) -> Result<()> {
        if events_token.trim().is_empty() {
            return Err(Error::invalid_argument(
                "events_token is empty — obtain one from the XAU events guide and save it in Settings",
            ));
        }

        let body = self.render_event(title_id, achievement_id, xuid)?;

        // Strip the numeric user-hash from the XBL3.0 auth so it matches
        // the C# Regex.Replace(`XBL3\.0 x=\d+;`, "XBL3.0 x=-;").
        let authxtoken = blank_user_hash(xauth);

        // Reuse the process-wide telemetry client (see
        // `shared_telemetry_client` above for rationale).
        let client = shared_telemetry_client()?;

        // Retry connection-level send failures. Unlike the direct
        // `xbox::client` path (which sits behind the retry layer in
        // `xbox::client::retry`), the OneCollector transport has NO
        // lower retry layer — a bare timeout / TCP reset here would be
        // a PERMANENT unlock failure. On the re-run that achievement
        // then lands after its successors, violating the per-title
        // chronological invariant the bit-by-bit replay depends on.
        // We retry ONLY transient send errors (timeout/connect/reset,
        // classified exactly like the direct path via
        // `is_transient_send_err`); 4xx/5xx responses are returned as
        // `Error::HttpStatus` for the caller's cancellation-aware
        // app-level retry (`unlock_with_transient_retry`) so nothing
        // double-retries. The unlock POST is idempotent — re-granting
        // an already-owned achievement is a server-side no-op — so
        // resending is safe.
        const MAX_SEND_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        let resp = loop {
            attempt += 1;
            let req = client
                .post(TELEMETRY_URL)
                .header("user-agent", "MSDW")
                .header("cache-control", "no-cache")
                .header("accept", "application/json")
                .header("accept-encoding", "gzip, deflate")
                .header("reliability-mode", "standard")
                .header("client-version", CLIENT_VERSION)
                .header("apikey", API_KEY)
                .header("Client-Id", "NO_AUTH")
                .header("host", "v20.events.data.microsoft.com")
                .header("connection", "close")
                .header("authxtoken", &authxtoken)
                .header("tickets", format!("\"1\"=\"{events_token}\""))
                .header("content-type", "application/x-json-stream")
                .body(body.clone());
            match req.send().await {
                Ok(r) => break r,
                Err(e)
                    if attempt < MAX_SEND_ATTEMPTS
                        && crate::xbox::client::is_transient_send_err(&e) =>
                {
                    tracing::debug!(
                        ?e,
                        attempt,
                        %title_id,
                        %achievement_id,
                        "events POST transient send error; retrying after backoff"
                    );
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                }
                Err(e) => return Err(Error::other(format!("events POST failed: {e}"))),
            }
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::HttpStatus { status, body });
        }
        tracing::info!(
            %title_id,
            %achievement_id,
            "event-based unlock posted ok"
        );
        Ok(())
    }
}

fn blank_user_hash(xauth: &str) -> String {
    // `XBL3.0 x=<USERHASH>;<JWT>` → `XBL3.0 x=-;<JWT>`
    if let Some(rest) = xauth.strip_prefix("XBL3.0 x=") {
        if let Some(idx) = rest.find(';') {
            return format!("XBL3.0 x=-;{}", &rest[idx + 1..]);
        }
    }
    xauth.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_user_hash() {
        let out = blank_user_hash("XBL3.0 x=123456;eyJhbGciOi...");
        assert_eq!(out, "XBL3.0 x=-;eyJhbGciOi...");
    }

    #[test]
    fn leaves_alone_if_shape_is_unexpected() {
        // No `XBL3.0 x=` prefix at all.
        let out = blank_user_hash("weird");
        assert_eq!(out, "weird");
        // Prefix but no semicolon separating user-hash from JWT.
        let out = blank_user_hash("XBL3.0 x=junk");
        assert_eq!(out, "XBL3.0 x=junk");
    }
}
