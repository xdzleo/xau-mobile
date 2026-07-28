//! Inspect a saved plan.json end-to-end using the EXACT same parser
//! and analysis functions the GUI/CLI use. Verifies that XAU can
//! load arbitrary plans and extract every derived metric without
//! losing data or mis-classifying titles.
//!
//! Usage:
//!     cargo run --release --example inspect_plan -- <path-to-plan.json>
//!
//! This is a diagnostic tool, not production code — the `-D warnings`
//! clippy gate is relaxed so inline println formatting ("{}", x)
//! stays as-is rather than forcing full migration to "{x}" syntax
//! for a throwaway binary.
#![allow(
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::items_after_statements
)]

use std::collections::HashSet;
use std::env;
use std::fs;
use std::process::ExitCode;

use xau_core::plan::{Plan, TitleCompletion};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: inspect_plan <path-to-plan.json>");
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
    eprintln!("file size: {} bytes", raw.len());

    // ==== Step 1: Parse ====
    let t0 = std::time::Instant::now();
    let plan: Plan = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse failed: {e}");
            return ExitCode::from(1);
        }
    };
    let parse_ms = t0.elapsed().as_millis();
    eprintln!("parse: {} ms", parse_ms);

    // ==== Step 2: Top-level fields ====
    println!();
    println!("=== PLAN METADATA ===");
    println!("source_gamertag:   {}", plan.source_gamertag);
    println!("source_xuid:       {}", plan.source_xuid);
    println!("built_at:          {}", plan.built_at);
    println!("total (field):     {}", plan.total);
    println!("entries (actual):  {}", plan.entries.len());
    if plan.total != plan.entries.len() {
        println!(
            "  ⚠ mismatch: `total` field says {}, entries vec has {}",
            plan.total,
            plan.entries.len()
        );
    }
    println!("title_totals:      {} keys", plan.title_totals.len());

    // ==== Step 3: Per-entry integrity ====
    println!();
    println!("=== ENTRY INTEGRITY ===");
    let mut empty_ach_id = 0;
    let mut empty_title_id = 0;
    let mut empty_ach_name = 0;
    let mut empty_game_name = 0;
    let mut empty_scid = 0;
    let mut no_gs = 0;
    let mut no_timestamp = 0;
    let mut invalid_timestamp = 0;
    let mut parseable_timestamps = 0;
    for e in &plan.entries {
        if e.title_id.is_empty() {
            empty_title_id += 1;
        }
        if e.achievement_id.is_empty() {
            empty_ach_id += 1;
        }
        if e.achievement_name.is_empty() {
            empty_ach_name += 1;
        }
        if e.game_name.is_empty() {
            empty_game_name += 1;
        }
        if e.service_config_id.is_empty() {
            empty_scid += 1;
        }
        if e.gamerscore.is_none() {
            no_gs += 1;
        }
        if e.time_unlocked.is_empty() {
            no_timestamp += 1;
        } else if xau_core::plan::runner::parse_iso_public(&e.time_unlocked).is_some() {
            parseable_timestamps += 1;
        } else {
            invalid_timestamp += 1;
        }
    }
    println!("empty title_id:        {}", empty_title_id);
    println!("empty achievement_id:  {}", empty_ach_id);
    println!("empty achievement_name:{}", empty_ach_name);
    println!("empty game_name:       {}", empty_game_name);
    println!("empty service_config_id:{}", empty_scid);
    println!("missing gamerscore:    {}", no_gs);
    println!("empty timestamp:       {}", no_timestamp);
    println!("invalid timestamp:     {}", invalid_timestamp);
    println!("parseable timestamps:  {}", parseable_timestamps);

    // ==== Step 4: Plan-level totals ====
    println!();
    println!("=== TOTALS ===");
    let total_gs = plan.total_gamerscore();
    println!("total gamerscore (plan): {}", total_gs);
    let unique_titles: HashSet<&String> = plan.entries.iter().map(|e| &e.title_id).collect();
    println!("unique titles:           {}", unique_titles.len());
    let unique_keys = plan.unlock_keys().len();
    println!("unique (title, ach) pairs: {}", unique_keys);
    if unique_keys != plan.entries.len() {
        println!(
            "  ⚠ {} duplicate (title_id, achievement_id) pairs present",
            plan.entries.len() - unique_keys
        );
    }

    // ==== Step 5: Chronology ====
    println!();
    println!("=== CHRONOLOGY ===");
    let first = plan.entries.first().map(|e| e.time_unlocked.as_str());
    let last = plan.entries.last().map(|e| e.time_unlocked.as_str());
    println!("first entry time: {}", first.unwrap_or("-"));
    println!("last entry time:  {}", last.unwrap_or("-"));
    let first_ts = first.and_then(xau_core::plan::runner::parse_iso_public);
    let last_ts = last.and_then(xau_core::plan::runner::parse_iso_public);
    if let (Some(f), Some(l)) = (first_ts, last_ts) {
        let days = (l - f) / 86400;
        println!("span:             {} seconds (~{} days)", l - f, days);
    }
    // Are entries strictly chronological?
    let mut monotone = true;
    let mut out_of_order = 0usize;
    for pair in plan.entries.windows(2) {
        let a = xau_core::plan::runner::parse_iso_public(&pair[0].time_unlocked);
        let b = xau_core::plan::runner::parse_iso_public(&pair[1].time_unlocked);
        if let (Some(a), Some(b)) = (a, b) {
            if b < a {
                monotone = false;
                out_of_order += 1;
            }
        }
    }
    println!(
        "chronological order: {} (out-of-order pairs: {})",
        if monotone { "yes" } else { "no" },
        out_of_order
    );

    // ==== Step 6: Per-title span ranking ====
    println!();
    println!("=== SPAN RANKING (shortest → longest, top 15) ===");
    let spans = plan.title_spans_shortest_first();
    println!("total spans (titles with entries): {}", spans.len());
    for (i, sp) in spans.iter().take(15).enumerate() {
        let dur = format_duration(sp.duration_seconds);
        println!(
            "  #{:>2} {:>4} ach · {:>12} · {} ({})",
            i + 1,
            sp.achievement_count,
            dur,
            sp.game_name,
            sp.title_id,
        );
    }

    // ==== Step 7: 100%-completion ranking ====
    println!();
    println!("=== FULLY 100%-COMPLETED TITLES (shortest → longest, top 15) ===");
    let full100 = plan.title_spans_100_percent_shortest_first();
    println!("fully-completed titles in plan: {}", full100.len());
    if full100.is_empty() {
        println!("  (none — either this target hasn't 100%'d any multi-ach title, or");
        println!("   title_totals isn't populated enough to prove it)");
    }
    for (i, sp) in full100.iter().take(15).enumerate() {
        let dur = format_duration(sp.duration_seconds);
        println!(
            "  #{:>2} {:>4} ach · {:>12} · {} ({})",
            i + 1,
            sp.achievement_count,
            dur,
            sp.game_name,
            sp.title_id,
        );
    }

    // ==== Step 8: title_totals coverage ====
    println!();
    println!("=== title_totals COVERAGE ===");
    let titles_with_totals: usize = unique_titles
        .iter()
        .filter(|tid| plan.title_totals.contains_key(tid.as_str()))
        .count();
    let orphan_totals: usize = plan
        .title_totals
        .keys()
        .filter(|k| !unique_titles.contains(k))
        .count();
    println!(
        "entries' titles covered by title_totals: {} / {}",
        titles_with_totals,
        unique_titles.len()
    );
    println!(
        "orphan title_totals keys (not in entries): {}",
        orphan_totals
    );
    let titles_complete: usize = plan
        .title_totals
        .values()
        .filter(|tc: &&TitleCompletion| tc.is_complete())
        .count();
    println!(
        "titlehub says complete (raw is_complete, may be wrong): {}",
        titles_complete
    );

    // ==== Step 9: Top titles by gamerscore and achievement count ====
    println!();
    println!("=== TOP 10 TITLES BY PLAN GAMERSCORE ===");
    use std::collections::HashMap;
    let mut by_title_gs: HashMap<String, (String, i64, usize)> = HashMap::new();
    for e in &plan.entries {
        let row = by_title_gs
            .entry(e.title_id.clone())
            .or_insert_with(|| (e.game_name.clone(), 0, 0));
        row.1 += i64::from(e.gamerscore.unwrap_or(0));
        row.2 += 1;
    }
    let mut by_gs: Vec<_> = by_title_gs.into_iter().collect();
    by_gs.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    for (i, (_, (name, gs, count))) in by_gs.iter().take(10).enumerate() {
        println!("  #{:>2} {:>6}G · {:>3} ach · {}", i + 1, gs, count, name);
    }

    // ==== Step 10: sanity assertions ====
    println!();
    println!("=== SANITY CHECKS ===");
    let mut issues = Vec::new();
    if empty_title_id > 0 || empty_ach_id > 0 {
        issues.push(format!(
            "CRITICAL: {} entries have empty title_id or achievement_id — plan is corrupted",
            empty_title_id + empty_ach_id
        ));
    }
    if !monotone {
        issues.push(format!(
            "warning: plan is not chronologically sorted ({} out-of-order pairs)",
            out_of_order
        ));
    }
    if invalid_timestamp > 0 {
        issues.push(format!(
            "warning: {} entries have unparseable timestamps",
            invalid_timestamp
        ));
    }
    if titles_with_totals < unique_titles.len() {
        let missing = unique_titles.len() - titles_with_totals;
        issues.push(format!(
            "notice: {missing} title(s) in entries have no title_totals — 100%-ranking can't include them"
        ));
    }
    if issues.is_empty() {
        println!("✓ no issues detected");
    } else {
        for i in &issues {
            println!("- {}", i);
        }
    }

    println!();
    println!(
        "DONE. (parse {} ms, {} entries)",
        parse_ms,
        plan.entries.len()
    );
    ExitCode::SUCCESS
}

fn format_duration(sec: i64) -> String {
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m {}s", sec / 60, sec % 60)
    } else if sec < 86400 {
        format!("{}h {}m", sec / 3600, (sec % 3600) / 60)
    } else {
        let d = sec / 86400;
        let h = (sec % 86400) / 3600;
        format!("{d}d {h}h")
    }
}
