//! Per-title achievement reads + all unlock methods. Grouped because
//! `unlock_all_for_title` needs `get_achievements_for_title` (fetches
//! the catalogue to know what to unlock), and `unlock_title_based_*`
//! variants share the chunking + POST path.

use std::time::{Duration, Instant};

use reqwest::Method;

use crate::error::{Error, Result};
use crate::models::{
    Achievement, AchievementsArrayEntry, AchievementsResponse, UnlockTitleBasedAchievementRequest,
    Xbox360AchievementResponse,
};
use crate::xbox::constants as c;

use super::{require_non_empty, UnlockAllSummary, XboxClient};

/// Summary of a chunked unlock stream. Returned by
/// [`XboxClient::unlock_title_based_achievements_detailed`] so callers
/// can route partial-chunk failures correctly — chunks 1..N-1 are
/// live on Xbox when chunk N fails, and the runner MUST NOT report
/// "0 unlocks" for a title whose first chunks already committed. See
/// audit bug BBB-Unlock#2.
#[derive(Debug, Default)]
pub struct UnlockChunkSummary {
    /// IDs confirmed to have landed on Xbox (chunks whose POST
    /// returned 2xx or 304). Order matches the POST order, which
    /// matches `achievement_ids` order from the caller.
    pub succeeded_ids: Vec<String>,
    /// The error that stopped the stream, if any. When `None`, every
    /// chunk succeeded. When `Some`, `succeeded_ids` reflects what
    /// already committed before the failure.
    pub last_error: Option<Error>,
    /// Number of chunks actually POSTed (including the failing one).
    /// Useful for telemetry: divides `succeeded_ids.len()` by
    /// `chunks_attempted` to get a per-chunk success rate.
    pub chunks_attempted: usize,
}

impl XboxClient {
    pub async fn get_achievements_for_title(
        &self,
        xuid: &str,
        title_id: &str,
    ) -> Result<AchievementsResponse> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(title_id, "title_id")?;
        // Cache hit? Serve instantly if < 60s old. This makes the GUI
        // feel snappy when the user clicks between titles — the second
        // time they open Void Blasters (for example), the achievements
        // list renders immediately instead of waiting for a round-trip.
        let cache_key = (xuid.to_string(), title_id.to_string());
        {
            let cache = self
                .inner
                .achievements_cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((fetched_at, resp)) = cache.get(&cache_key) {
                if fetched_at.elapsed() < Duration::from_secs(60) {
                    return Ok(resp.clone());
                }
            }
        }
        let build_headers = || {
            self.headers_with(&[
                (
                    c::header_names::CONTRACT_VERSION,
                    c::header_values::CONTRACT_VERSION_4,
                ),
                (c::header_names::HOST, c::hosts::ACHIEVEMENTS),
                (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
            ])
        };
        // First page.
        let mut all: AchievementsResponse = self
            .request_json(
                Method::GET,
                &c::query_achievements_url(xuid, title_id),
                build_headers()?,
                None,
            )
            .await?;

        // Follow continuation tokens until the server runs out. Defensive:
        // titles with >1000 achievements are rare but exist, and without
        // this loop we'd silently truncate. Cap at 50 pages (50k items)
        // so a pathological server response can't run us forever.
        let mut guard = 0;
        while let Some(token) = all
            .paging_info
            .as_ref()
            .and_then(|p| p.continuation_token.as_deref())
            .filter(|t| !t.is_empty())
        {
            if guard >= 50 {
                tracing::warn!(
                    %title_id,
                    guard,
                    "continuation guard hit — stopping; achievement list may be incomplete"
                );
                break;
            }
            guard += 1;
            let url = c::query_achievements_url_continued(xuid, title_id, token);
            let next: AchievementsResponse = self
                .request_json(Method::GET, &url, build_headers()?, None)
                .await?;
            all.achievements.extend(next.achievements);
            all.paging_info = next.paging_info;
        }
        // Populate cache for subsequent reads.
        self.inner
            .achievements_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cache_key, (Instant::now(), all.clone()));
        Ok(all)
    }

    pub async fn get_achievements_for_360_title(
        &self,
        xuid: &str,
        title_id: &str,
    ) -> Result<Xbox360AchievementResponse> {
        require_non_empty(xuid, "xuid")?;
        require_non_empty(title_id, "title_id")?;
        let headers = self.headers_with(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_3,
            ),
            (c::header_names::HOST, c::hosts::ACHIEVEMENTS),
            (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
        ])?;
        let url = c::query_achievements_360_url(xuid, title_id);
        self.request_json(Method::GET, &url, headers, None).await
    }

    /// Unlock a single title-based achievement. Equivalent to
    /// `UnlockTitleBasedAchievementAsync` in the original C# code.
    pub async fn unlock_title_based_achievement(
        &self,
        service_config_id: &str,
        title_id: &str,
        xuid: &str,
        achievement_id: &str,
    ) -> Result<()> {
        self.unlock_title_based_achievements(
            service_config_id,
            title_id,
            xuid,
            &[achievement_id.to_string()],
        )
        .await
    }

    /// Unlock multiple title-based achievements. Automatically chunks into
    /// batches of `UNLOCK_CHUNK_SIZE` because Xbox Live rejects bodies with
    /// too many entries.
    ///
    /// Calls [`unlock_title_based_achievements_chunked`] with the default
    /// `UNLOCK_CHUNK_SIZE`. Use the `_chunked` variant directly when the
    /// caller wants **strict per-game chronology** (chunk_size=1: each
    /// achievement in its own POST, server-side `timeUnlocked` grows
    /// strictly monotonic in the order IDs appear in `achievement_ids`).
    pub async fn unlock_title_based_achievements(
        &self,
        service_config_id: &str,
        title_id: &str,
        xuid: &str,
        achievement_ids: &[String],
    ) -> Result<()> {
        self.unlock_title_based_achievements_chunked(
            service_config_id,
            title_id,
            xuid,
            achievement_ids,
            c::UNLOCK_CHUNK_SIZE,
        )
        .await
    }

    /// Like [`unlock_title_based_achievements`] but lets the caller
    /// pick the chunk size. `chunk_size=1` → one achievement per POST
    /// → strict timestamp order on the server side (Xbox stamps each
    /// POST with its own `timeUnlocked`, so sequential POSTs give
    /// sequential timestamps). `chunk_size` gets clamped to `[1, 64]`
    /// because Xbox rejects batches above 64 with `{"code":6}` (see
    /// `UNLOCK_CHUNK_SIZE` doc comment for the empirical probe).
    ///
    /// Returns `Err` if ANY chunk fails. Callers that need to know
    /// which ids actually landed on Xbox before a mid-stream failure
    /// should use [`unlock_title_based_achievements_detailed`]
    /// instead — this wrapper discards that information.
    pub async fn unlock_title_based_achievements_chunked(
        &self,
        service_config_id: &str,
        title_id: &str,
        xuid: &str,
        achievement_ids: &[String],
        chunk_size: usize,
    ) -> Result<()> {
        let summary = self
            .unlock_title_based_achievements_detailed(
                service_config_id,
                title_id,
                xuid,
                achievement_ids,
                chunk_size,
            )
            .await?;
        if let Some(e) = summary.last_error {
            return Err(e);
        }
        Ok(())
    }

    /// Low-level variant that returns exactly which ids landed on
    /// Xbox + the error (if any) that stopped the stream. Callers
    /// (notably `bit_by_bit::process_title`) use this to route
    /// partial-chunk failures into `TitleResult::Partial` instead
    /// of losing the chunks-that-succeeded when chunk N+1 fails.
    /// See audit bug BBB-Unlock#2.
    ///
    /// Always returns `Ok(summary)` from the function itself — the
    /// per-chunk error is surfaced via `summary.last_error`. A
    /// short-circuit `Err` still comes from argument validation
    /// (empty xuid / title_id / scid).
    pub async fn unlock_title_based_achievements_detailed(
        &self,
        service_config_id: &str,
        title_id: &str,
        xuid: &str,
        achievement_ids: &[String],
        chunk_size: usize,
    ) -> Result<UnlockChunkSummary> {
        require_non_empty(service_config_id, "service_config_id")?;
        require_non_empty(title_id, "title_id")?;
        require_non_empty(xuid, "xuid")?;
        if achievement_ids.is_empty() {
            return Ok(UnlockChunkSummary::default());
        }
        // Clamp: 0 is useless, anything above server max fails.
        let chunk_size = chunk_size.clamp(1, c::UNLOCK_CHUNK_SIZE);

        let mut extra_headers: Vec<(&'static str, &str)> = vec![
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_2,
            ),
            (c::header_names::HOST, c::hosts::ACHIEVEMENTS),
            (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
            (
                c::header_names::USER_AGENT,
                c::header_values::ACHIEVEMENTS_USER_AGENT,
            ),
        ];
        if self.inner.fake_signature {
            extra_headers.push((c::header_names::SIGNATURE, c::header_values::FAKE_SIGNATURE));
        }
        // CRITICAL: unlock is a mutating POST. The body encodes `xuid`
        // (the account we're unlocking on); the Authorization header
        // MUST carry the token for that same xuid. `headers_with`
        // would rotate across the pool and could mix a sibling
        // token's XUID with this body's XUID → Xbox mismatch error
        // at best, silent cross-account unlock at worst. Use the
        // active-identity variant + `request_text_active` which
        // also refuses pool rotation on 401 (audit BBB-Unlock#1).
        let headers = self.headers_with_active(&extra_headers)?;
        // Apply the base-URL override when set so integration tests
        // (tests/common/mock_xbox.rs) can intercept unlock POSTs.
        // In production the override is `None` and this is a no-op.
        let url = super::rewrite_base_url(
            &c::update_achievements_url(xuid, service_config_id),
            self.inner.achievements_base_override.as_deref(),
        );
        let mut summary = UnlockChunkSummary::default();

        for chunk in achievement_ids.chunks(chunk_size) {
            let achievements: Vec<AchievementsArrayEntry> = chunk
                .iter()
                .map(|id| AchievementsArrayEntry::completed(id.clone()))
                .collect();
            let body = UnlockTitleBasedAchievementRequest::new(
                service_config_id,
                title_id,
                xuid,
                achievements,
            );
            summary.chunks_attempted = summary.chunks_attempted.saturating_add(1);
            match self
                .request_text_active(Method::POST, &url, headers.clone(), Some(&body))
                .await
            {
                Ok(_) => {
                    // Xbox returned success (or 304 — idempotent
                    // success, already unlocked). Record every id in
                    // this chunk as landed.
                    summary.succeeded_ids.extend(chunk.iter().cloned());
                }
                Err(e) => {
                    // Chunk N failed. Chunks 1..N-1 ARE live on Xbox
                    // (server-side mutations are per-chunk atomic).
                    // Record the error and stop — do NOT retry later
                    // chunks on an error path because (a) the error
                    // may be a permission / plan-shape issue that
                    // repeats, (b) retrying chunk N+1 after chunk N
                    // failed risks creating gaps in chronology order
                    // if chunk_size==1.
                    summary.last_error = Some(e);
                    break;
                }
            }
        }
        if !summary.succeeded_ids.is_empty() {
            self.invalidate_achievements_cache_entry(xuid, title_id);
        }
        Ok(summary)
    }

    /// Combines `get_achievements_for_title` with `unlock_title_based_achievements`.
    /// Returns the list of IDs unlocked. Caller provides a filter closure if it
    /// only wants to unlock a subset (e.g. not-yet-achieved ones). Populates
    /// the service-config-id cache as a side effect.
    pub async fn unlock_all_for_title(
        &self,
        title_id: &str,
        xuid: &str,
        filter: impl Fn(&Achievement) -> bool,
    ) -> Result<UnlockAllSummary> {
        require_non_empty(title_id, "title_id")?;
        require_non_empty(xuid, "xuid")?;

        let achievements = self.get_achievements_for_title(xuid, title_id).await?;
        if achievements.achievements.is_empty() {
            return Ok(UnlockAllSummary {
                service_config_id: String::new(),
                ids: Vec::new(),
                total: 0,
                attempted: 0,
                failed_ids: Vec::new(),
            });
        }

        let service_config_id = achievements
            .achievements
            .iter()
            .find(|a| !a.service_config_id.is_empty())
            .map(|a| a.service_config_id.clone())
            .ok_or_else(|| Error::other("ServiceConfigId not found for the title"))?;

        // Populate cache for subsequent single-unlock calls.
        self.inner
            .scid_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(title_id.to_string(), service_config_id.clone());

        let ids: Vec<String> = achievements
            .achievements
            .iter()
            .filter(|a| filter(a))
            .map(|a| a.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(UnlockAllSummary {
                service_config_id,
                ids: Vec::new(),
                total: 0,
                attempted: 0,
                failed_ids: Vec::new(),
            });
        }

        // If this title is event-based (covered by the Events bundle) AND
        // we have an events token, iterate via the telemetry path — the
        // direct `/achievements/update` call we'd otherwise use returns
        // `code 49 · None of the submitted achievements may be updated in
        // this fashion` for stat-based achievements, so the call would
        // silently fail per-item.
        if let Some(bundle) = &self.inner.events {
            if bundle.is_event_based(title_id) {
                // Hot-reload from auth.json when the cached token is
                // empty so a long-running unlock_all sees a freshly
                // device-code-acquired `events_xauth` without restart.
                let mut token = self.events_token().unwrap_or_default();
                if token.trim().is_empty() {
                    self.reload_events_token_from_auth();
                    token = self.events_token().unwrap_or_default();
                }
                if token.trim().is_empty() {
                    return Err(Error::invalid_argument(format!(
                        "title {title_id} is event-based; set events_token in Settings to enable bulk unlock via telemetry"
                    )));
                }
                // Per-achievement POST — OneCollector has no bulk endpoint
                // for these events. Best-effort: collect partial success.
                let mut successful = Vec::with_capacity(ids.len());
                let mut failed_ids = Vec::new();
                for id in &ids {
                    match bundle
                        .unlock(title_id, id, xuid, &self.inner.xauth, &token)
                        .await
                    {
                        Ok(()) => successful.push(id.clone()),
                        Err(e) => {
                            tracing::warn!(title_id, ach_id = %id, ?e, "event unlock failed — continuing");
                            failed_ids.push(id.clone());
                        }
                    }
                }
                if !successful.is_empty() {
                    self.invalidate_achievements_cache_entry(xuid, title_id);
                }
                return Ok(UnlockAllSummary {
                    service_config_id,
                    total: successful.len(),
                    ids: successful,
                    attempted: ids.len(),
                    failed_ids,
                });
            }
        }

        // Non-event-based → classic bulk POST.
        let attempted = ids.len();
        self.unlock_title_based_achievements(&service_config_id, title_id, xuid, &ids)
            .await?;
        self.invalidate_achievements_cache_entry(xuid, title_id);

        Ok(UnlockAllSummary {
            service_config_id,
            total: attempted,
            ids,
            attempted,
            failed_ids: Vec::new(),
        })
    }
}
