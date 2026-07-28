//! Cross-plan aggregation: "meta-plan of the best times".
//!
//! Given N saved plans (from different Xbox profiles the user has
//! scanned or imported), produce a synthetic plan that combines the
//! **fastest per-title completion** from any of the inputs. The
//! practical use: someone is building a replay strategy and wants
//! "if profile A finished Gears 5 in 20 min and profile B took 30
//! min, use A's entries for Gears 5". Applied across every title,
//! you get the theoretical "fastest route to plat" assembled from
//! real data.
//!
//! # Algorithm
//!
//! 1. Collect every title_id that appears in ANY input plan.
//! 2. For each title_id, find the input plan whose entries for that
//!    title represent the fastest completion. "Fastest" means
//!    (in priority order): largest `achievement_count` (more
//!    complete is better — 30/30 unlocks beats 3/30 even if the
//!    3/30 was time-compact); then smallest span (first→last
//!    unlock timestamp); then smallest `built_at` timestamp
//!    (older = more reproducible).
//! 3. Copy the winner's entries for that title into the output.
//! 4. Dedupe by `(title_id, achievement_id)` at the end — same
//!    entry appearing from multiple plans reduces to one.
//! 5. Sort chronologically (oldest unlock first). Output plan is
//!    drop-in compatible with the runner.
//!
//! # Merged title_totals
//!
//! The output's `title_totals` takes the winning plan's entry for
//! each title — the entry that corresponds to the chosen fastest
//! completion. Fallback: if the winner has no totals entry, try
//! the OTHER plans' totals for that title (prefer `total > 0`),
//! because a runner relies on `title_totals` to decide 100%.
//!
//! # What this is NOT
//!
//! This does not verify that the aggregate is *achievable* for the
//! signed-in user — every title_id copied is someone else's
//! unlocks, and some achievements are time-windowed (expired) or
//! revoked. Those get caught by the safety gate at replay time.
//! The aggregate is a "best-known route" in terms of pacing, not a
//! guarantee that every entry is still unlockable today.
//!
//! # When you should use it
//!
//! - You have 5 saved plans from various streamers/speedrunners.
//! - You want to replay the "fastest route" across your whole
//!   library.
//! - You accept that titles only one profile has played only
//!   contribute their own pace (nothing to compare against).

use std::collections::{HashMap, HashSet};

use crate::plan::{compute_span_seconds, Plan, PlanEntry, TitleCompletion};

/// Aggregate several plans into a synthetic "best-of" plan. See
/// module docs for the algorithm.
///
/// Returns a valid `Plan` with:
/// - `source_gamertag: "best-of"` so callers can distinguish.
/// - `source_xuid: "aggregate"` (placeholder) or
///   `aggregate-of-1:{gamertag}` / `aggregate-of-N`.
/// - `built_at` = ISO timestamp NOW.
/// - `entries` = union of winner entries, deduped by
///   `(title_id, achievement_id)`, sorted chronologically with
///   secondary `(title_id, achievement_id)` order for ties.
/// - `title_totals` = winner's totals per title, with fallback from
///   other inputs when the winner's totals are absent/junk, and
///   `current_achievements` always normalized to the deduped entry
///   count so `is_complete()` reflects what the aggregate contains.
///
/// Safe with zero inputs: returns an empty plan.
/// With one input: normalizes it (dedupe + sort) and tags as
/// `aggregate-of-1:{gamertag}`. NOT a pass-through — the normalization
/// is intentional so the output invariants match the multi-input
/// path (a caller can rely on "output is always deduped + sorted").
///
/// Deterministic: same inputs → same outputs. All internal HashMap /
/// HashSet iteration is funneled through a sort before it can
/// influence the output order.
#[must_use]
pub fn best_of_aggregate(plans: &[&Plan]) -> Plan {
    if plans.is_empty() {
        return Plan {
            source_gamertag: "best-of".to_string(),
            source_xuid: "aggregate".to_string(),
            built_at: now_iso(),
            total: 0,
            entries: Vec::new(),
            title_totals: HashMap::new(),
            // Aggregates have no scan of their own to report.
            build_stats: Default::default(),
            title_min_completion_secs: HashMap::new(),
            source_xuids: Vec::new(),
        };
    }
    if plans.len() == 1 {
        // Single input — tag it as best-of + normalize entries
        // (dedupe internal duplicates, sort chronologically) so the
        // output invariants match the multi-input path. Previously
        // this cloned `p.entries` raw; a corrupted/hand-edited input
        // with duplicate (title, ach) pairs or out-of-order rows
        // passed through silently, breaking the contract "aggregate
        // output is always chronological + deduped". BUG HUNTER A5.
        let p = plans[0];
        let mut entries = p.entries.clone();
        let mut seen: HashSet<(String, String)> = HashSet::with_capacity(entries.len());
        entries.retain(|e| seen.insert((e.title_id.clone(), e.achievement_id.clone())));
        entries.sort_by(deterministic_entry_order);
        let source_xuids: Vec<String> = p
            .source_xuids
            .iter()
            .cloned()
            .chain(std::iter::once(p.source_xuid.clone()))
            .filter(|xu| is_real_xuid(xu))
            .fold(Vec::new(), |mut acc, xu| {
                if !acc.contains(&xu) {
                    acc.push(xu);
                }
                acc
            });
        return Plan {
            source_gamertag: "best-of".to_string(),
            source_xuid: format!("aggregate-of-1:{}", p.source_gamertag),
            built_at: now_iso(),
            total: entries.len(),
            entries,
            title_totals: p.title_totals.clone(),
            // Aggregates have no scan of their own to report.
            build_stats: Default::default(),
            title_min_completion_secs: p.title_min_completion_secs.clone(),
            source_xuids,
        };
    }

    // Pre-index: for each plan, build `title_id -> Vec<&PlanEntry>`
    // ONCE. The original algorithm filtered `plan.entries` per
    // (title × plan) combination — O(N_titles × N_plans × N_entries),
    // which on realistic workloads (5 plans × 125k entries × 1500
    // unique titles) runs ~937M iterations and visibly freezes the
    // app during aggregate. Pre-indexing makes the lookups O(1) and
    // brings the whole pass to ~1ms. See audit bug #27.
    let mut per_plan_index: Vec<HashMap<&str, Vec<&PlanEntry>>> = Vec::with_capacity(plans.len());
    for p in plans {
        let mut idx: HashMap<&str, Vec<&PlanEntry>> = HashMap::new();
        for e in &p.entries {
            idx.entry(e.title_id.as_str()).or_default().push(e);
        }
        per_plan_index.push(idx);
    }

    // Collect every title_id seen across all inputs.
    let mut all_titles: HashSet<String> = HashSet::new();
    for p in plans {
        for e in &p.entries {
            all_titles.insert(e.title_id.clone());
        }
    }

    // BUG HUNTER A3: iterate `all_titles` in a DETERMINISTIC order
    // so the `winners` HashMap (and downstream `out_entries`) don't
    // inherit HashSet's RandomState ordering. Not strictly required
    // for correctness (final sort normalizes most of it) but entries
    // with identical timestamps would otherwise end up in random
    // pre-sort positions. Same input → same output is a documented
    // property of this function; lock it down.
    let mut sorted_titles: Vec<&String> = all_titles.iter().collect();
    sorted_titles.sort();

    // For each title, score each plan that has that title, pick winner.
    let mut winners: HashMap<String, PerTitleWinner> = HashMap::new();
    for title_id in sorted_titles {
        let mut best: Option<PerTitleWinner> = None;
        for (plan_idx, plan) in plans.iter().enumerate() {
            let Some(entries_for_title) = per_plan_index[plan_idx].get(title_id.as_str()) else {
                continue;
            };
            if entries_for_title.is_empty() {
                continue;
            }
            // BUG HUNTER A1 re-analysis: using `entries.len()` as the
            // completeness score is CORRECT. The aggregate's value
            // lives in the unlocks it can EXECUTE, not in the
            // target's reported catalogue state. Re-routing to
            // `title_totals.current_achievements` would let a capped
            // plan (entries.len()=3, current=30) win the tiebreak
            // over a no-cap plan (entries.len()=30) — the aggregate
            // would then only dispatch 3 unlocks when 30 were
            // available. Kept the raw entries count here; any
            // future "weighted completeness" metric needs to also
            // penalize cap-truncation. See the regression test
            // `a1_prefers_more_executable_entries_even_if_target_complete`.
            //
            // BUG HUNTER N16: count UNIQUE (title_id, achievement_id)
            // pairs, not raw `entries.len()`. A plan with internal
            // duplicates `{T1/a, T1/a}` would otherwise report
            // count=2 and beat a clean `{T1/a}` plan with count=1 —
            // but the aggregate only dispatches the deduped set, so
            // the "winner" contributes fewer actual unlocks than its
            // inflated score suggested. Dedupe via a HashSet of
            // achievement_ids under this title_id. O(n) per-title
            // which is bounded by the title's ach count (never more
            // than a few hundred).
            let unique_ach_ids: HashSet<&str> = entries_for_title
                .iter()
                .map(|e| e.achievement_id.as_str())
                .collect();
            let count = unique_ach_ids.len();
            let span =
                compute_span_seconds(entries_for_title.iter().map(|e| e.time_unlocked.as_str()));
            let candidate = PerTitleWinner {
                plan_idx,
                count,
                span_seconds: span,
                // Parse the built_at once per candidate for the
                // tiebreak; fall back to i64::MAX so un-parseable
                // dates sort LAST (i.e. lose the tiebreak), which
                // matches "older = more reproducible wins".
                built_at_unix: crate::plan::runner::parse_iso_public(&plan.built_at)
                    .unwrap_or(i64::MAX),
            };
            best = Some(match best {
                None => candidate,
                Some(current) => pick_better(current, candidate),
            });
        }
        if let Some(w) = best {
            winners.insert(title_id.clone(), w);
        }
    }

    // Materialize entries from each winning plan — reuse the index
    // so the materialization is also O(winners) instead of
    // O(winners × N_entries).
    //
    // BUG HUNTER A3: iterate `winners` via a SORTED key list so the
    // pre-final-sort order is deterministic. Matters for entries
    // with identical `time_unlocked` — the stable sort at the end
    // preserves their relative pre-sort order.
    let mut winner_keys: Vec<&String> = winners.keys().collect();
    winner_keys.sort();
    let mut out_entries: Vec<PlanEntry> = Vec::new();
    let mut out_totals: HashMap<String, TitleCompletion> = HashMap::new();
    // Track the winner's entry count per title so the
    // `current_achievements` in out_totals always matches what the
    // aggregate actually contains (see A4 below).
    let mut winner_entry_count: HashMap<String, usize> = HashMap::new();
    for title_id in winner_keys {
        let w = &winners[title_id];
        let entries_for_title = per_plan_index[w.plan_idx]
            .get(title_id.as_str())
            .cloned()
            .unwrap_or_default();
        // BUG HUNTER N16: mirror the dedupe-before-count logic from
        // the winner-selection loop. `winner_entry_count` is later
        // written into `title_totals[title].current_achievements`,
        // so it must match what the final out_entries dedup will
        // produce. Otherwise a winner with internal duplicates would
        // report `current=2` on a title whose aggregate actually
        // contains 1 unique entry — breaking `is_complete()`.
        let unique_ach_ids: HashSet<&str> = entries_for_title
            .iter()
            .map(|e| e.achievement_id.as_str())
            .collect();
        winner_entry_count.insert(title_id.clone(), unique_ach_ids.len());
        for e in &entries_for_title {
            out_entries.push((*e).clone());
        }
        if let Some(tc) = plans[w.plan_idx].title_totals.get(title_id) {
            out_totals.insert(title_id.clone(), *tc);
        }
    }

    // Fill title_totals gaps: for a title where the winner didn't
    // have totals populated (or had total=0 junk), consult the
    // OTHER input plans in order and use the first with
    // `total > 0`. A real total helps the 100%-ranking; better to
    // have a second-choice source than to have none.
    //
    // BUG HUNTER A4: when falling back to ANOTHER plan's totals,
    // preserve that plan's `total_achievements` (the catalog size —
    // a property of the title, not the user) but SET
    // `current_achievements` to the winner's entry count for this
    // title. Previously the fallback copied the whole TitleCompletion
    // struct, so the aggregate's current count reflected an
    // unrelated plan's unlock state. Example (pre-fix): Alice wins
    // T1 with 3 entries; her totals are junk (0/0). Fallback picks
    // Bob's 5/10. Plan now says "T1 current=5, total=10" but only
    // has 3 entries to execute. `is_complete()` returns 5>=10=false
    // when it should reflect the 3-of-10 reality. Same logic also
    // corrects the winner-path out_totals when the winner's
    // current_achievements disagrees with entries.len() (happens
    // when the winner's plan was capped — A1's sibling issue).
    //
    // MSRV 1.75 doesn't have `Option::is_none_or` (stable 1.82),
    // so use an explicit match for the fallback gate.
    // Also iterate `all_titles` in sorted order to keep the fallback
    // resolution deterministic (relevant when multiple plans have
    // non-junk totals and the first-match rule fires).
    let mut fallback_titles: Vec<&String> = all_titles.iter().collect();
    fallback_titles.sort();
    for title_id in fallback_titles {
        let need_fallback = match out_totals.get(title_id) {
            None => true,
            Some(tc) => tc.total_achievements <= 0,
        };
        if need_fallback {
            for p in plans {
                if let Some(tc) = p.title_totals.get(title_id) {
                    if tc.total_achievements > 0 {
                        out_totals.insert(title_id.clone(), *tc);
                        break;
                    }
                }
            }
        }
        // After the fallback resolution, normalize `current_achievements`
        // to the winner's actual entry count — keeps the aggregate's
        // self-consistency so `is_complete()` reflects what the plan
        // CAN execute, not what some unrelated source reported.
        if let (Some(tc), Some(&winner_count)) = (
            out_totals.get_mut(title_id),
            winner_entry_count.get(title_id),
        ) {
            tc.current_achievements = i32::try_from(winner_count).unwrap_or(i32::MAX);
        }
    }

    // Dedupe by (title_id, achievement_id) preserving first
    // occurrence, then sort with the deterministic ordering helper.
    //
    // BUG HUNTER A2: the old `a.time_unlocked.cmp(&b.time_unlocked)`
    // was a LEX compare on ISO strings — `"T00:00:00.500Z"` sorts
    // BEFORE `"T00:00:00Z"` because `.` (0x2E) < `Z` (0x5A). Mixed
    // fractional/non-fractional timestamps (common when aggregating
    // plans from different eras) produced silently-wrong chronology.
    // `deterministic_entry_order` parses timestamps once then
    // secondary-sorts on (title_id, achievement_id) — fixes A2 + A3
    // in one place.
    let mut seen: HashSet<(String, String)> = HashSet::with_capacity(out_entries.len());
    out_entries.retain(|e| seen.insert((e.title_id.clone(), e.achievement_id.clone())));
    out_entries.sort_by(deterministic_entry_order);

    // Per-title real-playtime floor (sequential-mode pacing): take the
    // MIN across every source profile that reported a MinutesPlayed
    // floor — the FASTEST real completion of that game among the
    // aggregated profiles. This is the one place that still has each
    // profile's own floor, so it must be computed here or the identity
    // is lost (PlanEntry carries no source tag). A title whose inputs
    // all lacked the stat is simply absent → the runner falls back to
    // that title's calendar span.
    let mut out_min_secs: HashMap<String, i64> = HashMap::new();
    for p in plans {
        for (tid, &secs) in &p.title_min_completion_secs {
            if secs <= 0 {
                continue;
            }
            out_min_secs
                .entry(tid.clone())
                .and_modify(|m| {
                    if secs < *m {
                        *m = secs;
                    }
                })
                .or_insert(secs);
        }
    }

    // Preserve the REAL input profile xuids (dedup, drop synthetic /
    // empty ones) so the runner can LIVE-fetch a missing title's floor
    // from the actual speedrunners and re-derive the fastest completion,
    // instead of collapsing to the 60 s fallback. `source_xuid` above is
    // the synthetic "aggregate-of-N" display id and can't be queried.
    let mut source_xuids: Vec<String> = Vec::new();
    for p in plans {
        for xu in p
            .source_xuids
            .iter()
            .cloned()
            .chain(std::iter::once(p.source_xuid.clone()))
        {
            if is_real_xuid(&xu) && !source_xuids.contains(&xu) {
                source_xuids.push(xu);
            }
        }
    }

    Plan {
        source_gamertag: "best-of".to_string(),
        source_xuid: format!("aggregate-of-{}", plans.len()),
        built_at: now_iso(),
        total: out_entries.len(),
        entries: out_entries,
        title_totals: out_totals,
        // Aggregates have no scan of their own to report.
        build_stats: Default::default(),
        title_min_completion_secs: out_min_secs,
        source_xuids,
    }
}

/// A real, re-queryable Xbox xuid: all-digits and non-empty. Filters
/// out synthetic aggregate ids ("aggregate-of-3", "aggregate-of-1:Gt").
fn is_real_xuid(xu: &str) -> bool {
    !xu.is_empty() && xu.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Clone)]
struct PerTitleWinner {
    plan_idx: usize,
    count: usize,
    span_seconds: i64,
    /// BUG HUNTER A7: parsed unix-seconds for the tiebreak.
    /// Previous `built_at: String` lex-compared ISO strings, which
    /// works only when every caller uses the same ISO shape — a
    /// fragile contract to bet on. Parsing once per candidate is
    /// O(plans) cost, effectively free.
    built_at_unix: i64,
}

/// Decide which of two title-winner candidates is "better".
/// Priority (higher = preferred):
/// 1. Higher `count` (more complete — 30/30 beats 5/30 even if
///    the 5/30 is time-compact).
/// 2. Smaller `span_seconds` (tighter completion window).
/// 3. Older `built_at_unix` (stable / reproducible wins ties, rather
///    than favoring whichever scan happened most recently).
/// 4. Lower `plan_idx` (final stable tiebreak — deterministic on
///    genuinely-identical candidates, same plan-list order wins).
fn pick_better(a: PerTitleWinner, b: PerTitleWinner) -> PerTitleWinner {
    use std::cmp::Ordering;
    match a.count.cmp(&b.count) {
        Ordering::Greater => return a,
        Ordering::Less => return b,
        Ordering::Equal => {}
    }
    match a.span_seconds.cmp(&b.span_seconds) {
        Ordering::Less => return a,
        Ordering::Greater => return b,
        Ordering::Equal => {}
    }
    match a.built_at_unix.cmp(&b.built_at_unix) {
        Ordering::Less => return a,
        Ordering::Greater => return b,
        Ordering::Equal => {}
    }
    // Genuinely-identical candidates: lower plan_idx wins so the
    // caller-supplied order is the final arbiter. Makes tests stable
    // under future re-ordering.
    if a.plan_idx <= b.plan_idx {
        a
    } else {
        b
    }
}

/// Deterministic ordering for plan entries — used both in the
/// multi-input and single-input return paths, so the invariant
/// "output entries are sorted chronologically then by
/// (title_id, achievement_id)" holds identically everywhere.
///
/// Parses `time_unlocked` via `parse_iso_public` so mixed-fractional
/// ISO shapes sort correctly (BUG HUNTER A2). Secondary sort on
/// `(title_id, achievement_id)` pins the order of entries sharing
/// an exact second (BUG HUNTER A3) — relevant when aggregating
/// chunked unlocks where Xbox collapsed multiple timestamps into one.
fn deterministic_entry_order(a: &PlanEntry, b: &PlanEntry) -> std::cmp::Ordering {
    let parse = crate::plan::runner::parse_iso_public;
    let ta = parse(&a.time_unlocked).unwrap_or(i64::MIN);
    let tb = parse(&b.time_unlocked).unwrap_or(i64::MIN);
    ta.cmp(&tb)
        .then_with(|| a.title_id.cmp(&b.title_id))
        .then_with(|| a.achievement_id.cmp(&b.achievement_id))
}

fn now_iso() -> String {
    // Reuse the same chrono-free ISO-8601 formatter that scan/
    // build_plan use — keeps aggregates visually consistent with
    // regular plans in the summary UI.
    crate::plan::builder::iso_now_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(title: &str, ach: &str, unlocked: &str, gs: i32) -> PlanEntry {
        PlanEntry {
            title_id: title.into(),
            service_config_id: String::new(),
            achievement_id: ach.into(),
            achievement_name: format!("Ach {ach}"),
            game_name: format!("Title {title}"),
            time_unlocked: unlocked.into(),
            gamerscore: Some(gs),
        }
    }

    fn plan(gamertag: &str, built_at: &str, entries: Vec<PlanEntry>) -> Plan {
        Plan {
            source_gamertag: gamertag.into(),
            source_xuid: format!("xuid-{gamertag}"),
            built_at: built_at.into(),
            total: entries.len(),
            entries,
            title_totals: HashMap::new(),
            // Aggregates have no scan of their own to report.
            build_stats: Default::default(),
            title_min_completion_secs: HashMap::new(),
            source_xuids: Vec::new(),
        }
    }

    #[test]
    fn aggregate_takes_min_playtime_floor_across_profiles() {
        // Two profiles completed title "t1": Alice in 1200s, Bob in
        // 600s. The aggregate floor for t1 must be the FASTEST (600s).
        // "t2" only Alice has a floor (300s) → kept. "t3" nobody has a
        // floor → absent (runner falls back to span).
        let mut a = plan(
            "Alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("t1", "a", "2024-01-01T00:00:00Z", 10),
                mk_entry("t3", "z", "2024-01-01T00:00:00Z", 5),
            ],
        );
        a.title_min_completion_secs.insert("t1".into(), 1200);
        a.title_min_completion_secs.insert("t2".into(), 300);
        let mut b = plan(
            "Bob",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("t1", "a", "2024-01-02T00:00:00Z", 10)],
        );
        b.title_min_completion_secs.insert("t1".into(), 600);

        let agg = best_of_aggregate(&[&a, &b]);
        assert_eq!(
            agg.title_min_completion_secs.get("t1"),
            Some(&600),
            "aggregate floor = fastest real completion across profiles"
        );
        assert_eq!(agg.title_min_completion_secs.get("t2"), Some(&300));
        assert_eq!(agg.title_min_completion_secs.get("t3"), None);
    }

    #[test]
    fn is_real_xuid_filters_synthetic_ids() {
        assert!(is_real_xuid("2535453323076376"));
        assert!(!is_real_xuid(""));
        assert!(!is_real_xuid("aggregate"));
        assert!(!is_real_xuid("aggregate-of-3"));
        assert!(!is_real_xuid("aggregate-of-1:Alice"));
        assert!(!is_real_xuid("xuid-alice"));
    }

    #[test]
    fn aggregate_preserves_real_source_xuids_for_live_fetch() {
        // Two profiles with REAL numeric xuids feed the best-of; the
        // aggregate must keep both (deduped) so the runner can live-fetch
        // missing floors from them — NOT the synthetic "aggregate-of-N".
        let mut a = plan(
            "Alice",
            "2024-01-01T00:00:00Z",
            vec![mk_entry("t1", "a", "2024-01-01T00:00:00Z", 10)],
        );
        a.source_xuid = "1111111111111111".into();
        let mut b = plan(
            "Bob",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("t1", "a", "2024-01-02T00:00:00Z", 10)],
        );
        b.source_xuid = "2222222222222222".into();
        let agg = best_of_aggregate(&[&a, &b]);
        assert!(agg.source_xuid.starts_with("aggregate-of-"));
        assert_eq!(
            agg.source_xuids,
            vec![
                "1111111111111111".to_string(),
                "2222222222222222".to_string()
            ],
            "aggregate must carry both real input xuids for live floor lookups"
        );
        // Single-input path preserves the one real xuid too.
        let single = best_of_aggregate(&[&a]);
        assert_eq!(single.source_xuids, vec!["1111111111111111".to_string()]);
        // Dedup: same profile twice → one xuid.
        let dup = best_of_aggregate(&[&a, &a]);
        assert_eq!(dup.source_xuids, vec!["1111111111111111".to_string()]);
    }

    #[test]
    fn empty_input_returns_empty_best_of() {
        let p = best_of_aggregate(&[]);
        assert_eq!(p.source_gamertag, "best-of");
        assert!(p.entries.is_empty());
    }

    #[test]
    fn single_input_passes_entries_through() {
        let a = plan(
            "alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T00:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T00:30:00Z", 10),
            ],
        );
        let agg = best_of_aggregate(&[&a]);
        assert_eq!(agg.entries.len(), 2);
        assert_eq!(agg.source_gamertag, "best-of");
    }

    #[test]
    fn faster_span_wins_when_count_is_equal() {
        // Both profiles have T1 with 2 entries. Alice took 30 min,
        // Bob took 20 min. Aggregate should contain Bob's entries.
        let alice = plan(
            "alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T10:30:00Z", 10),
            ],
        );
        let bob = plan(
            "bob",
            "2024-01-02T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-02T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-02T10:20:00Z", 10),
            ],
        );
        let agg = best_of_aggregate(&[&alice, &bob]);
        assert_eq!(agg.entries.len(), 2, "should carry 2 entries from bob");
        // All winning entries should be from bob's timeline.
        for e in &agg.entries {
            assert!(
                e.time_unlocked.starts_with("2024-01-02"),
                "entry {:?} not from bob",
                e.time_unlocked
            );
        }
    }

    #[test]
    fn higher_count_beats_faster_span() {
        // Alice platted T1 completely (5/5) in 1 hour.
        // Bob "platted" T1 (1/1 — but there's really 5 achievements)
        // in 1 minute.
        // Alice's completeness wins — we want the 5-entry set.
        let alice = plan(
            "alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T10:15:00Z", 10),
                mk_entry("T1", "c", "2024-01-01T10:30:00Z", 10),
                mk_entry("T1", "d", "2024-01-01T10:45:00Z", 10),
                mk_entry("T1", "e", "2024-01-01T11:00:00Z", 10),
            ],
        );
        let bob = plan(
            "bob",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("T1", "a", "2024-01-02T10:00:00Z", 10)],
        );
        let agg = best_of_aggregate(&[&alice, &bob]);
        assert_eq!(agg.entries.len(), 5, "should prefer alice's complete run");
        for e in &agg.entries {
            assert!(e.time_unlocked.starts_with("2024-01-01"));
        }
    }

    #[test]
    fn different_titles_from_different_profiles_all_merged() {
        // Alice only has T1, Bob only has T2. Aggregate has both.
        let alice = plan(
            "alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T10:10:00Z", 10),
            ],
        );
        let bob = plan(
            "bob",
            "2024-01-02T00:00:00Z",
            vec![
                mk_entry("T2", "x", "2024-01-02T10:00:00Z", 20),
                mk_entry("T2", "y", "2024-01-02T10:05:00Z", 20),
            ],
        );
        let agg = best_of_aggregate(&[&alice, &bob]);
        assert_eq!(agg.entries.len(), 4);
        let titles: HashSet<_> = agg.entries.iter().map(|e| e.title_id.clone()).collect();
        assert!(titles.contains("T1"));
        assert!(titles.contains("T2"));
    }

    #[test]
    fn dedupe_identical_entries_across_plans() {
        // Alice and Bob have the EXACT same (title, ach, ts) entry.
        // Aggregate shouldn't double-count it.
        let e = mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10);
        let alice = plan("alice", "2024-01-01T00:00:00Z", vec![e.clone()]);
        let bob = plan("bob", "2024-01-02T00:00:00Z", vec![e]);
        let agg = best_of_aggregate(&[&alice, &bob]);
        assert_eq!(agg.entries.len(), 1, "duplicate (title, ach) collapsed");
    }

    #[test]
    fn output_entries_are_sorted_chronologically() {
        let alice = plan(
            "alice",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T2", "x", "2024-02-01T10:00:00Z", 10),
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
            ],
        );
        let bob = plan(
            "bob",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("T3", "z", "2024-03-01T10:00:00Z", 10)],
        );
        let agg = best_of_aggregate(&[&alice, &bob]);
        let times: Vec<&str> = agg
            .entries
            .iter()
            .map(|e| e.time_unlocked.as_str())
            .collect();
        for pair in times.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "entries must be chronologically sorted: {times:?}"
            );
        }
    }

    // ============================================================
    // BUG HUNTER A1-A7 regression tests. Each bug gets a dedicated
    // test that would fail under the prior behaviour.
    // ============================================================

    /// A1 (NO-REGRESSION): the plan with MORE executable entries
    /// wins over a plan whose `title_totals.current_achievements`
    /// claims higher completeness but whose entries.len() is smaller.
    /// Aggregate value = unlocks we can actually dispatch, not
    /// target-side catalogue stats. A capped plan (3 entries,
    /// current=30) must LOSE to a no-cap plan (30 entries, current=30).
    #[test]
    fn a1_prefers_more_executable_entries_even_if_target_complete() {
        // Capped Alice: only 3 entries available to execute, but
        // her target-side current=30 (titlehub claims 30/30 on T1).
        let mut alice_totals = HashMap::new();
        alice_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 30,
                total_achievements: 30,
            },
        );
        let alice = Plan {
            title_totals: alice_totals,
            ..plan(
                "alice",
                "2024-01-01T00:00:00Z",
                vec![
                    mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                    mk_entry("T1", "b", "2024-01-01T10:01:00Z", 10),
                    mk_entry("T1", "c", "2024-01-01T10:02:00Z", 10),
                ],
            )
        };
        // Bob: no cap, 10 entries for T1, target 10/10.
        let mut bob_totals = HashMap::new();
        bob_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 10,
                total_achievements: 10,
            },
        );
        let bob = Plan {
            title_totals: bob_totals,
            ..plan(
                "bob",
                "2024-01-02T00:00:00Z",
                (0..10)
                    .map(|i| {
                        mk_entry(
                            "T1",
                            &format!("b{i}"),
                            &format!("2024-01-02T10:{:02}:00Z", i),
                            10,
                        )
                    })
                    .collect(),
            )
        };
        let agg = best_of_aggregate(&[&alice, &bob]);
        // Bob wins — 10 executable entries > 3. We'll dispatch 10
        // unlocks instead of 3, which is the point of "best-of".
        assert_eq!(
            agg.entries.len(),
            10,
            "should pick Bob's 10 entries over Alice's 3"
        );
        for e in &agg.entries {
            assert!(
                e.time_unlocked.starts_with("2024-01-02"),
                "expected Bob's entries, got {:?}",
                e.time_unlocked
            );
        }
    }

    /// A3: output order is deterministic across repeated runs even
    /// when entries share an exact second (Xbox chunked unlocks
    /// collapse multiple achievement timestamps onto one second).
    /// All entries here come from a single plan / single winner per
    /// title, so they all survive the winner-selection and exercise
    /// the final sort's secondary key (title_id, achievement_id).
    #[test]
    fn a3_output_order_is_deterministic_on_equal_timestamps() {
        // One plan, one title "T1", multiple entries all stamped at
        // the same second. Pre-fix: HashSet iteration → pre-sort
        // order randomized → stable sort preserved that random order.
        // Post-fix: secondary (title_id, ach_id) key pins output.
        let p = plan(
            "solo",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "z", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "m", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T2", "y", "2024-01-01T10:00:00Z", 10),
                mk_entry("T2", "b", "2024-01-01T10:00:00Z", 10),
            ],
        );
        let r1 = best_of_aggregate(&[&p]);
        let r2 = best_of_aggregate(&[&p]);
        let r3 = best_of_aggregate(&[&p]);
        let order1: Vec<_> = r1
            .entries
            .iter()
            .map(|e| (e.title_id.clone(), e.achievement_id.clone()))
            .collect();
        let order2: Vec<_> = r2
            .entries
            .iter()
            .map(|e| (e.title_id.clone(), e.achievement_id.clone()))
            .collect();
        let order3: Vec<_> = r3
            .entries
            .iter()
            .map(|e| (e.title_id.clone(), e.achievement_id.clone()))
            .collect();
        assert_eq!(order1, order2);
        assert_eq!(order2, order3);
        // Expected: timestamps equal → by title_id asc, then ach_id asc.
        assert_eq!(
            order1,
            vec![
                ("T1".into(), "a".into()),
                ("T1".into(), "m".into()),
                ("T1".into(), "z".into()),
                ("T2".into(), "b".into()),
                ("T2".into(), "y".into()),
            ]
        );
    }

    /// A4: when title_totals fallback fires, `current_achievements`
    /// is set to the winner's entry count, not the fallback plan's
    /// unrelated current count. Preserves internal consistency so
    /// `is_complete()` reflects the aggregate's contents.
    #[test]
    fn a4_fallback_totals_set_current_to_winner_entry_count() {
        // Alice wins T1 with 3 entries but her totals are junk (0/0).
        // Bob is NOT the winner but has totals 5/10. Pre-fix: the
        // whole TitleCompletion got copied → "current=5, total=10"
        // on a 3-entry plan. Post-fix: total=10 kept, current=3
        // (winner's entries count).
        let mut alice_totals = HashMap::new();
        alice_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 0,
                total_achievements: 0,
            },
        );
        let mut bob_totals = HashMap::new();
        bob_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 5,
                total_achievements: 10,
            },
        );
        let alice = Plan {
            title_totals: alice_totals,
            ..plan(
                "alice",
                "2024-01-01T00:00:00Z",
                vec![
                    mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                    mk_entry("T1", "b", "2024-01-01T10:10:00Z", 10),
                    mk_entry("T1", "c", "2024-01-01T10:20:00Z", 10),
                ],
            )
        };
        let bob = Plan {
            title_totals: bob_totals,
            ..plan(
                "bob",
                "2024-01-02T00:00:00Z",
                vec![mk_entry("T1", "x", "2024-01-02T10:00:00Z", 10)],
            )
        };
        let agg = best_of_aggregate(&[&alice, &bob]);
        assert_eq!(
            agg.entries.len(),
            3,
            "Alice's 3 entries should be the winner"
        );
        let tc = agg
            .title_totals
            .get("T1")
            .expect("T1 totals should be populated via fallback");
        assert_eq!(tc.total_achievements, 10, "fallback kept Bob's valid total");
        assert_eq!(
            tc.current_achievements, 3,
            "current MUST equal winner's entry count, not fallback's 5"
        );
    }

    /// N16: a plan with internal duplicates for a title must NOT
    /// win over a cleaner plan with the same unique-count of entries.
    /// The winner-selection metric is UNIQUE `achievement_id` count
    /// per title, not raw entries.len() (which would inflate when
    /// the plan file has duplicate rows).
    #[test]
    fn n16_internal_duplicates_dont_inflate_winner_count() {
        // Dirty: {T1/a, T1/a, T1/b} — 3 entries but 2 unique.
        let dirty = plan(
            "dirty",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10), // dup
                mk_entry("T1", "b", "2024-01-01T10:05:00Z", 10),
            ],
        );
        // Clean: {T1/a, T1/b, T1/c} — 3 unique, tighter span.
        let clean = plan(
            "clean",
            "2024-01-02T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-02T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-02T10:01:00Z", 10),
                mk_entry("T1", "c", "2024-01-02T10:02:00Z", 10),
            ],
        );
        let agg = best_of_aggregate(&[&dirty, &clean]);
        // Clean's unique count = 3; Dirty's unique count = 2. Clean wins.
        assert_eq!(agg.entries.len(), 3, "clean plan should win");
        for e in &agg.entries {
            assert!(
                e.time_unlocked.starts_with("2024-01-02"),
                "expected clean plan's entries, got {:?}",
                e.time_unlocked
            );
        }
    }

    /// N16 (winner-case): when the winning plan itself has internal
    /// duplicates, the final output dedupes AND the title_totals'
    /// `current_achievements` reflects the UNIQUE count — matching
    /// what's actually in the aggregate's entries vec.
    #[test]
    fn n16_winner_with_duplicates_reports_unique_count() {
        // Single input with internal duplicates, WITH title_totals
        // populated so the fallback + normalization paths both fire.
        let mut totals = HashMap::new();
        totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 99,
                total_achievements: 99,
            },
        );
        let dup_plan = Plan {
            title_totals: totals,
            ..plan(
                "solo",
                "2024-01-01T00:00:00Z",
                vec![
                    mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                    mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                    mk_entry("T1", "b", "2024-01-01T10:05:00Z", 10),
                ],
            )
        };
        // Pair with an empty placeholder to force the multi-input path.
        let empty_plan = plan("empty", "2024-01-01T00:00:00Z", vec![]);
        let agg = best_of_aggregate(&[&dup_plan, &empty_plan]);
        // Aggregate: dedupe of {a, a, b} = 2 entries.
        assert_eq!(agg.entries.len(), 2);
        // current_achievements must match the DEDUPED count (2),
        // not the original plan's junk 99 nor the raw entries.len() 3.
        let tc = agg.title_totals.get("T1").expect("T1 totals present");
        assert_eq!(
            tc.current_achievements, 2,
            "current should reflect deduped winner entries"
        );
    }

    /// A5: single-input path runs the same dedup + sort as the
    /// multi-input path. A corrupted single input (duplicate entries,
    /// out-of-order rows) gets normalized.
    #[test]
    fn a5_single_input_is_deduped_and_sorted() {
        // Single plan with a duplicate entry + out-of-order timestamps.
        let dup = mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10);
        let later = mk_entry("T1", "b", "2024-01-01T11:00:00Z", 10);
        let earlier = mk_entry("T2", "c", "2024-01-01T09:00:00Z", 10);
        let p = plan(
            "solo",
            "2024-01-01T00:00:00Z",
            vec![dup.clone(), later.clone(), dup.clone(), earlier.clone()],
        );
        let agg = best_of_aggregate(&[&p]);
        assert_eq!(agg.entries.len(), 3, "duplicate collapsed");
        // Chronological: earlier (09:00) → dup (10:00) → later (11:00).
        assert_eq!(agg.entries[0].time_unlocked, "2024-01-01T09:00:00Z");
        assert_eq!(agg.entries[1].time_unlocked, "2024-01-01T10:00:00Z");
        assert_eq!(agg.entries[2].time_unlocked, "2024-01-01T11:00:00Z");
    }

    /// A2: cross-second ordering is CORRECT when ISO strings mix
    /// fractional and non-fractional shapes. Pre-fix: lex sort made
    /// `"T00:00:00.999Z"` sort BEFORE `"T00:00:01Z"` (because `.`
    /// (0x2E) < `0`-`9` but `Z` (0x5A) is greater than any digit —
    /// depends on which chars are compared). Post-fix: `parse_iso`
    /// returns second-precision unix time so both strings map to
    /// the correct seconds and sort numerically.
    ///
    /// We use seconds ONE apart plus a fractional on the earlier
    /// one; lex sort would swap them, parse sort keeps them in
    /// chronological order.
    #[test]
    fn a2_mixed_fractional_timestamps_sort_chronologically() {
        // `T10:00:00.999Z` parses to unix T10:00:00 (fractional
        // stripped). `T10:00:01Z` parses to unix T10:00:01.
        // Chronological: 00 before 01. Lex: `.999Z` may sort before
        // `:01Z` depending on byte class but does NOT universally
        // produce the chronological order. Parse sort does.
        let p1 = plan(
            "p1",
            "2024-01-01T00:00:00Z",
            vec![mk_entry("T1", "a", "2024-01-01T10:00:00.999Z", 10)],
        );
        let p2 = plan(
            "p2",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("T2", "b", "2024-01-01T10:00:01Z", 10)],
        );
        let agg = best_of_aggregate(&[&p1, &p2]);
        assert_eq!(agg.entries.len(), 2);
        // T1 "a" (10:00:00) comes BEFORE T2 "b" (10:00:01).
        assert_eq!(agg.entries[0].achievement_id, "a");
        assert_eq!(agg.entries[1].achievement_id, "b");
    }

    // ============================================================
    // Edge-case audit pass — adversarial inputs that shouldn't crash
    // or produce junk output.
    // ============================================================

    /// Empty plan in multi-input list should be tolerated (no panic,
    /// no wasted winner slot, output == other plans' aggregate).
    #[test]
    fn audit_empty_plan_in_list_is_ignored() {
        let real = plan(
            "real",
            "2024-01-01T00:00:00Z",
            vec![mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10)],
        );
        let empty = plan("empty", "2024-01-01T00:00:00Z", vec![]);
        let agg = best_of_aggregate(&[&real, &empty]);
        assert_eq!(agg.entries.len(), 1);
        assert_eq!(agg.entries[0].achievement_id, "a");
    }

    /// Same plan passed TWICE should behave like a single input.
    /// Tests that pick_better's final plan_idx tiebreak produces a
    /// stable, deterministic winner when candidates are identical.
    #[test]
    fn audit_same_plan_twice_behaves_like_one() {
        let p = plan(
            "dup",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T10:01:00Z", 10),
            ],
        );
        let agg = best_of_aggregate(&[&p, &p]);
        assert_eq!(
            agg.entries.len(),
            2,
            "duplicate plans collapse to single entry set"
        );
    }

    /// All plans have unparseable `built_at` — shouldn't panic; the
    /// tiebreak falls through to plan_idx.
    #[test]
    fn audit_unparseable_built_at_falls_back_to_plan_idx_tiebreak() {
        let a = plan(
            "a",
            "nonsense-date-string",
            vec![mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10)],
        );
        let b = plan(
            "b",
            "also-nonsense",
            vec![mk_entry("T1", "b", "2024-01-01T10:00:00Z", 10)],
        );
        let agg = best_of_aggregate(&[&a, &b]);
        assert_eq!(agg.entries.len(), 1);
        // Plan-idx tiebreak: lower idx (a) wins → entry "a".
        assert_eq!(agg.entries[0].achievement_id, "a");
    }

    /// Entries whose ISO timestamp fails to parse should still end up
    /// in the output (not silently dropped). They sort to the front
    /// (i64::MIN) which is a predictable position.
    #[test]
    fn audit_unparseable_entry_timestamps_are_retained() {
        let p = plan(
            "solo",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "good", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "bad", "not-an-iso-string", 10),
            ],
        );
        let agg = best_of_aggregate(&[&p]);
        assert_eq!(agg.entries.len(), 2, "both entries retained");
        // "bad" parses to i64::MIN, sorts first.
        assert_eq!(agg.entries[0].achievement_id, "bad");
        assert_eq!(agg.entries[1].achievement_id, "good");
    }

    /// Plans with entries but ZERO `title_totals` entries at all.
    /// Fallback loop shouldn't panic; final aggregate should have
    /// NO title_totals populated (nothing to fall back on).
    #[test]
    fn audit_no_title_totals_on_any_plan() {
        let a = plan(
            "a",
            "2024-01-01T00:00:00Z",
            vec![mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10)],
        );
        let b = plan(
            "b",
            "2024-01-02T00:00:00Z",
            vec![mk_entry("T2", "x", "2024-01-02T10:00:00Z", 20)],
        );
        let agg = best_of_aggregate(&[&a, &b]);
        assert_eq!(agg.entries.len(), 2);
        assert!(
            agg.title_totals.is_empty(),
            "no source had totals → output totals empty"
        );
    }

    /// Adversarial: 1000 plans each with 1 entry, all unique titles.
    /// Shouldn't explode memory or time.
    #[test]
    fn audit_many_plans_scales_linearly() {
        let plans: Vec<Plan> = (0..1000)
            .map(|i| {
                plan(
                    &format!("p{i}"),
                    "2024-01-01T00:00:00Z",
                    vec![mk_entry(&format!("T{i}"), "a", "2024-01-01T10:00:00Z", 10)],
                )
            })
            .collect();
        let refs: Vec<&Plan> = plans.iter().collect();
        let agg = best_of_aggregate(&refs);
        assert_eq!(agg.entries.len(), 1000);
    }

    /// Invariant: output entries count == deduped sum of winners'
    /// entry sets. No entry is dropped by the final dedup unless it
    /// was genuinely a duplicate in its winner plan.
    #[test]
    fn audit_output_entry_count_equals_dedupe_of_winners() {
        let a = plan(
            "a",
            "2024-01-01T00:00:00Z",
            vec![
                mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                mk_entry("T1", "b", "2024-01-01T10:01:00Z", 10),
            ],
        );
        let b = plan(
            "b",
            "2024-01-02T00:00:00Z",
            vec![
                mk_entry("T2", "x", "2024-01-02T10:00:00Z", 20),
                mk_entry("T2", "y", "2024-01-02T10:01:00Z", 20),
                mk_entry("T2", "z", "2024-01-02T10:02:00Z", 20),
            ],
        );
        let agg = best_of_aggregate(&[&a, &b]);
        // Winners: T1 = a (2 entries), T2 = b (3 entries). Total 5.
        assert_eq!(agg.entries.len(), 5);
        // total field matches entries.len()
        assert_eq!(agg.total, 5);
    }

    #[test]
    fn title_totals_carried_from_winner_with_fallback() {
        let mut alice_totals = HashMap::new();
        alice_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 0, // junk
                total_achievements: 0,
            },
        );
        let mut bob_totals = HashMap::new();
        bob_totals.insert(
            "T1".into(),
            TitleCompletion {
                current_achievements: 5,
                total_achievements: 5,
            },
        );
        let alice = Plan {
            title_totals: alice_totals,
            ..plan(
                "alice",
                "2024-01-01T00:00:00Z",
                vec![
                    mk_entry("T1", "a", "2024-01-01T10:00:00Z", 10),
                    mk_entry("T1", "b", "2024-01-01T10:10:00Z", 10),
                ],
            )
        };
        let bob = Plan {
            title_totals: bob_totals,
            ..plan(
                "bob",
                "2024-01-02T00:00:00Z",
                vec![mk_entry("T1", "a", "2024-01-02T10:00:00Z", 10)],
            )
        };
        // Alice wins (2 entries vs 1) but her title_totals is junk.
        // The fallback should pick up bob's valid totals.
        let agg = best_of_aggregate(&[&alice, &bob]);
        let tc = agg.title_totals.get("T1").expect("T1 totals present");
        assert_eq!(tc.total_achievements, 5, "fallback to bob's real totals");
    }
}
