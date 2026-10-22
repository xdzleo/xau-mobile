//! Async, reusable Xbox Live REST client.
//!
//! Rust port of `XAU/Networking/XboxRestApi.cs`. Each method maps to one route
//! on Xbox Live, preserving the original contract-version headers, URLs and
//! request shapes so that traffic is observationally identical.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method};
// `serde::de::DeserializeOwned` moved to submodules that parse JSON.
use serde::Deserialize;
use serde_json::Value;

pub mod offer_names;
pub mod offers;
/// Heuristic for "retry this request". Matches:
/// - network timeouts (reqwest tagged)
/// - body decode failures that look transient (mid-stream reset)
/// - Windows `os error 10054` — "An existing connection was forcibly
///   closed by the remote host" (Xbox's soft rate-limit: it just drops
///   the TCP connection instead of returning 429 sometimes)
// ---- Submodules ----
// client.rs is split by responsibility (retry machinery, scan pipeline,
// unlock/titles endpoints, etc.) once it crossed 2000 lines. Each
// submodule declares `impl XboxClient { ... }` with just the methods
// in its area — Rust merges inherent impl blocks across files so
// callers see a single flat API.
pub mod retry;
pub mod rewards;
mod title_art;
mod scan;
pub mod unlock;

/// Rewrite the production achievements-endpoint prefix of a URL to a
/// test-supplied base. No-op when `override_base` is `None`. Expects
/// URLs shaped like the Xbox achievements endpoint; anything else
/// passes through unchanged so non-achievement URLs aren't affected.
#[allow(clippy::doc_markdown, clippy::doc_lazy_continuation)]
fn rewrite_base_url(url: &str, override_base: Option<&str>) -> String {
    match override_base {
        Some(base) => {
            const PROD: &str = "https://achievements.xboxlive.com";
            url.strip_prefix(PROD)
                .map_or_else(|| url.to_string(), |rest| format!("{base}{rest}"))
        }
        None => url.to_string(),
    }
}

pub(crate) fn is_transient_send_err(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_connect() || e.is_body() {
        return true;
    }
    // Walk the source chain looking for the specific IO errors Xbox Live
    // emits during rate-limit storms. Use string match because reqwest
    // doesn't expose the underlying io::ErrorKind directly.
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = src {
        let msg = err.to_string();
        if msg.contains("connection")
            && (msg.contains("reset") || msg.contains("closed") || msg.contains("10054"))
        {
            return true;
        }
        if msg.contains("IncompleteMessage") || msg.contains("unexpected end of file") {
            return true;
        }
        src = err.source();
    }
    false
}

/// Shape of Xbox Live's rate-limit response body. Fields match the
/// JSON exactly:
/// `{"currentRequests":N,"maxRequests":M,"periodInSeconds":S,"limitType":"Rate"}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo {
    #[serde(default)]
    current_requests: Option<u32>,
    #[serde(default)]
    max_requests: Option<u32>,
    #[serde(default)]
    period_in_seconds: Option<u32>,
    #[serde(default)]
    limit_type: Option<String>,
}

use crate::error::{Error, Result};
use crate::models::{
    AchievementsResponse, BasicProfile, GamePassProducts, GameStat, GameStatsRequest,
    GameStatsResponse, GameTitle, GameTitleRequest, Gamepass, GamepassProductsRequest,
    HeartbeatRequest, Profile, TitlesList,
};
use crate::xbox::constants as c;

/// Stateless, reusable Xbox Live client.
///
/// Internally wraps a single shared `reqwest::Client` (with gzip+deflate
/// decompression enabled). All methods are `&self` and safe to call
/// concurrently from multiple tasks.
#[derive(Clone)]
pub struct XboxClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: Client,
    xauth: String,
    accept_language: String,
    fake_signature: bool,
    /// Optional events (telemetry) bundle. When present, `unlock_achievement`
    /// routes through the OneCollector path for titles that appear in
    /// `Data.json`. Loaded once at client construction via
    /// [`XboxClientOptions::events_bundle`].
    events: Option<Arc<crate::events::EventsBundle>>,
    /// Events token paired with `events`. Only read on the event-unlock
    /// path — never sent to `achievements.xboxlive.com`.
    ///
    /// Wrapped in a `RwLock` so a long-running CLI / GUI process that
    /// started without an events token (memory-scan login) can pick up
    /// a fresh `events_xauth` AFTER the user does a device-code login —
    /// `auth.json` gets a populated `events_xauth`, the auth-watcher
    /// task notices the mtime change, and calls
    /// `XboxClient::set_events_token` so the BBB / nuclear / replay
    /// runners' cached `Arc<Inner>` clones all see the update without
    /// a restart. Reads are O(1) (clone of an `Option<String>`) and
    /// only happen on the event-unlock path, which is rare relative
    /// to the regular POST path.
    events_token: RwLock<Option<String>>,
    /// Cache of `title_id -> service_config_id` shared across all callers of
    /// the same `XboxClient`. Populated lazily by
    /// [`XboxClient::resolve_service_config_id`] and every `unlock_*` call.
    /// Matches the `ServiceConfigCache` in the original
    /// `Services/HttpServer/AchievementRoutes.cs`.
    scid_cache: RwLock<HashMap<String, String>>,
    /// Cache of `(xuid,title_id) -> (fetched_at, AchievementsResponse)` so
    /// the GUI can navigate back and forth between titles without
    /// re-downloading the list each time. TTL is 60s — short enough that
    /// a user clicking Unlock still sees progress reflected, long enough
    /// that casual browsing is instant.
    achievements_cache: RwLock<HashMap<(String, String), (Instant, AchievementsResponse)>>,
    /// Observer fired whenever the client sees an HTTP 429 / 503 from
    /// Xbox Live. Argument is the wait-seconds the client is about to
    /// sleep for. Lets the GUI surface "Xbox is rate-limiting — waiting
    /// Ns" to the user so a throttled scan doesn't look frozen.
    on_throttle: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// Optional override of the `https://achievements.xboxlive.com`
    /// base URL. Set via `XboxClientOptions::achievements_base_url_override`.
    /// When `None`, the default Xbox endpoint is used. Tests set this
    /// to a local axum mock so the bulk scan pipeline can be exercised
    /// end-to-end without touching Xbox servers.
    achievements_base_override: Option<String>,
    /// Optional multi-token pool. When present, each outgoing request
    /// rotates across **distinct authenticated XUIDs** in the pool;
    /// this is useful when the user has signed in with several Xbox
    /// accounts and wants scans to split traffic across them. Multiple
    /// tokens for the same XUID do NOT multiply budget — Xbox almost
    /// certainly keys rate-limit buckets on the authenticated
    /// principal (the XUID in the JWT), so sibling tokens for one
    /// identity share one bucket. They're kept purely as fallbacks
    /// against token rotation / 401s.
    ///
    /// When `None`, falls back to `xauth` above for every request.
    token_pool: Option<Arc<crate::oauth::TokenPool>>,
    /// Per-endpoint throttle gates. When we see a 429, we parse the
    /// JSON body (`{"currentRequests","maxRequests","periodInSeconds"}`)
    /// and set the matching gate to `Instant::now() + period_seconds`.
    /// Every outgoing request checks its endpoint's gate first and
    /// sleeps until it opens. This replaces the old "every request
    /// retries independently with 10s backoff" loop, which on Xbox's
    /// REAL budgets (update: 100/15s; scan: 150/300s) caused O(N × 10s)
    /// wasted work when a burst bumped the budget.
    ///
    /// Keyed by [`EndpointKind`]. Gates are independent — hitting the
    /// update budget doesn't pause scan requests and vice-versa.
    throttle_gates: Mutex<HashMap<EndpointKind, Instant>>,
}

/// Classification of a URL for rate-limit bucket purposes. Xbox Live
/// enforces **different** budgets per API surface:
/// - `UpdateAchievement` → 100 req / 15s (bulk unlock POSTs)
/// - `ScanAchievements` → 150 req / 300s (cross-title achievement reads)
/// - `Other` → everything else (profile, games list, heartbeats) —
///   never hit 429 in practice, share a single permissive gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EndpointKind {
    UpdateAchievement,
    ScanAchievements,
    Other,
}

fn classify_endpoint(url: &str, method: &Method) -> EndpointKind {
    // `/achievements/<scid>/update` → POST update
    if method == Method::POST && url.contains("/achievements/") && url.ends_with("/update") {
        return EndpointKind::UpdateAchievement;
    }
    // `/achievements?maxItems=...` → GET scan (no `/update` suffix)
    if url.contains("/achievements?") || url.contains("/titleachievements?") {
        return EndpointKind::ScanAchievements;
    }
    // Anything that looks like the achievements endpoint but wasn't
    // caught above — bucket conservatively into scan.
    if url.contains("achievements.xboxlive.com") {
        return EndpointKind::ScanAchievements;
    }
    EndpointKind::Other
}

/// Wait at the throttle gate for the given endpoint until it opens.
/// No-op if the gate is not set or has already expired. Uses a short
/// sleep loop rather than a single long sleep so the gate can be
/// extended mid-wait (one task's 429 bumps the gate for all waiters).
async fn wait_at_gate(
    gates: &Mutex<HashMap<EndpointKind, Instant>>,
    kind: EndpointKind,
    on_throttle: Option<&Arc<dyn Fn(u64) + Send + Sync>>,
) {
    loop {
        let wait = {
            let mut map = gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Opportunistic cleanup — if the gate for this kind has
            // expired, drop it so the map doesn't keep stale Instants
            // around between bursts. Tiny, but it means future
            // `wait_at_gate` calls take the fast-path (no entry)
            // instead of the slow-path (get-match-Instant-compare).
            // See audit bug #26.
            if let Some(until) = map.get(&kind) {
                if *until <= Instant::now() {
                    map.remove(&kind);
                    return;
                }
            }
            match map.get(&kind) {
                Some(until) if *until > Instant::now() => *until - Instant::now(),
                _ => return,
            }
        };
        // Fire observer on the FIRST wait so the UI sees "rate-limited
        // — Ns to go" right away. Subsequent loop passes are short.
        if let Some(cb) = on_throttle {
            cb(wait.as_secs().max(1));
        }
        // Cap the sleep at 1s per iteration so if the gate is extended
        // again mid-wait, we pick that up quickly.
        let step = std::cmp::min(wait, Duration::from_secs(1));
        tokio::time::sleep(step).await;
    }
}

#[derive(Clone, Default)]
pub struct XboxClientOptions {
    /// The `XBL3.0 x=...;...` token value to send as `Authorization`.
    pub xauth: String,
    /// When true, forces `en-GB` as `accept-language`. When false, the host
    /// system locale is used (equivalent to `CultureInfo.CurrentCulture.Name`).
    pub region_override: bool,
    /// Optional override of the culture name detected from the system. When
    /// `None`, defaults to `en-US` (matches behaviour on non-Windows hosts).
    pub culture_name: Option<String>,
    /// When true, the `Signature` header is added for achievement unlock
    /// requests, preserving the original "fake signature" behaviour.
    pub fake_signature_enabled: bool,
    /// Request timeout. Defaults to 60s when not set.
    pub timeout: Option<Duration>,
    /// Pre-loaded events bundle. If `Some`, `unlock_achievement` uses the
    /// telemetry path for titles the bundle covers; otherwise it falls
    /// back to the direct `/achievements/update` call.
    pub events_bundle: Option<Arc<crate::events::EventsBundle>>,
    /// Events token. Required alongside `events_bundle` for event-based
    /// unlocks; ignored if the bundle is absent.
    pub events_token: Option<String>,
    /// Fired whenever the client sleeps on a 429 / 503 from Xbox. The
    /// argument is the actual wait-seconds the client will block for.
    /// GUI front-ends use this to render "Xbox is rate-limiting —
    /// waiting 30s" instead of a silently-frozen progress bar.
    pub on_throttle: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// Multi-token pool for rotating authorization across requests.
    /// When `Some`, each outgoing request picks a validated token
    /// from the pool — rotation is **by distinct authenticated
    /// XUID**, not by raw token string (see
    /// `TokenPool::pick_for_request` for the rationale). So N tokens
    /// for the SAME account share budget; N tokens across N
    /// different accounts split budget. On 401, the offending token
    /// is quarantined automatically. When `None`, falls back to
    /// `self.xauth` for every request (single-token behaviour).
    ///
    /// See `oauth::token_pool` for the full lifecycle.
    pub token_pool: Option<Arc<crate::oauth::TokenPool>>,
    /// Override the `https://achievements.xboxlive.com` base URL.
    /// **Test-only** — lets integration tests redirect bulk-scan
    /// calls to a local mock axum server. `None` = production.
    /// When set, must end without a trailing slash and have the
    /// form `http(s)://host:port` (no trailing path).
    #[doc(hidden)]
    pub achievements_base_url_override: Option<String>,
}

impl XboxClient {
    pub fn new(opts: XboxClientOptions) -> Result<Self> {
        let accept_language = if opts.region_override {
            "en-GB".to_string()
        } else {
            opts.culture_name.unwrap_or_else(|| "en-US".into())
        };
        // Why 300s: the request timeout in reqwest covers both
        // connect + body read. For maxItems=10000 responses against
        // mega-profiles (Riffai 125k), the 11.7MB body can take 140s
        // just to download over a 10-20 Mbps link. Raising the cap
        // well above the worst observed 142s gives enough margin
        // that occasional network stalls don't trigger body-read
        // timeouts.
        //
        // Live validation: first Riffai scan at 180s captured 12890
        // of 125381 (10%) because ~90% of pages hit body-read
        // timeouts on large gzip bodies. Bumping to 300s per request
        // + `--resume` flag collapses this to ~2 runs of 6-8 min.
        //
        // Smaller requests (unlock, heartbeat, own-profile scan) all
        // complete in <5s so the extra budget only bites on
        // cross-user scans of 100k+ achievement profiles.
        let timeout = opts.timeout.unwrap_or(Duration::from_secs(300));

        let http = Client::builder()
            .gzip(true)
            .deflate(true)
            .timeout(timeout)
            // Pool tuned for the "follow another profile" path: the bulk
            // achievements endpoint runs up to 24 pages in-flight, plus
            // the GUI may dispatch preview/unlock calls alongside, so a
            // single host routinely needs 32+ concurrent sockets. 128
            // gives plenty of headroom without paying fresh TLS
            // handshakes mid-scan on idle slots.
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(128)
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            // Stay on HTTP/1.1. Xbox Live rejects our explicit `Host:`
            // header under HTTP/2 with an opaque IIS-level 400. The
            // keep-alive pool above still lets us saturate the link via
            // persistent HTTP/1.1 connections.
            .http1_only()
            .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                http,
                xauth: opts.xauth,
                accept_language,
                fake_signature: opts.fake_signature_enabled,
                events: opts.events_bundle,
                events_token: RwLock::new(opts.events_token),
                on_throttle: opts.on_throttle,
                throttle_gates: Mutex::new(HashMap::new()),
                scid_cache: RwLock::new(HashMap::new()),
                achievements_cache: RwLock::new(HashMap::new()),
                achievements_base_override: opts
                    .achievements_base_url_override
                    .map(|s| s.trim_end_matches('/').to_string()),
                token_pool: opts.token_pool,
            }),
        })
    }

    /// BRUTAL SCAN FIX C2.2: accessor for the attached token pool so
    /// callers (CLI, IPC handlers) can build a scan_pool for the
    /// pool-aware bulk scan path. Returns None when the client was
    /// constructed without a pool (e.g. auxiliary clients built by
    /// the scan pool itself, to avoid recursive pool attachment).
    #[must_use]
    pub fn token_pool(&self) -> Option<Arc<crate::oauth::TokenPool>> {
        self.inner.token_pool.clone()
    }

    /// Clear the in-memory achievements cache. Call this after performing
    /// unlocks so the next read picks up the fresh server-side state.
    pub fn clear_achievements_cache(&self) {
        self.inner
            .achievements_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn invalidate_achievements_cache_entry(&self, xuid: &str, title_id: &str) {
        self.inner
            .achievements_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(xuid.to_string(), title_id.to_string()));
    }

    /// Bulk-insert a batch of `title_id -> service_config_id` mappings
    /// into the SCID cache. Called by `plan::builder::build_plan` right
    /// after the bulk achievements scan lands — the Xbox Live endpoint
    /// ships `serviceConfigId` on every row, so we can prime the cache
    /// for free and skip per-title resolve round-trips at replay time.
    ///
    /// For a SiegfriedX-tier profile (200+ titles) this hides ~16 seconds
    /// of latency that would otherwise surface as a stall between
    /// "click replay" and the first unlock landing.
    ///
    /// Duplicate keys overwrite the existing entry — if two rows disagree
    /// on a title's SCID (which shouldn't happen in practice), the
    /// last-write-wins matches how `resolve_service_config_id` would
    /// cache a fresh lookup. Empty or `ZERO_UID` values are silently
    /// skipped.
    pub fn warm_scid_cache_bulk(&self, entries: impl IntoIterator<Item = (String, String)>) {
        use crate::xbox::constants::ZERO_UID;
        let mut cache = self
            .inner
            .scid_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inserted = 0usize;
        let mut skipped = 0usize;
        for (title_id, scid) in entries {
            if scid.is_empty() || scid == ZERO_UID {
                skipped += 1;
                continue;
            }
            cache.insert(title_id, scid);
            inserted += 1;
        }
        tracing::info!(inserted, skipped, "scid cache warmed from bulk scan");
    }

    /// `true` if the title's event-based unlock template is loaded in the
    /// bundle AND we have a token, i.e. we'd try the telemetry path for
    /// it. Callers (like the bulk replay runner) use this to decide
    /// whether to do per-id sequential POSTs (events path) vs a single
    /// chunked bulk POST (direct path).
    pub fn is_event_based_title(&self, title_id: &str) -> bool {
        self.inner
            .events
            .as_ref()
            .is_some_and(|b| b.is_event_based(title_id))
    }

    /// Whether this client has an events bundle loaded + an events token
    /// set. When both are true, `unlock_achievement` uses the telemetry
    /// path for titles covered by the bundle.
    pub fn has_events_path(&self) -> bool {
        self.inner.events.is_some()
            && self
                .inner
                .events_token
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|t| !t.trim().is_empty())
    }

    /// Hot-swap the events token. Used by the auth-watcher background
    /// task to push a freshly-acquired `events_xauth` into a long-running
    /// runner's cached client. Setting `None` clears the token (the next
    /// event-based unlock attempt will return the "set events_token" error).
    ///
    /// All clones of this `XboxClient` (BBB / replay / nuclear runners
    /// hold their own `Arc<Inner>`) see the update because `events_token`
    /// is wrapped in a `RwLock` shared via `Arc<Inner>` — no per-runner
    /// notification needed.
    pub fn set_events_token(&self, token: Option<String>) {
        let mut guard = self
            .inner
            .events_token
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = token;
    }

    /// Snapshot the current events token (clone). Returns `None` if not set.
    /// Cheap — clone is one heap alloc when present.
    pub fn events_token(&self) -> Option<String> {
        self.inner
            .events_token
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Re-read `auth.json` and update the cached `events_token` if a
    /// freshly-derived `events_xauth` is present. Returns `true` when a
    /// non-empty token was installed (or was already set), `false` when
    /// no token is available. Idempotent — calling repeatedly with the
    /// same auth.json is a no-op (the lock just rewrites the same value).
    ///
    /// Two sources: the OAuth-saved session (`events_xauth`, minted from
    /// the events relying party at sign-in — the normal path) and
    /// `Settings.events_token` (pasted by hand, or recovered by the ETW
    /// fallback in `events::etw`).
    ///
    /// When exactly ONE of them decodes as a genuine events-relying-party
    /// token, that one wins regardless of source. Otherwise the session
    /// keeps priority, preserving the historical order. See
    /// `xau-cli/src/main.rs` and `xau-gui/src/ipc.rs::make_client`.
    pub fn reload_events_token_from_auth(&self) -> bool {
        // Read both sources outside the lock to keep the critical
        // section minimal.
        let from_session = crate::oauth::AuthSession::load()
            .ok()
            .flatten()
            .and_then(|s| s.events_xauth.map(|t| format!("x:{t}")));
        let from_settings = crate::settings::Settings::load_or_default()
            .ok()
            .and_then(|s| s.events_token.filter(|t| !t.trim().is_empty()));
        // Prefer a token that PROVABLY belongs to the events relying
        // party. Both sources look identical (`XBL3.0 x=…;<jwe>`), but
        // only an events-RP token actually unlocks anything — an XAUTH
        // is accepted by OneCollector with a 200 and then dropped.
        // Decoding the JWE header settles it locally.
        //
        // This matters for the ETW/manual fallback: it is used exactly
        // when the sign-in mint produced nothing usable, so a blind
        // "session always wins" would discard the good token in favour
        // of the broken one it was fetched to replace.
        let session_ok = from_session
            .as_deref()
            .is_some_and(crate::events::is_events_rp_token);
        let settings_ok = from_settings
            .as_deref()
            .is_some_and(crate::events::is_events_rp_token);
        let token = match (session_ok, settings_ok) {
            // Exactly one is a real events token — take it.
            (true, false) => from_session,
            (false, true) => from_settings,
            // Both valid, or neither provably valid: keep the historical
            // order (session first) so behaviour is unchanged for every
            // account where the mint works.
            _ => from_session.or(from_settings),
        };
        let has_token = token.as_ref().is_some_and(|t| !t.trim().is_empty());
        self.set_events_token(token);
        has_token
    }

    /// Returns the cached `ServiceConfigId` for the title, falling back to
    /// fetching the achievements list and extracting it from the first
    /// achievement entry. Subsequent calls for the same `title_id` are served
    /// from cache. `xuid` is only used for the miss path.
    pub async fn resolve_service_config_id(&self, xuid: &str, title_id: &str) -> Result<String> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(title_id, "title_id")?;
        if let Some(cached) = self
            .inner
            .scid_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(title_id)
            .cloned()
        {
            return Ok(cached);
        }
        let resp = self.get_achievements_for_title(xuid, title_id).await?;
        // Treat empty AND ZERO_UID as "not a real SCID" — matches
        // `warm_scid_cache_bulk` above at ~L393. Without this, the
        // first achievement entry carrying the serde-default sentinel
        // `"00000000-0000-0000-0000-000000000000"` was returned
        // verbatim, cached, and every subsequent unlock POST went to
        // `/scids/00000000…/achievements/update` → 404 → whole title
        // reported as Error. Audit: Agent-2 finding #5.
        use crate::xbox::constants::ZERO_UID;
        let scid = resp
            .achievements
            .iter()
            .find(|a| !a.service_config_id.is_empty() && a.service_config_id != ZERO_UID)
            .map(|a| a.service_config_id.clone())
            .ok_or_else(|| Error::other("ServiceConfigId not found for the title"))?;
        self.inner
            .scid_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(title_id.to_string(), scid.clone());
        Ok(scid)
    }

    /// Unlock a single achievement for `title_id`.
    ///
    /// When the title is covered by the loaded events bundle AND the client
    /// was built with an events token, this routes through the OneCollector
    /// telemetry path (the only way to unlock stat-based achievements —
    /// anything tracked by a gameplay counter). Otherwise it falls back to
    /// the direct `/achievements/update` endpoint.
    ///
    /// Typed arguments: `TitleId`, `Xuid`, and `AchievementId` are
    /// compile-time distinct, so passing arguments in the wrong order
    /// fails to compile. Previously all three were `&str` — the
    /// compiler accepted `unlock_achievement(ach_id, title_id, xuid)`
    /// and the server returned an opaque 400.
    ///
    /// Returns the `ServiceConfigId` that was used (empty string on the
    /// events path, since that endpoint doesn't care about SCID).
    pub async fn unlock_achievement(
        &self,
        title_id: &crate::types::TitleId,
        xuid: &crate::types::Xuid,
        achievement_id: &crate::types::AchievementId,
    ) -> Result<String> {
        let title_id_s = title_id.as_str();
        let xuid_s = xuid.as_str();
        let achievement_id_s = achievement_id.as_str();
        if let Some(bundle) = &self.inner.events {
            if bundle.is_event_based(title_id_s) {
                // Hot-reload from auth.json on every event-based unlock
                // when the cached token is empty — covers the "user
                // started bbb before doing OAuth login, then logged in
                // mid-run" case so the runner doesn't need to restart
                // to pick up the freshly-minted `events_xauth`. No-op
                // when a token is already cached.
                let mut token = self.events_token().unwrap_or_default();
                if token.trim().is_empty() {
                    self.reload_events_token_from_auth();
                    token = self.events_token().unwrap_or_default();
                }
                if token.trim().is_empty() {
                    return Err(Error::invalid_argument(format!(
                        "title {title_id_s} is event-based; set events_token in Settings (see the XAU events guide)"
                    )));
                }
                bundle
                    .unlock(
                        title_id_s,
                        achievement_id_s,
                        xuid_s,
                        &self.inner.xauth,
                        &token,
                    )
                    .await?;
                self.invalidate_achievements_cache_entry(xuid_s, title_id_s);
                return Ok(String::new());
            }
        }

        // Non-event-based title → classic direct-update endpoint.
        let scid = self.resolve_service_config_id(xuid_s, title_id_s).await?;
        self.unlock_title_based_achievement(&scid, title_id_s, xuid_s, achievement_id_s)
            .await?;
        self.invalidate_achievements_cache_entry(xuid_s, title_id_s);
        Ok(scid)
    }

    pub fn xauth(&self) -> &str {
        &self.inner.xauth
    }

    // ---------- header assembly ----------

    /// Pick an XAUTH for this request. When the multi-token pool is
    /// configured + has at least one validated entry, rotate across
    /// distinct authenticated XUIDs (see `TokenPool::pick_for_request`
    /// for why rotation is by identity, not by token string).
    /// Otherwise fall back to the single `inner.xauth` (original
    /// behaviour).
    pub(crate) fn pick_request_xauth(&self) -> String {
        if let Some(pool) = &self.inner.token_pool {
            if let Some(entry) = pool.pick_for_request() {
                return entry.xauth;
            }
        }
        self.inner.xauth.clone()
    }

    /// Notify the token pool that `xauth` got a 401 from Xbox so it
    /// gets quarantined and skipped on subsequent picks. No-op when
    /// no pool is configured.
    pub(crate) fn report_token_unauthorized(&self, xauth: &str) {
        if let Some(pool) = &self.inner.token_pool {
            crate::oauth::token_pool::report_401(pool, xauth);
        }
    }

    fn default_headers(&self) -> Result<HeaderMap> {
        // Read path: pool-rotated auth is OK (and desirable — spreads
        // rate-limit cost across every available XUID).
        self.default_headers_inner(&self.pick_request_xauth())
    }

    /// Like `default_headers` but ALWAYS uses the active identity's
    /// xauth (no pool rotation). Required for mutating endpoints
    /// where the Authorization token's XUID must match the body
    /// xuid — otherwise Xbox either rejects with a mismatch error
    /// or (worst case) applies the mutation to a sibling pool
    /// account. See Bug #2 in the audit.
    fn default_headers_active(&self) -> Result<HeaderMap> {
        self.default_headers_inner(&self.inner.xauth)
    }

    /// Shared implementation — callers pick the xauth (rotated or
    /// active) and this stamps the common non-auth headers.
    fn default_headers_inner(&self, xauth_for_request: &str) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        let auth = HeaderValue::from_str(xauth_for_request)
            .map_err(|e| Error::invalid_argument(format!("invalid xauth header: {e}")))?;
        h.insert(HeaderName::from_static("authorization"), auth);
        h.insert(
            HeaderName::from_static("accept-language"),
            HeaderValue::from_str(&self.inner.accept_language)
                .unwrap_or(HeaderValue::from_static("en-US")),
        );
        h.insert(
            HeaderName::from_static("accept-encoding"),
            HeaderValue::from_static(c::header_values::ACCEPT_ENCODING),
        );
        h.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static(c::header_values::ACCEPT),
        );
        Ok(h)
    }

    fn headers_with(&self, extra: &[(&'static str, &str)]) -> Result<HeaderMap> {
        Self::apply_extras(self.default_headers()?, extra)
    }

    /// Like `headers_with` but pinned to the active identity. Use for
    /// POST/PUT/DELETE calls whose body references `self.inner.xuid`.
    pub(crate) fn headers_with_active(&self, extra: &[(&'static str, &str)]) -> Result<HeaderMap> {
        Self::apply_extras(self.default_headers_active()?, extra)
    }

    fn apply_extras(mut h: HeaderMap, extra: &[(&'static str, &str)]) -> Result<HeaderMap> {
        for (k, v) in extra {
            // `HeaderName::from_static` PANICS on any non-lowercase byte. Use
            // the fallible constructor and return an error instead — the
            // constants module should already hand us lowercase names, but
            // this guards against future typos from crashing the thread.
            let name = HeaderName::try_from(*k)
                .map_err(|e| Error::other(format!("invalid header name `{k}`: {e}")))?;
            let val = HeaderValue::from_str(v).map_err(|e| Error::other(e.to_string()))?;
            h.insert(name, val);
        }
        Ok(h)
    }

    // ---------- profile ----------

    /// `GET https://profile.xboxlive.com/users/me/profile/settings?settings=Gamertag`
    pub async fn get_basic_profile(&self) -> Result<BasicProfile> {
        let headers = self.headers_with(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_2,
            ),
            (c::header_names::HOST, c::hosts::PROFILE),
            (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
        ])?;
        self.request_json(Method::GET, c::urls::GAMERTAG, headers, None)
            .await
    }

    /// Avatar URL and gamerscore for `xuid`, in one profile call.
    ///
    /// Split out from `get_profile` because the mobile account list only
    /// needs these two fields, and the full profile payload is large
    /// enough that fetching it per row on a phone is wasteful.
    ///
    /// # Errors
    /// Propagates transport and Xbox-side failures. A profile that
    /// simply has no picture yields an empty string, not an error.
    pub async fn get_display_bits(&self, xuid: &str) -> Result<(String, i64)> {
        require_non_empty(xuid, "xuid")?;
        let url = format!(
            "https://{}/users/xuid({xuid})/profile/settings?settings=GameDisplayPicRaw,Gamerscore",
            c::hosts::PROFILE
        );
        let headers = self.headers_with(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_2,
        )])?;
        let profile: crate::models::profile::BasicProfile =
            self.request_json(Method::GET, &url, headers, None).await?;
        let mut pic = String::new();
        let mut score = 0i64;
        for user in &profile.profile_users {
            for setting in &user.settings {
                match setting.id.as_deref() {
                    Some("GameDisplayPicRaw") => pic = setting.value.clone().unwrap_or_default(),
                    Some("Gamerscore") => {
                        score = setting
                            .value
                            .as_deref()
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }
        Ok((pic, score))
    }

    pub async fn get_profile(&self, xuid: &str) -> Result<Profile> {
        require_non_empty(xuid, "xuid")?;
        let headers = self.headers_with(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_5,
            ),
            (c::header_names::HOST, c::hosts::PEOPLE_HUB),
            (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
        ])?;
        let url = c::profile_url(xuid);
        self.request_json(Method::GET, &url, headers, None).await
    }

    /// Returns the full JSON response from `profile.xboxlive.com/users/gt(...)` as a
    /// raw `serde_json::Value` since the shape is untyped in the original code.
    pub async fn get_gamertag_profile(&self, gamertag: &str) -> Result<Value> {
        require_non_empty(gamertag, "gamertag")?;
        let headers = self.headers_with(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_2,
            ),
            (c::header_names::HOST, c::hosts::PROFILE),
        ])?;
        let url = c::gamertag_search_url(gamertag);
        self.request_json(Method::GET, &url, headers, None).await
    }

    // ---------- games / titles ----------

    pub async fn get_game_title(&self, xuid: &str, title_id: &str) -> Result<GameTitle> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(title_id, "title_id")?;
        let headers = self.headers_with(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_2,
        )])?;
        let body = GameTitleRequest {
            pfns: None,
            title_ids: vec![title_id.to_string()],
        };
        self.request_json(Method::POST, &c::title_url(xuid), headers, Some(&body))
            .await
    }

    /// Batch resolve title_ids → cover art + per-title achievement
    /// counts + SCID via the titlehub `/titles/batch/decoration/scid,
    /// image,achievement` endpoint.
    ///
    /// Accepts an arbitrary list of title IDs — WORKS for titles that
    /// the user has never launched (unlike `get_games_list`, which
    /// only shows games with gameplay presence). This makes it the
    /// complement to the bulk achievements scan: bulk gives us every
    /// title_id the profile has achievements for, batch gives us the
    /// cover image + name for those IDs.
    ///
    /// `title_ids.len()` cap: 200 per request is a safe empirical
    /// bound. Caller handles chunking above that.
    pub async fn get_game_titles_batch(
        &self,
        xuid: &str,
        title_ids: &[String],
    ) -> Result<GameTitle> {
        require_non_empty(xuid, "xuid")?;
        if title_ids.is_empty() {
            return Ok(GameTitle::default());
        }
        let headers = self.headers_with(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_2,
        )])?;
        let body = GameTitleRequest {
            pfns: None,
            title_ids: title_ids.to_vec(),
        };
        self.request_json(
            Method::POST,
            &c::title_batch_with_images_url(xuid),
            headers,
            Some(&body),
        )
        .await
    }

    pub async fn get_gamepass_membership(&self, xuid: &str) -> Result<Gamepass> {
        require_non_empty(xuid, "xuid")?;
        let headers = self.default_headers()?;
        let url = c::gamepass_membership_url(xuid);
        self.request_json(Method::GET, &url, headers, None).await
    }

    pub async fn get_games_list(&self, xuid: &str) -> Result<TitlesList> {
        require_non_empty(xuid, "xuid")?;
        let headers = self.headers_with(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_2,
            ),
            (c::header_names::HOST, c::hosts::TITLE_HUB),
            (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
        ])?;
        let url = c::titles_url(xuid);
        self.request_json(Method::GET, &url, headers, None).await
    }

    pub async fn get_title_ids_from_gamepass(&self, product_id: &str) -> Result<GamePassProducts> {
        require_non_empty(product_id, "product_id")?;
        let headers = self.default_headers()?;
        let body = GamepassProductsRequest {
            products: vec![product_id.to_string()],
        };
        self.request_json(
            Method::POST,
            c::urls::GAMEPASS_CATALOG,
            headers,
            Some(&body),
        )
        .await
    }

    // ---------- stats ----------

    pub async fn get_game_stats(&self, xuid: &str, title_id: &str) -> Result<GameStatsResponse> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(title_id, "title_id")?;
        let headers = self.headers_with(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_2,
        )])?;
        let body = GameStatsRequest {
            arrange_by_field: "xuid".into(),
            xuids: vec![xuid.to_string()],
            stats: vec![GameStat {
                name: "MinutesPlayed".into(),
                title_id: Some(title_id.to_string()),
            }],
        };
        self.request_json(Method::POST, c::urls::USER_STATS, headers, Some(&body))
            .await
    }

    /// Batch-fetch the target's real `MinutesPlayed` for many titles in
    /// one (chunked) userstats call, returning `title_id → minutes`.
    ///
    /// This is the same "time played" figure TrueAchievements shows on
    /// its completionist lists — the authoritative "how long did it
    /// actually take" number, unlike the calendar span between first
    /// and last unlock. Used at plan-build to populate
    /// `Plan.title_min_completion_secs` for sequential-mode pacing.
    ///
    /// Best-effort per title: a title whose stat is private, absent, or
    /// non-numeric is simply omitted from the map (never an error).
    /// `MinutesPlayed` is the queried `xuid`'s playtime, so pass the
    /// TARGET's xuid (the profile being cloned), not the caller's.
    pub async fn get_minutes_played_map(
        &self,
        xuid: &str,
        title_ids: &[String],
    ) -> Result<std::collections::HashMap<String, i64>> {
        require_non_empty(xuid, "xuid")?;
        let mut out: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        if title_ids.is_empty() {
            return Ok(out);
        }
        // Many stats (one per title) per POST; chunk so the request body
        // stays well within limits on huge libraries.
        const CHUNK: usize = 100;
        for chunk in title_ids.chunks(CHUNK) {
            let headers = self.headers_with(&[(
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_2,
            )])?;
            let body = GameStatsRequest {
                arrange_by_field: "xuid".into(),
                xuids: vec![xuid.to_string()],
                stats: chunk
                    .iter()
                    .map(|t| GameStat {
                        name: "MinutesPlayed".into(),
                        title_id: Some(t.clone()),
                    })
                    .collect(),
            };
            // PER-CHUNK best-effort, matching this function's documented
            // contract ("never an error"). The `?` that used to be here
            // made one failed chunk abort the whole call, and the only
            // caller — the sequential runner's live floor recovery — does
            // `if let Ok(map)`, so a single transient 500 on chunk 7 of 20
            // silently discarded the OTHER NINETEEN chunks' worth of
            // recovered playtime. Every title then fell back to its
            // calendar span, which is how a run ended up pacing one game
            // across ~1095 days. Keep what we got; log what we lost.
            let resp: GameStatsResponse = match self
                .request_json(Method::POST, c::urls::USER_STATS, headers, Some(&body))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(
                        error = ?e,
                        chunk_len = chunk.len(),
                        recovered_so_far = out.len(),
                        "MinutesPlayed chunk failed; keeping the other chunks"
                    );
                    continue;
                }
            };
            for coll in &resp.stat_lists_collection {
                for stat in &coll.stats {
                    // We only requested MinutesPlayed, but be defensive:
                    // accept an unnamed stat, reject any other named one.
                    let name_ok = stat
                        .name
                        .as_deref()
                        .map_or(true, |n| n.eq_ignore_ascii_case("minutesplayed"));
                    if !name_ok {
                        continue;
                    }
                    let (Some(tid), Some(val)) = (stat.title_id.as_deref(), stat.value.as_deref())
                    else {
                        continue;
                    };
                    if let Ok(mins) = val.trim().parse::<i64>() {
                        if mins > 0 {
                            let e = out.entry(tid.to_string()).or_insert(0);
                            *e = (*e).max(mins);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    // ---------- spoofing ----------

    pub async fn send_heartbeat(&self, xuid: &str, spoofed_title_id: &str) -> Result<()> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(spoofed_title_id, "spoofed_title_id")?;
        // Mutating presence on `xuid` — MUST use the active identity's
        // token so the Authorization XUID matches the URL XUID. Pool
        // rotation here would send presence for account A using token
        // for account B → either rejected or silently cross-written.
        let headers = self.headers_with_active(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_3,
        )])?;
        let body = HeartbeatRequest::for_title(spoofed_title_id);
        let _ = self
            .request_text_active(Method::POST, &c::heartbeat_url(xuid), headers, Some(&body))
            .await?;
        Ok(())
    }

    /// Send a single `presence-heartbeat` covering multiple titles in one
    /// request. The Xbox API accepts up to 10 entries per call; longer lists
    /// are chunked and sent sequentially.
    pub async fn send_heartbeat_bulk(&self, xuid: &str, title_ids: &[String]) -> Result<usize> {
        require_non_empty(xuid, "xuid")?;
        if title_ids.is_empty() {
            return Ok(0);
        }
        // See comment on `send_heartbeat` — pinned to active identity.
        let headers = self.headers_with_active(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_3,
        )])?;
        let url = c::heartbeat_url(xuid);
        let mut total = 0usize;
        for chunk in title_ids.chunks(c::HEARTBEAT_CHUNK_SIZE) {
            let body = HeartbeatRequest::for_titles(chunk.iter().cloned());
            let _ = self
                .request_text_active(Method::POST, &url, headers.clone(), Some(&body))
                .await?;
            total += chunk.len();
        }
        // BUG HUNTER M-presence-trace: emit an info-level event after a
        // successful presence heartbeat POST so live test runs can prove
        // the keepalive cadence (every `FARM_PING_INTERVAL_SECS`) without
        // resorting to trace-level logging.
        tracing::info!(titles = total, "presence-heartbeat sent");
        Ok(total)
    }

    pub async fn stop_heartbeat(&self, xuid: &str) -> Result<()> {
        require_non_empty(xuid, "xuid")?;
        // Mutating presence DELETE — same active-identity rule as
        // `send_heartbeat` above.
        let headers = self.headers_with_active(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_3,
        )])?;
        let _ = self
            .request_text_active(Method::DELETE, &c::heartbeat_url(xuid), headers, None)
            .await?;
        // BUG HUNTER M-presence-trace: emit an info-level event after the
        // presence DELETE so the runner-stop path can be audited without
        // network capture (proves the RAII PresenceGuard fired and the
        // server-side presence record was torn down).
        tracing::info!("presence-heartbeat stopped (DELETE)");
        Ok(())
    }
}

fn require_non_empty(s: &str, name: &str) -> Result<()> {
    if s.trim().is_empty() {
        Err(Error::invalid_argument(format!(
            "`{name}` must not be empty"
        )))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct UnlockAllSummary {
    pub service_config_id: String,
    pub ids: Vec<String>,
    pub total: usize,
    pub attempted: usize,
    pub failed_ids: Vec<String>,
}

impl UnlockAllSummary {
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.failed_ids.is_empty()
    }
}

// ----- tiny helper trait for passing heterogeneous Serialize bodies ------

mod erased_json {
    use serde::Serialize;

    use crate::error::Result;

    pub trait JsonBody {
        fn as_json_string(&self) -> Result<String>;
    }

    impl<T: Serialize> JsonBody for T {
        fn as_json_string(&self) -> Result<String> {
            Ok(serde_json::to_string(self)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_with_rejects_invalid_xauth_instead_of_panicking() {
        let client = XboxClient::new(XboxClientOptions {
            xauth: "XBL3.0 x=1;bad\nheader".into(),
            ..Default::default()
        })
        .expect("client");

        let err = client
            .headers_with(&[])
            .expect_err("invalid xauth should error");
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert!(
            err.to_string().contains("invalid xauth header"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn invalidate_achievements_cache_entry_only_removes_target_key() {
        let client = XboxClient::new(XboxClientOptions {
            xauth: "XBL3.0 x=1;dummy".into(),
            ..Default::default()
        })
        .expect("client");

        let target_key = ("xuid-1".to_string(), "title-a".to_string());
        let other_key = ("xuid-1".to_string(), "title-b".to_string());
        {
            let mut cache = client
                .inner
                .achievements_cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.insert(
                target_key.clone(),
                (Instant::now(), AchievementsResponse::default()),
            );
            cache.insert(
                other_key.clone(),
                (Instant::now(), AchievementsResponse::default()),
            );
        }

        client.invalidate_achievements_cache_entry("xuid-1", "title-a");

        let cache = client
            .inner
            .achievements_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!cache.contains_key(&target_key));
        assert!(cache.contains_key(&other_key));
    }
}
