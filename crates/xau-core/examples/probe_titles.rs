//! Probe every title in a plan with TWO unlock POSTs to discover which
//! titles are unlockable via the modern Xbox Live `/achievements/{scid}/update`
//! endpoint and which are not. Parallelised + re-confirmation pass for
//! non-OK titles so a one-off transient failure doesn't get listed as
//! permanently broken.
//!
//! For each unique `title_id` in the plan we pick the first TWO
//! achievements and POST each one as its own single-id chunk. The title's
//! category is the **better** of the two (OK > EVENT_BASED > PERMANENT > …).
//! After the main pass, every title NOT classified as OK is retried ONCE
//! more (single ach again) to filter out one-off blips. Titles that fail
//! BOTH the main pass and the re-confirmation are treated as
//! definitively non-unlockable in the report.
//!
//! Output: `probe-titles-report.md` in the cwd. Sections are sorted from
//! the most actionable category to the least.
//!
//! Usage:
//!     cargo run --release --example probe_titles -- <plan.json>
//!     cargo run --release --example probe_titles -- <plan> --max-titles 50
//!     cargo run --release --example probe_titles -- <plan> --xauth "XBL3.0 ..."
//!     cargo run --release --example probe_titles -- <plan> --concurrency 8
//!
//! Env: `XAU_TOKEN` is honored if `--xauth` is omitted.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use xau_core::error::Error;
use xau_core::plan::{Plan, PlanEntry};
use xau_core::xbox::client::{XboxClient, XboxClientOptions};

#[derive(Debug, Clone)]
struct ProbeResult {
    title_id: String,
    game_name: String,
    /// First achievement that gave us the most-permissive answer.
    achievement_id_winner: String,
    /// Joined detail of the two attempts.
    detail: String,
    category: Category,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Category {
    /// Best — at least ONE of the two attempts landed.
    Ok,
    /// Both attempts came back as event-based (code 49).
    EventBased,
    /// Both attempts returned a permanent error other than event-based.
    Permanent,
    /// Both attempts returned 401/403.
    Auth,
    /// Both attempts were transient (429/5xx) — ambiguous.
    Transient,
    /// Both attempts were network-level errors.
    Network,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::EventBased => "EVENT_BASED",
            Self::Permanent => "PERMANENT",
            Self::Auth => "AUTH",
            Self::Transient => "TRANSIENT",
            Self::Network => "NETWORK",
        }
    }
}

#[derive(Debug, Clone)]
struct Outcome {
    category: Category,
    detail: String,
}

fn classify_error(e: &Error) -> Outcome {
    let s = e.to_string();
    if let Error::HttpStatus { status, body } = e {
        let code = status.as_u16();
        if code == 400 && (body.contains(r#""code":49"#) || body.contains(r#""code": 49"#)) {
            return Outcome {
                category: Category::EventBased,
                detail: "HTTP 400 code:49".into(),
            };
        }
        if code == 401 || code == 403 {
            return Outcome {
                category: Category::Auth,
                detail: format!("HTTP {code}"),
            };
        }
        if code == 429 || (500..600).contains(&code) {
            return Outcome {
                category: Category::Transient,
                detail: format!("HTTP {code}"),
            };
        }
        if (400..500).contains(&code) {
            let snippet = body.chars().take(80).collect::<String>();
            return Outcome {
                category: Category::Permanent,
                detail: format!("HTTP {code} {snippet}"),
            };
        }
    }
    if s.contains("network") || s.contains("connection") || s.contains("timed out") {
        return Outcome {
            category: Category::Network,
            detail: s.chars().take(120).collect(),
        };
    }
    Outcome {
        category: Category::Permanent,
        detail: s.chars().take(120).collect(),
    }
}

/// Single POST and classify.
async fn probe_one(
    client: &XboxClient,
    xuid: &str,
    scid: &str,
    title_id: &str,
    ach_id: &str,
) -> Outcome {
    if scid.is_empty() {
        return Outcome {
            category: Category::Permanent,
            detail: "missing service_config_id in plan".into(),
        };
    }
    let res = client
        .unlock_title_based_achievements_detailed(
            scid,
            title_id,
            xuid,
            std::slice::from_ref(&ach_id.to_string()),
            1,
        )
        .await;
    match res {
        Ok(summary) => match summary.last_error {
            None => Outcome {
                category: Category::Ok,
                detail: "200/304".into(),
            },
            Some(err) => classify_error(&err),
        },
        Err(err) => classify_error(&err),
    }
}

/// Probe a single title with up to 2 achievements. Returns the BEST
/// outcome (lowest enum value = best).
async fn probe_title(
    client: Arc<XboxClient>,
    xuid: String,
    title_id: String,
    game_name: String,
    achs: Vec<(String, String)>, // (scid, ach_id) pairs, up to 2
) -> ProbeResult {
    let mut best: Option<(Category, String, String)> = None; // (cat, detail, ach_id_winner)
    for (scid, ach_id) in &achs {
        let out = probe_one(&client, &xuid, scid, &title_id, ach_id).await;
        let candidate = (out.category, out.detail.clone(), ach_id.clone());
        match &best {
            None => best = Some(candidate),
            Some((cur_cat, _, _)) if candidate.0 < *cur_cat => best = Some(candidate),
            _ => {}
        }
    }
    let (cat, detail, ach_id_winner) = best.unwrap_or((
        Category::Permanent,
        "no achievements probed".into(),
        String::new(),
    ));
    ProbeResult {
        title_id,
        game_name,
        achievement_id_winner: ach_id_winner,
        detail,
        category: cat,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: probe_titles <plan.json> [--max-titles N] [--xauth TOKEN] [--concurrency N]"
        );
        return ExitCode::from(2);
    }
    let plan_path = &args[1];
    let mut max_titles: Option<usize> = None;
    let mut xauth_arg: Option<String> = None;
    let mut concurrency: usize = 6;
    // BUG HUNTER PROBE-PRE2018: optional cutoff — only probe titles
    // whose EARLIEST entry in the plan is before this ISO date. Used
    // to focus on legacy / pre-Windows-store titles that are the
    // most likely candidates for "retired and un-unlockable" status.
    // Format `YYYY-MM-DD`. Default: no filter.
    let mut before_date: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--max-titles" if i + 1 < args.len() => {
                max_titles = args[i + 1].parse().ok();
                i += 2;
            }
            "--xauth" if i + 1 < args.len() => {
                xauth_arg = Some(args[i + 1].clone());
                i += 2;
            }
            "--concurrency" if i + 1 < args.len() => {
                concurrency = args[i + 1].parse().unwrap_or(6).clamp(1, 16);
                i += 2;
            }
            "--before" if i + 1 < args.len() => {
                before_date = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }

    // === Load plan ===
    let raw = match fs::read_to_string(plan_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {plan_path}: {e}");
            return ExitCode::from(1);
        }
    };
    let plan: Plan = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse {plan_path}: {e}");
            return ExitCode::from(1);
        }
    };

    // === Build per-title ach list, take FIRST TWO + earliest date ===
    let mut by_title: HashMap<String, (String, Vec<(String, String)>, Option<String>)> =
        HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for e in &plan.entries {
        let entry = by_title
            .entry(e.title_id.clone())
            .or_insert_with(|| (e.game_name.clone(), Vec::new(), None));
        if entry.1.len() < 2 {
            entry
                .1
                .push((e.service_config_id.clone(), e.achievement_id.clone()));
        }
        // Track the EARLIEST `time_unlocked` for this title (lexicographic
        // works for ISO 8601 dates).
        match entry.2.as_ref() {
            None => entry.2 = Some(e.time_unlocked.clone()),
            Some(cur) if &e.time_unlocked < cur => entry.2 = Some(e.time_unlocked.clone()),
            _ => {}
        }
        if !order.contains(&e.title_id) {
            order.push(e.title_id.clone());
        }
    }
    // Apply --before cutoff: keep only titles whose earliest entry is
    // strictly before the date.
    if let Some(cutoff) = before_date.as_deref() {
        order.retain(|tid| {
            by_title
                .get(tid)
                .and_then(|(_, _, t)| t.as_deref())
                .is_some_and(|earliest| earliest < cutoff)
        });
        eprintln!(
            "filtered to {} titles with earliest entry before {}",
            order.len(),
            cutoff
        );
    }
    if let Some(n) = max_titles {
        order.truncate(n);
    }
    eprintln!(
        "loaded {} entries / {} titles · probing {} titles × 2 ach · concurrency={}",
        plan.entries.len(),
        by_title.len(),
        order.len(),
        concurrency,
    );

    // === Auth ===
    let xauth = xauth_arg
        .or_else(|| env::var("XAU_TOKEN").ok())
        .or_else(|| {
            directories::UserDirs::new()
                .and_then(|u| u.document_dir().map(|p| p.to_path_buf()))
                .and_then(|d| fs::read_to_string(d.join("XAU").join("xauth.txt")).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let Some(xauth) = xauth else {
        eprintln!("no XAUTH provided");
        return ExitCode::from(1);
    };

    let xuid = match xau_core::xbox::validate_token(&xauth).await {
        Ok(Some(p)) => p.xuid.unwrap_or_default(),
        Ok(None) => {
            eprintln!("validate_token: token rejected — run `xau get-token` to refresh");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("validate_token: {e}");
            return ExitCode::from(1);
        }
    };
    if xuid.is_empty() {
        eprintln!("validate_token returned no xuid");
        return ExitCode::from(1);
    }
    let client = Arc::new(
        match XboxClient::new(XboxClientOptions {
            xauth,
            ..Default::default()
        }) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("XboxClient: {e}");
                return ExitCode::from(1);
            }
        },
    );
    eprintln!("authenticated as xuid={xuid}");

    // === MAIN PASS — fan out N titles in parallel ===
    let started_at = Instant::now();
    let total_titles = order.len();
    let done_counter = Arc::new(AtomicUsize::new(0));
    let mut joins: tokio::task::JoinSet<ProbeResult> = tokio::task::JoinSet::new();
    let mut iter = order.into_iter();
    // Prime the pool.
    for _ in 0..concurrency {
        if let Some(tid) = iter.next() {
            let (game_name, achs, _t) = by_title.get(&tid).cloned().unwrap_or_default();
            let c = client.clone();
            let x = xuid.clone();
            let counter = done_counter.clone();
            joins.spawn(async move {
                let r = probe_title(c, x, tid, game_name, achs).await;
                counter.fetch_add(1, Ordering::Relaxed);
                r
            });
        }
    }
    let mut results: Vec<ProbeResult> = Vec::with_capacity(total_titles);
    while let Some(j) = joins.join_next().await {
        let r = match j {
            Ok(r) => r,
            Err(e) => {
                eprintln!("title task panicked: {e}");
                continue;
            }
        };
        results.push(r);
        let done = done_counter.load(Ordering::Relaxed);
        if done % 50 == 0 || done == total_titles {
            let elapsed = started_at.elapsed().as_secs();
            eprintln!(
                "  [{:3}%] {}/{} titles · {} elapsed · last: {} = {}",
                done * 100 / total_titles.max(1),
                done,
                total_titles,
                fmt_secs(elapsed),
                results.last().map_or("", |r| r.game_name.as_str()),
                results.last().map_or("?", |r| r.category.label())
            );
        }
        // Refill the pool.
        if let Some(tid) = iter.next() {
            let (game_name, achs, _t) = by_title.get(&tid).cloned().unwrap_or_default();
            let c = client.clone();
            let x = xuid.clone();
            let counter = done_counter.clone();
            joins.spawn(async move {
                let r = probe_title(c, x, tid, game_name, achs).await;
                counter.fetch_add(1, Ordering::Relaxed);
                r
            });
        }
    }
    eprintln!(
        "\nmain pass complete in {}",
        fmt_secs(started_at.elapsed().as_secs())
    );

    // === RE-CONFIRMATION PASS — every non-OK title gets ONE more shot. ===
    let to_retry: Vec<String> = results
        .iter()
        .filter(|r| r.category != Category::Ok)
        .map(|r| r.title_id.clone())
        .collect();
    eprintln!(
        "re-confirming {} non-OK titles (one more POST each)…",
        to_retry.len()
    );
    let retry_started = Instant::now();
    let mut retry_results: HashMap<String, Outcome> = HashMap::new();
    let mut retry_joins: tokio::task::JoinSet<(String, Outcome)> = tokio::task::JoinSet::new();
    let mut retry_iter = to_retry.into_iter();
    for _ in 0..concurrency {
        if let Some(tid) = retry_iter.next() {
            let (_gn, achs, _t) = by_title.get(&tid).cloned().unwrap_or_default();
            let (scid, ach_id) = achs
                .first()
                .cloned()
                .unwrap_or_else(|| (String::new(), String::new()));
            let c = client.clone();
            let x = xuid.clone();
            retry_joins.spawn(async move {
                let o = probe_one(&c, &x, &scid, &tid, &ach_id).await;
                (tid, o)
            });
        }
    }
    let retry_total = retry_results.capacity().max(1);
    let mut retry_done = 0;
    while let Some(j) = retry_joins.join_next().await {
        let Ok((tid, out)) = j else { continue };
        retry_results.insert(tid, out);
        retry_done += 1;
        if retry_done % 50 == 0 {
            eprintln!(
                "  retry {}/{} · {} elapsed",
                retry_done,
                retry_total,
                fmt_secs(retry_started.elapsed().as_secs())
            );
        }
        if let Some(tid) = retry_iter.next() {
            let (_gn, achs, _t) = by_title.get(&tid).cloned().unwrap_or_default();
            let (scid, ach_id) = achs
                .first()
                .cloned()
                .unwrap_or_else(|| (String::new(), String::new()));
            let c = client.clone();
            let x = xuid.clone();
            retry_joins.spawn(async move {
                let o = probe_one(&c, &x, &scid, &tid, &ach_id).await;
                (tid, o)
            });
        }
    }

    // === Apply retry results — promote to OK if retry succeeded; else
    //     keep the worse of the two for transparency. ===
    for r in &mut results {
        if r.category == Category::Ok {
            continue;
        }
        if let Some(retry) = retry_results.get(&r.title_id) {
            if retry.category == Category::Ok {
                r.category = Category::Ok;
                r.detail = format!("{} · retry={}", r.detail, retry.detail);
            } else {
                // Pick the better of the two non-OK outcomes (lower enum
                // value = "better-known" answer). Confirmation kept.
                r.detail = format!("{} · retry={}", r.detail, retry.detail);
                if retry.category < r.category {
                    r.category = retry.category;
                }
            }
        }
    }
    eprintln!(
        "re-confirmation complete in {}",
        fmt_secs(retry_started.elapsed().as_secs())
    );

    // === Build markdown report ===
    let total_elapsed = started_at.elapsed().as_secs();
    let mut by_cat: HashMap<Category, Vec<&ProbeResult>> = HashMap::new();
    for r in &results {
        by_cat.entry(r.category).or_default().push(r);
    }
    let total = results.len();
    let mut md = String::new();
    md.push_str("# Title-unlock probe report\n\n");
    md.push_str(&format!("**Plan:** `{plan_path}`\n"));
    md.push_str(&format!("**Probed XUID:** `{xuid}`\n"));
    md.push_str(&format!("**Probed titles:** {total}\n"));
    md.push_str("**Per title:** 2 achievements probed, single retry on non-OK\n");
    md.push_str(&format!("**Wall-clock:** {}\n\n", fmt_secs(total_elapsed)));
    md.push_str("## Summary\n\n");
    md.push_str("| Category | Count | % | Description |\n");
    md.push_str("|---|---|---|---|\n");
    for cat in [
        Category::Ok,
        Category::EventBased,
        Category::Permanent,
        Category::Auth,
        Category::Transient,
        Category::Network,
    ] {
        let n = by_cat.get(&cat).map_or(0, Vec::len);
        let pct = if total > 0 { n * 100 / total } else { 0 };
        let desc = match cat {
            Category::Ok => "Unlock POST accepted (or 304 idempotent — already unlocked)",
            Category::EventBased => "Modern event-based — needs OneCollector + events bundle",
            Category::Permanent => {
                "Cannot unlock via this endpoint (retired / gone / no permission)"
            }
            Category::Auth => "401 / 403 — token issue",
            Category::Transient => "Rate-limit / 5xx — confirmed flaky after retry",
            Category::Network => "Network / connection error after retry",
        };
        md.push_str(&format!(
            "| **{}** | {} | {}% | {} |\n",
            cat.label(),
            n,
            pct,
            desc
        ));
    }
    for cat in [
        Category::Permanent,
        Category::EventBased,
        Category::Auth,
        Category::Network,
        Category::Transient,
        Category::Ok,
    ] {
        let entries = match by_cat.get(&cat) {
            Some(v) => v,
            None => continue,
        };
        if entries.is_empty() {
            continue;
        }
        md.push_str(&format!("\n## {} ({})\n\n", cat.label(), entries.len()));
        md.push_str("| title_id | game_name | first_ach | detail |\n");
        md.push_str("|---|---|---|---|\n");
        let mut sorted: Vec<&&ProbeResult> = entries.iter().collect();
        sorted.sort_by(|a, b| a.game_name.cmp(&b.game_name));
        for r in sorted {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                r.title_id,
                r.game_name.replace('|', "\\|"),
                r.achievement_id_winner,
                r.detail
                    .replace('|', "\\|")
                    .chars()
                    .take(160)
                    .collect::<String>()
            ));
        }
    }
    let out_path = "probe-titles-report.md";
    if let Err(e) = fs::write(out_path, md) {
        eprintln!("write {out_path}: {e}");
        return ExitCode::from(1);
    }
    eprintln!("\nwrote {out_path}");
    println!("{out_path}");
    ExitCode::SUCCESS
}

fn fmt_secs(s: u64) -> String {
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h {m}m {sec}s")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}
