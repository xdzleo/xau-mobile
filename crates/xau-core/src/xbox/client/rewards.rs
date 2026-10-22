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
    /// Cover art of the games that count towards a pack.
    pub images: Vec<String>,
}

/// One day/tier box of a streak mission.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MissionStep {
    /// "Dia 1", "5d", …
    pub label: String,
    /// Points that box pays. 0 for boxes the page leaves blank.
    pub points: i64,
    pub done: bool,
    /// Extra line under the points, e.g. the "Next week 2x" multiplier.
    pub note: Option<String>,
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
    }
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

fn flag(map: &Map, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(serde_json::Value::as_bool))
}

const POINT_KEYS: &[&str] = &[
    "rewardsPoints",
    "points",
    "totalPoints",
    "basePoints",
    "bonusPoints",
    "streakPoints",
    "maximumBasePoints",
];

fn step_done(o: &Map) -> bool {
    if let Some(b) = flag(
        o,
        &["isCompleted", "completed", "isComplete", "isClaimed", "claimed", "achieved", "isAchieved"],
    ) {
        return b;
    }
    text(o, &["status", "state"]).is_some_and(|s| {
        ["claimed", "completed", "complete", "done", "achieved", "earned"]
            .iter()
            .any(|d| s.eq_ignore_ascii_case(d))
    })
}

/// A day/tier array the activity carries itself, if any.
///
/// No captured `programInfo` has shown one yet, so the key is not
/// known: accept any short array of plain objects that carry points or
/// a done flag, one or two levels down, and never one that holds other
/// activities.
fn explicit_steps(map: &Map, depth: u8) -> Vec<MissionStep> {
    for (key, val) in map {
        if key == "activities" {
            continue;
        }
        if let serde_json::Value::Array(items) = val {
            let objs: Vec<&Map> = items.iter().filter_map(serde_json::Value::as_object).collect();
            let looks_like_steps = (2..=10).contains(&objs.len())
                && objs.len() == items.len()
                && objs.iter().all(|o| !o.contains_key("activityId"))
                && objs.iter().any(|o| {
                    POINT_KEYS.iter().any(|k| o.contains_key(*k))
                        || flag(o, &["isCompleted", "completed", "isClaimed"]).is_some()
                });
            if looks_like_steps {
                return objs
                    .iter()
                    .map(|o| MissionStep {
                        label: text(o, &["title", "label", "name", "displayName"]).unwrap_or_default(),
                        points: num(o, POINT_KEYS).unwrap_or(0),
                        done: step_done(o),
                        note: text(o, &["subtitle", "note", "multiplierLabel", "description"]),
                    })
                    .collect();
            }
        }
        if depth > 0 {
            if let Some(child) = val.as_object().filter(|o| !o.contains_key("activityId")) {
                let found = explicit_steps(child, depth - 1);
                if !found.is_empty() {
                    return found;
                }
            }
        }
    }
    Vec::new()
}

/// Image URLs anywhere inside the activity, in order, deduplicated.
fn collect_images(v: &serde_json::Value, key: &str, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            let k = key.to_ascii_lowercase();
            let lower = s.to_ascii_lowercase();
            let imagey = k.contains("image")
                || k.contains("art")
                || k.contains("icon")
                || k.contains("poster")
                || lower.contains("store-images.")
                || [".png", ".jpg", ".jpeg", ".webp"].iter().any(|e| lower.split('?').next().unwrap_or("").ends_with(e));
            if lower.starts_with("http") && imagey && !out.contains(s) {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                collect_images(it, key, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                collect_images(val, k, out);
            }
        }
        _ => {}
    }
}

/// How the official Rewards page lays out the missions it knows, for
/// when the payload only says `progress` and points.
///
/// Values copied from the xbox.com Rewards page: the mobile app streak
/// pays 2/6/10/14/18 over 5 days, the weekly console/PC bonus pays its
/// whole amount on day 5, the Game Pass weekly streak pays +40 on day 5
/// (and doubles next week), +10 on day 6, +20 on day 7. Monthly packs
/// count 4 or 8 games.
fn template(id: &str, title: &str, points: i64) -> Option<(&'static str, Vec<MissionStep>, i64)> {
    let t = format!("{id} {title}").to_ascii_lowercase();
    let days = |pts: &[i64], label: &dyn Fn(usize) -> String| -> Vec<MissionStep> {
        pts.iter()
            .enumerate()
            .map(|(i, p)| MissionStep { label: label(i), points: *p, ..MissionStep::default() })
            .collect()
    };
    let dia = |i: usize| format!("Dia {}", i + 1);
    let gamepass = t.contains("game pass") || t.contains("gamepass");

    if ["8-pack", "8pack", "8 pack", "eight"].iter().any(|k| t.contains(k)) {
        return Some(("pack", Vec::new(), 8));
    }
    if ["4-pack", "4pack", "4 pack", "four"].iter().any(|k| t.contains(k)) {
        return Some(("pack", Vec::new(), 4));
    }
    if gamepass && t.contains("streak") {
        let mut s = days(&[0, 0, 0, 0, 40, 10, 20], &|i| format!("{}d", i + 1));
        s[4].note = Some("Próx. semana 2x".into());
        return Some(("streak", s, 7));
    }
    if t.contains("weekly") && t.contains("bonus") {
        let last = if points > 0 { points } else { 110 };
        return Some(("streak", days(&[0, 0, 0, 0, last], &dia), 5));
    }
    if t.contains("mobile") || t.contains("xboxapp") || t.contains("companion") {
        return Some(("streak", days(&[2, 6, 10, 14, 18], &dia), 5));
    }
    None
}

fn parse_mission(map: &Map) -> Mission {
    let s = |k: &str| text(map, &[k]).unwrap_or_default();
    let activity_id = s("activityId");
    let title = s("title");
    let status = s("status");
    let label = s("statusLabel");
    let points = num(map, &["rewardsPoints", "totalPoints", "maximumBasePoints"]).unwrap_or(0);
    let progress = num(map, &["progress"]).unwrap_or(0).clamp(0, 100);

    let mut m = Mission {
        description: text(map, &["description", "subtitle", "body"]).unwrap_or_default(),
        locked: status.eq_ignore_ascii_case("locked")
            || label.eq_ignore_ascii_case("locked")
            || flag(map, &["isLocked", "locked"]).unwrap_or(false),
        requirement: text(
            map,
            &["requirement", "requirementText", "lockedReason", "lockReason", "unlockRequirement", "eligibilityText"],
        ),
        failed: ["failed", "expired", "missed", "lost"]
            .iter()
            .any(|f| status.eq_ignore_ascii_case(f))
            || flag(map, &["isFailed", "failed", "isExpired"]).unwrap_or(false)
            || label.to_ascii_lowercase().contains("try again"),
        activity_id,
        title,
        points,
        status,
        label,
        progress,
        kind: "single".into(),
        ..Mission::default()
    };
    collect_images(&serde_json::Value::Object(map.clone()), "", &mut m.images);

    let explicit = explicit_steps(map, 2);
    let tpl = template(&m.activity_id, &m.title, m.points);
    let mut count = num(
        map,
        &[
            "currentProgress", "progressCount", "completedCount", "completedDays", "daysCompleted",
            "currentStreak", "streakCount", "streakDays", "gamesPlayed", "progressValue", "current", "count",
        ],
    );
    let mut target = num(
        map,
        &[
            "target", "targetCount", "goal", "threshold", "requiredCount", "requiredDays",
            "gamesRequired", "totalDays", "maxProgress", "total",
        ],
    );

    if !explicit.is_empty() {
        m.kind = "streak".into();
        count = Some(explicit.iter().filter(|s| s.done).count() as i64);
        target = Some(explicit.len() as i64);
        m.steps = explicit;
    } else if let Some((kind, steps, n)) = tpl {
        m.kind = kind.into();
        let n = target.filter(|t| *t > 0).unwrap_or(n);
        target = Some(n);
        // `progress` is 0-100 over the whole streak; turn it back into
        // whole days the same way the page does, rounding down.
        let c = count.unwrap_or(progress * n / 100).clamp(0, n);
        count = Some(c);
        m.steps = steps;
        for (i, st) in m.steps.iter_mut().enumerate() {
            st.done = (i as i64) < c;
        }
    } else if target.is_some_and(|t| t > 1) && m.images.len() >= 2 {
        m.kind = "pack".into();
    }

    if m.is_done() {
        for st in &mut m.steps {
            st.done = true;
        }
        if let Some(t) = target {
            count = Some(t);
        }
    }
    m.count = count;
    m.target = target;

    let finished = target.is_some_and(|t| count.is_some_and(|c| c >= t));
    m.current = (m.kind != "single" && !m.locked && !m.failed && !finished)
        .then(|| usize::try_from(count.unwrap_or(0)).unwrap_or(0));

    m.claimed_points = num(map, &["claimedPoints", "pointsClaimed", "earnedPoints", "pointsEarned", "claimedTotal"])
        .or_else(|| {
            // The page's "Claimed ✓ 6" is the latest paid box reached.
            m.steps.iter().filter(|s| s.done && s.points > 0).last().map(|s| s.points)
        })
        .or_else(|| m.status.eq_ignore_ascii_case("claimed").then_some(m.points));
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

    /// The official page's "Use the XBOX mobile app" card: two days
    /// done, day 3 outlined, "Claimed ✓ 6".
    #[test]
    fn mobile_streak_falls_back_to_the_official_layout() {
        let m = mission(
            r#"{"activityId":"xboxAppStreak","title":"Use the XBOX mobile app",
                "rewardsPoints":18,"status":"active","statusLabel":"Available","progress":40}"#,
        );
        assert_eq!(m.kind, "streak");
        let pts: Vec<i64> = m.steps.iter().map(|s| s.points).collect();
        assert_eq!(pts, [2, 6, 10, 14, 18]);
        assert_eq!(m.count, Some(2));
        assert_eq!(m.current, Some(2));
        assert_eq!(m.claimed_points, Some(6));
    }

    #[test]
    fn a_lost_week_has_no_current_day() {
        let m = mission(
            r#"{"activityId":"consoleWeeklyBonus","title":"Weekly console bonus",
                "rewardsPoints":110,"status":"failed","progress":0}"#,
        );
        assert_eq!(m.steps.len(), 5);
        assert_eq!(m.steps[4].points, 110);
        assert!(m.failed);
        assert_eq!(m.current, None);
    }

    #[test]
    fn locked_game_pass_streak_reports_seven_days() {
        let m = mission(
            r#"{"activityId":"gpWeeklyStreak","title":"Game Pass weekly streak",
                "status":"locked","statusLabel":"Locked"}"#,
        );
        assert!(m.locked);
        assert_eq!(m.steps.len(), 7);
        assert_eq!(m.steps[4].points, 40);
        assert_eq!(m.current, None);
    }

    #[test]
    fn explicit_steps_win_over_the_template() {
        let m = mission(
            r#"{"activityId":"weeklyBonus","title":"Weekly console bonus","rewardsPoints":110,
                "streak":{"days":[
                  {"points":0,"isCompleted":true},{"points":0,"isCompleted":true},
                  {"points":0,"isCompleted":false},{"points":110,"isCompleted":false}]}}"#,
        );
        assert_eq!(m.steps.len(), 4);
        assert_eq!(m.count, Some(2));
        assert_eq!(m.current, Some(2));
    }

    #[test]
    fn packs_count_games_and_keep_their_art() {
        let m = mission(
            r#"{"activityId":"gpMonthly8Pack","title":"Game Pass monthly 8-pack",
                "rewardsPoints":280,"status":"locked","gamesPlayed":3,
                "games":[{"imageUrl":"https://store-images.s-microsoft.com/a"},
                         {"imageUrl":"https://store-images.s-microsoft.com/b"}]}"#,
        );
        assert_eq!(m.kind, "pack");
        assert_eq!(m.target, Some(8));
        assert_eq!(m.count, Some(3));
        assert_eq!(m.images.len(), 2);
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
