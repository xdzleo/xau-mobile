//! Pure aggregator that turns a bit-by-bit run's JSONL log into a
//! human-readable report — the "see what happened after everything is
//! done" primitive. No I/O, no network: give it the lines, get back a
//! [`BbbRunReport`]. Shared by the CLI (`xau bbb-report`) and the GUI.
//!
//! Input lines are the records written by `plan::bit_by_bit`:
//! `bbb_run_start`, `bbb_wire` (one per unlock POST), `bbb_run_end`.
//! Order/drift are computed from the `bbb_wire` records; totals prefer
//! the authoritative `bbb_run_end` summary when present.

use serde::{Deserialize, Serialize};

/// Per-title rollup within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TitleReport {
    pub title_id: String,
    /// Human-readable game name (from the wire records), falling back to
    /// the title_id when older logs didn't carry it.
    pub game: String,
    pub unlocked: usize,
    pub failed: usize,
    /// Times a POST went out out-of-order vs the intended schedule
    /// within this title (must be 0 for a faithful clone).
    pub order_inversions: usize,
}

/// Aggregated, human-presentable summary of one bit-by-bit run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BbbRunReport {
    pub run_id: String,
    pub plan_source: String,
    pub state: String,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    /// End − start, when both markers are present.
    pub elapsed_ms: Option<i64>,
    pub unlocked: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub gamerscore: i32,
    /// Send-latency of the POST vs its scheduled target (how late we
    /// fired). Percentiles over all successful `bbb_wire` events.
    pub drift_p50_ms: i64,
    pub drift_p95_ms: i64,
    pub drift_max_ms: i64,
    /// Total wire-order inversions across every title. **0 means every
    /// achievement landed in the target's chronological order** — the
    /// core "never wrong order" guarantee, proven from the actual log.
    pub order_inversions: usize,
    pub per_title: Vec<TitleReport>,
    /// Number of `bbb_wire` records seen (all statuses).
    pub wire_events: usize,
}

#[derive(Default)]
struct TitleAccum {
    game: String,
    unlocked: usize,
    failed: usize,
    // (scheduled_ms, ts_sent_ms) in file order — for the inversion check.
    fired: Vec<(i64, i64)>,
}

/// Parse a run's JSONL lines into a [`BbbRunReport`]. Unparseable lines
/// and records from OTHER runs (different `run_id`) are ignored — pass
/// `expect_run_id` to filter a shared history file, or `None` to accept
/// a single-run file as-is.
#[must_use]
pub fn parse_bbb_run_report<'a, I>(lines: I, expect_run_id: Option<&str>) -> BbbRunReport
where
    I: IntoIterator<Item = &'a str>,
{
    use std::collections::BTreeMap;
    let mut run_id = expect_run_id.unwrap_or("").to_string();
    let mut plan_source = String::new();
    let mut state = String::from("(unknown)");
    let mut started_ms: Option<i64> = None;
    let mut ended_ms: Option<i64> = None;
    let mut end_unlocked: Option<usize> = None;
    let mut end_gamerscore: Option<i32> = None;
    let mut cancelled = 0usize;
    let mut drifts: Vec<i64> = Vec::new();
    let mut titles: BTreeMap<String, TitleAccum> = BTreeMap::new();
    let mut wire_events = 0usize;

    for raw in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        // Filter by run_id when asked (shared-history case).
        if let Some(want) = expect_run_id {
            if v.get("run_id").and_then(serde_json::Value::as_str) != Some(want) {
                continue;
            }
        }
        match v.get("kind").and_then(serde_json::Value::as_str) {
            Some("bbb_run_start") => {
                if run_id.is_empty() {
                    run_id = v
                        .get("run_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                plan_source = v
                    .get("plan_source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                started_ms = v.get("ts_ms").and_then(serde_json::Value::as_i64);
            }
            Some("bbb_run_end") => {
                ended_ms = v.get("ts_ms").and_then(serde_json::Value::as_i64);
                state = v
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string();
                end_unlocked = v
                    .get("unlocked")
                    .and_then(serde_json::Value::as_u64)
                    .map(|u| u as usize);
                end_gamerscore = v
                    .get("gamerscore")
                    .and_then(serde_json::Value::as_i64)
                    .map(|g| g as i32);
            }
            Some("bbb_wire") => {
                wire_events += 1;
                let status = v
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let title_id = v
                    .get("title_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let acc = titles.entry(title_id).or_default();
                if acc.game.is_empty() {
                    if let Some(g) = v.get("game").and_then(serde_json::Value::as_str) {
                        if !g.is_empty() {
                            acc.game = g.to_string();
                        }
                    }
                }
                match status {
                    "success" => {
                        acc.unlocked += 1;
                        if let (Some(sched), Some(sent)) = (
                            v.get("scheduled_ms").and_then(serde_json::Value::as_i64),
                            v.get("ts_sent_ms").and_then(serde_json::Value::as_i64),
                        ) {
                            acc.fired.push((sched, sent));
                            if let Some(d) = v.get("drift_ms").and_then(serde_json::Value::as_i64) {
                                drifts.push(d);
                            }
                        }
                    }
                    "failed" => acc.failed += 1,
                    "cancelled" => cancelled += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Per-title order inversions: entries fire in schedule order, so
    // ts_sent_ms should be monotonic when read in ascending scheduled_ms
    // order. Any decrease is an out-of-order POST.
    let mut total_inversions = 0usize;
    let mut per_title: Vec<TitleReport> = titles
        .into_iter()
        .map(|(title_id, mut acc)| {
            acc.fired.sort_by_key(|(sched, _)| *sched);
            let mut inv = 0usize;
            for w in acc.fired.windows(2) {
                if w[1].1 < w[0].1 {
                    inv += 1;
                }
            }
            total_inversions += inv;
            let game = if acc.game.is_empty() {
                title_id.clone()
            } else {
                acc.game
            };
            TitleReport {
                title_id,
                game,
                unlocked: acc.unlocked,
                failed: acc.failed,
                order_inversions: inv,
            }
        })
        .collect();
    per_title.sort_by(|a, b| {
        b.unlocked
            .cmp(&a.unlocked)
            .then(a.title_id.cmp(&b.title_id))
    });

    let unlocked = end_unlocked.unwrap_or_else(|| per_title.iter().map(|t| t.unlocked).sum());
    let failed = per_title.iter().map(|t| t.failed).sum();
    let (p50, p95, dmax) = drift_percentiles(&mut drifts);

    BbbRunReport {
        run_id,
        plan_source,
        state,
        started_ms,
        ended_ms,
        elapsed_ms: match (started_ms, ended_ms) {
            (Some(s), Some(e)) => Some(e.saturating_sub(s)),
            _ => None,
        },
        unlocked,
        failed,
        cancelled,
        gamerscore: end_gamerscore.unwrap_or(0),
        drift_p50_ms: p50,
        drift_p95_ms: p95,
        drift_max_ms: dmax,
        order_inversions: total_inversions,
        per_title,
        wire_events,
    }
}

/// One achievement a run intended to unlock (a successful `bbb_wire`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntendedEntry {
    pub title_id: String,
    pub game: String,
    pub ach_id: String,
}

/// Result of verifying a finished run against the user's live profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifyReport {
    pub run_id: String,
    /// Achievements the run reported as successfully unlocked.
    pub intended: usize,
    /// Of those, how many are actually present on the profile now.
    pub matched: usize,
    /// Intended unlocks that are NOT on the profile (didn't stick).
    pub missing: Vec<IntendedEntry>,
}

/// Extract the set of achievements a run's log says it unlocked
/// (`bbb_wire` records with `status == "success"`), preserving game
/// names. Deduped by (title_id, ach_id).
#[must_use]
pub fn intended_set_from_lines<'a, I>(lines: I) -> Vec<IntendedEntry>
where
    I: IntoIterator<Item = &'a str>,
{
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for raw in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if v.get("kind").and_then(serde_json::Value::as_str) != Some("bbb_wire") {
            continue;
        }
        if v.get("status").and_then(serde_json::Value::as_str) != Some("success") {
            continue;
        }
        let title_id = v
            .get("title_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let ach_id = v
            .get("ach_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if title_id.is_empty() || ach_id.is_empty() {
            continue;
        }
        if seen.insert((title_id.to_string(), ach_id.to_string())) {
            out.push(IntendedEntry {
                title_id: title_id.to_string(),
                game: v
                    .get("game")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(title_id)
                    .to_string(),
                ach_id: ach_id.to_string(),
            });
        }
    }
    out
}

/// Diff a run's intended unlocks against the profile's actual unlocks.
/// `is_unlocked(title_id, ach_id)` returns whether the achievement is
/// present on the profile now (from a fresh scan). Pure.
#[must_use]
pub fn verify_intended(
    run_id: &str,
    intended: &[IntendedEntry],
    is_unlocked: impl Fn(&str, &str) -> bool,
) -> VerifyReport {
    let mut matched = 0usize;
    let mut missing = Vec::new();
    for e in intended {
        if is_unlocked(&e.title_id, &e.ach_id) {
            matched += 1;
        } else {
            missing.push(e.clone());
        }
    }
    VerifyReport {
        run_id: run_id.to_string(),
        intended: intended.len(),
        matched,
        missing,
    }
}

/// Path to the most recently modified per-run log in `bbb-runs/`, or
/// `None` if the directory is missing/empty.
#[must_use]
pub fn latest_run_log() -> Option<std::path::PathBuf> {
    let dir = crate::plan::runner::bbb_runs_dir()?;
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
        if let Some(mt) = mtime {
            if newest.as_ref().map_or(true, |(best, _)| mt > *best) {
                newest = Some((mt, path));
            }
        }
    }
    newest.map(|(_, p)| p)
}

/// Read a per-run log file and parse it into a report. The file is a
/// single run's log, so no run_id filter is applied.
///
/// # Errors
/// Propagates the file read error.
pub fn load_run_report(path: &std::path::Path) -> std::io::Result<BbbRunReport> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse_bbb_run_report(text.lines(), None))
}

/// Load the report for a specific `run_id` from its dedicated log file,
/// or (when `run_id` is `None`/empty) the most recent run. Returns the
/// report alongside the log-file path so the caller can offer "open log".
///
/// # Errors
/// Errors when the directory can't be resolved or the file is missing.
pub fn load_run_report_for(
    run_id: Option<&str>,
) -> std::io::Result<(BbbRunReport, std::path::PathBuf)> {
    let path = match run_id.filter(|id| !id.is_empty()) {
        Some(id) => crate::plan::runner::bbb_run_log_path(id)
            .ok_or_else(|| std::io::Error::other("cannot resolve XAU documents dir"))?,
        None => latest_run_log()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no runs found"))?,
    };
    let report = load_run_report(&path)?;
    Ok((report, path))
}

/// (p50, p95, max) of `drifts` in ms. Sorts in place. Zeros on empty.
fn drift_percentiles(drifts: &mut [i64]) -> (i64, i64, i64) {
    if drifts.is_empty() {
        return (0, 0, 0);
    }
    drifts.sort_unstable();
    let pct = |p: f64| -> i64 {
        // Nearest-rank; clamp index into range.
        let idx = ((p * (drifts.len() as f64)).ceil() as usize).saturating_sub(1);
        drifts[idx.min(drifts.len() - 1)]
    };
    (pct(0.50), pct(0.95), *drifts.last().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_run() {
        let lines = vec![
            r#"{"kind":"bbb_run_start","run_id":"r1","plan_source":"target","ts_ms":1000,"plan_total":50}"#,
            r#"{"kind":"bbb_wire","run_id":"r1","title_id":"t1","ach_id":"a","status":"success","scheduled_ms":1000,"ts_sent_ms":1100,"drift_ms":100}"#,
            r#"{"kind":"bbb_wire","run_id":"r1","title_id":"t1","ach_id":"b","status":"success","scheduled_ms":2000,"ts_sent_ms":2300,"drift_ms":300}"#,
            r#"{"kind":"bbb_wire","run_id":"r1","title_id":"t2","ach_id":"c","status":"failed","scheduled_ms":1500,"ts_sent_ms":1600,"drift_ms":100}"#,
            r#"{"kind":"bbb_run_end","run_id":"r1","ts_ms":5000,"state":"Done","unlocked":2,"gamerscore":30,"errored_titles":0}"#,
        ];
        let r = parse_bbb_run_report(lines.iter().copied(), None);
        assert_eq!(r.run_id, "r1");
        assert_eq!(r.plan_source, "target");
        assert_eq!(r.state, "Done");
        assert_eq!(r.unlocked, 2);
        assert_eq!(r.failed, 1);
        assert_eq!(r.gamerscore, 30);
        assert_eq!(r.elapsed_ms, Some(4000));
        assert_eq!(r.order_inversions, 0, "clean run has no inversions");
        assert_eq!(r.drift_max_ms, 300);
        assert_eq!(r.wire_events, 3);
        assert_eq!(r.per_title.len(), 2);
    }

    #[test]
    fn detects_out_of_order_post() {
        // Within t1: the later-scheduled entry (2000) was SENT (1500)
        // before the earlier-scheduled one (1000, sent 1800) → 1 inversion.
        let lines = vec![
            r#"{"kind":"bbb_wire","run_id":"r","title_id":"t1","ach_id":"a","status":"success","scheduled_ms":1000,"ts_sent_ms":1800,"drift_ms":800}"#,
            r#"{"kind":"bbb_wire","run_id":"r","title_id":"t1","ach_id":"b","status":"success","scheduled_ms":2000,"ts_sent_ms":1500,"drift_ms":-500}"#,
        ];
        let r = parse_bbb_run_report(lines.iter().copied(), None);
        assert_eq!(r.order_inversions, 1, "out-of-order send is flagged");
    }

    #[test]
    fn filters_by_run_id_in_shared_history() {
        let lines = vec![
            r#"{"kind":"bbb_wire","run_id":"other","title_id":"t","ach_id":"x","status":"success","scheduled_ms":1,"ts_sent_ms":1,"drift_ms":0}"#,
            r#"{"kind":"bbb_wire","run_id":"mine","title_id":"t","ach_id":"y","status":"success","scheduled_ms":1,"ts_sent_ms":1,"drift_ms":0}"#,
        ];
        let r = parse_bbb_run_report(lines.iter().copied(), Some("mine"));
        assert_eq!(r.unlocked, 1, "only 'mine' counted");
        assert_eq!(r.wire_events, 1);
    }

    #[test]
    fn verify_intended_matches_and_reports_missing() {
        use std::collections::HashSet;
        let lines = vec![
            r#"{"kind":"bbb_wire","run_id":"r","title_id":"t1","game":"Game One","ach_id":"a","status":"success","scheduled_ms":1,"ts_sent_ms":1}"#,
            r#"{"kind":"bbb_wire","run_id":"r","title_id":"t1","game":"Game One","ach_id":"b","status":"success","scheduled_ms":2,"ts_sent_ms":2}"#,
            r#"{"kind":"bbb_wire","run_id":"r","title_id":"t2","game":"Game Two","ach_id":"x","status":"failed"}"#,
        ];
        let intended = intended_set_from_lines(lines.iter().copied());
        assert_eq!(intended.len(), 2, "only successes count");
        assert_eq!(intended[0].game, "Game One");
        // Profile has t1/a but NOT t1/b → 1 matched, 1 missing.
        let present: HashSet<(&str, &str)> = [("t1", "a")].into_iter().collect();
        let report = verify_intended("r", &intended, |t, a| present.contains(&(t, a)));
        assert_eq!(report.intended, 2);
        assert_eq!(report.matched, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].ach_id, "b");
        assert_eq!(report.missing[0].game, "Game One");
    }

    #[test]
    fn empty_log_is_safe() {
        let r = parse_bbb_run_report(std::iter::empty(), None);
        assert_eq!(r.unlocked, 0);
        assert_eq!(r.drift_p50_ms, 0);
        assert_eq!(r.order_inversions, 0);
    }
}
