//! Bit-by-bit stress test: load a real-world plan, run the
//! bit-by-bit schedulers (`compute_schedule_for_title` + `compute_tiered_schedule`)
//! across **every** title, and assert the invariants the runner
//! relies on.
//!
//! Why: unit tests cover crafted edge cases. A 240k-entry aggregate
//! plan exercises shapes those tests don't — same-second bursts in
//! legacy 360 titles, near-overflow timestamps from 2013-onwards
//! chains, single-achievement titles, missing `title_totals`, etc.
//! If the schedulers can survive the real corpus, we know they
//! survive every realistic input.
//!
//! Usage:
//!     cargo run --release --example bbb_stress -- <path-to-plan.json>
//!
//! Output: a per-mode summary + a list of any titles that triggered
//! invariant violations (which would be NEW BUGS uncovered by the
//! corpus). The exit code is non-zero on any violation.

#![allow(
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::print_stdout,
    clippy::items_after_statements
)]

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::Duration;

use xau_core::plan::bit_by_bit::{
    compute_schedule_for_title, compute_tiered_schedule, GapMode, ScheduledEntry,
};
use xau_core::plan::{Plan, PlanEntry};

#[derive(Default, Debug, Clone)]
struct Stats {
    titles: usize,
    entries: usize,
    monotonicity_bumps: usize,
    subsec_zero: usize,
    subsec_nonzero: usize,
    same_source_second_bursts: usize,
    largest_burst: usize,
    invariant_violations: Vec<String>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: bbb_stress <path-to-plan.json>");
        return ExitCode::from(2);
    }
    let path = &args[1];
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let t0 = std::time::Instant::now();
    let plan: Plan = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse failed: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "loaded {} entries / {} bytes in {} ms",
        plan.entries.len(),
        raw.len(),
        t0.elapsed().as_millis()
    );

    let by_title = bucket_by_title(&plan.entries);
    eprintln!("unique titles: {}", by_title.len());

    let now_unix = 1_745_000_000; // around 2025-04-18; arbitrary "now" anchor
    let user_unlocked: HashSet<String> = HashSet::new();

    let modes: Vec<(&str, GapMode)> = vec![
        ("Exact", GapMode::Exact),
        (
            "Capped(30 days)",
            GapMode::Capped(Duration::from_secs(30 * 24 * 3600)),
        ),
        ("Capped(0)", GapMode::Capped(Duration::ZERO)),
        ("Scaled(0.1)", GapMode::Scaled(0.1)),
    ];
    let tiered_tiers: Vec<(u64, f64)> = vec![
        (3600, 1.0),
        (7200, 2.0),
        (259_200, 50.0),
        (2_592_000, 1000.0),
        (u64::MAX, 1_000_000.0),
    ];

    let mut had_failures = false;

    for (mode_name, mode) in &modes {
        eprintln!("\n=== MODE: {} ===", mode_name);
        let stats = stress_mode(
            &by_title,
            mode.clone(),
            now_unix,
            &user_unlocked,
            /*tiered=*/ None,
        );
        report_stats(mode_name, &stats);
        if !stats.invariant_violations.is_empty() {
            had_failures = true;
        }
    }

    eprintln!("\n=== MODE: Tiered (PSICOTA presets) ===");
    let stats = stress_mode(
        &by_title,
        GapMode::Tiered(tiered_tiers.clone()),
        now_unix,
        &user_unlocked,
        Some(&tiered_tiers),
    );
    report_stats("Tiered", &stats);
    if !stats.invariant_violations.is_empty() {
        had_failures = true;
    }

    // Source-side burst analysis (independent of any GapMode) — tells
    // us how many same-source-second clusters this plan contains, the
    // single most extreme cluster, and how often sub-second offsets
    // exist at all.
    eprintln!("\n=== SOURCE-SIDE BURST PROFILE ===");
    burst_profile(&by_title);

    if had_failures {
        eprintln!("\n*** INVARIANT VIOLATIONS FOUND — see lists above ***");
        return ExitCode::from(3);
    }
    eprintln!("\nAll modes survived the corpus.");
    ExitCode::SUCCESS
}

fn bucket_by_title(entries: &[PlanEntry]) -> HashMap<String, Vec<PlanEntry>> {
    let mut out: HashMap<String, Vec<PlanEntry>> = HashMap::new();
    for e in entries {
        out.entry(e.title_id.clone()).or_default().push(e.clone());
    }
    out
}

fn stress_mode(
    by_title: &HashMap<String, Vec<PlanEntry>>,
    mode: GapMode,
    now_unix: i64,
    user_unlocked: &HashSet<String>,
    tiered_tiers: Option<&[(u64, f64)]>,
) -> Stats {
    let mut stats = Stats::default();
    let mut violation_count = 0usize;
    const MAX_VIOLATIONS_REPORTED: usize = 20;

    for (title_id, entries) in by_title {
        stats.titles += 1;
        stats.entries += entries.len();

        // Run the right scheduler for this mode.
        let sched: Vec<ScheduledEntry> = if let Some(tiers) = tiered_tiers {
            compute_tiered_schedule(entries, Some(now_unix), user_unlocked, tiers, now_unix)
        } else {
            compute_schedule_for_title(
                entries,
                Some(now_unix),
                user_unlocked,
                mode.clone(),
                now_unix,
            )
        };

        if sched.len() != entries.len() {
            violation_count += 1;
            if violation_count <= MAX_VIOLATIONS_REPORTED {
                stats.invariant_violations.push(format!(
                    "title `{}`: scheduler returned {} entries for input of {}",
                    title_id,
                    sched.len(),
                    entries.len()
                ));
            }
            continue;
        }

        // Per-entry checks + chain monotonicity.
        let mut prev_combined_ms: Option<i64> = None;
        for (i, s) in sched.iter().enumerate() {
            // Sub-second range invariant.
            if s.scheduled_subsec_ms >= 1000 {
                violation_count += 1;
                if violation_count <= MAX_VIOLATIONS_REPORTED {
                    stats.invariant_violations.push(format!(
                        "title `{}` entry #{}: scheduled_subsec_ms = {} (must be < 1000)",
                        title_id, i, s.scheduled_subsec_ms
                    ));
                }
            }
            if s.scheduled_subsec_ms == 0 {
                stats.subsec_zero += 1;
            } else {
                stats.subsec_nonzero += 1;
            }
            let combined = s
                .scheduled_unix
                .saturating_mul(1000)
                .saturating_add(i64::from(s.scheduled_subsec_ms));
            // Strict-monotonic chain (allowing equality only when the
            // current entry is `already_done` — a placeholder).
            if let Some(prev) = prev_combined_ms {
                let ok = if s.already_done {
                    combined >= prev
                } else {
                    combined > prev || (i == 0)
                };
                if !ok {
                    violation_count += 1;
                    if violation_count <= MAX_VIOLATIONS_REPORTED {
                        stats.invariant_violations.push(format!(
                            "title `{}` entry #{}: non-monotonic schedule. \
                             prev_ms={} cur_ms={} delta={} (already_done={})",
                            title_id,
                            i,
                            prev,
                            combined,
                            combined - prev,
                            s.already_done
                        ));
                    }
                }
                // Count monotonicity bumps: cur - prev == 1 ms is the
                // signature of the strict-ordering guard kicking in
                // because compression collapsed a real source gap.
                if combined - prev == 1 {
                    stats.monotonicity_bumps += 1;
                }
            }
            prev_combined_ms = Some(combined);
        }
    }

    // Source-side burst counters (per-title).
    for entries in by_title.values() {
        let mut by_source_sec: HashMap<i64, usize> = HashMap::new();
        for e in entries {
            if let Some(s) = xau_core::plan::runner::parse_iso_public(&e.time_unlocked) {
                *by_source_sec.entry(s).or_default() += 1;
            }
        }
        for &count in by_source_sec.values() {
            if count >= 2 {
                stats.same_source_second_bursts += 1;
                if count > stats.largest_burst {
                    stats.largest_burst = count;
                }
            }
        }
    }

    if violation_count > MAX_VIOLATIONS_REPORTED {
        stats.invariant_violations.push(format!(
            "(... {} more violations not shown)",
            violation_count - MAX_VIOLATIONS_REPORTED
        ));
    }
    stats
}

fn report_stats(mode_name: &str, s: &Stats) {
    eprintln!("  titles processed:           {}", s.titles);
    eprintln!("  entries scheduled:          {}", s.entries);
    eprintln!(
        "  subsec_ms == 0 entries:     {} ({:.1}%)",
        s.subsec_zero,
        100.0 * s.subsec_zero as f64 / (s.entries.max(1) as f64)
    );
    eprintln!(
        "  subsec_ms != 0 entries:     {} ({:.1}%)",
        s.subsec_nonzero,
        100.0 * s.subsec_nonzero as f64 / (s.entries.max(1) as f64)
    );
    eprintln!("  monotonicity bumps (+1ms):  {}", s.monotonicity_bumps);
    eprintln!(
        "  same-source-second bursts:  {} (largest cluster: {} entries)",
        s.same_source_second_bursts, s.largest_burst
    );
    if s.invariant_violations.is_empty() {
        eprintln!("  invariant violations:       0 ✓");
    } else {
        eprintln!(
            "  invariant violations:       {}",
            s.invariant_violations.len()
        );
        for v in &s.invariant_violations {
            eprintln!("    - {}", v);
        }
    }
    eprintln!("(mode `{}` done)", mode_name);
}

fn burst_profile(by_title: &HashMap<String, Vec<PlanEntry>>) {
    // Collect bursts across the whole plan: count of same-source-
    // second clusters, distribution of cluster sizes, sample of the
    // 5 largest clusters.
    let mut total_clusters = 0usize;
    let mut by_size: HashMap<usize, usize> = HashMap::new();
    let mut top: Vec<(usize, String, i64)> = Vec::new();
    for (title_id, entries) in by_title {
        let mut by_source_sec: HashMap<i64, usize> = HashMap::new();
        for e in entries {
            if let Some(s) = xau_core::plan::runner::parse_iso_public(&e.time_unlocked) {
                *by_source_sec.entry(s).or_default() += 1;
            }
        }
        for (sec, count) in by_source_sec {
            if count >= 2 {
                total_clusters += 1;
                *by_size.entry(count).or_default() += 1;
                top.push((count, title_id.clone(), sec));
            }
        }
    }
    top.sort_by(|a, b| b.0.cmp(&a.0));
    eprintln!("total same-source-second clusters: {}", total_clusters);
    let mut sizes: Vec<usize> = by_size.keys().copied().collect();
    sizes.sort_unstable();
    for size in sizes.iter().rev().take(10) {
        eprintln!("  cluster size = {}: {} clusters", size, by_size[size]);
    }
    eprintln!("top 5 largest bursts (count · title · unix_sec):");
    for (count, title_id, sec) in top.iter().take(5) {
        eprintln!("  {} entries at {} for title {}", count, sec, title_id);
    }
}
