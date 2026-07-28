//! Verbatim port of `XAU/Util/Constants/Constants.cs`.
//!
//! All URLs, header names, header values and hosts match the original so that
//! Xbox Live servers observe identical traffic.

pub const ZERO_UID: &str = "00000000-0000-0000-0000-000000000000";
pub const GAMERSCORE_SETTING: &str = "Gamerscore";
pub const ACHIEVED_LITERAL: &str = "Achieved";

/// HTTP header names MUST be lowercase — `http::HeaderName::from_static`
/// panics on any uppercase byte. Xbox Live and HTTP itself are
/// case-insensitive on header names, so keeping them lowercase is
/// wire-identical to the C# source (which used Pascal-case literals in
/// `Constants.cs`).
pub mod header_names {
    pub const CONTRACT_VERSION: &str = "x-xbl-contract-version";
    pub const ACCEPT_ENCODING: &str = "accept-encoding";
    pub const ACCEPT: &str = "accept";
    pub const AUTHORIZATION: &str = "authorization";
    pub const ACCEPT_LANGUAGE: &str = "accept-language";
    pub const HOST: &str = "host";
    pub const CONNECTION: &str = "connection";
    pub const SIGNATURE: &str = "signature";
    pub const USER_AGENT: &str = "user-agent";
}

pub mod header_values {
    pub const CONTRACT_VERSION_2: &str = "2";
    pub const CONTRACT_VERSION_3: &str = "3";
    pub const CONTRACT_VERSION_4: &str = "4";
    pub const CONTRACT_VERSION_5: &str = "5";
    pub const ACCEPT_ENCODING: &str = "gzip, deflate";
    pub const ACCEPT: &str = "application/json";
    pub const KEEP_ALIVE: &str = "Keep-Alive";
    /// "Fake signature" literal used by the unlock-achievements route when the
    /// corresponding setting is enabled. Preserved verbatim from upstream.
    pub const FAKE_SIGNATURE: &str =
        "RGFtbklHb3R0YU1ha2VUaGlzU3RyaW5nU3VwZXJMb25nSHVoLkRvbnRFdmVuS25vd1doYXRTaG91bGRCZUhlcmVEcmFmZlN0cmluZw==";
    pub const ACHIEVEMENTS_USER_AGENT: &str = "XboxServicesAPI/2021.10.20211005.0 c";
}

pub mod hosts {
    pub const ACHIEVEMENTS: &str = "achievements.xboxlive.com";
    pub const PROFILE: &str = "profile.xboxlive.com";
    pub const PEOPLE_HUB: &str = "peoplehub.xboxlive.com";
    pub const TITLE_HUB: &str = "titlehub.xboxlive.com";
    pub const TELEMETRY: &str = "v20.events.data.microsoft.com";
    /// Microsoft Rewards on Xbox: balance, claimable activities, and
    /// the claim itself. Not documented anywhere; found by watching
    /// the Xbox PC app's own traffic.
    pub const XGRANT: &str = "xgrant.xboxlive.com";
    pub const GITHUB_API: &str = "api.github.com";
    pub const GITHUB_RAW: &str = "raw.githubusercontent.com";
}

pub mod urls {
    pub const GAMERTAG: &str =
        "https://profile.xboxlive.com/users/me/profile/settings?settings=Gamertag";
    pub const WATERMARKS: &str = "https://dlassets-ssl.xboxlive.com/public/content/ppl/watermarks/";
    pub const GAMEPASS_CATALOG: &str =
        "https://catalog.gamepass.com/products?market=GB&language=en-GB&hydration=PCHome";
    pub const TELEMETRY: &str = "https://v20.events.data.microsoft.com/OneCollector/1.0/";
    pub const USER_STATS: &str = "https://userstats.xboxlive.com/batch";
}

pub fn gamepass_membership_url(xuid: &str) -> String {
    format!("https://xgrant.xboxlive.com/users/xuid({xuid})/programInfo?filter=profile,activities,catalog")
}

pub fn profile_url(xuid: &str) -> String {
    format!("https://peoplehub.xboxlive.com/users/me/people/xuids({xuid})/decoration/detail,preferredColor,presenceDetail,multiplayerSummary")
}

pub fn title_url(xuid: &str) -> String {
    format!("https://titlehub.xboxlive.com/users/xuid({xuid})/titles/batch/decoration/GamePass,Achievement,Stats")
}

/// Same batch endpoint as `title_url` but with `image,achievement,scid`
/// decoration — returns cover art + per-title achievement counts for
/// an arbitrary list of title_ids. Used by the deep-scan Games tab to
/// enrich bulk-scan results with covers (bulk achievements endpoint
/// by itself never includes `displayImage`).
pub fn title_batch_with_images_url(xuid: &str) -> String {
    format!("https://titlehub.xboxlive.com/users/xuid({xuid})/titles/batch/decoration/scid,image,achievement")
}

pub fn titles_url(xuid: &str) -> String {
    format!("https://titlehub.xboxlive.com/users/xuid({xuid})/titles/titleHistory/decoration/Achievement,detail,scid?maxItems=10000")
}

pub fn query_achievements_url(xuid: &str, title_id: &str) -> String {
    format!("https://achievements.xboxlive.com/users/xuid({xuid})/achievements?titleId={title_id}&maxItems=1000")
}

/// Page size used by the bulk profile-level achievement scan.
///
/// **Tuned against live Xbox Live under concurrent load, NOT serial curl probes.**
///
/// | maxItems | serial  | 12 parallel (siegfriedx 126k)                 |
/// |----------|---------|------------------------------------------------|
/// |     1000 |  ~43s   | ✅ works, ~10 rounds × 30s = ~300s total       |
/// |     5000 |  ~40s   | ❌ all 12 timeout after 300s (server saturates)|
/// |    10000 |  ~68s   | ❌ all 12 timeout (even worse)                 |
///
/// Xbox Live enforces per-user concurrency caps beyond its 150 req /
/// 300 sec rate limit: with large `maxItems` values, 12 concurrent
/// requests all queue behind each other server-side and every one
/// exceeds our 300s reqwest timeout. With `maxItems=1000`, the body
/// is 1 MB (vs 11 MB for 10000), server cost is low enough that
/// Xbox actually serves all 12 in parallel.
///
/// Net for a 126k-achievement profile:
///   127 pages / 12 parallel = 11 rounds × ~30s = ~5.5 min total
///
/// Synthetic curl benchmarks against 1-at-a-time probes misled us
/// into 10000 and then 5000; both worked serially, both failed under
/// parallel load. 1000 is the only size empirically proven to scan
/// siegfriedx-class profiles to completion.
pub const BULK_SCAN_PAGE_SIZE: usize = 1000;

/// Profile-level achievements endpoint — returns every achievement the
/// user has on Xbox Live, across every title, with `titleAssociations`
/// identifying which title each one belongs to. Paginated at
/// [`BULK_SCAN_PAGE_SIZE`] per page. Used by `build_plan` to replace
/// thousands of per-title fetches with one dense scan — 37x fewer
/// requests for a SiegfriedX-sized profile.
pub fn query_all_achievements_url(xuid: &str, unlocked_only: bool) -> String {
    format!(
        "https://achievements.xboxlive.com/users/xuid({xuid})/achievements?maxItems={BULK_SCAN_PAGE_SIZE}&unlockedOnly={unlocked_only}"
    )
}

pub fn query_all_achievements_url_continued(
    xuid: &str,
    unlocked_only: bool,
    continuation_token: &str,
) -> String {
    format!(
        "https://achievements.xboxlive.com/users/xuid({xuid})/achievements?maxItems={BULK_SCAN_PAGE_SIZE}&unlockedOnly={unlocked_only}&continuationToken={continuation_token}"
    )
}

/// Like `query_all_achievements_url` but targets a specific offset via
/// `skipItems`. Critical for the parallel scan: the continuation token
/// is opaque (server-generated) so we can't compute page N+1 without
/// receiving page N — but `skipItems=N` accepts any offset directly,
/// letting us fire all pages in parallel once we know the total.
pub fn query_all_achievements_url_offset(xuid: &str, unlocked_only: bool, skip: usize) -> String {
    format!(
        "https://achievements.xboxlive.com/users/xuid({xuid})/achievements?maxItems={BULK_SCAN_PAGE_SIZE}&unlockedOnly={unlocked_only}&skipItems={skip}"
    )
}

/// Probe URL — `maxItems=1` to get `totalRecords` FAST before dispatching
/// the parallel page fetches. Key optimization: a probe typically
/// returns in <5s even on mega-profiles, versus 150s+ for a full 10k
/// page. We use the probe's totalRecords to compute all offsets, then
/// fire ALL real pages (including offset=0) in parallel. Total scan
/// wall-clock ≈ single parallel round time instead of
/// `page1 + parallel_round`.
pub fn query_all_achievements_probe_url(xuid: &str, unlocked_only: bool) -> String {
    format!(
        "https://achievements.xboxlive.com/users/xuid({xuid})/achievements?maxItems=1&unlockedOnly={unlocked_only}"
    )
}

/// Achievements URL with a server-provided continuation token. Xbox Live
/// paginates at 1000 items per page; this variant lets the fetcher loop
/// until the server stops handing out a `continuationToken`. Matches the
/// approach OpenXbox/xbox-webapi-python uses for profiles with >1000
/// achievements in a single title (rare, but happens — long-running MMOs).
pub fn query_achievements_url_continued(
    xuid: &str,
    title_id: &str,
    continuation_token: &str,
) -> String {
    format!(
        "https://achievements.xboxlive.com/users/xuid({xuid})/achievements?titleId={title_id}&maxItems=1000&continuationToken={continuation_token}"
    )
}

pub fn query_achievements_360_url(xuid: &str, title_id: &str) -> String {
    format!("https://achievements.xboxlive.com/users/xuid({xuid})/titleachievements?titleId={title_id}&maxItems=1000")
}

pub fn update_achievements_url(xuid: &str, service_config_id: &str) -> String {
    format!("https://achievements.xboxlive.com/users/xuid({xuid})/achievements/{service_config_id}/update")
}

pub fn heartbeat_url(xuid: &str) -> String {
    format!("https://presence-heartbeat.xboxlive.com/users/xuid({xuid})/devices/current/")
}

pub fn gamertag_search_url(gamertag: &str) -> String {
    format!("https://profile.xboxlive.com/users/gt({gamertag})/profile/settings?settings=GameDisplayPicRaw,Gamerscore,Gamertag")
}

pub mod process_names {
    pub const XBOX_PC_APP: &str = "XboxPcApp";
}

pub mod events_urls {
    pub const ZIP: &str =
        "https://github.com/Fumo-Unlockers/Xbox-Achievement-Unlocker/raw/Events-Data/Events.zip";
    pub const META: &str =
        "https://raw.githubusercontent.com/Fumo-Unlockers/Xbox-Achievement-Unlocker/Events-Data/meta.json";
}

/// Maximum number of achievements to unlock in a single POST to Xbox Live.
///
/// Exact limit: **64** achievements per request (measured empirically
/// against the live `/achievements/{scid}/update` endpoint with valid
/// SCID + title, bogus achievement IDs). N=64 returns HTTP 400 with
/// `{"code":49,...}` (body accepted, IDs rejected individually —
/// expected since IDs are bogus). N=65+ returns HTTP 400 with
/// `{"code":6,"description":"The body of the request was invalid."}`
/// (server caps count BEFORE per-ID processing). The cap is on
/// *count*, not body bytes: N=64 with 36-char UUID IDs (4897 bytes)
/// succeeds; N=65 with 1-char IDs (2696 bytes) fails.
///
/// Previous value was 50 ("empirically safe below 100"). Bumping to
/// 64 gives +28% unlocks per request — more throughput inside the
/// same rate-limit budget (100 req / 15s on update endpoint).
pub const UNLOCK_CHUNK_SIZE: usize = 64;

/// Maximum number of titles packed into a single `presence-heartbeat` POST.
///
/// Matches the upper bound used by the sister `rewards_farming_rust` project;
/// the Xbox API itself silently caps at around this value.
pub const HEARTBEAT_CHUNK_SIZE: usize = 10;

/// Default number of random games picked by the GUI's farm mode when the
/// user doesn't override the list.
pub const FARM_DEFAULT_TITLES: usize = 8;

/// Default interval between heartbeats in farm mode (seconds).
pub const FARM_PING_INTERVAL_SECS: u64 = 180;

/// Default duration of a single farm cycle (seconds).
pub const FARM_CYCLE_SECS: u64 = 900;

/// Byte pattern `XBL3.0 x=` used to find XAUTH tokens in ASCII/UTF-8
/// process memory (the most common case).
pub const XAUTH_SCAN_PATTERN: &[u8] = b"XBL3.0 x=";

/// UTF-16 LE encoding of `XBL3.0 x=` — what UWP / WinRT apps (and the modern
/// Xbox app) often have in their process heap because C# / WinRT strings are
/// UTF-16 by default.
pub const XAUTH_SCAN_PATTERN_UTF16_LE: &[u8] = b"X\0B\0L\0\x33\0.\0\x30\0 \0x\0=\0";
