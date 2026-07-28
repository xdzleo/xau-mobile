//! Quick check: how much of a real plan does the discontinued
//! blocklist filter out?
//!
//!     cargo run --release --example check_discontinued -- <plan.json>

#![allow(clippy::print_stdout, clippy::uninlined_format_args)]

use std::env;
use std::fs;
use std::process::ExitCode;

use xau_core::plan::discontinued::{filter_plan_remove_discontinued, load_discontinued_set};
use xau_core::plan::Plan;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: check_discontinued <path-to-plan.json>");
        return ExitCode::from(2);
    }
    let raw = match fs::read_to_string(&args[1]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read: {e}");
            return ExitCode::from(1);
        }
    };
    let mut plan: Plan = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse: {e}");
            return ExitCode::from(1);
        }
    };
    let pre_titles: usize = {
        let mut tids = std::collections::HashSet::new();
        for e in &plan.entries {
            tids.insert(e.title_id.clone());
        }
        tids.len()
    };
    let pre_entries = plan.entries.len();

    let blocklist = load_discontinued_set();
    println!("blocklist size: {} titles", blocklist.len());
    println!(
        "plan input:     {} entries · {} titles",
        pre_entries, pre_titles
    );

    let removed = filter_plan_remove_discontinued(&mut plan, &blocklist);
    let post_titles: usize = {
        let mut tids = std::collections::HashSet::new();
        for e in &plan.entries {
            tids.insert(e.title_id.clone());
        }
        tids.len()
    };
    let post_entries = plan.entries.len();
    let blocked_entries: usize = removed.iter().map(|r| r.entry_count).sum();

    println!(
        "after filter:   {} entries · {} titles",
        post_entries, post_titles
    );
    println!(
        "removed:        {} entries · {} titles ({:.1}% of entries)",
        blocked_entries,
        removed.len(),
        100.0 * blocked_entries as f64 / pre_entries.max(1) as f64
    );
    println!("\nTop 20 blocked titles by entry count:");
    let mut by_count = removed.clone();
    by_count.sort_by(|a, b| b.entry_count.cmp(&a.entry_count));
    for r in by_count.iter().take(20) {
        println!(
            "  {:>5} entries · {} ({})",
            r.entry_count, r.game_name, r.title_id
        );
    }
    ExitCode::SUCCESS
}
