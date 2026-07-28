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
                let s = |k: &str| {
                    map.get(k)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                out.push(Mission {
                    activity_id: s("activityId"),
                    title: s("title"),
                    points: map
                        .get("rewardsPoints")
                        .or_else(|| map.get("totalPoints"))
                        .or_else(|| map.get("maximumBasePoints"))
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0),
                    status: s("status"),
                    label: s("statusLabel"),
                    progress: map
                        .get("progress")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0),
                });
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
