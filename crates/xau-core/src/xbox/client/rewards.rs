//! Microsoft Rewards on Xbox — read the balance, claim what is pending.
//!
//! Reverse-engineered from the Xbox PC app by intercepting its own
//! traffic (Schannel `EncryptMessage`, since the app drives these calls
//! through WinRT `Windows.Web.Http` over HTTP/2 — nothing shows up at
//! the WinHTTP layer, and the request line is HPACK-compressed on the
//! wire). Two endpoints, both on `xgrant.xboxlive.com`:
//!
//! ```text
//! GET  /users/xuid({xuid})/programInfo?filter=profile,catalog,activities,spotlights
//! POST /users/xuid({xuid})/claimPoints          body: {}
//! ```
//!
//! The claim body really is an empty object: the caller does not say
//! *what* to claim, the service settles everything outstanding. That is
//! why the button in the Xbox app reads "Claim **all** points".
//!
//! Authentication is ordinary Xbox Live XSTS — the same
//! `Authorization: XBL3.0 x=<uhs>;<token>` every other call in this
//! crate uses. No separate rewards credential, no cookie.
//!
//! The captured request also carried a `signature` header (the ECDSA
//! scheme in [`crate::oauth::signer`]). We do not send one: that
//! signature belongs to the device/SISU handshake, and XSTS-authenticated
//! service calls generally accept its absence. If a future contract
//! version starts rejecting unsigned calls this is the first place to
//! look — the failure would be a 401 on an otherwise well-formed request.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::XboxClient;
use crate::error::{Error, Result};
use crate::xbox::constants as c;

/// Points balance and what is waiting to be claimed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RewardsInfo {
    /// Current points balance.
    pub balance: i64,
    /// Points sitting unclaimed — what the button would credit.
    pub unclaimed: i64,
    /// Individual activities that are claimable right now.
    pub pending: Vec<PendingActivity>,
    /// Rewards program the account is enrolled in, when reported.
    pub program: Option<String>,
    /// Membership level 1-3. `None` when the payload did not carry
    /// one; that is not the same as level 1.
    pub level: Option<i64>,
    /// Every activity the account can see, done or not.
    pub missions: Vec<Mission>,
    /// Game Pass tier as the service names it, e.g. `PCGamePass`,
    /// `GamePassUltimate`. `None` means no membership was reported —
    /// which is what an account without Game Pass looks like.
    pub gamepass: Option<String>,
    /// Every point the account ever earned (`lifetimeEarnedPoints`).
    /// `None` when the payload did not carry it, which is not 0.
    #[serde(default)]
    pub lifetime: Option<i64>,
    /// How far the account is towards the next level this period.
    #[serde(default)]
    pub tier_progress: Option<TierProgress>,
    /// The day the account joined Microsoft Rewards, `YYYY-MM-DD`
    /// (`profile.attributes.created`).
    #[serde(default)]
    pub member_since: Option<String>,
}

/// The account's way to the next Rewards level, as `tiersProfile` states it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TierProgress {
    /// The level it holds now, e.g. "Membro Silver".
    pub tier: String,
    /// The level after it; `None` at the top.
    pub next: Option<String>,
    pub progress: i64,
    pub max: i64,
    /// What the next level asks for, in the service's own words.
    pub requirement: Option<String>,
}

/// One Rewards activity, whatever state it is in.
///
/// `progress` is the service's own 0-100 figure. It is the reason this
/// exists: the app previously estimated mission progress from its own
/// farm clock, which is a guess about how long a mission takes, while
/// this is the answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mission {
    pub activity_id: String,
    pub title: String,
    pub points: i64,
    /// `active`, `claimed`, `completed`, `locked`, …
    pub status: String,
    /// Human-facing label the service supplies: Available, Claimed, …
    pub label: String,
    pub progress: i64,
    /// The service's one-line explanation, e.g. "Play 5 days this week
    /// and increase your total."
    pub description: String,
    /// How the official page draws it: `streak` (a row of day boxes),
    /// `pack` (a row of game covers to fill) or `single`.
    pub kind: String,
    /// One entry per day/tier box, in order. Empty for `single`.
    pub steps: Vec<MissionStep>,
    /// Index into `steps` of the box being worked on today. `None` once
    /// the streak is finished, lost or locked.
    pub current: Option<usize>,
    /// Days/games done so far, when the mission counts something.
    pub count: Option<i64>,
    /// How many it takes.
    pub target: Option<i64>,
    /// Points already credited from this mission ("Claimed ✓ 6").
    pub claimed_points: Option<i64>,
    pub locked: bool,
    /// What unlocks it, when locked ("Requires Game Pass subscription").
    pub requirement: Option<String>,
    /// The period was lost: the official page's "Try again next week".
    pub failed: bool,
    /// The mission's own tile art (`imageUris.tile`).
    pub thumbnail: Option<String>,
    /// When the period ends, ISO-8601 (`resetDate`) — the page's "9d left".
    pub reset_date: Option<String>,
}

/// One box of a streak, or one game slot of a monthly pack — a `punch`
/// in the service's own words.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MissionStep {
    /// "Day 1", "5d", … Empty for a pack's game slots.
    pub label: String,
    /// Points that box pays. 0 for boxes the page leaves blank.
    pub points: i64,
    pub done: bool,
    /// Extra line under the points, e.g. the "Next week 2x" multiplier.
    pub note: Option<String>,
    /// Title id of the game that filled a pack slot.
    pub source_id: Option<String>,
    /// That game's box art. Filled after parsing, from titlehub: the
    /// Rewards payload names the game but carries no picture of it.
    pub image: Option<String>,
}

impl Mission {
    /// Nothing more to earn here: finished and settled.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.status.eq_ignore_ascii_case("claimed") || self.progress >= 100
    }

    /// Available to work on now — not done, and not locked behind
    /// something the user has not reached.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.is_done() && !self.status.eq_ignore_ascii_case("locked")
    }
}

/// One claimable activity ("Play a game on console", a daily quest, …).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingActivity {
    pub activity_id: String,
    pub title: String,
    pub points: i64,
}

/// Result of a claim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaimResult {
    /// Points credited by this call.
    pub points_added: i64,
    /// Balance the service reports afterwards.
    pub balance: i64,
    /// One entry per activity that was settled.
    pub claimed: Vec<ClaimedActivity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaimedActivity {
    pub activity_id: String,
    pub title: String,
    pub base_points: i64,
    pub bonus_points: i64,
    pub streak_points: i64,
    pub total_points: i64,
}

// ---------- wire types ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireClaimResponse {
    #[serde(default)]
    rewards_activities: Vec<WireActivity>,
    #[serde(default)]
    points_added: i64,
    #[serde(default)]
    balance: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireActivity {
    #[serde(default)]
    activity_id: String,
    /// Non-zero means the service refused this particular activity; the
    /// call as a whole can still be a 200, so a caller that only checks
    /// the status code would report a success that credited nothing.
    #[serde(default)]
    code: i64,
    #[serde(default)]
    claim_details: Option<WireClaimDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireClaimDetails {
    #[serde(default)]
    title: String,
    #[serde(default)]
    base_points: i64,
    #[serde(default)]
    bonus_points: Vec<serde_json::Value>,
    #[serde(default)]
    streak_points: i64,
    #[serde(default)]
    total_points: i64,
}

impl XboxClient {
    /// Read the Rewards balance and everything currently claimable.
    ///
    /// # Errors
    /// Propagates transport failures and non-2xx responses. A signed-out
    /// or non-enrolled account surfaces as an Xbox-side error rather than
    /// an empty success.
    pub async fn get_rewards_info(&self, xuid: &str) -> Result<RewardsInfo> {
        if xuid.trim().is_empty() {
            return Err(Error::invalid_argument("empty xuid"));
        }
        let url = format!(
            "https://{}/users/xuid({xuid})/programInfo?filter=profile,catalog,activities,spotlights",
            c::hosts::XGRANT
        );
        let headers = self.headers_with_active(&[(
            c::header_names::CONTRACT_VERSION,
            c::header_values::CONTRACT_VERSION_5,
        )])?;
        let raw: serde_json::Value = self.request_json(Method::GET, &url, headers, None).await?;
        Ok(parse_program_info(&raw))
    }

    /// Claim every outstanding Rewards activity.
    ///
    /// The body is `{}` — the service decides what is owed. Returns what
    /// was actually credited, which is not always what the balance
    /// display promised: an activity can come back with a non-zero
    /// `code` and contribute nothing.
    ///
    /// # Errors
    /// Propagates transport failures and non-2xx responses.
    pub async fn claim_rewards_points(&self, xuid: &str) -> Result<ClaimResult> {
        if xuid.trim().is_empty() {
            return Err(Error::invalid_argument("empty xuid"));
        }
        let url = format!(
            "https://{}/users/xuid({xuid})/claimPoints",
            c::hosts::XGRANT
        );
        let headers = self.headers_with_active(&[
            (
                c::header_names::CONTRACT_VERSION,
                c::header_values::CONTRACT_VERSION_5,
            ),
            ("content-type", "application/json"),
            ("cache-control", "no-cache"),
        ])?;
        let body = serde_json::json!({});
        let resp: WireClaimResponse = self
            .request_json(Method::POST, &url, headers, Some(&body))
            .await?;

        let claimed = resp
            .rewards_activities
            .into_iter()
            .filter(|a| a.code == 0)
            .filter_map(|a| {
                let d = a.claim_details?;
                Some(ClaimedActivity {
                    activity_id: a.activity_id,
                    title: d.title,
                    base_points: d.base_points,
                    bonus_points: i64::try_from(d.bonus_points.len()).unwrap_or(0),
                    streak_points: d.streak_points,
                    total_points: d.total_points,
                })
            })
            .collect();

        Ok(ClaimResult {
            points_added: resp.points_added,
            balance: resp.balance,
            claimed,
        })
    }
}

/// Pull balance and claimable activities out of a `programInfo` body.
///
/// Field names and the meaning of each status come from a real captured
/// response, not guesswork. The shape:
///
/// ```jsonc
/// {
///   "unclaimedBalance": 5,          // points waiting — what the button credits
///   "balance": 0,                   // current spendable balance
///   "profile": { "attributes": { "program": "MSAAUTOENROLL" } },
///   "programOfferings": [
///     { "activities": [
///         { "activityId": "playConsole", "title": "Play a game on console",
///           "rewardsPoints": 5, "status": "completed",
///           "statusLabel": "Unclaimed" }
///     ]}
///   ]
/// }
/// ```
///
/// `status` is the load-bearing field and its values are NOT what the
/// names suggest: `active`/`Available` means "still earnable, not done",
/// `locked` means unavailable, and only **`completed`/`Unclaimed`** is
/// claimable right now. Treating `active` as claimable would list every
/// quest the user has not done yet as if points were waiting.
/// Rewards membership level, 1-3.
///
/// Read from a real `programInfo` response, not guessed:
///
/// ```jsonc
/// "profile":      { "attributes": { "level": "newLevel1" } },  // a STRING
/// "tiersProfile": { "programs": [ { "currentTierIndex": 0 } ] } // 0-BASED
/// ```
///
/// `currentTierIndex` is authoritative and zero-based, so the level is
/// index + 1. Confirmed against two live accounts: one without Game
/// Pass reads 1, another reads 2, matching the Rewards app.
///
/// **`None` is a real answer.** An account that has not levelled up and
/// an account whose level could not be read are different facts.
fn find_level(v: &serde_json::Value) -> Option<i64> {
    let by_index = v
        .get("tiersProfile")
        .and_then(|t| t.get("programs"))
        .and_then(serde_json::Value::as_array)
        .and_then(|programs| {
            programs.iter().find_map(|p| {
                p.get("currentTierIndex")
                    .and_then(serde_json::Value::as_i64)
            })
        })
        .map(|i| i + 1);

    // Fallback: "newLevel1" -> 1, taking the trailing digits.
    let by_name = v
        .get("profile")
        .and_then(|p| p.get("attributes"))
        .and_then(|a| a.get("level"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| {
            let digits: String = s
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            digits.parse::<i64>().ok()
        });

    by_index.or(by_name).filter(|n| (1..=3).contains(n))
}

fn parse_program_info(v: &serde_json::Value) -> RewardsInfo {
    let balance = v
        .get("balance")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let unclaimed = v
        .get("unclaimedBalance")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let program = v
        .get("profile")
        .and_then(|p| p.get("attributes"))
        .and_then(|a| a.get("program"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let level = find_level(v);

    // Same response, no extra call: the programInfo body already carries
    // the membership under one of two spellings. The capital-P form is
    // current; the lowercase one is what older responses used, and the
    // C# original expected. Accept both rather than betting on one.
    let gamepass = ["gamePassMembership", "gamepassMembership"]
        .iter()
        .find_map(|k| {
            v.get(k)
                .or_else(|| v.get("data").and_then(|d| d.get(k)))
                .or_else(|| {
                    v.get("profile")
                        .and_then(|p| p.get("attributes"))
                        .and_then(|a| a.get(k))
                })
                .and_then(serde_json::Value::as_str)
        })
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
        .map(str::to_string);

    let mut pending = Vec::new();
    collect_pending(v, &mut pending);
    let mut missions = Vec::new();
    collect_missions(v, &mut missions);

    RewardsInfo {
        balance,
        unclaimed,
        pending,
        program,
        level,
        missions,
        gamepass,
        lifetime: v.get("lifetimeEarnedPoints").and_then(serde_json::Value::as_i64),
        tier_progress: find_tier_progress(v),
        member_since: v
            .get("profile")
            .and_then(|p| p.get("attributes"))
            .and_then(|a| a.get("created"))
            .and_then(serde_json::Value::as_str)
            .and_then(iso_day),
    }
}

/// `tiersProfile.programs[]`: the current tier by `currentTierIndex`, the
/// next one from `allTiers`, and the period's `progressions[0]`.
fn find_tier_progress(v: &serde_json::Value) -> Option<TierProgress> {
    let program = v.get("tiersProfile")?.get("programs")?.as_array()?.first()?;
    let index = usize::try_from(program.get("currentTierIndex")?.as_i64()?).ok()?;
    let tiers = program.get("allTiers").and_then(serde_json::Value::as_array);
    // Named by id, in Portuguese: the service answers in whatever language
    // the request did not ask for ("Silver Member"), and the app is pt-BR.
    let title = |i: usize| {
        let t = tiers.and_then(|t| t.get(i))?;
        let by_id = match t.get("id").and_then(serde_json::Value::as_str) {
            Some("newLevel1") => Some("Membro"),
            Some("newLevel2") => Some("Membro Silver"),
            Some("newLevel3") => Some("Membro Gold"),
            _ => None,
        };
        by_id
            .map(str::to_string)
            .or_else(|| t.get("title").and_then(serde_json::Value::as_str).map(str::to_string))
    };
    let p = program.get("progressions")?.as_array()?.first()?;
    Some(TierProgress {
        tier: title(index)
            .or_else(|| p.get("title").and_then(serde_json::Value::as_str).map(str::to_string))
            .unwrap_or_default(),
        next: title(index + 1),
        progress: p.get("progress")?.as_i64()?,
        max: p.get("max")?.as_i64()?.max(1),
        requirement: p
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
    })
}

/// The service writes this date two ways: `2018-05-05T12:48:12…` and
/// `8/2/2018 4:11:25 AM +00:00` (month first). Both → `YYYY-MM-DD`.
fn iso_day(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() >= 10 && raw.as_bytes()[4] == b'-' {
        return Some(raw[..10].to_string());
    }
    let date = raw.split_whitespace().next()?;
    let mut parts = date.split('/').map(str::parse::<u32>);
    let (m, d, y) = (parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && y > 1999).then(|| format!("{y:04}-{m:02}-{d:02}"))
}

/// Walk the payload for EVERY activity, in any state.
///
/// Same recursive shape as [`collect_pending`] and for the same reason:
/// the offerings array mixes container kinds and its nesting carries no
/// documented guarantee, so a positional walk would silently return
/// nothing the next time the layout moves.
fn collect_missions(v: &serde_json::Value, out: &mut Vec<Mission>) {
    match v {
        serde_json::Value::Object(map) => {
            if map.contains_key("activityId") {
                out.push(parse_mission(map));
            }
            for val in map.values() {
                collect_missions(val, out);
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                collect_missions(it, out);
            }
        }
        _ => {}
    }
}

type Map = serde_json::Map<String, serde_json::Value>;

/// First of `keys` holding a number, also accepting `"5"` and `5.0`.
fn num(map: &Map, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| {
        let v = map.get(*k)?;
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f.round() as i64))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

/// First of `keys` holding a non-empty string.
fn text(map: &Map, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        map.get(*k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn obj<'a>(map: &'a Map, key: &str) -> Option<&'a Map> {
    map.get(key).and_then(serde_json::Value::as_object)
}

/// One activity, read the way the xbox.com Rewards page reads it.
///
/// Shapes taken from a real `programInfo` (Ultimate account, Oct 2026):
///
/// ```jsonc
/// "displayTemplates": { "inline": "gallery" },      // basic | streakedPunchCard | gallery
/// "progressInfo": { "completedSteps": 4, "totalSteps": 8 },
/// "punchcard": { "punches": [                        // day boxes, or a pack's game slots
///   { "text": "5d", "pointValue": 80, "hintText": "Next week 2x", "state": "active" },
///   { "sourceType": "Title", "sourceId": "1938834516", "state": "punched" } ] },
/// "streakLabels": ["Day 1", …], "streakAdditions": [6, 14, …], "currentStreak": 1,
/// "status": "expired",                               // the week was lost
/// "resetDate": "2026-11-01T05:00:00Z"
/// ```
///
/// Login streaks (`resetsWhenBroken`) have no punchcard: their boxes are
/// `streakLabels` paired with `streakAdditions`, filled up to
/// `currentStreak`.
fn parse_mission(map: &Map) -> Mission {
    let s = |k: &str| text(map, &[k]).unwrap_or_default();
    let status = s("status");
    let is = |st: &str| status.eq_ignore_ascii_case(st);

    let mut m = Mission {
        activity_id: s("activityId"),
        title: s("title"),
        // The weekly streak pays 0 now and up to `maximumBasePoints`; the
        // page's "+70" is the latter.
        points: num(map, &["rewardsPoints"])
            .filter(|p| *p > 0)
            .or_else(|| num(map, &["maximumBasePoints", "totalPoints"]))
            .unwrap_or(0),
        label: s("statusLabel"),
        progress: num(map, &["progress"]).unwrap_or(0).clamp(0, 100),
        description: s("description"),
        locked: is("locked"),
        failed: is("expired") || is("failed"),
        reset_date: text(map, &["resetDate"])
            .or_else(|| obj(map, "dates").and_then(|d| text(d, &["resetDate"]))),
        thumbnail: obj(map, "imageUris").and_then(|i| text(i, &["tile", "thumbnail"])),
        kind: "single".into(),
        ..Mission::default()
    };
    if m.locked {
        m.requirement = obj(map, "powerUpStrings").and_then(|p| text(p, &["description"]));
    }

    let punches: Vec<&Map> = obj(map, "punchcard")
        .and_then(|p| p.get("punches"))
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_object).collect())
        .unwrap_or_default();
    let labels: Vec<String> = map
        .get("streakLabels")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    if !punches.is_empty() {
        m.steps = punches
            .iter()
            .map(|p| MissionStep {
                label: text(p, &["text"]).unwrap_or_default(),
                points: num(p, &["pointValue"]).unwrap_or(0),
                done: text(p, &["state"]).is_some_and(|st| st.eq_ignore_ascii_case("punched")),
                note: text(p, &["hintText"]),
                source_id: text(p, &["sourceId"]),
                image: None,
            })
            .collect();
    } else if !labels.is_empty() {
        let adds: Vec<i64> = map
            .get("streakAdditions")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect())
            .unwrap_or_default();
        let streak = usize::try_from(num(map, &["currentStreak"]).unwrap_or(0)).unwrap_or(0);
        m.steps = labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| MissionStep {
                label,
                points: adds.get(i).copied().unwrap_or(0),
                done: i < streak,
                ..MissionStep::default()
            })
            .collect();
    }

    let inline = obj(map, "displayTemplates").and_then(|d| text(d, &["inline"]));
    m.kind = match inline.as_deref() {
        Some("gallery") => "pack",
        _ if !m.steps.is_empty() => "streak",
        _ => "single",
    }
    .into();

    if m.kind == "single" {
        let info = obj(map, "progressInfo");
        m.count = info.and_then(|i| num(i, &["completedSteps"]));
        m.target = info.and_then(|i| num(i, &["totalSteps"]));
    } else {
        // Count the boxes rather than trusting `progressInfo`: a login
        // streak reports 1/1 for today while showing five days.
        m.count = Some(m.steps.iter().filter(|s| s.done).count() as i64);
        m.target = Some(m.steps.len() as i64);
    }

    // Today's box is outlined only while the period is still open.
    let open = !(m.locked || m.failed || is("claimed") || is("completed"));
    m.current = if open && m.kind != "single" {
        m.steps.iter().position(|s| !s.done)
    } else {
        None
    };

    m.claimed_points = (is("claimed") || is("completed"))
        .then(|| num(map, &["totalPoints", "rewardsPoints"]))
        .flatten()
        .filter(|p| *p > 0);
    m
}

fn collect_pending(v: &serde_json::Value, out: &mut Vec<PendingActivity>) {
    match v {
        serde_json::Value::Object(map) => {
            let claimable = map
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("completed"))
                || map
                    .get("statusLabel")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("unclaimed"));
            if claimable && map.contains_key("activityId") {
                out.push(PendingActivity {
                    activity_id: map
                        .get("activityId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    title: map
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    points: map
                        .get("rewardsPoints")
                        .or_else(|| map.get("totalPoints"))
                        .or_else(|| map.get("maximumBasePoints"))
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0),
                });
            }
            for child in map.values() {
                collect_pending(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_pending(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact response the Xbox app received for a real claim,
    /// captured off the wire. If parsing of this ever regresses the
    /// feature is silently reporting zero points while the service
    /// credited them.
    const REAL_CLAIM_RESPONSE: &str = r#"{
      "rewardsActivities": [{
        "correlationId": "aPgoDscA/i+m7ETcPvWRlO.134.28",
        "code": 0,
        "activityId": "playConsole",
        "reportId": "2026-07-18_playConsole",
        "claimDetails": {
          "title": "Play a game on console",
          "basePoints": 5,
          "bonusPoints": [],
          "streakPoints": 0,
          "totalPoints": 5,
          "activityCompletionTime": "2026-07-18T22:05:48.4069958-05:00",
          "claimedTime": "2026-07-28T03:16:31.2434036Z"
        }
      }],
      "pointsAdded": 5,
      "balance": 5
    }"#;

    #[test]
    fn parses_the_real_captured_claim_response() {
        let wire: WireClaimResponse = serde_json::from_str(REAL_CLAIM_RESPONSE).expect("parse");
        assert_eq!(wire.points_added, 5);
        assert_eq!(wire.balance, 5);
        assert_eq!(wire.rewards_activities.len(), 1);
        let a = &wire.rewards_activities[0];
        assert_eq!(a.activity_id, "playConsole");
        assert_eq!(a.code, 0);
        let d = a.claim_details.as_ref().expect("claimDetails");
        assert_eq!(d.title, "Play a game on console");
        assert_eq!(d.total_points, 5);
    }

    /// A 200 can carry activities the service refused. Counting those as
    /// claimed would report points that were never credited.
    #[test]
    fn activities_with_a_nonzero_code_are_not_reported_as_claimed() {
        let body = r#"{
          "rewardsActivities": [
            {"code": 0, "activityId": "ok",   "claimDetails": {"title": "A", "totalPoints": 5}},
            {"code": 4, "activityId": "nope", "claimDetails": {"title": "B", "totalPoints": 99}}
          ],
          "pointsAdded": 5,
          "balance": 5
        }"#;
        let wire: WireClaimResponse = serde_json::from_str(body).expect("parse");
        let kept: Vec<_> = wire
            .rewards_activities
            .into_iter()
            .filter(|a| a.code == 0)
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].activity_id, "ok");
    }

    /// Trimmed from a REAL `programInfo` response captured off the wire.
    /// Statuses are verbatim: only `playConsole` was claimable, and the
    /// service credited exactly it when the claim ran.
    const REAL_PROGRAM_INFO: &str = r#"{
      "hasProgramAccess": true,
      "profile": { "attributes": { "program": "MSAAUTOENROLL", "country": "br" } },
      "gamePassMembership": "None",
      "unclaimedBalance": 5,
      "programOfferings": [
        { "containerKind": "Redeemables", "redeemables": [{"price": 2840}] },
        { "displayTemplate": "channel", "activities": [
          {"activityId":"PCGamePlay","title":"Play a PC game","rewardsPoints":5,
           "status":"active","statusLabel":"Available"},
          {"activityId":"playConsole","title":"Play a game on console","rewardsPoints":5,
           "status":"completed","statusLabel":"Unclaimed"},
          {"activityId":"pcWeeklyBonus","title":"Weekly bonus","rewardsPoints":110,
           "status":"active","statusLabel":"Available"},
          {"activityId":"playGamePass","title":"Game Pass","rewardsPoints":10,
           "status":"locked","statusLabel":"Locked"}
        ]}
      ],
      "balance": 0,
      "lifetimeEarnedPoints": 0
    }"#;

    #[test]
    fn reads_the_real_program_info_shape() {
        let v: serde_json::Value = serde_json::from_str(REAL_PROGRAM_INFO).expect("json");
        let info = parse_program_info(&v);
        assert_eq!(info.unclaimed, 5, "unclaimedBalance is the pending total");
        assert_eq!(info.balance, 0);
        assert_eq!(info.program.as_deref(), Some("MSAAUTOENROLL"));
        assert_eq!(info.lifetime, Some(0), "lifetimeEarnedPoints is read");
        let none = parse_program_info(&serde_json::json!({ "balance": 1 }));
        assert_eq!(none.lifetime, None, "absent is unknown, not 0");
        assert_eq!(none.tier_progress, None);
        assert_eq!(none.member_since, None);
    }

    /// The shape of a live response (Silver, 736 of 750 towards Gold).
    #[test]
    fn tier_progress_and_member_since_are_read() {
        let v = serde_json::json!({
            "profile": { "attributes": { "created": "8/2/2018 4:11:25 AM +00:00" } },
            "tiersProfile": { "programs": [{
                "currentTierIndex": 1,
                "progressions": [{ "title": "Membro Silver", "description": "Ganhe 750 pontos", "progress": 736, "max": 750 }],
                "allTiers": [{ "title": "Membro" }, { "title": "Membro Silver" }, { "title": "Membro Gold" }]
            }]}
        });
        let info = parse_program_info(&v);
        assert_eq!(
            info.tier_progress,
            Some(TierProgress {
                tier: "Membro Silver".into(),
                next: Some("Membro Gold".into()),
                progress: 736,
                max: 750,
                requirement: Some("Ganhe 750 pontos".into()),
            })
        );
        assert_eq!(info.member_since.as_deref(), Some("2018-08-02"), "month first");
        assert_eq!(iso_day("2018-05-05T12:48:12.4459920+00:00").as_deref(), Some("2018-05-05"));
        assert_eq!(iso_day("nonsense"), None);
    }

    /// The whole point of the status rules: `active` means "you have not
    /// done this yet", NOT "points are waiting". Counting those would
    /// advertise 125 pending points when only 5 can be claimed, and the
    /// claim button would promise something the service will not pay.
    #[test]
    fn only_completed_unclaimed_activities_count_as_pending() {
        let v: serde_json::Value = serde_json::from_str(REAL_PROGRAM_INFO).expect("json");
        let info = parse_program_info(&v);
        assert_eq!(
            info.pending.len(),
            1,
            "expected only playConsole, got {:?}",
            info.pending
        );
        assert_eq!(info.pending[0].activity_id, "playConsole");
        assert_eq!(info.pending[0].title, "Play a game on console");
        assert_eq!(info.pending[0].points, 5);
    }

    /// The pending list must agree with the headline number, otherwise
    /// the card shows "7 to claim" over a list that sums to something
    /// else and neither number can be trusted.
    #[test]
    fn pending_points_sum_to_the_unclaimed_balance() {
        let v: serde_json::Value = serde_json::from_str(REAL_PROGRAM_INFO).expect("json");
        let info = parse_program_info(&v);
        let sum: i64 = info.pending.iter().map(|a| a.points).sum();
        assert_eq!(sum, info.unclaimed);
    }

    fn mission(body: &str) -> Mission {
        let v: serde_json::Value = serde_json::from_str(body).expect("json");
        parse_mission(v.as_object().expect("object"))
    }

    /// Trimmed from a real `gamepassMonthly8Titles`: four games played,
    /// each punch naming the title that filled it.
    #[test]
    fn monthly_pack_slots_name_the_games_played() {
        let m = mission(
            r#"{"activityId":"gamepassMonthly8Titles","title":"Game Pass monthly 8-pack",
                "displayTemplates":{"details":"galleryDetails","inline":"gallery"},
                "maximumBasePoints":280,"rewardsPoints":280,"totalPoints":560,
                "status":"active","statusLabel":"Available","progress":50,
                "resetDate":"2026-11-01T05:00:00Z",
                "imageUris":{"background":"https://cms/bg.jpg","tile":"https://cms/tile.png"},
                "progressInfo":{"completedSteps":4,"totalSteps":8},
                "punchcard":{"cardType":"AllOrNothing","currentPunch":0,"punches":[
                  {"pointValue":0,"sourceId":"1938834516","sourceType":"Title","state":"punched","text":null},
                  {"pointValue":0,"sourceId":"1810111157","sourceType":"Title","state":"punched","text":null},
                  {"pointValue":0,"sourceId":"2075317023","sourceType":"Title","state":"punched","text":null},
                  {"pointValue":0,"sourceId":"1708767297","sourceType":"Title","state":"punched","text":null},
                  {"pointValue":0,"sourceId":null,"sourceType":"Title","state":"active","text":null},
                  {"pointValue":0,"sourceId":null,"sourceType":"Title","state":"active","text":null},
                  {"pointValue":0,"sourceId":null,"sourceType":"Title","state":"active","text":null},
                  {"pointValue":0,"sourceId":null,"sourceType":"Title","state":"active","text":null}]}}"#,
        );
        assert_eq!(m.kind, "pack");
        assert_eq!(m.steps.len(), 8);
        assert_eq!((m.count, m.target), (Some(4), Some(8)));
        assert_eq!(m.current, Some(4), "slot 5 is the one outlined");
        assert_eq!(m.steps[0].source_id.as_deref(), Some("1938834516"));
        assert!(m.steps[4].source_id.is_none());
        assert_eq!(m.points, 280);
        assert_eq!(m.reset_date.as_deref(), Some("2026-11-01T05:00:00Z"));
        assert_eq!(m.thumbnail.as_deref(), Some("https://cms/tile.png"));
        assert!(m.is_open());
    }

    /// Real `gamePassWeekly`: 2 of 7 days, day 5 pays 80 and doubles
    /// next week. It pays 0 so far; the page shows the maximum.
    #[test]
    fn weekly_streak_reads_its_punches() {
        let m = mission(
            r#"{"activityId":"gamePassWeekly","title":"Game Pass weekly streak",
                "displayTemplates":{"inline":"streakedPunchCard"},
                "maximumBasePoints":70,"rewardsPoints":0,"totalPoints":0,"status":"active",
                "progressInfo":{"completedSteps":2,"totalSteps":7},
                "punchcard":{"currentPunch":2,"punches":[
                  {"pointValue":0,"state":"punched","text":"1d"},
                  {"pointValue":0,"state":"punched","text":"2d"},
                  {"pointValue":0,"state":"active","text":"3d"},
                  {"pointValue":0,"state":"active","text":"4d"},
                  {"hintText":"Next week 2x","pointValue":80,"state":"active","text":"5d"},
                  {"pointValue":20,"state":"active","text":"6d"},
                  {"pointValue":40,"state":"active","text":"7d"}]}}"#,
        );
        assert_eq!(m.kind, "streak");
        assert_eq!(m.current, Some(2));
        assert_eq!(m.steps[4].points, 80);
        assert_eq!(m.steps[4].note.as_deref(), Some("Next week 2x"));
        assert_eq!(m.points, 70);
    }

    /// Real `griffinLogin` (the mobile app): no punchcard, the boxes are
    /// `streakLabels` + `streakAdditions` up to `currentStreak`.
    #[test]
    fn login_streak_uses_labels_and_additions() {
        let m = mission(
            r#"{"activityId":"griffinLogin","title":"Use the XBOX mobile app",
                "displayTemplates":{"inline":"streakedPunchCard"},"currentStreak":1,
                "rewardsPoints":2,"totalPoints":8,"status":"claimed","statusLabel":"Claimed",
                "progressInfo":{"completedSteps":1,"totalSteps":1},
                "streakAdditions":[6,14,22,30,48],
                "streakLabels":["Day 1","Day 2","Day 3","Day 4","Day 5"],
                "streakType":"resetsWhenBroken"}"#,
        );
        assert_eq!(m.kind, "streak");
        assert_eq!(m.steps.len(), 5);
        assert!(m.steps[0].done && !m.steps[1].done);
        assert_eq!(m.steps[1].points, 14);
        assert_eq!(m.current, None, "claimed today: nothing to outline");
        assert_eq!(m.claimed_points, Some(8));
    }

    /// Real `pcWeeklyBonus` with `status: expired` — the page's
    /// "Try again next week".
    #[test]
    fn an_expired_week_is_lost() {
        let m = mission(
            r#"{"activityId":"pcWeeklyBonus","title":"Weekly PC bonus",
                "displayTemplates":{"inline":"streakedPunchCard"},
                "rewardsPoints":110,"status":"expired",
                "punchcard":{"punches":[
                  {"pointValue":0,"state":"active","text":"Day 1"},
                  {"pointValue":260,"state":"active","text":"Day 5"}]}}"#,
        );
        assert!(m.failed);
        assert_eq!(m.current, None);
        assert_eq!(m.points, 110);
    }

    #[test]
    fn an_empty_or_odd_payload_does_not_panic() {
        for body in ["{}", "[]", "null", r#"{"a":{"b":[1,2,3]}}"#] {
            let v: serde_json::Value = serde_json::from_str(body).expect("json");
            let info = parse_program_info(&v);
            assert_eq!(info.balance, 0);
            assert!(info.pending.is_empty());
        }
    }
}
