//! Hard blocklist of discontinued / unsafe titles.
//!
//! These are titles whose servers are dead, whose achievement
//! pipeline rejects unlock POSTs, or which the user has flagged as
//! "must NEVER unlock anything on this title". The list is fed by:
//!
//! 1. `discontinued_default.txt` — embedded at compile time. The
//!    user-curated blocklist that ships with the binary.
//! 2. `Documents/XAU/discontinued.txt` (optional, layered on top at
//!    runtime). Same line-per-title format. Lets the user add or
//!    remove titles WITHOUT rebuilding XAU.
//!
//! Format: one title's `game_name` per line. Empty lines and lines
//! starting with `#` are ignored. Matching is **case-insensitive
//! after trim** so minor differences in spacing / capitalization
//! between Xbox API responses and the blocklist don't slip through.
//!
//! Wiring: every unlock-producing entry-point (`bit_by_bit::run`,
//! `runner::run_bulk`, `nuclear::run`) calls
//! [`filter_plan_remove_discontinued`] BEFORE building the schedule
//! so blocked titles never reach the unlock gate. Skipped titles
//! are audited to `unlock-safety-skipped.jsonl` with
//! `reason: "discontinued"`.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::plan::Plan;

/// Embedded default blocklist. Generated from the user's curated
/// `jogos DISCONTINUADO.txt`. Re-run `cp` from the source file to
/// refresh.
const DEFAULT_BLOCKLIST: &str = include_str!("../../discontinued_default.txt");

/// Optional path to the user's runtime override file. Returns
/// `None` on platforms where `directories` can't resolve a
/// `Documents` dir.
fn override_path() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(|p| p.to_path_buf()))
        .map(|p| p.join("XAU").join("discontinued.txt"))
}

/// Build the canonical blocklist: embedded defaults + the user's
/// override file (if present) merged into a single `HashSet` of
/// trimmed-lowercase game names. Performs the I/O once; callers
/// should reuse the returned set across many `is_discontinued`
/// calls in the same run.
#[must_use]
pub fn load_discontinued_set() -> HashSet<String> {
    let mut out = HashSet::new();
    ingest_blocklist(DEFAULT_BLOCKLIST, &mut out);
    if let Some(path) = override_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            tracing::info!(
                path = %path.display(),
                "applying user override for discontinued blocklist"
            );
            ingest_blocklist(&content, &mut out);
        }
    }
    tracing::debug!(size = out.len(), "discontinued blocklist loaded");
    out
}

fn ingest_blocklist(text: &str, out: &mut HashSet<String>) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.insert(normalize_name(line));
    }
}

/// Canonicalize a game name for blocklist comparison.
///
/// The blocklist is TrueAchievements-derived text while `game_name`
/// comes verbatim from Xbox's `titleAssociations[].name`, so the two
/// often differ by cosmetic glyphs that a naive `to_ascii_lowercase`
/// comparison treats as distinct — letting a discontinued title slip
/// through the exact-match filter. This folds the **safe** classes
/// (zero over-block risk — they don't change which *game* a name
/// refers to):
/// - curly apostrophes/quotes → ASCII `'` / `"` (e.g. `Hades’ Star`
///   vs `Hades' Star`)
/// - en/em/figure dashes → ASCII `-`
/// - trademark/registered/copyright/service marks (™ ® ©
///   ℠) dropped
/// - non-breaking / narrow / figure spaces → normal space
/// - Unicode-aware lowercasing (handles accented titles)
/// - runs of whitespace collapsed, ends trimmed
///
/// It deliberately does NOT strip parenthetical platform/region tags
/// (`(Windows)`, `(CN)`, …): a regional tag like `(CN)` distinguishes a
/// discontinued SKU from a still-live global one, so blindly dropping it
/// would over-block. Those tagged-only entries remain a known
/// false-negative class better fixed by keying the blocklist on the
/// stable `title_id` (see the module TODO).
#[must_use]
fn normalize_name(s: &str) -> String {
    let mut buf = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2019}' | '\u{2018}' | '\u{02BC}' => buf.push('\''),
            '\u{201C}' | '\u{201D}' => buf.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2012}' | '\u{2015}' => buf.push('-'),
            '\u{2122}' | '\u{00AE}' | '\u{00A9}' | '\u{2120}' => {}
            '\u{00A0}' | '\u{202F}' | '\u{2007}' => buf.push(' '),
            _ => buf.extend(ch.to_lowercase()),
        }
    }
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check whether a single title (by `game_name`) is on the
/// blocklist. Empty / whitespace-only inputs are NOT blocked —
/// safer to let an unknown title through and depend on the safety
/// gate downstream than to block accidentally on metadata gaps.
#[must_use]
pub fn is_discontinued(blocklist: &HashSet<String>, game_name: &str) -> bool {
    let needle = normalize_name(game_name);
    if needle.is_empty() {
        return false;
    }
    blocklist.contains(&needle)
}

/// Removed-title record: emitted by [`filter_plan_remove_discontinued`]
/// so callers can audit / log the skipped titles upstream of the
/// safety gate's per-achievement skip log.
#[derive(Debug, Clone)]
pub struct RemovedTitle {
    pub title_id: String,
    pub game_name: String,
    /// How many entries in the input plan belonged to this title
    /// before it was filtered out.
    pub entry_count: usize,
}

/// Filter the plan **in place**, dropping every entry that belongs
/// to a discontinued title. Returns one `RemovedTitle` record per
/// distinct title that was removed, with the entry count it
/// represented.
///
/// Mutates `plan.entries` and re-syncs `plan.total`. Does NOT touch
/// `title_totals` — leaving it intact means downstream code that
/// looks up titles by id still gets the original metadata; the
/// removal is visible only via the absence of entries.
///
/// **Performance note (BBB-Discontinued)**: a 240k-entry aggregate
/// plan over 9.7k titles takes ~5ms here on a release build. The
/// HashSet is built once at run start; the per-entry lookup is O(1)
/// after a single trim+lowercase.
pub fn filter_plan_remove_discontinued(
    plan: &mut Plan,
    blocklist: &HashSet<String>,
) -> Vec<RemovedTitle> {
    use std::collections::HashMap;
    if plan.entries.is_empty() || blocklist.is_empty() {
        return Vec::new();
    }
    // First pass: collect title_id → (game_name, count) for every
    // discontinued title. Done in one walk so we don't pay the
    // lowercase-cost twice.
    let mut removed: HashMap<String, RemovedTitle> = HashMap::new();
    for e in &plan.entries {
        if is_discontinued(blocklist, &e.game_name) {
            removed
                .entry(e.title_id.clone())
                .and_modify(|r| r.entry_count = r.entry_count.saturating_add(1))
                .or_insert(RemovedTitle {
                    title_id: e.title_id.clone(),
                    game_name: e.game_name.clone(),
                    entry_count: 1,
                });
        }
    }
    if removed.is_empty() {
        return Vec::new();
    }
    // Second pass: drop those titles' entries.
    let removed_ids: HashSet<&str> = removed.keys().map(String::as_str).collect();
    plan.entries
        .retain(|e| !removed_ids.contains(e.title_id.as_str()));
    plan.total = plan.entries.len();
    let mut out: Vec<RemovedTitle> = removed.into_values().collect();
    // Stable, alphabetical-by-name order for deterministic logs.
    out.sort_by(|a, b| a.game_name.cmp(&b.game_name));
    out
}

// ==========================================================================
//  Tests
// ==========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanEntry;

    fn test_set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_ascii_lowercase()).collect()
    }

    fn mk(plan_entries: Vec<(&str, &str)>) -> Plan {
        Plan {
            entries: plan_entries
                .into_iter()
                .map(|(tid, gn)| PlanEntry {
                    title_id: tid.into(),
                    service_config_id: "scid".into(),
                    achievement_id: "1".into(),
                    achievement_name: "ach".into(),
                    game_name: gn.into(),
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    gamerscore: Some(10),
                })
                .collect(),
            total: 0,
            ..Plan::default()
        }
    }

    #[test]
    fn embedded_default_loads_nontrivially() {
        let set = load_discontinued_set();
        // Sanity bound — the user-supplied list ships ~900+ titles.
        assert!(
            set.len() > 100,
            "expected >100 titles in the embedded blocklist, got {}",
            set.len()
        );
        // Spot-check a few canonical entries (case-insensitive).
        assert!(is_discontinued(&set, "FIFA 14"));
        assert!(is_discontinued(&set, "fifa 14")); // case-insensitive
        assert!(is_discontinued(&set, "  FIFA 14  ")); // trim
        assert!(is_discontinued(&set, "Sea of Thieves"));
    }

    #[test]
    fn empty_or_whitespace_game_name_is_not_blocked() {
        let set = test_set(&["FIFA 14"]);
        assert!(!is_discontinued(&set, ""));
        assert!(!is_discontinued(&set, "   "));
        assert!(!is_discontinued(&set, "\t\n"));
    }

    #[test]
    fn unknown_game_is_not_blocked() {
        let set = test_set(&["FIFA 14", "Anthem"]);
        assert!(!is_discontinued(&set, "Halo Infinite"));
        assert!(!is_discontinued(&set, "Forza Horizon 5"));
    }

    #[test]
    fn filter_removes_blocked_titles_and_keeps_others() {
        let mut plan = mk(vec![
            ("100", "FIFA 14"),
            ("100", "FIFA 14"), // same title, multi-entry
            ("200", "Halo Infinite"),
            ("300", "Anthem"),
            ("400", "Forza 5"),
        ]);
        let set = test_set(&["FIFA 14", "Anthem"]);
        let removed = filter_plan_remove_discontinued(&mut plan, &set);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.total, 2);
        let surviving_ids: Vec<_> = plan.entries.iter().map(|e| &e.title_id).collect();
        assert!(surviving_ids.contains(&&"200".to_string()));
        assert!(surviving_ids.contains(&&"400".to_string()));
        // Two distinct titles removed, FIFA contributed 2 entries.
        assert_eq!(removed.len(), 2);
        let fifa = removed.iter().find(|r| r.game_name == "FIFA 14").unwrap();
        assert_eq!(fifa.entry_count, 2);
        let anthem = removed.iter().find(|r| r.game_name == "Anthem").unwrap();
        assert_eq!(anthem.entry_count, 1);
    }

    #[test]
    fn filter_no_op_on_empty_plan_or_empty_blocklist() {
        let mut empty_plan = mk(vec![]);
        let removed = filter_plan_remove_discontinued(&mut empty_plan, &test_set(&["X"]));
        assert!(removed.is_empty());

        let mut plan = mk(vec![("1", "FIFA 14")]);
        let removed = filter_plan_remove_discontinued(&mut plan, &HashSet::new());
        assert!(removed.is_empty());
        assert_eq!(plan.entries.len(), 1);
    }

    #[test]
    fn normalization_matches_cosmetic_glyph_variants() {
        // Blocklist carries the ASCII form; Xbox returns the "fancy"
        // typographic form (or vice-versa). Both must match.
        let mut set = HashSet::new();
        ingest_blocklist(
            "Hades' Star: DARK NEBULA\nAssassin's Creed\nHalo™: Reach",
            &mut set,
        );
        // curly apostrophe + en-dash-ish spacing from the Xbox side
        assert!(is_discontinued(&set, "Hades\u{2019} Star: DARK NEBULA"));
        assert!(is_discontinued(&set, "Assassin\u{2019}s Creed"));
        // trademark symbol on the Xbox side, absent in the list
        assert!(is_discontinued(&set, "Halo: Reach"));
        assert!(is_discontinued(&set, "Halo\u{2122}: Reach"));
        // non-breaking space + doubled spaces collapse
        assert!(is_discontinued(&set, "Assassin's\u{00A0}Creed"));
        // Reverse direction: fancy glyph in the list, ASCII in the lookup
        let mut set2 = HashSet::new();
        ingest_blocklist("Ryse\u{2122}: Son of Rome", &mut set2);
        assert!(is_discontinued(&set2, "Ryse: Son of Rome"));
    }

    #[test]
    fn normalization_does_not_overblock_regional_tags() {
        // Regional/platform parenthetical tags are NOT stripped, so a
        // discontinued regional SKU never blocks the live global title.
        let mut set = HashSet::new();
        ingest_blocklist("Neverwinter (CN)\nNeverwinter (HK)", &mut set);
        assert!(is_discontinued(&set, "Neverwinter (CN)"));
        assert!(!is_discontinued(&set, "Neverwinter")); // global stays allowed
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let mut set = HashSet::new();
        ingest_blocklist(
            "# header comment\nFIFA 14\n\n# inline\nAnthem\n   \n",
            &mut set,
        );
        assert_eq!(set.len(), 2);
        assert!(set.contains("fifa 14"));
        assert!(set.contains("anthem"));
    }
}
