//! Bulk profile-level achievement scan. The heavy lifter: fetch every
//! achievement the user has across every title in one paginated pass,
//! replacing thousands of per-title calls with a handful of bulk pages.
//!
//! Split from `client.rs` because `bulk_pages_inner` alone is ~500
//! lines of pipeline code (PROBE + parallel fetches + drain + recovery)
//! and the caller shims (`get_all_unlocked_achievements*`) push the
//! group past 600 lines total.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::models::{Achievement, AchievementsResponse, SlimAchievement, SlimAchievementsResponse};
use crate::xbox::constants as c;

use super::{require_non_empty, rewrite_base_url, XboxClient};

/// BRUTAL SCAN FIX C1.2/1.3: persistent checkpoint state. Serialized
/// every N pages during scan so a stall / timeout / crash never loses
/// progress. On retry (resume), this file is loaded and already-fetched
/// offsets are skipped — mega profiles (126k+) that stall mid-scan
/// converge in 2-4 retry runs instead of starting from zero every time.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScanCheckpoint<Item> {
    /// Schema version. Bump on breaking changes.
    version: u32,
    /// Scanner xuid (IP bound) — reject cache if wrong xuid to avoid
    /// cross-profile contamination.
    xuid: String,
    /// Known total_records from Xbox pagingInfo. None when sentinel
    /// fallback (pagingInfo absent).
    total_records: Option<usize>,
    /// Offsets already successfully fetched. Next run skips these.
    completed_offsets: Vec<usize>,
    /// Items collected so far.
    items: Vec<Item>,
}

impl<Item: Serialize + DeserializeOwned> ScanCheckpoint<Item> {
    fn load(path: &Path, expected_xuid: &str) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let state: Self = serde_json::from_str(&raw).ok()?;
        if state.version != 1 || state.xuid != expected_xuid {
            return None;
        }
        Some(state)
    }

    fn save(
        path: &Path,
        xuid: &str,
        total_records: Option<usize>,
        completed_offsets: &[usize],
        items: &[Item],
    ) -> Result<()>
    where
        Item: Clone,
    {
        let state = ScanCheckpoint {
            version: 1u32,
            xuid: xuid.to_string(),
            total_records,
            completed_offsets: completed_offsets.to_vec(),
            items: items.to_vec(),
        };
        let tmp = path.with_extension("scan_cache.tmp");
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string(&state)
            .map_err(|e| Error::other(format!("serialize scan checkpoint: {e}")))?;
        std::fs::write(&tmp, json)
            .map_err(|e| Error::other(format!("write scan checkpoint tmp: {e}")))?;
        // Atomic rename so readers never see a torn write.
        std::fs::rename(&tmp, path)
            .map_err(|e| Error::other(format!("rename scan checkpoint: {e}")))?;
        Ok(())
    }

    fn delete(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

impl XboxClient {
    // ---------- achievements ----------

    /// **Bulk fetch**: every achievement the user has on Xbox Live, across
    /// every title, in one paginated scan. Returns ALL `Achievement` rows
    /// with `title_associations` identifying which title each belongs to,
    /// plus `progression.time_unlocked` timestamps for any `Achieved`
    /// entries. Accepts `unlocked_only` to restrict to the ones we'd
    /// actually replay.
    ///
    /// This replaces thousands of per-title `get_achievements_for_title`
    /// calls with ~N/1000 requests (N = user's total achievement count).
    /// For SiegfriedX (~126k achievements) that's 126 requests vs ~4700.
    /// Typical wall-clock: **under 30 seconds** for the whole profile.
    ///
    /// This is the **full-fat** variant — the response carries every
    /// field the Xbox endpoint returns. For plan-building callers,
    /// `get_all_unlocked_achievements_slim_with_progress` is strictly
    /// faster (7 fields vs ~18, parsed on the blocking pool).
    pub async fn get_all_unlocked_achievements(
        &self,
        xuid: &str,
        unlocked_only: bool,
    ) -> Result<Vec<Achievement>> {
        self.get_all_unlocked_achievements_with_progress(
            xuid,
            unlocked_only,
            None::<fn(usize, Option<i64>)>,
        )
        .await
    }

    /// Bulk achievements fetch with per-page progress callback.
    ///
    /// **Parallel strategy**: page 1 tells us `totalRecords`, then we
    /// dispatch all remaining pages concurrently via `skipItems=N`
    /// offsets (bounded at 24 in-flight to saturate the achievements
    /// endpoint's per-user budget without tripping 429s). For a
    /// SiegfriedX-sized profile (126k achievements, 127 pages) this
    /// takes ~4-6s instead of 15-30s sequential.
    ///
    /// The callback fires every time a page lands, with
    /// `(items_so_far, total_records_if_known)` so the UI can display
    /// a live percentage.
    pub async fn get_all_unlocked_achievements_with_progress<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
    ) -> Result<Vec<Achievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.get_all_unlocked_achievements_with_progress_resumable(
            xuid,
            unlocked_only,
            progress,
            None,
        )
        .await
    }

    /// BRUTAL SCAN FIX C1.2/1.3: resumable variant. When
    /// `scan_cache_path` is Some(path), the scan saves its state to
    /// `path` every 5 pages. On retry with the same path, already-
    /// fetched offsets are skipped — mega profiles (SiegfriedX 126k)
    /// converge reliably across 2-4 retry runs instead of starting
    /// from zero every time. Cache file is deleted automatically on
    /// successful Ok(_) return.
    pub async fn get_all_unlocked_achievements_with_progress_resumable<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
        scan_cache_path: Option<PathBuf>,
    ) -> Result<Vec<Achievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.get_all_unlocked_achievements_pooled(
            xuid,
            unlocked_only,
            progress,
            scan_cache_path,
            None,
        )
        .await
    }

    /// BRUTAL SCAN FIX C2.2: pool-aware variant. Each page request
    /// round-robins through `scan_pool`, so N validated xauth tokens
    /// = N independent rate-limit buckets = up to N× effective
    /// throughput. Combine with `scan_cache_path` for mega profiles
    /// that need both resume AND multi-token acceleration.
    pub async fn get_all_unlocked_achievements_pooled<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
        scan_cache_path: Option<PathBuf>,
        scan_pool: Option<Arc<Vec<XboxClient>>>,
    ) -> Result<Vec<Achievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.bulk_pages_inner::<AchievementsResponse, Achievement, F, (String, String)>(
            xuid,
            unlocked_only,
            progress,
            |r| r.achievements,
            |r| r.paging_info.as_ref().and_then(|p| p.total_records),
            |a| {
                (
                    a.title_associations
                        .first()
                        .and_then(|t| t.id.clone())
                        .unwrap_or_default(),
                    a.id.clone(),
                )
            },
            scan_cache_path,
            scan_pool,
        )
        .await
    }

    /// Slim variant of the bulk scan — returns only the fields
    /// `plan::builder::build_plan` actually reads. Dramatically faster
    /// for SiegfriedX-sized libraries: on a ~126k-row response the
    /// `serde_json` parse step drops from ~5s to ~700ms because the
    /// slim struct skips ~11 unused Optional fields.
    ///
    /// `'static` on `F` is required by the new v2 pipeline: the
    /// progress callback runs on its own tokio task that outlives this
    /// function's stack frame by a few microseconds (during the final
    /// flush), so it can't borrow from the caller.
    pub async fn get_all_unlocked_achievements_slim_with_progress<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
    ) -> Result<Vec<SlimAchievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.get_all_unlocked_achievements_slim_with_progress_resumable(
            xuid,
            unlocked_only,
            progress,
            None,
        )
        .await
    }

    /// BRUTAL SCAN FIX C1.2/1.3: resumable slim variant. See
    /// `get_all_unlocked_achievements_with_progress_resumable` for
    /// the full explanation. For mega profiles, the caller (`build_plan`)
    /// should pass `Some(derived_path)` so progress survives stalls.
    pub async fn get_all_unlocked_achievements_slim_with_progress_resumable<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
        scan_cache_path: Option<PathBuf>,
    ) -> Result<Vec<SlimAchievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.get_all_unlocked_achievements_slim_pooled(
            xuid,
            unlocked_only,
            progress,
            scan_cache_path,
            None,
        )
        .await
    }

    /// BRUTAL SCAN FIX C2.2: pool-aware slim variant. Mirrors
    /// `get_all_unlocked_achievements_pooled` but returns the slim
    /// type the plan builder actually needs.
    pub async fn get_all_unlocked_achievements_slim_pooled<F>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        progress: Option<F>,
        scan_cache_path: Option<PathBuf>,
        scan_pool: Option<Arc<Vec<XboxClient>>>,
    ) -> Result<Vec<SlimAchievement>>
    where
        F: FnMut(usize, Option<i64>) + Send + 'static,
    {
        self.bulk_pages_inner::<SlimAchievementsResponse, SlimAchievement, F, (String, String)>(
            xuid,
            unlocked_only,
            progress,
            |r| r.achievements,
            |r| r.paging_info.as_ref().and_then(|p| p.total_records),
            |a| {
                (
                    a.title_associations
                        .first()
                        .and_then(|t| t.id.clone())
                        .unwrap_or_default(),
                    a.id.clone(),
                )
            },
            scan_cache_path,
            scan_pool,
        )
        .await
    }

    /// Shared implementation of the parallel-paginated bulk fetch.
    ///
    /// Generic over the page response type so both the full
    /// `AchievementsResponse` and the slim `SlimAchievementsResponse`
    /// can share the tuning knobs, retry loop, and progress bookkeeping
    /// without duplication.
    ///
    /// # Architecture (v3 — probe-first)
    ///
    /// v2 (pipeline rewrite) fetched page 1 fully (up to 155s for 10k
    /// records), THEN dispatched the remaining parallel offsets. Total
    /// wall-clock = page1 + parallel_round ≈ 2× the parallel round.
    ///
    /// v3 uses a **PROBE** step: fire `maxItems=1` first (~1-5s) to
    /// read `totalRecords`, then fire ALL real pages including
    /// `skipItems=0` in parallel. Total wall-clock ≈ 1× parallel
    /// round — roughly **2× faster** on mega-profiles.
    ///
    /// Benefits beyond speed:
    /// - UI sees `totalRecords` almost immediately (probe is <5s),
    ///   so the progress bar transitions from "?" to "N/126219" fast.
    /// - Fewer code paths — no special-casing of "page 1".
    ///
    /// ```text
    ///   probe (~1-5s) ───► totalRecords
    ///                          │
    ///                          ▼
    ///   offsets[0..N] ──► fetch pool (12) ──► pages channel ──► drain
    ///                                              │
    ///                                              ▼
    ///                                       progress task ◄── throttle 100ms
    ///                                                           + 5s heartbeat
    /// ```
    ///
    /// # Tuning
    ///
    /// - `PAGE_SIZE = 10000`: Xbox's real server max is >100k; 10k balances
    ///   parse cost (simd-json is fast, but holding 10k items momentarily
    ///   still uses ~10MB) against round-trip amortisation.
    /// - `PARALLELISM = 12`: budget-safe. Xbox Live allows 150 req / 300s
    ///   per user on this endpoint.
    /// - `MAX_PAGE_RETRIES = 3`: cap so a persistent server-side
    ///   failure for one offset doesn't wedge the scan forever.
    /// - `SCAN_HARD_TIMEOUT = 600s`: overall ceiling. A scan that
    ///   legitimately takes longer than 10 min indicates server-side
    ///   trouble — better to fail fast and let the caller `--resume`
    ///   after a cooldown than freeze the UI for an hour.
    #[allow(clippy::too_many_lines)]
    async fn bulk_pages_inner<R, Item, F, K>(
        &self,
        xuid: &str,
        unlocked_only: bool,
        mut progress: Option<F>,
        into_items: fn(R) -> Vec<Item>,
        total_records_of: fn(&R) -> Option<i64>,
        // BUG HUNTER H5: identity extractor used for a final
        // whole-vec dedup pass before return.
        identity: fn(&Item) -> K,
        // BRUTAL SCAN FIX C1.2/1.3: optional persistent checkpoint
        // path. When Some, state is saved every N pages so stalled
        // scans can resume on retry instead of restarting from zero.
        // Cache deleted on successful return.
        scan_cache_path: Option<PathBuf>,
        // BRUTAL SCAN FIX C2.2: optional pool of XboxClients. When
        // Some, each page request round-robins through the pool
        // (distinct xauth tokens = independent per-xuid rate limit
        // buckets). N tokens ≈ N× effective throughput without
        // hitting Xbox's 150 req/5min per-user cap. When None, all
        // pages use `self` (backward-compat single-token scan).
        //
        // Pool semantics:
        // - Each offset picks pool[offset/PAGE_SIZE % N] for its
        //   scan lifetime (not per-request — simpler, respects
        //   HTTP keep-alive).
        // - Page 1 uses `self` (the caller's primary client).
        // - Phase 4 recovery cycles through the pool too.
        scan_pool: Option<Arc<Vec<XboxClient>>>,
    ) -> Result<Vec<Item>>
    where
        R: DeserializeOwned + Send + 'static,
        Item: Clone + Send + 'static + Serialize + DeserializeOwned,
        F: FnMut(usize, Option<i64>) + Send + 'static,
        K: Eq + std::hash::Hash,
    {
        const PAGE_SIZE: usize = c::BULK_SCAN_PAGE_SIZE;
        // Parallelism floor for small/medium profiles. Mega profiles
        // (>50k records) drop to `PARALLELISM_MEGA` because Xbox
        // silently stalls many-parallel requests against a single
        // user on large offsets — the index scan cost is O(offset)
        // per request, so piling 12 concurrent scans at offset 100k+
        // saturates the backend shard and streams start dropping
        // without even a 429.
        // BRUTAL SCAN FIX: parallelism halved from 8→4 / 4→2.
        // Prior: 8 workers × 10k-item bursts overran the 150 req/5min
        // Xbox cap AND tripped the per-IP Cloudflare WAF on mega
        // profiles (SiegfriedX 126k always stalled). New 4/2 profile:
        // - Normal: 4 concurrent pages (burst ~4 req/s sustainable)
        // - Mega: 2 concurrent (crawl but NEVER trigger WAF)
        // Wall-clock impact: ~2× longer per scan, but >10× more
        // reliable. Given mega stalls were taking infinite time with
        // 0% success, "2× slower with 100% success" is a net win.
        const PARALLELISM_NORMAL: usize = 4;
        const PARALLELISM_MEGA: usize = 2;
        const MEGA_THRESHOLD: usize = 50_000;
        // How many times each worker retries a single offset before
        // giving up and queueing it for phase 4 sequential recovery.
        // Loop uses `attempt + 1 < MAX_PAGE_RETRIES` as the retry
        // guard, with `attempt` starting at 0 and incrementing on
        // each failure. For MAX_PAGE_RETRIES=3 that yields the
        // sequence {attempt=0, 1, 2} — i.e. THREE total fetch
        // attempts per offset (one initial + two retries), matching
        // the constant name. `request_bytes` adds 4 internal retries
        // of its own and phase 4 gives one more shot.
        //
        // BRUTAL SCAN FIX: retries 3→5 — an intermittent 429 or 5xx
        // should never cost us a page, and phase 4 recovery is limited
        // so outer retry is the cheap safety net.
        const MAX_PAGE_RETRIES: usize = 5;
        const PROGRESS_THROTTLE: Duration = Duration::from_millis(100);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
        // BRUTAL SCAN FIX: per-page timeout 90→300s. Specific shards
        // (Angelweaver, GD BerserkerTTD, Long Integer) consistently
        // take 120-180s for page 1 — prior 90s budget killed them
        // every time. 300s = 5min tolerates 99% of slow shards while
        // still bailing on genuinely-hung requests.
        const PER_PAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
        // BRUTAL SCAN FIX: stall detection 75→180s. Mega profiles
        // naturally pause between page deliveries (Xbox backend
        // shuffling), 75s was too jumpy and triggered premature
        // degrade. 180s gives Xbox breathing room; degrade still
        // fires on genuine stalls but survives normal pauses.
        const STALL_DEGRADE_AFTER: Duration = Duration::from_secs(180);
        // BRUTAL SCAN FIX: total scan timeout 1200s→2400s. SiegfriedX
        // math: 126k ÷ 2 parallel ÷ 1k/page × ~90s worst = 5670s
        // worst case. 2400s (40 min) is a middle-ground ceiling for
        // mega-profile single-pass; repeated --resume calls cover
        // beyond that without this fn hanging forever.
        const SCAN_HARD_TIMEOUT: Duration = Duration::from_secs(2400);
        // BRUTAL SCAN FIX: phase 4 budget 90→300s. Recovery pages
        // are the ones Xbox already threw 429 at, so they need MORE
        // patience, not less. 300s lets each dead offset have its
        // full retry run.
        const PHASE4_BUDGET: Duration = Duration::from_secs(300);

        require_non_empty(xuid, "xuid")?;
        let scan_started = Instant::now();

        // BRUTAL SCAN FIX C1.2/1.3: try to resume from cached state.
        // If a previous scan of this xuid stalled / crashed and the
        // caller provided a cache path, load what we already have and
        // skip re-fetching those offsets. Cache is per-xuid keyed so
        // wrong-xuid files are rejected.
        let resume_state: Option<ScanCheckpoint<Item>> = scan_cache_path
            .as_deref()
            .and_then(|p| ScanCheckpoint::<Item>::load(p, xuid));
        let resume_items_count = resume_state.as_ref().map(|s| s.items.len()).unwrap_or(0);
        let resume_offsets_set: std::collections::HashSet<usize> = resume_state
            .as_ref()
            .map(|s| s.completed_offsets.iter().copied().collect())
            .unwrap_or_default();
        let _resume_total_records = resume_state.as_ref().and_then(|s| s.total_records);
        if let Some(state) = &resume_state {
            tracing::info!(
                completed_offsets = state.completed_offsets.len(),
                items = state.items.len(),
                total_records = ?state.total_records,
                "resuming scan from cached checkpoint"
            );
        }

        let build_headers = || {
            self.headers_with(&[
                (
                    c::header_names::CONTRACT_VERSION,
                    c::header_values::CONTRACT_VERSION_2,
                ),
                (c::header_names::HOST, c::hosts::ACHIEVEMENTS),
                (c::header_names::CONNECTION, c::header_values::KEEP_ALIVE),
            ])
        };

        // ============================================================
        //  PHASE 1 — PAGE 1 (dual-purpose: data + totalRecords)
        //
        //  Earlier versions did a separate `maxItems=1` "probe" to get
        //  `totalRecords` fast, then fired parallel pages. Live testing
        //  against siegfriedx (126k achievements) showed the probe was
        //  actually SLOWER + less reliable than fetching page 1 at the
        //  normal page size:
        //
        //  | approach                         | wall-clock  |
        //  |----------------------------------|-------------|
        //  | probe(maxItems=1) → all parallel | 30-120s+var |
        //  | page1(maxItems=5000) → parallel  |    40s stab |
        //
        //  Xbox's response time is dominated by the index scan
        //  (O(N) on profile size), not the body streaming. maxItems=5000
        //  takes the same time as maxItems=1. So we fetch page 1 with
        //  the real page size, extract `totalRecords` from its
        //  `pagingInfo`, and use page 1's items directly (no discard).
        //
        //  Timeout: 120s. Xbox's observed max for maxItems=5000 on
        //  mega-profiles is ~45s; 120s gives 2.5× safety margin for
        //  flaky connections without letting a truly hung request
        //  block the scan forever.
        // ============================================================
        let page1_started = Instant::now();
        tracing::info!("scan phase 1: fetching page 1 (totalRecords comes from pagingInfo)");
        let page1_url = rewrite_base_url(
            &c::query_all_achievements_url(xuid, unlocked_only),
            self.inner.achievements_base_override.as_deref(),
        );
        // BRUTAL SCAN FIX: page 1 timeout 120→300s. Same motivation as
        // PER_PAGE_REQUEST_TIMEOUT — slow-shard profiles (Angelweaver,
        // GD_BerserkerTTD, Long_Integer) consistently needed 120-180s
        // for page 1. 300s kills only genuinely-hung servers.
        let page1: R = tokio::time::timeout(
            Duration::from_secs(300),
            self.request_json_parallel(Method::GET, &page1_url, build_headers()?, None),
        )
        .await
        .map_err(|_| {
            Error::other("scan page 1 timed out after 300s — Xbox Live is slow or unreachable")
        })??;
        let total_hint = total_records_of(&page1);
        let mut page1_items = into_items(page1);
        // BUG HUNTER H4: when Xbox omits `pagingInfo.totalRecords`
        // on page 1 (seen in the wild — the endpoint occasionally
        // ships it only on later pages), the prior
        // `unwrap_or(page1_items.len())` fallback collapsed the
        // scan ceiling to page-1 length (≤10k). A SiegfriedX-sized
        // 126k profile with that quirk was silently truncated to
        // 10k with no warning. Fix: when total is unknown but
        // page 1 came back FULL (strong signal of "more pages
        // ahead"), escalate to a safe sentinel so parallel phase 2
        // keeps fetching until Xbox starts returning short/empty
        // pages. 1,000,000 is comfortably above the largest
        // historically-observed profile (~150k) and empty offsets
        // return cheaply, so the worst-case is a handful of
        // harmless no-op fetches instead of data loss.
        //
        // `total_hint_for_ui` is the number we forward to progress
        // callbacks. When we used the sentinel we DON'T want the UI
        // showing "15000 / 1,000,000 = 1.5%" forever, so pass None
        // in that case — the progress bar degrades to
        // indeterminate, which is honest.
        let (total_records, total_hint_for_ui) = match total_hint
            .and_then(|t| usize::try_from(t).ok())
        {
            Some(0) => {
                // BUG HUNTER M-scan-zero: Xbox returned
                // `totalRecords = 0` (empty profile or transient
                // backend hiccup). If a stale cache from a
                // previous successful scan exists, the resume
                // path would otherwise present those 126k cached
                // items as the "fresh" result of a 0-record
                // scan. Wipe the cache file from disk and
                // return an empty Vec — the user sees the
                // truth: empty profile.
                if let Some(p) = &scan_cache_path {
                    ScanCheckpoint::<Item>::delete(p);
                }
                tracing::warn!(
                        "Xbox returned totalRecords=0 — wiping any stale resume cache and reporting empty"
                    );
                return Ok(Vec::new());
            }
            Some(t) => {
                // Wave-8 audit resource-F3: clamp the server-supplied
                // `totalRecords` against a sanity ceiling. Without it,
                // a hostile or compromised network path (rogue Wi-Fi,
                // DNS hijack, malicious VPN, MITM proxy) could return
                // `pagingInfo.totalRecords: 1_000_000_000` on the first
                // page; the next line's `Vec::with_capacity(t)` would
                // try to reserve ~24 GB of address space on a 64-bit
                // `Item`-sized struct and OOM the process before any
                // further pagination ran. The 1_000_000 ceiling we
                // already applied for the sentinel arm at line 580
                // gives us a known-good upper bound.
                const SCAN_TOTAL_RECORDS_HARD_CAP: usize = 1_000_000;
                let clamped = t.min(SCAN_TOTAL_RECORDS_HARD_CAP);
                if clamped < t {
                    tracing::warn!(
                        reported = t,
                        clamped,
                        "Xbox-reported totalRecords exceeds sanity cap — \
                         clamping to avoid OOM via attacker-controlled response"
                    );
                }
                let t_i64 = i64::try_from(clamped).unwrap_or(i64::MAX);
                (clamped, Some(t_i64))
            }
            None if page1_items.len() >= PAGE_SIZE => {
                tracing::warn!(
                    page1_items = page1_items.len(),
                    "Xbox omitted pagingInfo.totalRecords on page 1 — using sentinel ceiling (H4)"
                );
                (1_000_000, None)
            }
            None => {
                // Page 1 was short AND no total hint — this IS the
                // whole profile. Trust page1.len() as the total.
                let t = page1_items.len();
                (t, Some(t as i64))
            }
        };
        // Retained previously as total_records_i64 for progress callbacks;
        // now superseded by `total_hint_for_ui` which carries None when
        // we fell back to the sentinel. Kept as a debug-log value only.
        let _total_records_dbg = i64::try_from(total_records).unwrap_or(i64::MAX);
        tracing::info!(
            total_records,
            page1_items = page1_items.len(),
            page1_elapsed_ms = page1_started.elapsed().as_millis() as u64,
            "scan phase 1 complete — page 1 landed, planning parallel remainder"
        );

        // Collect page 1 items immediately — no discard. If the whole
        // profile fit in one page (small user), we're already done.
        //
        // BRUTAL SCAN FIX C1.2/1.3: when resuming from a cached state,
        // start `out` with the previously-fetched items. If offset 0
        // was already in the completed set, those items are in the
        // cache — DON'T re-append page1_items. We still refetched
        // page 1 for totalRecords; the data is discarded below.
        // BUG HUNTER scan-sentinel-prealloc: under the H4 sentinel
        // ceiling `total_records` is a fake 1_000_000; reserving against
        // it pre-allocated ~150 MB (1M × size_of::<Item>) for a profile
        // that may hold only a few thousand rows. Only trust
        // `total_records` as a capacity hint when it's a REAL server total
        // (`total_hint_for_ui.is_some()`); under the sentinel fall back to
        // what we actually have so far (page 1 + any resumed items) and
        // let the Vec grow on demand.
        let cap_hint = if total_hint_for_ui.is_some() {
            total_records
        } else {
            page1_items.len().max(resume_items_count)
        };
        let mut out: Vec<Item> =
            Vec::with_capacity(cap_hint.max(page1_items.len()).max(resume_items_count));
        if let Some(state) = resume_state {
            out.extend(state.items);
            if !resume_offsets_set.contains(&0) {
                // Resume cache didn't include page 1 — append now.
                out.append(&mut page1_items);
            }
            // Else: page 1 data already in cache, page1_items discarded.
        } else {
            out.append(&mut page1_items);
        }

        if let Some(cb) = progress.as_mut() {
            cb(out.len(), total_hint_for_ui);
        }

        // Edge case: small profile fully covered by page 1 (or already
        // cached to completion).
        if out.len() >= total_records {
            // Cleanup cache file since scan is complete.
            if let Some(p) = &scan_cache_path {
                ScanCheckpoint::<Item>::delete(p);
            }
            return Ok(out);
        }

        // ============================================================
        //  PHASE 2 — PARALLEL REMAINDER
        //
        //  Offsets start at PAGE_SIZE (we already have [0, PAGE_SIZE)
        //  from page 1). Fire the rest in parallel, bounded by
        //  PARALLELISM.
        //
        //  BRUTAL SCAN FIX C1.3: filter out offsets already present in
        //  resume cache — they're already in `out` from the resume
        //  load above. Skipping them costs zero redundant HTTP.
        // ============================================================
        let offsets: Vec<usize> = (PAGE_SIZE..total_records)
            .step_by(PAGE_SIZE)
            .filter(|off| !resume_offsets_set.contains(off))
            .collect();
        if offsets.is_empty() && !resume_offsets_set.is_empty() {
            tracing::info!(
                cached = resume_items_count,
                "all offsets already in resume cache — scan done from cache"
            );
            if let Some(p) = &scan_cache_path {
                ScanCheckpoint::<Item>::delete(p);
            }
            return Ok(out);
        }
        let offsets_len = offsets.len();
        // Adaptive parallelism — large profiles are what empirically
        // break under heavy concurrent load (the siegfriedx-class
        // 126k scans that stall at 1% with 12 parallel).
        let parallelism = if total_records >= MEGA_THRESHOLD {
            PARALLELISM_MEGA
        } else {
            PARALLELISM_NORMAL
        };
        tracing::info!(
            total_records,
            pages = offsets_len,
            page_size = PAGE_SIZE,
            parallelism,
            mega_mode = total_records >= MEGA_THRESHOLD,
            "scan phase 2: dispatching parallel pages"
        );

        if offsets.is_empty() {
            return Ok(out);
        }

        // ==== PROGRESS TASK ====
        //
        // Runs on its own tokio task. Consumes a watch channel, fires
        // the callback at most every PROGRESS_THROTTLE. The last value
        // seen before the task exits is always flushed so the UI sees
        // the final 100%. Force-fires every HEARTBEAT_INTERVAL even
        // if no new items — see constant above.
        let (prog_tx, mut prog_rx) = tokio::sync::watch::channel::<usize>(out.len());
        let progress_handle = progress.take().map(|mut cb| {
            tokio::spawn(async move {
                let mut last_emitted = usize::MAX;
                let mut last_fire = Instant::now();
                let mut ticker = tokio::time::interval(PROGRESS_THROTTLE);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        changed = prog_rx.changed() => {
                            if changed.is_err() {
                                // Sender dropped — flush final value.
                                let final_v = *prog_rx.borrow();
                                if final_v != last_emitted {
                                    cb(final_v, total_hint_for_ui);
                                }
                                break;
                            }
                        }
                        _ = ticker.tick() => {
                            let v = *prog_rx.borrow();
                            // Fire if the value changed OR if 5s passed
                            // since the last fire (heartbeat).
                            let stale = last_fire.elapsed() >= HEARTBEAT_INTERVAL;
                            if v != last_emitted || stale {
                                cb(v, total_hint_for_ui);
                                last_emitted = v;
                                last_fire = Instant::now();
                            }
                        }
                    }
                }
            })
        });

        // ==== PAGES CHANNEL ====
        //
        // Pipeline: fetch tasks push `(offset, Option<Vec<Item>>)` here;
        // drain loop receives and appends. The drain task writes to
        // `out` without holding up any fetch worker. The `offset` is
        // carried through so the drain can record which offsets failed
        // and auto-resume on them sequentially at the end.
        let (pages_tx, mut pages_rx) =
            tokio::sync::mpsc::unbounded_channel::<(usize, Option<Vec<Item>>)>();

        // ==== FETCH POOL ====
        //
        // Async Semaphore bounds in-flight requests. Each offset task
        // acquires a permit, fetches, retries on failure, then drops
        // the permit — no join_next loop that could stall the pipeline.
        let sem = Arc::new(tokio::sync::Semaphore::new(parallelism));
        // Keep a local handle on the drain side so we can
        // `forget_permits` to degrade parallelism on stall without
        // having to reach into the moved coordinator closure.
        let sem_handle = sem.clone();
        // Shared abort flag — set by the drain loop on stall, polled
        // by workers before each retry attempt. Without this,
        // `fetch_coordinator.abort()` only cancels the coordinator
        // task itself; the per-offset worker `JoinHandle`s held by
        // its `handles` vec get dropped (which does NOT abort), so
        // in-flight Xbox HTTP keeps draining bandwidth and holding
        // semaphore permits long after the scan's user-visible
        // stall-abort. See audit bug #9.
        let abort_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let abort_flag_drain = abort_flag.clone();
        let xuid_owned = xuid.to_string();
        let headers = build_headers()?;
        let inner_http = self.inner.clone();
        let base_override_outer = self.inner.achievements_base_override.clone();
        // BRUTAL SCAN FIX C2.2: clone the pool Arc once — worker
        // tasks pick their client by offset index, so each offset
        // hits with a distinct xauth token (round-robin). Xbox's
        // rate limit is PER-SCANNER-XUID, so N tokens in the pool =
        // N independent rate buckets, up to N× effective throughput.
        let pool_ref = scan_pool.clone();

        // Spawn a coordinator task that fires all offset fetchers in
        // parallel, each gated by the semaphore. Coordinator finishes
        // when all fetches have completed and closes the pages channel.
        let fetch_coordinator = tokio::spawn({
            let pages_tx = pages_tx.clone();
            let abort_flag = abort_flag.clone();
            async move {
                let mut handles: Vec<(usize, tokio::task::JoinHandle<()>)> =
                    Vec::with_capacity(offsets.len());
                for offset in &offsets {
                    let offset = *offset;
                    let permit_sem = sem.clone();
                    let xuid = xuid_owned.clone();
                    let headers = headers.clone();
                    // BRUTAL SCAN FIX C2.2: pool-aware inner client.
                    // When a pool is provided, pick pool[offset/PAGE_SIZE
                    // % N] — each offset gets a deterministic but
                    // evenly-distributed token assignment.
                    let inner = if let Some(p) = &pool_ref {
                        if p.is_empty() {
                            inner_http.clone()
                        } else {
                            let idx = (offset / PAGE_SIZE) % p.len();
                            p[idx].inner.clone()
                        }
                    } else {
                        inner_http.clone()
                    };
                    let pages_tx = pages_tx.clone();
                    let base_override = base_override_outer.clone();
                    let abort_flag = abort_flag.clone();
                    let handle = tokio::spawn(async move {
                        // Quick abort check — if the drain loop
                        // already gave up on us, don't even acquire
                        // the permit, just return.
                        if abort_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        // Acquire permit — blocks until the pool has a
                        // free slot. Bounds in-flight to PARALLELISM.
                        let Ok(_permit) = permit_sem.acquire_owned().await else {
                            // Semaphore closed — coordinator aborted.
                            return;
                        };
                        let url = rewrite_base_url(
                            &c::query_all_achievements_url_offset(&xuid, unlocked_only, offset),
                            base_override.as_deref(),
                        );
                        let tmp_client = XboxClient { inner };
                        // Log dispatch so stderr shows when each
                        // offset BEGINS — gap between dispatch and
                        // land is where mega-profile stalls live.
                        tracing::info!(offset, "page dispatch");
                        // Retry loop — 3 attempts with growing backoff
                        // between them. The per-request retry in
                        // request_bytes handles transient network +
                        // 429 already; this outer retry catches HARD
                        // failures (parse error, non-retryable 4xx,
                        // timeout-after-all-retries) AND kills
                        // requests that exceed `PER_PAGE_REQUEST_TIMEOUT`
                        // cheaply — without this, a stuck stream
                        // would block the worker until reqwest's
                        // global 120s timeout, starving the
                        // semaphore for 3× longer than necessary.
                        let mut attempt = 0usize;
                        let result = loop {
                            // Check abort between retries — if the
                            // drain loop signalled stall, we stop
                            // retrying even if this offset hasn't
                            // exhausted MAX_PAGE_RETRIES. Prevents
                            // minute-long tails of HTTP draining
                            // after the user-visible "stall abort".
                            if abort_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                break None;
                            }
                            let fetch_start = Instant::now();
                            let fut = tmp_client.request_json_parallel::<R>(
                                Method::GET,
                                &url,
                                headers.clone(),
                                None,
                            );
                            let res: Result<R> =
                                match tokio::time::timeout(PER_PAGE_REQUEST_TIMEOUT, fut).await {
                                    Ok(r) => r,
                                    Err(_) => Err(Error::other(format!(
                                        "per-page request timed out after {}s (offset {})",
                                        PER_PAGE_REQUEST_TIMEOUT.as_secs(),
                                        offset
                                    ))),
                                };
                            match res {
                                Ok(r) => {
                                    tracing::info!(
                                        offset,
                                        attempt,
                                        elapsed_ms = fetch_start.elapsed().as_millis() as u64,
                                        "page landed"
                                    );
                                    break Some(into_items(r));
                                }
                                Err(e) if attempt + 1 < MAX_PAGE_RETRIES => {
                                    tracing::warn!(
                                        offset,
                                        attempt,
                                        ?e,
                                        "page fetch failed — retrying"
                                    );
                                    attempt += 1;
                                    // Outer backoff on top of
                                    // request_bytes's inner retry.
                                    let wait_ms = 500u64 * (1u64 << attempt);
                                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        offset,
                                        attempt,
                                        ?e,
                                        "page fetch failed after {} retries — queuing for recovery",
                                        MAX_PAGE_RETRIES
                                    );
                                    break None;
                                }
                            }
                        };
                        // Send result (or None for failure) to drain.
                        // Include the offset so the drain can queue
                        // failures for a final sequential recovery pass.
                        let _ = pages_tx.send((offset, result));
                    });
                    handles.push((offset, handle));
                }
                // Wait for every fetch task to complete before closing
                // the channel — otherwise the drain loop would see EOF
                // while pages are still in flight.
                for (offset, handle) in handles {
                    if let Err(e) = handle.await {
                        tracing::error!(
                            offset,
                            ?e,
                            "page fetch worker panicked — queuing for recovery"
                        );
                        let _ = pages_tx.send((offset, None));
                    }
                }
                drop(pages_tx);
            }
        });

        // ============================================================
        //  PHASE 3 — DRAIN
        //
        //  Receive pages as they land, append to `out`, update
        //  progress. The explicit drop of our copy of pages_tx above
        //  is critical — without it, pages_rx would never see EOF.
        // ============================================================
        tracing::info!("scan phase 3: draining parallel pages");
        drop(pages_tx);
        let drain_started = Instant::now();
        let mut completed_pages = 0usize;
        let mut failed_offsets: Vec<usize> = Vec::new();
        let mut last_progress_at = Instant::now();
        let mut current_parallelism = parallelism;
        let mut degrade_count = 0usize;
        let mut stall_aborted = false;
        // BRUTAL SCAN FIX C1.2: track completed offsets for persistent
        // checkpoint. Page 1 is offset 0 (always done by phase 1); we
        // pre-seed it here if present. Resume cache already includes
        // offsets from prior runs — merge so checkpoint survives
        // across retries.
        let mut completed_offsets_tracked: Vec<usize> =
            resume_offsets_set.iter().copied().collect();
        if !resume_offsets_set.contains(&0) {
            completed_offsets_tracked.push(0);
        }
        // Checkpoint write cadence. Every 5 pages = ~20s on mega
        // profiles → cheap per-write cost (JSON serialize for up to
        // ~2k entries at that granularity), and worst-case loss on
        // crash ≈ last 5 pages = <5% of a mega scan.
        const CHECKPOINT_EVERY_N_PAGES: usize = 5;
        let mut pages_since_checkpoint = 0usize;
        loop {
            // Hard timeout guard — 20 min is plenty for the common
            // mega-profile case; anything past this is likely
            // permanent backend trouble, user should --resume later.
            if scan_started.elapsed() >= SCAN_HARD_TIMEOUT {
                tracing::error!(
                    elapsed_secs = scan_started.elapsed().as_secs(),
                    completed_pages,
                    total_pages = offsets_len,
                    "scan hit hard timeout — returning partial"
                );
                stall_aborted = true;
                break;
            }
            // `recv` with a deadline so the drain loop can notice a
            // stall even when NO worker ever reports back (the
            // siegfriedx symptom: every stream wedged, no failures,
            // no successes, progress flat for minutes). `timeout`
            // returns Err on elapsed — we then check the stall
            // window and either break, degrade, or keep waiting.
            let recv_res = tokio::time::timeout(Duration::from_secs(5), pages_rx.recv()).await;
            match recv_res {
                Ok(Some((offset, page_opt))) => {
                    #[allow(clippy::single_match_else)]
                    match page_opt {
                        Some(mut page) => {
                            let items = page.len();
                            out.append(&mut page);
                            completed_pages += 1;
                            completed_offsets_tracked.push(offset);
                            pages_since_checkpoint += 1;
                            last_progress_at = Instant::now();
                            tracing::info!(
                                offset,
                                items,
                                elapsed_ms = drain_started.elapsed().as_millis() as u64,
                                collected = out.len(),
                                completed_pages,
                                total_pages = offsets_len,
                                parallelism = current_parallelism,
                                "scan page landed"
                            );
                            let _ = prog_tx.send(out.len());
                            // BRUTAL SCAN FIX C1.2: persist checkpoint.
                            // Every N landed pages, flush state so a
                            // subsequent stall / crash / timeout has a
                            // restart point. Failure to write is
                            // non-fatal — we just log and keep scanning.
                            if pages_since_checkpoint >= CHECKPOINT_EVERY_N_PAGES {
                                if let Some(p) = &scan_cache_path {
                                    if let Err(e) = ScanCheckpoint::<Item>::save(
                                        p,
                                        xuid,
                                        Some(total_records),
                                        &completed_offsets_tracked,
                                        &out,
                                    ) {
                                        tracing::warn!(?e, "checkpoint write failed (non-fatal)");
                                    } else {
                                        tracing::info!(
                                            collected = out.len(),
                                            offsets_saved = completed_offsets_tracked.len(),
                                            "scan checkpoint saved"
                                        );
                                    }
                                }
                                pages_since_checkpoint = 0;
                            }
                        }
                        None => {
                            failed_offsets.push(offset);
                            last_progress_at = Instant::now();
                            tracing::warn!(
                                offset,
                                elapsed_ms = drain_started.elapsed().as_millis() as u64,
                                "scan page FAILED after retries — queued for recovery"
                            );
                        }
                    }
                }
                Ok(None) => {
                    // Channel closed — all workers done.
                    break;
                }
                Err(_) => {
                    // 5s tick with no page. Check stall window.
                    let stall = last_progress_at.elapsed();
                    if stall >= STALL_DEGRADE_AFTER {
                        // Graceful degradation: cut current
                        // parallelism in half. This releases the
                        // semaphore pressure on Xbox's shard for
                        // this user and often unsticks the stalled
                        // streams (empirically Xbox resumes serving
                        // the same user at lower concurrency). Only
                        // if we've degraded all the way to 1 and
                        // STILL stalled do we give up.
                        if current_parallelism > 1 {
                            let new_parallelism = (current_parallelism / 2).max(1);
                            degrade_count += 1;
                            tracing::warn!(
                                stall_secs = stall.as_secs(),
                                from = current_parallelism,
                                to = new_parallelism,
                                degrade_count,
                                completed_pages,
                                total_pages = offsets_len,
                                "scan STALLED — degrading parallelism, keeping going"
                            );
                            // Shrink the semaphore by *forgetting*
                            // permits. `acquire_owned()` callers
                            // that were holding permits don't lose
                            // them; but future acquires see fewer
                            // slots. Downside: the in-flight set
                            // stays at `current_parallelism` for
                            // the next round before the new cap
                            // takes effect — acceptable; the
                            // already-in-flight are stuck anyway.
                            let to_forget = current_parallelism - new_parallelism;
                            sem_handle.forget_permits(to_forget);
                            current_parallelism = new_parallelism;
                            last_progress_at = Instant::now();
                        } else {
                            // parallelism=1 and still stalled for
                            // STALL_DEGRADE_AFTER seconds. Xbox is
                            // definitively not serving this user.
                            // Surrender with what we have.
                            tracing::error!(
                                stall_secs = stall.as_secs(),
                                completed_pages,
                                total_pages = offsets_len,
                                collected = out.len(),
                                total_records,
                                degrade_count,
                                "scan STALLED at parallelism=1 — giving up, returning partial"
                            );
                            stall_aborted = true;
                            break;
                        }
                    } else if stall.as_secs() % 30 == 0 && stall.as_secs() > 0 {
                        tracing::warn!(
                            stall_secs = stall.as_secs(),
                            completed_pages,
                            total_pages = offsets_len,
                            parallelism = current_parallelism,
                            "scan idle — no pages landing"
                        );
                    }
                }
            }
        }
        if stall_aborted {
            // BRUTAL SCAN FIX C1.2: FLUSH CHECKPOINT BEFORE ERROR.
            // Critical — without this, the partial state evaporates
            // and `--resume` on the CLI has nothing to pick up. Save
            // what we have so the retry converges instead of
            // starting from zero. Non-fatal if save fails (error is
            // only logged); the outer Err below is the authoritative
            // signal to the caller.
            if let Some(p) = &scan_cache_path {
                if let Err(e) = ScanCheckpoint::<Item>::save(
                    p,
                    xuid,
                    Some(total_records),
                    &completed_offsets_tracked,
                    &out,
                ) {
                    tracing::warn!(?e, "final checkpoint on stall failed");
                } else {
                    tracing::info!(
                        collected = out.len(),
                        offsets_saved = completed_offsets_tracked.len(),
                        "stall-abort checkpoint saved — use --resume to continue"
                    );
                }
            }
            // Surface as an Err so the caller's `--resume` can pick
            // up from where we left off.
            abort_flag_drain.store(true, std::sync::atomic::Ordering::Relaxed);
            sem_handle.close();
            let _ = prog_tx.send(out.len());
            drop(prog_tx);
            if let Some(h) = progress_handle {
                let _ = h.await;
            }
            fetch_coordinator.abort();
            return Err(Error::other(format!(
                "scan stalled: {completed_pages}/{offsets_len} pages in, \
                 collected {}/{total_records} achievements before stall abort \
                 (degraded {degrade_count}× before giving up). \
                 Use `--resume` to pick up.",
                out.len()
            )));
        }
        // Coordinator should already be done at this point, but await
        // its handle so we surface panics via tracing instead of silently
        // losing them.
        let mut coordinator_panicked = false;
        if let Err(e) = fetch_coordinator.await {
            tracing::error!(?e, "bulk fetch coordinator panicked");
            coordinator_panicked = true;
        }

        // ============================================================
        //  PHASE 4 — AUTO-RESUME (only if any parallel pages failed)
        //
        //  Any offsets that exhausted their parallel retries are tried
        //  ONE MORE TIME sequentially here, with a longer backoff.
        //  Catches transient server burps that happened to hit all 3
        //  of the parallel retries in a narrow window — often after a
        //  cool-down the request succeeds on the first try.
        //
        //  Sequential on purpose: we already failed under parallel
        //  load, so piling more concurrent requests on a flaky server
        //  is the opposite of what we want. One request every 2s gives
        //  the backend breathing room.
        // ============================================================
        let mut still_failed: Vec<usize> = Vec::new();
        if !failed_offsets.is_empty() {
            let recovery_count = failed_offsets.len();
            tracing::warn!(
                failed = recovery_count,
                "scan phase 4: sequential recovery pass for failed offsets"
            );
            let headers_recovery = build_headers()?;
            let mut recovered = 0usize;
            let phase4_started = Instant::now();
            for offset in failed_offsets {
                // Bail out of recovery if the scan has blown past its
                // hard timeout. Caller can --resume later. Shipping
                // what we have beats freezing the UI for another 20 min.
                if scan_started.elapsed() > SCAN_HARD_TIMEOUT {
                    tracing::warn!("scan phase 4: hit hard timeout during recovery, bailing");
                    still_failed.push(offset);
                    continue;
                }
                // Also bail if phase 4 itself is taking too long.
                // A pile of truly-dead offsets (siegfriedx after
                // bolt-tightening rate-limit penalty) could otherwise
                // keep the user waiting for minutes per offset. 90s
                // total is plenty to retry a few offsets that just
                // needed cool-down; past it, ship the partial and
                // let the caller `--resume`.
                if phase4_started.elapsed() > PHASE4_BUDGET {
                    tracing::warn!(
                        budget_secs = PHASE4_BUDGET.as_secs(),
                        recovered,
                        "scan phase 4: budget exhausted, marking rest as still-failed"
                    );
                    still_failed.push(offset);
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(2000)).await;
                let url = rewrite_base_url(
                    &c::query_all_achievements_url_offset(xuid, unlocked_only, offset),
                    self.inner.achievements_base_override.as_deref(),
                );
                let res: Result<R> = self
                    .request_json_parallel(Method::GET, &url, headers_recovery.clone(), None)
                    .await;
                match res {
                    Ok(r) => {
                        let mut page = into_items(r);
                        let page_items = page.len();
                        tracing::info!(offset, items = page_items, "recovery offset fetched");
                        out.append(&mut page);
                        recovered += 1;
                        completed_pages += 1;
                        // BUG HUNTER scan-phase4-tracking (#6): previously fase 4
                        // appended the recovered items but never marked
                        // `completed_offsets_tracked.push(offset)`. A
                        // subsequent `--resume` would then re-fetch every
                        // offset that fase 4 just successfully recovered,
                        // throwing the recovery effort away. Track the
                        // offset here so the checkpoint reflects reality.
                        completed_offsets_tracked.push(offset);
                        // BUG HUNTER scan-sentinel-cleanup (#9): when the
                        // probe omitted `totalRecords` and we adopted a
                        // 1_000_000 sentinel, the parallel fanout queued
                        // ~99 garbage offsets past the real end. As soon
                        // as fase 4 confirms an offset returned 0 items
                        // we can be sure the profile is exhausted — stop
                        // the recovery loop and don't pollute
                        // `still_failed` with the remaining queued
                        // garbage. `total_hint_for_ui.is_none()` is the
                        // exact signal that we adopted the sentinel.
                        if page_items == 0 && total_hint_for_ui.is_none() {
                            tracing::info!(
                                offset,
                                collected = out.len(),
                                "recovery hit empty page under sentinel ceiling — assuming profile exhausted",
                            );
                            break;
                        }
                        let _ = prog_tx.send(out.len());
                    }
                    Err(e) => {
                        tracing::error!(
                            offset,
                            ?e,
                            "recovery offset still failed — giving up on this page",
                        );
                        still_failed.push(offset);
                    }
                }
            }
            tracing::info!(
                recovered,
                still_failed = still_failed.len(),
                "recovery pass complete"
            );
            if !still_failed.is_empty() {
                tracing::warn!(
                    offsets = ?still_failed,
                    "scan completed with missing pages — caller may want to --resume",
                );
            }
        }

        // Close the progress channel so the progress task's `changed()`
        // future resolves Err, flushing the final value and exiting.
        drop(prog_tx);
        if let Some(handle) = progress_handle {
            let _ = handle.await;
        }

        // Hard-timeout enforcement. If the scan took longer than the
        // ceiling AND still came back incomplete, return an explicit
        // error so callers don't mistake partial data for a full scan.
        let scan_elapsed = scan_started.elapsed();
        let expected_items = total_records;
        let collected_items = out.len();
        // BUG HUNTER scan-sentinel-falsefail: under the H4 sentinel
        // ceiling (`totalRecords` omitted on a FULL page 1) `total_records`
        // is a fake 1_000_000, so `collected_items` is ALWAYS far below it
        // even on a fully-successful scan. The old unconditional
        // `collected_items < expected_items` then returned a spurious
        // "bulk scan incomplete … rerun with --resume" error, and the
        // re-probe on resume re-adopts the same sentinel — so it NEVER
        // converged. The item-count shortfall is only meaningful when we
        // have a REAL total (`total_hint_for_ui.is_some()`); in the
        // sentinel world completeness is defined purely by "no failed
        // offsets and the coordinator didn't panic" (offsets past the real
        // end return empty pages that already count as completed). This
        // mirrors the identical `total_hint_for_ui.is_some()` guard already
        // used for the TOCTOU surplus warning further down.
        let item_shortfall = total_hint_for_ui.is_some() && collected_items < expected_items;
        let scan_incomplete = coordinator_panicked || !still_failed.is_empty() || item_shortfall;
        if scan_elapsed > SCAN_HARD_TIMEOUT && scan_incomplete {
            tracing::warn!(
                elapsed_secs = scan_elapsed.as_secs(),
                collected = collected_items,
                expected = total_records,
                "scan exceeded hard timeout and remained incomplete"
            );
        }

        tracing::info!(
            completed_pages,
            collected = collected_items,
            expected = total_records,
            elapsed_secs = scan_elapsed.as_secs(),
            "bulk scan done (phase 3+4)"
        );

        if scan_incomplete {
            // BUG HUNTER scan-resume-flush (#6): previously the
            // `scan_incomplete` Err path returned without writing a
            // final checkpoint. Up to CHECKPOINT_EVERY_N_PAGES of fase
            // 3 progress and ALL fase 4 recoveries were lost — the
            // user's `--resume` would re-fetch them. Flush state now
            // so the retry converges. Non-fatal on save error: the
            // outer Err is the authoritative signal.
            if let Some(p) = &scan_cache_path {
                if let Err(e) = ScanCheckpoint::<Item>::save(
                    p,
                    xuid,
                    Some(total_records),
                    &completed_offsets_tracked,
                    &out,
                ) {
                    tracing::warn!(?e, "final checkpoint on incomplete scan failed");
                } else {
                    tracing::info!(
                        collected = out.len(),
                        offsets_saved = completed_offsets_tracked.len(),
                        "incomplete scan flushed to checkpoint — `--resume` will pick up here",
                    );
                }
            }
            let missing_items = expected_items.saturating_sub(collected_items);
            let mut detail = Vec::new();
            if coordinator_panicked {
                detail.push("fetch coordinator panicked".to_string());
            }
            if !still_failed.is_empty() {
                detail.push(format!("missing offsets: {still_failed:?}"));
            }
            if missing_items > 0 {
                detail.push(format!("missing {missing_items} item(s)"));
            }
            return Err(Error::other(format!(
                "bulk scan incomplete: collected {collected_items}/{expected_items} achievements{}; rerun with --resume",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", detail.join("; "))
                }
            )));
        }

        // BUG HUNTER scan-toctou-warn (#10): if the user's profile
        // grew DURING the scan (concurrent unlocks on Xbox's side, or
        // a fresh achievement landing mid-fan-out), `out.len()` can
        // legitimately exceed `total_records` — we'll still have all
        // the real records (post-dedup), but the discrepancy is worth
        // a heads-up. The sentinel-1M case (`total_hint_for_ui.is_none()`)
        // is its own world: there `total_records` is fake, so suppress
        // the warning then.
        if collected_items > expected_items && total_hint_for_ui.is_some() {
            tracing::warn!(
                collected = collected_items,
                expected = expected_items,
                surplus = collected_items - expected_items,
                "profile grew during scan (TOCTOU) — collected more rows than the probe predicted; trusting collected count",
            );
        }

        // BUG HUNTER H5: final dedup pass. Parallel pagination via
        // `skipItems=N` offsets CAN return the same achievement at
        // two offsets when Xbox renumbers mid-scan (target unlocks
        // something, index shifts). `out.append(&mut page)` at
        // phase-2 and phase-4 recovery has no dedup guard — the
        // duplicates propagate to `compute_profile_deep_totals`
        // (builder.rs:540-551) and `enumerate_titles_from_bulk_scan`
        // (builder.rs:430-460), both of which sum over the raw vec
        // and would double-count gamerscore. plan.rs checkpoints
        // dedup separately before plan_cache hits, but raw-scan
        // consumers need protection too.
        //
        // Single pass, O(n) time + O(n) hash memory. On a 126k-row
        // scan with zero dupes this is ~2ms of overhead — cheap
        // vs the 4-10s scan itself.
        {
            let before = out.len();
            let mut seen: std::collections::HashSet<K> =
                std::collections::HashSet::with_capacity(before);
            out.retain(|item| seen.insert(identity(item)));
            let after = out.len();
            if before != after {
                tracing::warn!(
                    before,
                    after,
                    duplicates = before - after,
                    "bulk scan deduped cross-offset duplicates (H5)"
                );
            }
        }

        // BRUTAL SCAN FIX C1.2: scan complete successfully — delete
        // the cache file. Leaving it behind would confuse the next
        // scan of the same xuid (wrong total_records, missing new
        // unlocks). Caller doesn't need to think about cleanup.
        if let Some(p) = &scan_cache_path {
            ScanCheckpoint::<Item>::delete(p);
            tracing::info!(path = %p.display(), "scan cache cleaned up after success");
        }

        Ok(out)
    }
}
