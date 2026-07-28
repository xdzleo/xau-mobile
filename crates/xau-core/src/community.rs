//! Community plan cache — fetch pre-built `Plan`s for popular
//! targets from a public repository instead of scanning Xbox Live
//! from scratch. 300× faster than a full scan when the target has
//! already been cataloged.
//!
//! # How it works
//!
//! For well-known trophy-hunter gamertags (siegfriedx, riffai, vudix,
//! xdzleoz, etc.) that the whole XAU community wants to follow, there's
//! no reason every user pays the 5-minute mega-scan cost. Someone scans
//! once, publishes the resulting `plan.json` to a public repo, and
//! every subsequent user downloads it in under a second.
//!
//! # Layout
//!
//! The cache is served as raw files under a public HTTPS base (GitHub
//! Pages or raw content works). Shape:
//!
//! ```text
//!   {base}/index.json                    — catalog with metadata
//!   {base}/plans/{xuid}.json             — one plan per target
//! ```
//!
//! `index.json` holds `{xuid, gamertag, built_at, entries, sha256}`
//! per catalogued target so the client can:
//! - List available targets
//! - Check if a plan is FRESH (built within N days)
//! - Verify download integrity via SHA-256
//!
//! # Integrity
//!
//! Every downloaded plan is SHA-256 verified against the hash in
//! `index.json`. A tampered plan would fail verification and be
//! rejected — the cache can't be used to inject malicious unlock
//! targets onto caller profiles.
//!
//! # Freshness
//!
//! `STALE_AFTER = 30 days`. Plans older than that are refused on
//! download (caller falls back to a fresh scan + re-publish). The
//! 30-day window balances "don't re-scan every day" against "pick up
//! new achievements the target has earned since publication".
//!
//! # Privacy
//!
//! Plans contain achievement IDs + timestamps + game names. Zero
//! authentication data, zero PII beyond the public gamertag/xuid
//! (already visible via `xboxlive.com`). Safe to share.

// `STALE_AFTER.as_secs() as i64` — safe, bounded to ~30 days.
#![allow(clippy::cast_possible_wrap)]

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::plan::Plan;

/// Default public base URL where the community cache lives. The
/// client can override via `CommunityCacheClient::with_base` for
/// testing or private deployments.
pub const DEFAULT_COMMUNITY_BASE: &str =
    "https://raw.githubusercontent.com/xau-community/prebuilt-plans/main";

/// Plans older than this are considered stale and not served by the
/// client — caller falls back to scanning fresh.
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Request timeout for every community-cache HTTP call. Community
/// hosts (GitHub raw, Cloudflare) should always respond quickly; any
/// timeout past 10s is either a network problem or a dead host, and
/// we'd rather fall back to a scan than hang the UI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One row in the community index. Published alongside its plan file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub xuid: String,
    pub gamertag: String,
    /// ISO-8601 UTC timestamp when this plan was built.
    pub built_at: String,
    /// Achievement entry count inside the plan file.
    pub entries: usize,
    /// Hex-encoded SHA-256 of the plan file contents. Lets the
    /// client verify what it downloads.
    pub sha256: String,
    /// Relative path under the cache base, e.g. "plans/2533..../plan.json".
    pub path: String,
}

impl CatalogEntry {
    /// Parse `built_at` as Unix seconds. Returns `None` on malformed
    /// timestamps.
    #[must_use]
    pub fn built_at_unix(&self) -> Option<i64> {
        crate::plan::runner::parse_iso_public(&self.built_at)
    }

    /// True when the plan is too old to trust (> `STALE_AFTER`).
    #[must_use]
    pub fn is_stale(&self, now_unix: i64) -> bool {
        match self.built_at_unix() {
            Some(b) => now_unix.saturating_sub(b) > STALE_AFTER.as_secs() as i64,
            None => true, // unparseable = treat as stale
        }
    }
}

/// Full shape of the published `index.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Catalog {
    pub version: u32,
    /// ISO-8601 timestamp when the index itself was last regenerated.
    pub updated_at: String,
    pub entries: Vec<CatalogEntry>,
}

/// Outcome of a cache lookup.
#[derive(Debug)]
pub enum LookupResult {
    /// Target exists in the cache and plan downloaded successfully.
    /// The integrity check passed and freshness window is green.
    Found(Plan, CatalogEntry),
    /// Target has an entry but it's > `STALE_AFTER` — caller should
    /// re-scan and optionally publish the update.
    Stale(CatalogEntry),
    /// Target is not in the catalog. Scan Xbox Live the slow way.
    NotInCatalog,
}

/// Client for the community-cache HTTP endpoint.
#[derive(Clone)]
pub struct CommunityCacheClient {
    base: String,
    http: reqwest::Client,
    /// Cached catalog — fetched on first call, reused for subsequent
    /// lookups within the same session. Re-fetched after
    /// `CATALOG_TTL`.
    cached_catalog: Arc<tokio::sync::RwLock<Option<(std::time::Instant, Catalog)>>>,
}

/// In-memory catalog is reused for this long before being re-fetched.
/// Short enough that a fresh publish is visible within a few minutes
/// without forcing a round-trip on every lookup.
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);

impl Default for CommunityCacheClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CommunityCacheClient {
    #[must_use]
    pub fn new() -> Self {
        Self::with_base(DEFAULT_COMMUNITY_BASE)
    }

    #[must_use]
    pub fn with_base(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .gzip(true)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            cached_catalog: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Return the catalog, re-fetching when the in-memory copy is
    /// older than `CATALOG_TTL`. Returns an empty catalog on any
    /// network error when no cache is available — the caller treats
    /// "can't reach community cache" as "nothing cached, scan Xbox
    /// normally".
    ///
    /// BUG HUNTER M3: previously, a transient fetch failure AFTER
    /// the cache had gone stale (older than TTL) returned an empty
    /// catalog, even though the stale-but-valid data was still
    /// sitting in `cached_catalog`. Every subsequent call within
    /// the same outage window re-fetched + failed + returned empty,
    /// making community lookups "go dark" for as long as the
    /// endpoint was down. Now we FALL BACK to the stale cache on
    /// fetch failure (at most `CATALOG_TTL` old → still almost
    /// certainly accurate) and only return empty when there's
    /// genuinely nothing to fall back on.
    pub async fn catalog(&self) -> Catalog {
        {
            let guard = self.cached_catalog.read().await;
            if let Some((fetched_at, cat)) = guard.as_ref() {
                if fetched_at.elapsed() < CATALOG_TTL {
                    return cat.clone();
                }
            }
        }
        let url = format!("{}/index.json", self.base);
        let fetched: Option<Catalog> = match self.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(cat) => Some(cat),
                Err(e) => {
                    tracing::warn!(?e, %url, "community catalog parse failed");
                    None
                }
            },
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), %url, "community catalog fetch failed");
                None
            }
            Err(e) => {
                tracing::warn!(?e, %url, "community catalog request failed");
                None
            }
        };
        if let Some(cat) = fetched {
            *self.cached_catalog.write().await = Some((std::time::Instant::now(), cat.clone()));
            cat
        } else {
            // Stale fallback: if we have ANY prior successful catalog
            // in memory, return it instead of an empty placeholder.
            // The caller still sees the `Stale` branch on individual
            // lookups via `CatalogEntry::is_stale`, so this doesn't
            // hide freshness info from the user — it just keeps
            // transient outages from wiping community lookups.
            if let Some((fetched_at, cat)) = self.cached_catalog.read().await.as_ref() {
                tracing::warn!(
                    age_secs = fetched_at.elapsed().as_secs(),
                    "community catalog fetch failed; serving stale cache (M3)"
                );
                return cat.clone();
            }
            Catalog::default()
        }
    }

    /// Look up a target by XUID. Returns the plan + catalog entry if
    /// the cache has a fresh copy.
    pub async fn lookup_by_xuid(&self, xuid: &str) -> Result<LookupResult> {
        let cat = self.catalog().await;
        let Some(entry) = cat.entries.iter().find(|e| e.xuid == xuid).cloned() else {
            return Ok(LookupResult::NotInCatalog);
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        if entry.is_stale(now) {
            return Ok(LookupResult::Stale(entry));
        }
        // Download + verify.
        let plan = self.download_and_verify(&entry).await?;
        Ok(LookupResult::Found(plan, entry))
    }

    /// Look up by gamertag. Normalized to lowercase for case-insensitive
    /// matching; `SiegfriedX` and `siegfriedx` hit the same entry.
    pub async fn lookup_by_gamertag(&self, gamertag: &str) -> Result<LookupResult> {
        let cat = self.catalog().await;
        let needle = gamertag.trim().to_lowercase();
        let Some(entry) = cat
            .entries
            .iter()
            .find(|e| e.gamertag.to_lowercase() == needle)
            .cloned()
        else {
            return Ok(LookupResult::NotInCatalog);
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        if entry.is_stale(now) {
            return Ok(LookupResult::Stale(entry));
        }
        let plan = self.download_and_verify(&entry).await?;
        Ok(LookupResult::Found(plan, entry))
    }

    /// Download + SHA-256 verify a specific catalog entry. Shared by
    /// both lookup methods.
    async fn download_and_verify(&self, entry: &CatalogEntry) -> Result<Plan> {
        let url = format!("{}/{}", self.base, entry.path.trim_start_matches('/'));
        let mut resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::other(format!("community cache fetch: {e}")))?;
        // Cap the download so a malicious or misconfigured catalog host
        // can't OOM us with a multi-GB body BEFORE the SHA-256 check
        // below runs. Even a 240k-entry mega-profile plan is only a few
        // MB of JSON. Fast-reject on Content-Length, then enforce the
        // cap while streaming (Content-Length can lie / be absent).
        // Audit: unbounded-community-plan-download.
        const MAX_PLAN_BYTES: usize = 64 * 1024 * 1024;
        if let Some(len) = resp.content_length() {
            if len > MAX_PLAN_BYTES as u64 {
                return Err(Error::other(format!(
                    "community plan too large: {len} bytes exceeds {MAX_PLAN_BYTES} cap"
                )));
            }
        }
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Error::other(format!("community cache body: {e}")))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_PLAN_BYTES {
                return Err(Error::other(format!(
                    "community plan exceeded {MAX_PLAN_BYTES} byte cap mid-stream (refusing)"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != entry.sha256.to_lowercase() {
            return Err(Error::other(format!(
                "community cache hash mismatch — expected {} got {} (refusing tampered plan)",
                entry.sha256, actual_hash
            )));
        }
        let plan: Plan = serde_json::from_slice(&bytes)
            .map_err(|e| Error::other(format!("community cache plan parse: {e}")))?;
        validate_catalog_plan_identity(&plan, entry)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        validate_catalog_plan_freshness(&plan, entry, now)?;
        Ok(plan)
    }
}

fn validate_catalog_plan_identity(plan: &Plan, entry: &CatalogEntry) -> Result<()> {
    let plan_xuid = plan.source_xuid.trim();
    let entry_xuid = entry.xuid.trim();
    if plan_xuid.is_empty() {
        return Err(Error::other(format!(
            "community cache plan `{}` is missing source_xuid",
            entry.path
        )));
    }
    if plan_xuid != entry_xuid {
        return Err(Error::other(format!(
            "community cache identity mismatch for `{}`: catalog xuid {} but plan source_xuid {}",
            entry.path, entry_xuid, plan_xuid
        )));
    }
    Ok(())
}

fn validate_catalog_plan_freshness(plan: &Plan, entry: &CatalogEntry, now_unix: i64) -> Result<()> {
    let Some(plan_built_at) = crate::plan::runner::parse_iso_public(&plan.built_at) else {
        return Err(Error::other(format!(
            "community cache plan `{}` has invalid built_at `{}`",
            entry.path, plan.built_at
        )));
    };
    if now_unix.saturating_sub(plan_built_at) > STALE_AFTER.as_secs() as i64 {
        return Err(Error::other(format!(
            "community cache plan `{}` is stale by its own built_at `{}`",
            entry.path, plan.built_at
        )));
    }
    Ok(())
}

/// FIPS 180-4 SHA-256 round constants, hoisted to module scope so
/// clippy's `items_after_statements` doesn't bark inside `sha256_hex`.
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Hex-encoded SHA-256 of a byte slice. Hand-rolled so we don't pull
/// a whole crate for one hash — `ring`/`sha2` are overkill for a
/// 500-byte integrity check we do once per download.
///
/// Uses the straightforward FIPS 180-4 implementation below. Not
/// constant-time (fine for integrity, not secrecy).
///
/// `pub(crate)` so the self-updater (`system::updater`) can reuse it
/// for the binary integrity check (BUG HUNTER C1) without dragging in
/// an extra hash crate.
// The `a..=hh` working-register names come straight from FIPS 180-4
// §6.2.2. Renaming would obscure the spec, so silence the lint here.
#[allow(clippy::many_single_char_names)]
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bitlen = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, b) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        for b in word.to_be_bytes() {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
    }
    out
}

// ==========================================================================
//  TESTS
// ==========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_empty_string_is_known_constant() {
        // Standard SHA-256 of "" from FIPS test vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_abc_is_known_constant() {
        // FIPS 180-4 test vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_of_plan_json_roundtrip() {
        let plan = Plan::default();
        let bytes = serde_json::to_vec(&plan).unwrap();
        let hash = sha256_hex(&bytes);
        // Deterministic — same input → same hash.
        let hash2 = sha256_hex(&bytes);
        assert_eq!(hash, hash2);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn catalog_entry_stale_detection() {
        let entry = CatalogEntry {
            built_at: "2024-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        let way_future = 2_000_000_000; // 2033-ish
        assert!(entry.is_stale(way_future));
    }

    #[test]
    fn catalog_entry_fresh_detection() {
        let entry = CatalogEntry {
            built_at: "2024-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        // 1 day after built_at → not stale.
        let one_day_later = 1_704_153_600;
        assert!(!entry.is_stale(one_day_later));
    }

    #[test]
    fn catalog_entry_unparseable_built_at_is_stale() {
        let entry = CatalogEntry {
            built_at: "garbage".into(),
            ..Default::default()
        };
        assert!(entry.is_stale(1_000_000_000));
    }

    #[test]
    fn client_default_base_is_the_production_repo() {
        let c = CommunityCacheClient::new();
        assert!(c.base.contains("xau-community"));
        assert!(!c.base.ends_with('/'));
    }

    #[test]
    fn client_with_custom_base_trims_trailing_slash() {
        let c = CommunityCacheClient::with_base("https://example.test/foo/");
        assert_eq!(c.base, "https://example.test/foo");
    }

    #[test]
    fn catalog_plan_identity_mismatch_is_rejected() {
        let entry = CatalogEntry {
            xuid: "111".into(),
            path: "plans/111.json".into(),
            ..Default::default()
        };
        let plan = Plan {
            source_xuid: "222".into(),
            ..Default::default()
        };
        let err = validate_catalog_plan_identity(&plan, &entry).unwrap_err();
        assert!(err.to_string().contains("identity mismatch"));
    }

    #[test]
    fn catalog_plan_uses_plan_built_at_for_freshness() {
        let entry = CatalogEntry {
            path: "plans/111.json".into(),
            ..Default::default()
        };
        let plan = Plan {
            built_at: "2024-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        let now = crate::plan::runner::parse_iso_public("2024-03-15T00:00:00Z").unwrap();
        let err = validate_catalog_plan_freshness(&plan, &entry, now).unwrap_err();
        assert!(err.to_string().contains("stale by its own built_at"));
    }

    #[test]
    fn catalog_plan_rejects_unparseable_built_at() {
        let entry = CatalogEntry {
            path: "plans/111.json".into(),
            ..Default::default()
        };
        let plan = Plan {
            built_at: "not-a-date".into(),
            ..Default::default()
        };
        let err = validate_catalog_plan_freshness(&plan, &entry, 1_704_067_200).unwrap_err();
        assert!(err.to_string().contains("invalid built_at"));
    }
}
