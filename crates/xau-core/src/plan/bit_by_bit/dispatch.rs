//! Run orchestration: `start_bit_by_bit`, the `run` dispatch loop (sequential/concurrent), outcome bookkeeping, terminal reconcile.

use super::*;

/// Per-title scheduling state. Computed once during the
/// `Resolving` phase and then used to drive the unlock phase.
#[derive(Debug, Clone)]
pub(super) struct TitlePlan {
    pub(super) title_id: String,
    pub(super) game_name: String,
    pub(super) scid: String,
    /// Source entries for this title, in chronological order.
    pub(super) source: Vec<PlanEntry>,
    /// IDs the user ALREADY has on this title.
    pub(super) user_unlocked: std::collections::HashSet<String>,
    /// The timestamp of the user's most recent unlock on this title,
    /// or `None` if they have no unlocks here yet.
    pub(super) user_last_unix: Option<i64>,
    /// The timestamp of the user's FIRST unlock on this title (the
    /// "shadow anchor" for `GapMode::Tiered`). Stable across runs —
    /// re-reading from Xbox always gives the same first-unlock
    /// timestamp, which is what makes the scheduler resumable
    /// without any local state file.
    pub(super) user_first_unix: Option<i64>,
}

/// Kick off a bit-by-bit replay task. Returns immediately; work
/// happens on a detached tokio task.
pub fn start_bit_by_bit(
    client: XboxClient,
    mut plan: Plan,
    config: BitByBitConfig,
) -> Result<BitByBitHandle> {
    if plan.is_empty() {
        return Err(Error::invalid_argument("plan is empty"));
    }
    if config.my_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("my_xuid is required"));
    }
    // BUG HUNTER BBB-Discontinued: drop every entry that belongs
    // to a user-blocklisted title BEFORE the runner spends any
    // cycle building schedules / hitting Xbox. The blocklist is
    // shipped as `discontinued_default.txt` and may be extended at
    // runtime via `Documents/XAU/discontinued.txt`. Skipped titles
    // are audited to `unlock-safety-skipped.jsonl` with
    // `reason: "discontinued"` so the user can see exactly which
    // ones were filtered. The plan can become empty after filtering;
    // surface that with a clear error so the GUI/CLI doesn't
    // silently report "Done. 0 unlocks" with no explanation.
    let blocklist = crate::plan::discontinued::load_discontinued_set();
    let removed = crate::plan::discontinued::filter_plan_remove_discontinued(&mut plan, &blocklist);
    if !removed.is_empty() {
        let total_skipped: usize = removed.iter().map(|r| r.entry_count).sum();
        tracing::warn!(
            titles = removed.len(),
            entries = total_skipped,
            "bit-by-bit: filtered discontinued titles before scheduling"
        );
        for r in &removed {
            tracing::info!(
                title_id = %r.title_id,
                game_name = %r.game_name,
                entries = r.entry_count,
                "discontinued title skipped (BBB-Discontinued)"
            );
            crate::plan::safety::append_skip_audit(&crate::plan::safety::SkipAuditEntry {
                iso_timestamp: crate::plan::runner::iso_now_simple_public(),
                reason: format!(
                    "discontinued blocklist match — filtered {} entries upstream of unlock gate",
                    r.entry_count
                ),
                title_id: r.title_id.clone(),
                title_name: r.game_name.clone(),
                achievement_id: String::new(),
                achievement_name: String::new(),
                gamerscore: 0,
                classification: crate::plan::safety::SafetyClass::Discontinued,
                xuid: config.my_xuid.clone(),
            });
        }
    }
    if plan.is_empty() {
        return Err(Error::invalid_argument(
            "every title in this plan is on the discontinued blocklist — \
             nothing left to unlock. Edit `Documents/XAU/discontinued.txt` \
             if you want to override.",
        ));
    }
    // BUG HUNTER runner-lock (#12): per-xuid exclusion across processes.
    let runner_lock = crate::plan::runner_lock::RunnerLock::try_acquire(&config.my_xuid)?;

    let progress = BitByBitProgress {
        state: BitByBitState::Resolving,
        ..Default::default()
    };
    let shared = Arc::new(RwLock::new(progress));
    // `watch` channel so EVERY spawned subtask (presence loop,
    // per-title workers, per-entry sleeps) can observe the stop
    // signal independently. Cloneable receivers + `tokio::select!`
    // on `changed()` makes long sleeps cancellable within ms.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // (see `my_minutes_played` in `run` for the resume fix)
    // Live knobs, seeded from the starting config. Cloned into `run`
    // below, so a caller turning them mid-run is observed at the next
    // dispatch boundary without restarting anything.
    let live = LiveControls::new(
        config.sequential_window.clamp(1, MAX_SIMULTANEOUS_TITLES),
        config.max_speedup,
    );
    let handle = BitByBitHandle {
        shared: shared.clone(),
        stop_tx,
        live: live.clone(),
    };
    // BUG HUNTER BBB-RunSupervisor: run() can panic — e.g.
    // `RwLock::write().unwrap_or_else(std::sync::PoisonError::into_inner)` on a previously-poisoned
    // shared state, a future `debug_assert!` mismatch, or a third-
    // party crate panic on weird input. The previous code spawned
    // run() inside a `tokio::spawn` whose JoinHandle was discarded.
    // On panic, the unwind dropped `_lock` (RAII removes the lock
    // file), but `shared.state` was NEVER updated past the last
    // write, so the GUI's progress emitter (which gates on
    // `BitByBitHandle::is_running()`, a state-only check) saw
    // `state=Running` forever, emitted the same zombie snapshot
    // every second, and the user saw "Running, 0 unlocks, ELAPSED
    // ticks but nothing happens". We now wrap the actual work in
    // an inner `tokio::spawn`, await its `JoinHandle`, and on any
    // panic / unexpected cancellation we force `state=Error` so
    // `is_running()` flips and the GUI surfaces the failure.
    tokio::spawn(async move {
        let _lock = runner_lock;
        let shared_for_recovery = shared.clone();
        // Wave-15 audit shut-H5: keep a copy of `client` + `my_xuid`
        // OUTSIDE the inner spawn so a panic in `run()` can still
        // issue the presence-DELETE keepalive drain. Without this
        // the user appears as "playing" the panicked title cohort
        // for the Xbox-side TTL (~5 min) and a forensic observer
        // sees a phantom-presence record after the run died.
        // `XboxClient` is `Clone` (already used elsewhere in BBB)
        // so the clone is cheap (Arc bump, no deep copy).
        let client_for_drain = client.clone();
        let my_xuid_for_drain = config.my_xuid.clone();
        let inner = tokio::spawn(async move {
            run(client, plan, config, shared, stop_rx, live).await;
        });
        match inner.await {
            Ok(()) => {}
            Err(je) => {
                let reason = if je.is_panic() {
                    format!("bit-by-bit run panicked — task supervised: {je}")
                } else if je.is_cancelled() {
                    "bit-by-bit run cancelled unexpectedly (runtime shutdown?)".to_string()
                } else {
                    format!("bit-by-bit run terminated abnormally: {je}")
                };
                tracing::error!(
                    ?je,
                    "bit-by-bit run() ended abnormally — flipping state to Error"
                );
                // Scope the lock guard tightly — the borrow checker
                // tracks `RwLockWriteGuard` as Send-unfriendly across
                // an await even with explicit `drop`, so place the
                // guard in its own lexical block. Once the block
                // ends, the guard is provably released.
                {
                    // Recover from a possibly-poisoned RwLock so we
                    // don't re-panic the supervisor itself trying to
                    // write the error state.
                    let mut w = match shared_for_recovery.write() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    // If `reconcile_terminal_state` already moved us
                    // to a terminal state (Done / Stopped / Error)
                    // before the panic, preserve it — the work
                    // LANDED, the panic was on the way out. Otherwise
                    // overwrite Resolving / Running with Error so the
                    // GUI's `is_running()` flips false and the
                    // progress-emit loop drains.
                    match w.state {
                        BitByBitState::Done | BitByBitState::Stopped | BitByBitState::Error => {}
                        _ => {
                            w.state = BitByBitState::Error;
                            if w.last_error.is_none() {
                                w.last_error = Some(reason);
                            }
                        }
                    }
                    // Abnormal exit skips `reconcile_terminal_state`,
                    // so clear the live-only fields here too — else
                    // the GUI shows an Error dot NEXT TO a live
                    // "▶ NOW / ⏳ floor gate" line from the dead run.
                    w.active_game_name = None;
                    w.active_title_done = 0;
                    w.active_title_total = 0;
                    w.active_title_floor_secs = 0;
                    w.floor_gate_active = false;
                    w.floor_gate_waiting = 0;
                    w.floor_gate_waited_secs = 0;
                    w.floor_gate_played_secs = 0;
                }
                // Wave-15 audit shut-H5: best-effort drain the
                // presence keepalive so Xbox-side stops showing the
                // user as playing the panicked cohort. 5s timeout
                // ensures supervisor exit isn't delayed by a slow /
                // dead-network DELETE.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client_for_drain.stop_heartbeat(&my_xuid_for_drain),
                )
                .await;
            }
        }
    });
    Ok(handle)
}

pub(super) async fn run(
    client: XboxClient,
    plan: Plan,
    config: BitByBitConfig,
    shared: Arc<RwLock<BitByBitProgress>>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    // Knobs the user can turn WHILE this runs. Re-read at dispatch
    // boundaries rather than captured, so the loop reacts without a
    // restart. See `LiveControls`.
    live: LiveControls,
) {
    // Per-run id: `unix_ms-pid` — unique across processes AND across
    // runs within a process (unlike the process-local counter), stable
    // for the whole run so every event and the run's own log file share
    // it. Published to the GUI/CLI via BitByBitProgress.run_id.
    let run_id: String = format!("{}-{}", now_unix_ms(), std::process::id());
    // Capture plan header info before `plan` is consumed into title_plans.
    let plan_source = plan.source_gamertag.clone();
    let plan_total = plan.total;
    // Real-playtime floors (MinutesPlayed → seconds) per title, for
    // sequential-mode pacing. Cloned out before `plan.entries` is
    // consumed below. Empty for plans built before the field existed —
    // the dispatch loop then falls back to the calendar span.
    let mut title_min_completion_secs = plan.title_min_completion_secs.clone();
    // LIVE floor fetch (sequential mode): for any title WITHOUT a real
    // playtime floor — the exact gap that made a best-of aggregate drop
    // Apex to the 60 s fallback — query the source speedrunner(s)'
    // MinutesPlayed right now and take the MIN>0 (the fastest real
    // completion). This is why the runner never has to settle for 60 s
    // when the true number is one request away. Source xuids come from
    // the plan (single = [source_xuid]; best-of = the real input
    // profiles); an aggregate's synthetic "aggregate-of-N" id is not
    // queryable and is correctly excluded upstream.
    if config.sequential {
        let missing: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            plan.entries
                .iter()
                .map(|e| e.title_id.clone())
                .filter(|tid| {
                    title_min_completion_secs
                        .get(tid)
                        .copied()
                        .filter(|&s| s > 0)
                        .is_none()
                })
                .filter(|tid| seen.insert(tid.clone()))
                .collect()
        };
        let sources: Vec<String> = if plan.source_xuids.is_empty() {
            // Old plan without attribution: fall back to the plan's own
            // source_xuid if it's a real (queryable) profile.
            if plan.source_xuid.bytes().all(|b| b.is_ascii_digit()) && !plan.source_xuid.is_empty()
            {
                vec![plan.source_xuid.clone()]
            } else {
                Vec::new()
            }
        } else {
            plan.source_xuids.clone()
        };
        if !missing.is_empty() && !sources.is_empty() {
            let mut fetched = 0usize;
            for xu in &sources {
                if let Ok(map) = client.get_minutes_played_map(xu, &missing).await {
                    for (tid, mins) in map {
                        let secs = i64::from(mins).saturating_mul(60);
                        if secs <= 0 {
                            continue;
                        }
                        // MIN across sources = fastest real completion.
                        title_min_completion_secs
                            .entry(tid)
                            .and_modify(|m| {
                                if *m <= 0 || secs < *m {
                                    *m = secs;
                                }
                            })
                            .or_insert(secs);
                        fetched += 1;
                    }
                }
            }
            tracing::info!(
                missing = missing.len(),
                sources = sources.len(),
                floors_recovered = fetched,
                "sequential live-fetch: recovered real MinutesPlayed floors \
                 for titles the plan lacked (no more 60 s fallback when the \
                 speedrunner's time is a query away)"
            );
        }
    }
    // MY OWN credited playtime per title. The floor is a TOTAL — "Xbox
    // must show at least this much time on this game" — and the gate
    // correctly compares my running total against it. The PACING,
    // however, always spread the whole floor starting from now, so
    // restarting a partially-done game re-spent time I had already
    // banked: 50 of 60 minutes credited still paced a fresh 60. That
    // is the "the time to finish resets every restart" report.
    //
    // Fetched once here, batched, and only in sequential mode. Failure
    // is harmless — an empty map means "assume nothing credited yet",
    // i.e. exactly the old behaviour.
    let mut my_minutes_played: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    if config.sequential {
        let unique: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            plan.entries
                .iter()
                .map(|e| e.title_id.clone())
                .filter(|t| seen.insert(t.clone()))
                .collect()
        };
        if !unique.is_empty() {
            if let Ok(map) = client
                .get_minutes_played_map(&config.my_xuid, &unique)
                .await
            {
                my_minutes_played = map
                    .into_iter()
                    .map(|(tid, mins)| (tid, mins.saturating_mul(60)))
                    .collect();
                tracing::info!(
                    titles = unique.len(),
                    with_playtime = my_minutes_played.len(),
                    "captured MY credited playtime — resumed games pace only the REMAINDER"
                );
            }
        }
    }
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.run_id = run_id.clone();
    }
    // ============================================================
    //  PHASE 1 — Resolving: fetch user's existing unlocks + build
    //  per-title schedule.
    // ============================================================
    // Live progress during the bulk scan. Previously this phase
    // looked like a frozen spinner for 15-30 s on big profiles
    // (SiegfriedX-sized: ~126 k achievements / 127 pages). The
    // `_with_progress` variant fires its callback every time a
    // page lands — we just forward the two numbers into the
    // shared BitByBitProgress struct. Audit bug BBB-#4.
    let progress_shared = shared.clone();
    let my_unlocks = match client
        .get_all_unlocked_achievements_with_progress(
            &config.my_xuid,
            true,
            Some(move |fetched_so_far: usize, total_if_known: Option<i64>| {
                if let Ok(mut w) = progress_shared.write() {
                    w.resolve_fetched = fetched_so_far;
                    // Only overwrite the total once it's known — the
                    // first few pages fire before `paging_info` is
                    // populated in the response, so preserving the
                    // `Some(_)` once we've seen it avoids flicker.
                    if total_if_known.is_some() {
                        w.resolve_total = total_if_known;
                    }
                }
            }),
        )
        .await
    {
        Ok(a) => a,
        Err(e) => {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.state = BitByBitState::Error;
            w.last_error = Some(format!("fetch my unlocks: {e}"));
            return;
        }
    };

    // Group my unlocks by title_id → set of achievement IDs + max
    // unlock timestamp AND min (first) unlock timestamp. The last is
    // used by the classic scheduler as the chain anchor; the first is
    // the "shadow anchor" used by `GapMode::Tiered` — when present,
    // every per-title schedule is computed as `first_user_unlock +
    // sum_of_scaled_gaps_from_target_entry_0`. Storing both is the
    // what makes the run RESUMABLE without any local state file:
    // re-running after days / weeks just re-reads Xbox's unlocks and
    // the first-unlock timestamp per title stays stable across runs.
    // UI/perf wave: FxHashMap on local hot maps. These are built
    // once per BBB run from the user's bulk-scan results (potentially
    // 240k entries on aggregate plans) — std SipHash costs measurable
    // wall-time on the build phase. FxHash is the same Hasher rustc
    // uses internally, designed for short keys like Xbox
    // achievement_id strings. The INNER HashSet stays std because it
    // gets passed to `compute_schedule_for_title_with_speedup` which
    // takes `&HashSet<String>` (changing that signature would break
    // every public caller).
    use rustc_hash::FxHashMap;
    let mut my_by_title: FxHashMap<
        String,
        (std::collections::HashSet<String>, Option<i64>, Option<i64>),
    > = FxHashMap::default();
    for a in &my_unlocks {
        let Some(tid) = a.title_associations.first().and_then(|ta| ta.id.clone()) else {
            continue;
        };
        let ts_unix = a
            .progression
            .as_ref()
            .and_then(|p| p.time_unlocked.as_deref())
            .and_then(parse_iso);
        let entry = my_by_title.entry(tid).or_default();
        entry.0.insert(a.id.clone());
        if let Some(t) = ts_unix {
            entry.1 = Some(entry.1.map_or(t, |cur| cur.max(t)));
            entry.2 = Some(entry.2.map_or(t, |cur| cur.min(t)));
        }
    }

    // Bucket plan entries by title_id (chronological order preserved
    // — plan is already sorted oldest-first by build_plan).
    // FxHashMap (same rationale as `my_by_title` above).
    let mut by_title: FxHashMap<String, (String, String, Vec<PlanEntry>)> = FxHashMap::default();
    let zero_uid = crate::xbox::constants::ZERO_UID;
    // ZERO_UID is treated as "scid missing" consistently with
    // `warm_scid_cache_bulk` and `process_title`. See audit bug
    // BBB-Unlock#3. Helper closure so both the initial insert and
    // the "upgrade with a later entry's scid" branch agree.
    let scid_is_real = |s: &str| !s.is_empty() && s != zero_uid;
    // BUG HUNTER RN-N1: defensive dedupe before bucketing. Same
    // rationale as `run_mirror` / `run_bulk` — loaded-from-disk
    // plans with internal duplicates would send the same POST twice
    // via the per-title unlocker, wasting budget without crediting.
    let mut seen_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(plan.entries.len());
    let dedup_entries: Vec<PlanEntry> = plan
        .entries
        .into_iter()
        .filter(|e| seen_keys.insert((e.title_id.clone(), e.achievement_id.clone())))
        .collect();
    for e in dedup_entries {
        let initial_scid = if scid_is_real(&e.service_config_id) {
            e.service_config_id.clone()
        } else {
            String::new()
        };
        let slot = by_title
            .entry(e.title_id.clone())
            .or_insert_with(|| (e.game_name.clone(), initial_scid, Vec::new()));
        if !scid_is_real(&slot.1) && scid_is_real(&e.service_config_id) {
            slot.1 = e.service_config_id.clone();
        }
        slot.2.push(e);
    }

    // Build one TitlePlan per title_id.
    let mut title_plans: Vec<TitlePlan> = Vec::with_capacity(by_title.len());
    for (title_id, (game_name, scid, source)) in by_title {
        let (user_unlocked, user_last_unix, user_first_unix) =
            my_by_title.get(&title_id).cloned().unwrap_or_default();
        title_plans.push(TitlePlan {
            title_id,
            game_name,
            scid,
            source,
            user_unlocked,
            user_first_unix,
            user_last_unix,
        });
    }

    // Fastest-G-rate ordering: start with the titles where the
    // target earned the most gamerscore per second of wall-clock
    // (3000G over 21s outranks 900G over 20s). Completion status is
    // IRRELEVANT here — a half-finished title with a blistering G
    // rate still goes first. This is the user-requested semantic:
    // "fastest" means G/time, not shortest span, not 100%-only.
    //
    // Uses the shared `compute_span_seconds` + `compute_gs_rate_milli`
    // helpers so this ordering is GUARANTEED identical to
    // `Plan::title_spans_fastest_gs_first` in the GUI summary, the
    // estimator preview and the CLI dry-run — a diverging copy would
    // silently make the "top-N fastest" replay apply a different set
    // than the card showed the user.
    //
    // Precedence: explicit `selected_titles` OVERRIDES the ranking.
    // When the user hand-picks a subset via the GUI, they've already
    // expressed intent — the top-N truncate would silently drop
    // titles they picked, contradicting the docs on `BitByBitConfig`.
    // See audit bug SELECT-vs-TOPN; the earlier claim "this is fixed"
    // was wrong — only the `estimate` path was fixed, the `run` path
    // wasn't, so preview and actual run diverged.
    // Real playtime per title, for the ranking below. Cloned out before
    // `plan.entries` is consumed; the live-fetch above has already
    // filled in whatever the plan file lacked, so this is the most
    // complete picture available at ordering time.
    let plan_ranking_secs: std::collections::HashMap<String, i64> = title_min_completion_secs
        .iter()
        .filter(|(_, &s)| s > 0)
        .map(|(k, &v)| (k.clone(), v))
        .collect();
    let has_explicit_selection = config
        .selected_titles
        .as_ref()
        .is_some_and(|s| !s.is_empty());
    if has_explicit_selection {
        // SAFETY: we just checked `has_explicit_selection`, so the
        // Option is Some(non_empty).
        let sel = config.selected_titles.as_ref().expect("checked above");
        title_plans.retain(|tp| sel.contains(&tp.title_id));
    }
    // PRE-SKIP known-impossible titles: a full-plan run shouldn't even
    // spend the ~1s probe on walls the failures DB already recorded as
    // deterministic code:49. An EXPLICIT selection is exempt — if the
    // user hand-picked a title, they want to retry it. Clearing
    // Documents/XAU/failed-titles.jsonl resets the skip list.
    let mut pre_skipped_known_dead = 0usize;
    if !has_explicit_selection {
        let dead = crate::plan::runner::known_impossible_title_ids();
        if !dead.is_empty() {
            let before = title_plans.len();
            title_plans.retain(|tp| !dead.contains(&tp.title_id));
            pre_skipped_known_dead = before.saturating_sub(title_plans.len());
            if pre_skipped_known_dead > 0 {
                tracing::info!(
                    pre_skipped_known_dead,
                    "pre-skipped known-impossible (code:49) titles from the failures DB"
                );
            }
        }
    }
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.pre_skipped_known_dead = pre_skipped_known_dead;
    }
    let use_ranking = !has_explicit_selection && config.top_n_titles > 0;

    // USER CONTRACT (2026-07-13): "fastest" = highest G/time rate,
    // and non-100% titles COUNT. The old top-N semantics (filter to
    // provably-100% titles with 2+ entries, rank by shortest span)
    // are gone on explicit request — top-N is now a pure truncate of
    // the rate ranking below, and the legacy `title_totals` guard is
    // unnecessary because completion data no longer gates anything.
    // `tp.source` is already deduped by (title, ach) at bucketing, so
    // the G sum can't be inflated by duplicate plan rows.
    title_plans.sort_by_cached_key(|tp| {
        let span =
            crate::plan::compute_span_seconds(tp.source.iter().map(|e| e.time_unlocked.as_str()));
        // Rank by the time the run is actually billed for, not by the
        // calendar span. A title 100%'d over one weekend has a short
        // span even when it cost 40 hours of play, while a 20-minute
        // game picked at across two years has a span of two years —
        // ranking by span put the long ones first and the quick ones
        // last, the opposite of "start with the fastest to complete".
        // Shared helper so this stays identical to the GUI summary,
        // the estimator preview and the CLI dry-run; a diverging copy
        // would make top-N replay a different set than the card showed.
        let duration = plan_ranking_secs
            .get(&tp.title_id)
            .copied()
            .unwrap_or_else(|| span.min(i64::try_from(FLOOR_GUESS_CAP_SECS).unwrap_or(i64::MAX)));
        let gamerscore: i64 = tp
            .source
            .iter()
            .map(|e| i64::from(e.gamerscore.unwrap_or(0)))
            .sum();
        // `(rate DESC, duration, count, title_id)` — cascading
        // tiebreakers so the order is deterministic across runs even
        // when integer rates truncate to the same value.
        (
            std::cmp::Reverse(crate::plan::compute_gs_rate_milli(gamerscore, duration)),
            duration,
            tp.source.len(),
            tp.title_id.clone(),
        )
    });

    // Truncate to `top_n_titles` ONLY when we're using the ranking
    // (i.e., no explicit selection). A hand-picked slice stays intact
    // at its original size.
    if use_ranking && title_plans.len() > config.top_n_titles {
        title_plans.truncate(config.top_n_titles);
    }

    // Shadow-replay slot promotion. When `active_slots_cap` is set,
    // limit the number of "not started" titles (user has 0 unlocks)
    // we process this run to `cap - currently_in_progress_count`.
    // This keeps the user's profile at most `cap` games "in progress"
    // at any moment — matches natural hunter behaviour. Titles
    // already in progress on the user's side are always processed
    // (to advance them toward completion). See audit bug BBB-Shadow.
    if config.active_slots_cap > 0 {
        apply_shadow_slot_promotion(&mut title_plans, config.active_slots_cap);
    }

    // ============================================================
    //  PHASE 2 — Scheduling: walk each title, compute which entries
    //  are eligible right now vs scheduled for the future.
    //
    //  The per-entry math lives in `compute_schedule_for_title` —
    //  pure, unit-tested, and shared with the `estimate` path so
    //  preview + real run can never diverge (audit bug #12).
    // ============================================================
    let now_unix = now_unix();
    // `time_budget` caps how far into the future entries qualify as
    // "eligible now" for THIS run. An entry whose scheduled_unix is
    // within [now, now + budget] fires this run; outside the window
    // defers to a future run. `None` = no budget = classic
    // "fire anything scheduled <= now" behaviour.
    let eligibility_horizon = match config.time_budget {
        // Saturating cast mirrors `bit_by_bit_estimate::estimate`.
        // `Duration::MAX.as_secs()` is `u64::MAX`; `as i64` wraps to
        // a large negative which silently nullifies the horizon
        // (R2 #2). `i64::try_from` clamps to `i64::MAX` so the
        // saturating_add below stays ceiling-correct.
        Some(d) => now_unix.saturating_add(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        None => now_unix,
    };
    let cap = if config.max_entries == 0 {
        usize::MAX
    } else {
        config.max_entries
    };
    let mut eligible_total = 0;
    let mut already_done_total = 0;
    let mut future_total = 0;
    let mut earliest_future: Option<i64> = None;

    // Compute schedules per-title using the shared scheduler. Cheap,
    // synchronous, no await. FxHashMap on the local accumulator
    // (same rationale as `by_title` / `my_by_title` above).
    let mut schedules_by_title: FxHashMap<String, Vec<ScheduledEntry>> = FxHashMap::default();
    for tp in &title_plans {
        // Dispatch to the tiered "shadow replay" scheduler when the
        // caller picked `GapMode::Tiered`. Tiered uses a different
        // anchor (user's FIRST unlock, not last) + per-gap speedup
        // tiers. Falls back to the classic per-title scheduler for
        // Exact/Capped/Scaled.
        let schedule = match &config.gap_mode {
            GapMode::Tiered(tiers) => compute_tiered_schedule_with_speedup(
                &tp.source,
                tp.user_first_unix,
                &tp.user_unlocked,
                tiers,
                now_unix,
                config.max_speedup,
            ),
            _ => compute_schedule_for_title_with_speedup(
                &tp.source,
                tp.user_last_unix,
                &tp.user_unlocked,
                config.gap_mode.clone(),
                now_unix,
                config.max_speedup,
            ),
        };
        // Anti-burst: unless the caller explicitly opted into Burst,
        // deficit-shift overdue entries forward so nothing fires "too
        // fast out of nowhere" after an absence — the earliest overdue
        // entry lands now, the rest keep their real gaps going forward.
        // MUST match the estimator (bit_by_bit_estimate.rs) so preview
        // == run.
        let mut schedule = schedule;
        if config.catch_up == CatchUpMode::Shift {
            apply_catch_up_shift(&mut schedule, now_unix);
        }
        // Roll up totals for the progress snapshot. Counting here
        // avoids a second pass later. "Eligible now" semantics
        // respect `eligibility_horizon` — entries within the
        // user-configured time_budget also count as eligible
        // because we'll try to fire them in this run.
        for s in &schedule {
            if s.already_done {
                already_done_total += 1;
            } else if s.scheduled_unix <= eligibility_horizon {
                eligible_total += 1;
            } else {
                future_total += 1;
                earliest_future =
                    Some(earliest_future.map_or(s.scheduled_unix, |e| e.min(s.scheduled_unix)));
            }
        }
        schedules_by_title.insert(tp.title_id.clone(), schedule);
    }

    // How many GAMES sequential mode keeps in flight. Deliberately NOT
    // `title_concurrency` (which defaults to 6 and bounds per-POST
    // permits): reusing it would flip every existing sequential user
    // from 1 game to 6 on upgrade. See `BitByBitConfig::sequential_window`.
    //
    // Read from `live` rather than `config` — the user can change it
    // mid-run, and every consumer below re-reads it at its own boundary.
    // This binding is only the value at snapshot-publish time.
    let sequential_window = live.window();

    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.state = BitByBitState::Running;
        w.eligible_now = eligible_total.min(cap);
        w.already_done = already_done_total;
        w.scheduled_for_future = future_total;
        w.earliest_future_unix = earliest_future.unwrap_or(0);
        // BUG HUNTER M-bbb-presence-active-flag: surface the live
        // presence-keepalive state to the GUI without an extra IPC
        // round-trip. Set true alongside `state=Running`; the
        // `reconcile_terminal_state` path later flips it back to
        // false on every Done/Stopped/Error exit (after the DELETE
        // has been issued by `drain_presence_cleanup`). We use the
        // CONFIG flag here (not the runtime cleanup-tuple, which is
        // declared later in this function) — equivalent in practice
        // because `presence_cleanup` is non-`None` iff
        // `config.presence_keepalive && !title_plans.is_empty()`,
        // and an empty `title_plans` would have early-returned long
        // before this Running state block fires.
        w.presence_keepalive_active = config.presence_keepalive;
        // Starting value of the speedup tier. NOT constant for the run
        // any more — the user can change it live, and
        // `BitByBitHandle::snapshot` overlays the current one, so this
        // write is just the initial seed.
        w.max_speedup = config.max_speedup;
        // Effective (clamped) sequential window, so the GUI knows
        // whether the single-slot per-game fields are meaningful.
        w.sequential_window = if config.sequential {
            sequential_window
        } else {
            0
        };
    }

    tracing::info!(
        eligible_total,
        already_done_total,
        future_total,
        earliest_future = earliest_future.unwrap_or(0),
        "bit-by-bit schedule computed"
    );

    // Open this run's dedicated log with a header (config echo + plan
    // info). Every wire event below appends to the same file, and
    // `reconcile_terminal_state` closes it with a run-end summary — so
    // `bbb-run-<run_id>.jsonl` is a complete, self-contained record.
    write_bbb_run_start(&run_id, &config.my_xuid, &plan_source, plan_total, &config);

    // ============================================================
    //  PHASE 3 — Execute: unlock eligible entries, bounded by
    //  title_concurrency (itself clamped to the Xbox presence
    //  ceiling MAX_SIMULTANEOUS_TITLES=8).
    // ============================================================
    let effective_concurrency = config.title_concurrency.clamp(1, MAX_SIMULTANEOUS_TITLES);
    let sem = Arc::new(tokio::sync::Semaphore::new(effective_concurrency));
    // Proactive unlock-rate pacer shared by ALL title workers: bounds
    // the aggregate POST rate to Xbox's 100-req/15s budget (see
    // `pace_unlock`). The `sem` above bounds concurrency; this bounds
    // rate — together they make rate-limit overflow (and the resulting
    // 429-driven failures) impossible by construction.
    let unlock_gate = Arc::new(tokio::sync::Mutex::new(Instant::now()));
    // Presence-ceiling gate (audit: unbounded-concurrent-title-activation).
    // The dispatch loop spawns one `process_title` per eligible title,
    // and each pushes itself into `active_titles` + fires a per-title
    // presence heartbeat. Xbox silently DROPS presence beyond
    // MAX_SIMULTANEOUS_TITLES (8) simultaneous titles, which then fails
    // the OneCollector event-based unlock path. This owned-permit
    // semaphore caps concurrently-active (presence-holding) titles to
    // 8 — DISTINCT from the per-POST `sem` above: a title holds its
    // activation permit for its whole lifetime, including while it
    // sleeps until a scheduled entry's time, because a sleeping title
    // is still broadcasting presence. Sized at the fixed Xbox ceiling
    // (not `effective_concurrency`) so presence can use all 8 slots
    // even when the user picked a smaller per-POST concurrency.
    let activation_sem = Arc::new(tokio::sync::Semaphore::new(MAX_SIMULTANEOUS_TITLES));
    // DYNAMIC presence set: the title_ids currently being processed.
    // Each `process_title` inserts its id after acquiring an activation
    // permit and removes it on completion; the keepalive loop heartbeats
    // exactly this live set, so presence rotates with the work and never
    // covers more than MAX_SIMULTANEOUS_TITLES titles. Starts empty —
    // the dispatch loop populates it as titles activate.
    let active_title_ids: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let chunk_size = config.chunk_size_override;

    // --- Presence keepalive ---
    // Xbox's OneCollector (event-based unlock path) + some title-based
    // endpoints check recent presence before accepting an update.
    // Each heartbeat extends the presence record for ~5 min (Xbox TTL);
    // we refresh every 180s to match what the farm module uses in
    // production (`FARM_PING_INTERVAL_SECS`) — same endpoint, same
    // cadence, proven to keep presence alive indefinitely. Stops
    // cleanly when the runner finishes or is stopped. `presence_cancel`
    // + `DELETE /users/xuid(...)/devices/current/titles/current` are
    // both no-ops when Xbox already removed the presence record, so
    // the cleanup is idempotent.
    // BUG HUNTER M-presence-stop-await: previously we spawned the
    // keepalive task with `tokio::spawn(async move {...})` and discarded
    // the JoinHandle. The PresenceGuard's drop pinged the cancel
    // channel synchronously, the spawned task woke up and started
    // `sender.stop().await` (DELETE /presence), BUT nothing on the
    // sync return path awaited the spawned task's completion. On
    // CLI exit the runtime tore down within ~50ms of `run()` returning,
    // killing the in-flight DELETE before it reached the wire — Xbox
    // continued to show the user as "playing" the last batch of
    // titles for several minutes after the runner exited. Live test
    // 2026-04-28: 12 `presence-heartbeat sent` events landed but 0
    // `presence-heartbeat stopped (DELETE)` events did. Fix: capture
    // the JoinHandle and the cancel sender together; explicitly
    // `drain_presence_cleanup` at every terminal exit so the DELETE
    // has a chance to complete on the wire before the runtime shuts
    // down. A best-effort PresenceGuard remains for panic unwind —
    // the DELETE may still race the runtime there but cancel is at
    // least signalled, matching the previous behaviour.
    let mut presence_cleanup: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)> =
        if config.presence_keepalive && !title_plans.is_empty() {
            // Presence now tracks the DYNAMIC active set (`active_title_ids`),
            // populated by each `process_title` as titles activate/finish, so
            // it rotates with the work and self-caps at MAX_SIMULTANEOUS_TITLES
            // — no fixed initial cohort, and dead titles' presence lapses
            // instead of lingering for the whole run. That also stops the old
            // behaviour of starving spoof_farm by claiming 8 slots up-front
            // (BBB-#6 / ARCHITECTURE.md Rule 9): we only ever broadcast the
            // titles we're actually processing right now.
            //
            // Per-title presence is established by each `process_title`
            // itself — lazily in the sequential path (one heartbeat AFTER
            // the first unlock lands, so dead/event titles cost none), or
            // up front for event/concurrent titles (see BBB-Presence#2).
            // Either way no fixed pre-dispatch bulk heartbeat is needed
            // here; the loop below just refreshes the live set every
            // `FARM_PING_INTERVAL_SECS`.
            let sender = (client.clone(), config.my_xuid.clone());
            let (tx, rx) = oneshot::channel::<()>();
            let loop_active = active_title_ids.clone();
            let task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
                let period = Duration::from_secs(crate::xbox::constants::FARM_PING_INTERVAL_SECS);
                let mut rx = rx;
                // `biased;` — poll the stop oneshot before the timer.
                // Without it, if both branches are simultaneously ready
                // (drain triggered while the period sleep already
                // elapsed), tokio picks pseudo-randomly and we may emit
                // one final heartbeat AFTER the user clicked Stop.
                tokio::select! {
                    biased;
                    _ = &mut rx => {
                        sender.stop().await;
                        return;
                    }
                    _ = tokio::time::sleep(period) => {}
                }
                presence_keepalive_loop(sender, loop_active, period, rx).await;
            });
            Some((tx, task))
        } else {
            None
        };
    // Async draining helper — pulled into a closure so each return
    // point in `run()` becomes a one-liner. Take()-style to ensure
    // a second call is a no-op (drains exactly once).
    //
    // Strategy refined 2026-04-28 after the first attempt missed the
    // DELETE: we do not rely on the spawned keepalive task to land
    // the DELETE on the wire. Instead we (a) signal the task to stop
    // emitting periodic heartbeats, (b) abort it after a brief
    // courtesy wait, and (c) issue the DELETE directly from `run()`'s
    // own async context — which is guaranteed to be awaited by the
    // outer supervisor before the runtime shuts down. The direct
    // DELETE pattern means the user always sees a presence stop on
    // every Done/Stopped exit, even when the keepalive task is
    // mid-tick on a separate endpoint.
    let stop_xuid = config.my_xuid.clone();
    let stop_client = client.clone();
    async fn drain_presence_cleanup(
        opt: &mut Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
        client: &XboxClient,
        xuid: &str,
    ) {
        if let Some((tx, task)) = opt.take() {
            // (a) signal the keepalive task to wind down — best-effort.
            let _ = tx.send(());
            // (b) brief courtesy wait so the task can terminate cleanly
            // if it's not blocked. If the task is parked behind the
            // rate-limit gate or Xbox is hanging, we don't wait — the
            // direct DELETE below covers cleanup.
            //
            // BUG HUNTER M-bbb-keepalive-abort: previously the
            // `tokio::time::timeout(500ms, task).await` simply DROPPED
            // the JoinHandle on timeout, which detaches the task
            // (does NOT cancel it). The detached task lived on,
            // possibly emitting one more heartbeat AFTER the
            // supervisor's `inner.await` returned — orphan presence
            // record. Now we hold an `AbortHandle` and call
            // `abort()` explicitly on timeout so the task is
            // actually cancelled.
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_millis(500), task)
                .await
                .is_err()
            {
                abort.abort();
                tracing::debug!(
                    "presence keepalive task did not finish in 500ms — \
                     aborted to prevent orphan heartbeat after supervisor exit"
                );
            }
        }
        // (c) issue the DELETE directly. This is awaited by the
        // outer supervisor task (`tokio::spawn` at start_bit_by_bit)
        // before run() returns, so the DELETE has the full HTTP
        // budget of `Other` endpoint to land. Failure is non-fatal
        // (presence record will expire on its own) but emits the
        // info-level "stopped" line either way for forensic audit.
        match tokio::time::timeout(Duration::from_secs(15), client.stop_heartbeat(xuid)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    ?e,
                    "presence DELETE failed at cleanup; record will expire on its own"
                );
            }
            Err(_) => {
                tracing::warn!(
                    "presence DELETE timed out after 15s; record will expire on its own"
                );
            }
        }
    }
    // Note on panic unwind: there is no PresenceGuard / sync-drop here.
    // We rely on `drain_presence_cleanup` being called at every
    // Done/Stopped/Error exit point in `run()`. On a Rust panic the
    // unwinder skips those calls; tokio's runtime then drops the
    // spawned keepalive task without invoking its `sender.stop()`,
    // and the presence record will expire on Xbox's side via the
    // server-side TTL. The supervisor at `start_bit_by_bit:577`
    // catches the panic and flips state to Error so the GUI surfaces
    // the failure, but it does NOT issue a presence DELETE. This is
    // an accepted limitation: panics are exceptional and Xbox's TTL
    // (a few minutes) covers the gap.
    let mut joins: tokio::task::JoinSet<TitleOutcome> = tokio::task::JoinSet::new();
    // Budget tracking (audit bug #4): previously decremented on
    // DISPATCH which burned budget on titles that later errored
    // (zero successful unlocks, budget spent). Now: decrement on
    // CONFIRMED unlock in the drain loop. Dispatch spawns
    // optimistically; if the budget is already hit we abort pending
    // tasks instead of letting them fire past the cap.
    let mut unlocked_budget_remaining = cap;
    // Budget is tracked in the drain loop now; here we still
    // per-title clip the `eligible` slice to the initial `cap` as a
    // defensive upper bound (prevents spawning hundreds of tasks
    // when a caller passes cap=1 on a massive plan).
    let mut dispatch_budget_remaining = cap;

    for tp in title_plans {
        if dispatch_budget_remaining == 0 {
            break;
        }
        // GAMERSCORE CAP: once this run's confirmed gamerscore reaches
        // the user's cap, don't START another game — the game already
        // in progress finished to 100% (sequential joins before the
        // next iteration), so we never stop a game half-done. The
        // crossing game's full G already landed, so the total can
        // overshoot by up to one game. `0` = no cap.
        if config.max_gamerscore > 0 {
            let gs_now = u64::try_from(
                shared
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .gamerscore_this_run
                    .max(0),
            )
            .unwrap_or(0);
            if gs_now >= config.max_gamerscore {
                tracing::info!(
                    gamerscore = gs_now,
                    cap = config.max_gamerscore,
                    "gamerscore cap reached — no new game will be started"
                );
                break;
            }
        }
        if *stop_rx.borrow() {
            // Recover any already-completed titles' outcomes before
            // finalizing (same rationale as the drain-loop cancel arm):
            // tasks spawned on earlier iterations may have committed
            // unlocks on Xbox already. abort_all() keeps their buffered
            // results; drain them so credits match server state.
            joins.abort_all();
            while let Some(j) = joins.try_join_next() {
                if let Ok(outcome) = j {
                    apply_title_outcome(&shared, outcome, &mut unlocked_budget_remaining);
                }
            }
            drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
            reconcile_terminal_state(&shared, BitByBitState::Stopped);
            return;
        }
        let sched = schedules_by_title.remove(&tp.title_id).unwrap_or_default();
        // Collect (id, gamerscore) pairs for achievements eligible
        // NOW, in source order. Carrying both together means the
        // event-path partial-success credit can sum ONLY the actual
        // succeeded gamerscore values — see process_title.
        // Carry `scheduled_unix` per-entry so process_title can honor
        // the target's timing gaps: when the user sets a `time_budget`
        // that exceeds `now_unix`, multiple entries become eligible
        // with per-entry scheduled times in the near future, and
        // process_title sleeps until each one's scheduled_unix before
        // posting — producing Xbox server timestamps that match the
        // target's pacing instead of collapsing everything to a single
        // moment. Audit bug BBB-Timing#4/#9.
        // Sequential mode processes ONE game fully to 100% this run,
        // pacing it across the per-game floor (below) instead of the
        // gap_mode schedule — so the whole game's not-already-done set
        // is eligible, ignoring the `eligibility_horizon` deferral
        // that the concurrent path uses to spread a title across runs.
        let eligible: Vec<(String, i32, i64, u16)> = sched
            .iter()
            .filter(|s| {
                !s.already_done && (config.sequential || s.scheduled_unix <= eligibility_horizon)
            })
            .map(|s| {
                (
                    s.achievement_id.clone(),
                    s.gamerscore,
                    s.scheduled_unix,
                    s.scheduled_subsec_ms,
                )
            })
            .take(dispatch_budget_remaining)
            .collect();
        if eligible.is_empty() {
            continue;
        }
        dispatch_budget_remaining = dispatch_budget_remaining.saturating_sub(eligible.len());

        // Per-game minimum plausible completion time (seconds). In
        // sequential mode, prefer the REAL playtime floor captured at
        // plan-build from Xbox's MinutesPlayed stat (the same number
        // TrueAchievements shows as "time played" — for a best-of
        // aggregate it's the fastest profile's real completion time).
        // Fallback for plans built before the field existed (or a
        // title whose stat was private/absent): the calendar span
        // between the target's first and last unlock. Either way,
        // floored at `min_game_seconds` so burst/single-ach titles
        // don't 100% instantly. `0` = concurrent mode → no floor
        // pacing (process_title honors the gap_mode schedule).
        //
        // The user's `max_speedup` tier then COMPRESSES that floor
        // (`floor_speedup_secs`): `2x` finishes the game in half the
        // target's real playtime, and so on. This is the only lever
        // that changes a sequential run's total duration — the tier's
        // effect on the gap schedule only reshapes cadence, because
        // `floor_adjusted_targets` re-normalises the whole window onto
        // `floor_secs`. Compression deliberately breaks the
        // "never faster than the target" guarantee, which is why the
        // tier defaults to `Off` and is surfaced in the UI.
        //
        // `min_game_seconds` survives the division and the result can
        // never be 0 — `0` is the CONCURRENT-MODE SENTINEL below, not
        // "no floor".
        let floor_secs: u64 = if config.sequential {
            // Real MinutesPlayed is authoritative and uncapped. Only the
            // calendar-span GUESS is bounded — left raw, a title whose
            // first and last unlock sit years apart yielded a floor of
            // ~1095 days, which paced the run to one achievement and
            // then a 438-day wait. See `FLOOR_GUESS_CAP_SECS`.
            let real = title_min_completion_secs
                .get(&tp.title_id)
                .copied()
                .filter(|&s| s > 0)
                .and_then(|s| u64::try_from(s).ok());
            let base = match real {
                Some(secs) => secs,
                None => cap_guessed_floor_secs(
                    u64::try_from(compute_title_span_secs(&tp.source)).unwrap_or(0),
                ),
            };
            let raw = base
                .max(config.min_game_seconds)
                // Belt-and-braces: a caller with min_game_seconds == 0
                // and an unparseable plan could otherwise land on the
                // concurrent-mode sentinel and silently drop pacing.
                .max(1);
            // Live tier, read as THIS game starts. Games already pacing
            // keep the floor they began with — their per-achievement
            // schedule was derived from it, so re-deriving mid-flight
            // would move targets that have already been slept against.
            floor_speedup_secs(raw, live.speedup(), config.min_game_seconds)
        } else {
            0
        };
        // How long this game still has to be paced across. The floor
        // above is the TOTAL Xbox must eventually show; whatever is
        // already credited to me does not need re-spending. On a fresh
        // title these are equal, so nothing changes; on a resumed one
        // the run picks up where the clock actually is instead of
        // starting the whole game over.
        let pace_secs: u64 = if floor_secs == 0 {
            0
        } else {
            let already = my_minutes_played
                .get(&tp.title_id)
                .copied()
                .and_then(|s| u64::try_from(s).ok())
                .unwrap_or(0);
            let outstanding = if already >= floor_secs {
                // OVERSHOOT: Xbox already credits me more time on this
                // game than the target spent finishing it, so the floor
                // has nothing left to enforce. Don't re-pace hours I
                // don't owe, and don't collapse to `min_game_seconds`
                // either — take the dedicated half-hour window.
                OVERSHOOT_PACE_WINDOW_SECS
            } else {
                floor_secs
                    .saturating_sub(already)
                    .max(config.min_game_seconds)
                    .max(1)
            };
            // Backstop for the OTHER way a window collapses (a hot
            // speedup tier on a short game): keep unlocks from landing
            // closer than `MIN_SECS_PER_ACHIEVEMENT`. On the overshoot
            // path this only engages past ~90 achievements, where 30
            // minutes really would be too tight.
            pace_window_with_min_spacing(outstanding, eligible.len())
        };
        // A clone of the client for the per-game presence OFFLINE that
        // fires after this game finishes (sequential mode only). The
        // task-scoped clone below is moved into the spawn.
        let offline_client = client.clone();

        let client = client.clone();
        let my_xuid = config.my_xuid.clone();
        let safety = config.safety;
        // `process_title` now acquires a semaphore permit PER POST
        // (just before each unlock HTTP call), not per-title. This
        // matters when entries sit in `sleep_until_scheduled` for
        // minutes/hours honoring the target's pacing: the old
        // per-title permit meant a single sleeping task hogged one
        // of the `effective_concurrency` slots for the whole
        // sleep, blocking other titles whose entries were eligible
        // RIGHT NOW. Now the permit is only held during the
        // HTTP round-trip, so N titles can be concurrently
        // sleeping on their own schedules while only K ≤ N of
        // them are actively POSTing. See audit bug
        // BBB-Permit-During-Sleep#2.
        let sem_for_task = sem.clone();
        let activation_for_task = activation_sem.clone();
        let task_cancel = stop_rx.clone();
        let shared_for_task = shared.clone();
        let active_ids_for_task = active_title_ids.clone();
        let run_id_for_task = run_id.clone();
        let unlock_gate_for_task = unlock_gate.clone();
        joins.spawn(async move {
            process_title(
                &client,
                &my_xuid,
                tp.title_id,
                tp.game_name,
                tp.scid,
                eligible,
                chunk_size,
                safety,
                sem_for_task,
                activation_for_task,
                task_cancel,
                shared_for_task,
                active_ids_for_task,
                run_id_for_task,
                unlock_gate_for_task,
                floor_secs,
                pace_secs,
            )
            .await
        });

        // Sequential "one game at a time" discipline: immediately
        // await THIS game to 100% (its whole slice + the floor pace)
        // before spawning the next, so games never interleave on the
        // wire. Then issue the explicit per-game presence OFFLINE. The
        // unscoped `stop_heartbeat` DELETE is safe here precisely
        // because sequential mode keeps exactly one game active — no
        // other title's presence to clobber. Stop stays responsive:
        // the join awaits a task whose internal sleeps race `cancel`,
        // and the top-of-loop `stop_rx` check fires next iteration.
        if config.sequential {
            // Hold the window: block only once `sequential_window`
            // games are already in flight, then reap until there's
            // room for the next. At the default window of 1 this is
            // byte-identical to the historical "spawn one, await it"
            // shape — `len() >= 1` right after the spawn, one join,
            // `len() == 0`, exit.
            // Re-read EVERY iteration so a +/- during the run takes
            // effect immediately. Lowering it never kills a running
            // game — the loop just keeps reaping until the in-flight
            // count fits the new window, so games finish normally and
            // only NEW ones are held back.
            while joins.len() >= live.window() {
                let Some(joined) = joins.join_next().await else {
                    break;
                };
                match joined {
                    Ok(outcome) => {
                        apply_title_outcome(&shared, outcome, &mut unlocked_budget_remaining);
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "sequential title task failed to join");
                    }
                }
            }
            // Token died mid-run (a title 403'd every unlock with
            // code:21) → STOP cleanly. The field timeline proved this is
            // token expiry, not a per-title wall; without stopping, every
            // remaining title 403s for hours. Bit-by-bit resumes from
            // Xbox state, so a refresh + Start continues where it left
            // off. Finalize as `Stopped` (partial, resumable) — the
            // token-expired message is already in `last_error`.
            let token_dead = shared
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .token_expired;
            if token_dead {
                tracing::warn!("bit-by-bit: auth token expired mid-run — stopping for re-auth");
                joins.abort_all();
                while let Some(j) = joins.try_join_next() {
                    if let Ok(outcome) = j {
                        apply_title_outcome(&shared, outcome, &mut unlocked_budget_remaining);
                    }
                }
                drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
                reconcile_terminal_state(&shared, BitByBitState::Stopped);
                return;
            }
            // Best-effort go-offline for the finished game before the
            // next one broadcasts its presence.
            //
            // ONLY at a window of 1. `stop_heartbeat` is an UNSCOPED
            // `DELETE /users/{xuid}/devices/current` — it takes no
            // title and wipes the device's whole presence record. At
            // window 1 that is exactly right (the finished game is the
            // only thing present). Above 1 it would tear down every
            // sibling still mid-run, which is not merely untidy:
            // MinutesPlayed only accrues while present, and
            // `await_playtime_floor` polls that stat, so the siblings
            // would starve their own playtime credit and give up at
            // the lag cap. `PresenceSender::stop` already refuses this
            // call for the same reason (BBB-Presence#3). Above 1 the
            // finished title's presence lapses on Xbox's ~5-min TTL
            // once `PresenceMembership`'s drop pulls it out of the
            // live keepalive set — no DELETE needed.
            if sequential_window == 1 {
                if let Err(e) = offline_client.stop_heartbeat(&config.my_xuid).await {
                    tracing::debug!(error = ?e, "per-game presence stop failed (non-fatal)");
                }
            }
        }
    }

    // Drain loop: race `join_next` against the cancel signal so a
    // Stop click is observed immediately, not just after the next
    // task completes. Previously the drain blocked on `join_next`
    // while every spawned task was `sleep_until_scheduled`'ing for
    // minutes/hours — the stop flag flipped but the outer loop
    // never noticed. See audit bug BBB-Sleep-Cancel#1.
    //
    // `biased;` so cancel is polled first each iteration.
    let mut cancel_watch = stop_rx.clone();
    loop {
        let joined = tokio::select! {
            biased;
            res = cancel_watch.changed() => {
                // `changed()` returns Err iff every Sender has been
                // dropped — i.e. the runner's owner is gone and no
                // further stop signal can ever arrive. Treat that
                // exactly like an explicit stop: abort tasks, clean
                // up presence, finalize. Otherwise we'd busy-loop
                // forever because `changed()` returns Err
                // immediately on every subsequent call (and the
                // current `borrow()` value never flips to true).
                let stop_observed = match res {
                    Ok(()) => *cancel_watch.borrow(),
                    Err(_) => true,
                };
                if stop_observed {
                    // Abort every in-flight task; the cancellable
                    // sleeps inside each will short-circuit within
                    // ms. `join_next` after abort returns
                    // `JoinError::is_cancelled` which the existing
                    // match-arm below classifies correctly.
                    joins.abort_all();
                    // Recover outcomes of tasks that had ALREADY
                    // completed when the stop arrived — their unlock
                    // POSTs were committed on Xbox before the task
                    // returned, and `abort_all()` does NOT discard
                    // results already buffered in the JoinSet. Without
                    // this drain the biased cancel arm would drop them,
                    // undercounting `unlocked_this_run` / gamerscore vs
                    // real server state on the stop edge. Audit:
                    // stop-edge-discards-queued-completed-outcomes.
                    while let Some(j) = joins.try_join_next() {
                        if let Ok(outcome) = j {
                            apply_title_outcome(
                                &shared,
                                outcome,
                                &mut unlocked_budget_remaining,
                            );
                        }
                    }
                    drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
                    reconcile_terminal_state(&shared, BitByBitState::Stopped);
                    return;
                }
                continue;
            }
            j = joins.join_next() => match j {
                Some(j) => j,
                None => break,
            },
        };
        // DO NOT check `*stop_rx.borrow()` here — previously this
        // branch returned early on late-arriving Stop, silently
        // discarding the `joined` outcome we just pulled off the
        // JoinSet. The title's unlocks HAD ALREADY LANDED on Xbox
        // (the HTTP POSTs were committed before the task returned),
        // but the local counters + last_error + budget accounting
        // never saw them, so the UI showed 0 unlocked while the
        // actual profile held the unlocks. We now ALWAYS apply the
        // outcome (match below) and defer the stop check until
        // AFTER bookkeeping, so credits stay consistent with
        // server state on the stop edge. Audit: Agent-2 MEDIUM.
        match joined {
            Ok(outcome) => {
                apply_title_outcome(&shared, outcome, &mut unlocked_budget_remaining);
                // Token died mid-run (403 code:21 on every unlock of a
                // title) → stop cleanly for re-auth instead of letting
                // the remaining concurrent tasks 403-hammer for hours.
                // Same rationale + resume story as the sequential path.
                let token_dead = shared
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .token_expired;
                if token_dead {
                    tracing::warn!("bit-by-bit: auth token expired mid-run — stopping for re-auth");
                    joins.abort_all();
                    while let Some(j) = joins.try_join_next() {
                        if let Ok(o) = j {
                            apply_title_outcome(&shared, o, &mut unlocked_budget_remaining);
                        }
                    }
                    drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
                    reconcile_terminal_state(&shared, BitByBitState::Stopped);
                    return;
                }
                // If we've hit the confirmed-unlock cap, abort any
                // still-pending tasks instead of letting them fire
                // past the budget. This matches the old "stop when
                // budget is done" semantics but using real
                // confirmations instead of optimistic reservations.
                if unlocked_budget_remaining == 0 {
                    joins.abort_all();
                    // Still drain remaining Joins to collect Ok/Err
                    // outcomes so cancelled tasks don't orphan.
                    // The while loop above handles them normally.
                }
            }
            Err(e) if e.is_cancelled() => {
                // Intentional cancel via `joins.abort_all()` when the
                // confirmed-unlock budget hits zero. NOT a panic — don't
                // count it as an error (would surface as phantom
                // "panicked" entries every time the user sets
                // max_entries). Debug-log for diagnosis; state counters
                // untouched. See audit bug BBB-#1.
                tracing::debug!(?e, "bit-by-bit task cancelled (budget cap or stop)");
            }
            Err(e) => {
                // Genuine task-level panic (propagated from tokio::spawn).
                // Counted, surfaced, and terminal state flips to `Error`
                // if nothing else landed. See audit bug #9.
                tracing::warn!(?e, "bit-by-bit title task panicked");
                let mut w = shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                w.errored_titles = w.errored_titles.saturating_add(1);
                w.last_error = Some(format!("title task panicked: {e}"));
            }
        }
        // Deferred stop check: apply the joined outcome FIRST (above),
        // THEN honour a pending stop. Covers the edge where a Stop
        // click arrived between the previous iteration's
        // `cancel_watch.changed()` poll and now — the `select!`
        // biased toward cancel won't re-fire unless the value
        // toggles, so we check the flag directly. Previously this
        // check lived BEFORE the match block and lost outcomes on
        // stop edge (see comment at the `match joined` site).
        if *stop_rx.borrow() {
            joins.abort_all();
            drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
            reconcile_terminal_state(&shared, BitByBitState::Stopped);
            return;
        }
    }

    // Normal exit — route through the unified reconcile.
    // BUG HUNTER M-presence-stop-await: drain BEFORE reconcile so the
    // presence DELETE goes out while `is_running()` is still true. The
    // CLI's main loop polls `is_running()`; flipping it to false in
    // `reconcile_terminal_state` causes the CLI to break immediately
    // and the runtime can shut down before the DELETE lands.
    drain_presence_cleanup(&mut presence_cleanup, &stop_client, &stop_xuid).await;
    // BUG HUNTER bbb-stop-on-empty-drain: the `None => break` arm (empty
    // JoinSet — e.g. every title's eligible-now slice was empty, so zero
    // tasks spawned) skips the in-loop deferred stop check above, and a
    // Stop whose value was already `true` when `cancel_watch` was cloned
    // never fires `changed()`. Without this guard such a run finalizes as
    // `Done`, silently misreporting the user's explicit Stop. Honour a
    // pending stop here too. (Tasks, if any, were already drained; nothing
    // to abort on this normal-exit path.)
    let final_state = if *stop_rx.borrow() {
        BitByBitState::Stopped
    } else {
        BitByBitState::Done
    };
    reconcile_terminal_state(&shared, final_state);
}

/// Apply one completed title's outcome to the shared progress
/// snapshot: credit confirmed unlocks / gamerscore, roll up
/// partial/error/skip counters, and debit the confirmed-unlock
/// budget. Extracted so the normal drain loop AND the stop-edge
/// recovery path (which drains already-completed tasks out of the
/// `JoinSet` before returning) apply IDENTICAL bookkeeping — a Stop
/// that races a just-finished title must not silently drop unlocks
/// that already landed on Xbox. Audit: stop-edge-discards-queued.
pub(super) fn apply_title_outcome(
    shared: &Arc<RwLock<BitByBitProgress>>,
    outcome: TitleOutcome,
    unlocked_budget_remaining: &mut usize,
) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.last_title = Some(outcome.name.clone());
    // Track ACTUAL confirmed unlocks against the budget. Counts only
    // things the outcome says landed (Unlocked.count / Partial.count),
    // never attempts or failures. On Error/Skip we spend zero budget.
    let real_unlocks = match &outcome.result {
        TitleResult::Unlocked { count, .. } | TitleResult::Partial { count, .. } => *count,
        TitleResult::EventBasedSkipped
        | TitleResult::SkippedUnauthorized
        | TitleResult::Error(_) => 0,
    };
    *unlocked_budget_remaining = unlocked_budget_remaining.saturating_sub(real_unlocks);
    match outcome.result {
        TitleResult::Unlocked { count, gamerscore } => {
            w.unlocked_this_run = w.unlocked_this_run.saturating_add(count);
            w.gamerscore_this_run = w.gamerscore_this_run.saturating_add(gamerscore);
            // Only clear last_error on a REAL success (count > 0).
            // `Unlocked { count: 0 }` is emitted when the safety gate
            // filtered every candidate — that's not a success that
            // should wipe earlier titles' error context. Audit:
            // Agent-2 finding #6.
            if count > 0 {
                w.last_error = None;
            }
        }
        TitleResult::Partial {
            count,
            gamerscore,
            failed,
            message,
        } => {
            // Credit the real successes but surface the failures too:
            // bump `errored_titles` by 1 and keep `last_error`
            // populated so partial event-based failures aren't hidden.
            w.unlocked_this_run = w.unlocked_this_run.saturating_add(count);
            w.gamerscore_this_run = w.gamerscore_this_run.saturating_add(gamerscore);
            w.errored_titles = w.errored_titles.saturating_add(1);
            w.last_error = Some(format!("{}: {message}", outcome.name));
            tracing::info!(
                title = %outcome.name,
                unlocked = count,
                gamerscore = gamerscore,
                failed_in_title = failed,
                "bit-by-bit title finished partial",
            );
        }
        TitleResult::EventBasedSkipped => {
            w.event_based_skipped_titles = w.event_based_skipped_titles.saturating_add(1);
            w.last_error = Some(format!("{} — event-based (not in bundle)", outcome.name));
            push_skipped_game(&mut w, &outcome.name, &outcome.title_id, "event_based");
        }
        TitleResult::SkippedUnauthorized => {
            w.unauthorized_skipped_titles = w.unauthorized_skipped_titles.saturating_add(1);
            // 403 code:21 on every unlock of a title = the token expired
            // mid-run. Flag it so the dispatch/drain loops stop cleanly
            // instead of 403-hammering the rest of the plan for hours.
            w.token_expired = true;
            w.last_error = Some(format!(
                "🔑 Auth token expired mid-run (403 security claims on “{}”). \
                 Refresh the token and press Start — bit-by-bit resumes from \
                 where it stopped.",
                outcome.name
            ));
            push_skipped_game(&mut w, &outcome.name, &outcome.title_id, "unauthorized");
        }
        TitleResult::Error(msg) => {
            w.errored_titles = w.errored_titles.saturating_add(1);
            w.last_error = Some(format!("{}: {msg}", outcome.name));
            push_skipped_game(&mut w, &outcome.name, &outcome.title_id, "error");
        }
    }
    // Reconcile the live per-achievement counter against the
    // authoritative title totals. The live counter is bumped
    // optimistically per-unlock while a title runs; the drain loop's
    // `unlocked_this_run` is the source of truth. Take the max so a
    // dropped bump (e.g. a wire success the live path missed) still
    // reflects the real count, and an over-count never regresses the
    // authoritative total.
    w.live_unlocked_this_run = w.live_unlocked_this_run.max(w.unlocked_this_run);
    w.live_gamerscore_this_run = w.live_gamerscore_this_run.max(w.gamerscore_this_run);
}

/// Compute and write the final `state` + reconcile bookkeeping.
///
/// ALL exit paths go through here so the terminal snapshot is
/// consistent regardless of whether the runner finished normally,
/// was stopped via `stop_rx`, or the budget cap aborted pending
/// tasks.
///
/// Contract:
/// - `preferred` is what the caller would WRITE if the counters were
///   silent (`Done` for normal exit, `Stopped` for user stop).
/// - If nothing landed AND at least one title errored, we prefer
///   `Error` — nothing actually worked.
/// - `Stopped` wins over `Error` when the user explicitly stopped
///   (the stop was deliberate; errors mid-stop are incidental).
///
/// Audit BBB-#2: the early-return sites used to bypass this block,
/// so `Stopped` could ship with a stale `last_error` from an
/// unrelated earlier title or an inconsistent (errored_titles > 0)
/// & (unlocked_this_run > 0) pair that never got the "is this
/// fundamentally a success or a failure" decision.
pub(super) fn reconcile_terminal_state(
    shared: &Arc<RwLock<BitByBitProgress>>,
    preferred: BitByBitState,
) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let nothing_landed = w.unlocked_this_run == 0;
    let any_errored = w.errored_titles > 0;
    let any_event_skipped = w.event_based_skipped_titles > 0 || w.unauthorized_skipped_titles > 0;
    // Always flip the presence-active flag off at terminal: by the
    // time `reconcile_terminal_state` runs, `drain_presence_cleanup`
    // has issued the DELETE (or timed out trying), so the GUI badge
    // should show "presence inactive" alongside the terminal state.
    w.presence_keepalive_active = false;
    // No game is "online" once the run is terminal.
    w.active_game_name = None;
    w.active_title_done = 0;
    w.active_title_total = 0;
    w.active_title_floor_secs = 0;
    // The floor gate can't be waiting once the run is terminal —
    // clear its telemetry so the GUI doesn't show a stale "⏳".
    // The counter goes with it: a task aborted mid-gate never runs
    // its `leave`, so without this a stopped run could leave a
    // permanently non-zero `floor_gate_waiting` behind.
    w.floor_gate_active = false;
    w.floor_gate_waiting = 0;
    w.floor_gate_waited_secs = 0;
    w.floor_gate_played_secs = 0;
    // No unlock is pending once terminal.
    w.next_unlock_at_ms = 0;
    w.state = match preferred {
        BitByBitState::Stopped => {
            // User asked to stop — respect that, even if errors
            // were in flight. But drop the last_error message if
            // it looks like a cancellation artifact.
            if nothing_landed && (any_errored || any_event_skipped) {
                // Leave last_error populated so the user knows the
                // stop was AT LEAST partly because of errors.
            }
            BitByBitState::Stopped
        }
        _ => {
            if nothing_landed && (any_errored || any_event_skipped) {
                BitByBitState::Error
            } else {
                // Clear last_error on clean Done — a previous title's
                // error message shouldn't linger on a final success.
                if !any_errored && !any_event_skipped {
                    w.last_error = None;
                }
                BitByBitState::Done
            }
        }
    };
    // Write the run-end summary to the shared history + this run's own
    // log so a finished run is self-describing ("this run: N unlocked,
    // M G, K errors, ..."). Skipped when run_id is empty (never really
    // started). Best-effort; downgrade the lock to read the snapshot.
    if !w.run_id.is_empty() {
        write_bbb_run_end(&w.run_id.clone(), &w);
    }
}

/// Shadow-replay slot promotion: given a mutable `title_plans` list
/// and a cap on "simultaneously in-progress" titles, retain every
/// title the user already has unlocks on (real-in-progress + ghosts)
/// plus the first `cap - in_progress_count` fresh (no-unlocks)
/// titles. The rest get dropped from this run and will be considered
/// on future runs once something completes.
///
/// "In progress" semantics: user has ≥1 unlock AND is missing ≥1
/// source entry. Titles where the user has already unlocked every
/// source entry ("ghost") do NOT count — they produce zero dispatch
/// work this run. Previously this routine treated ghosts as in-
/// progress, so a profile that 100%'d 5 of 8 selected titles with
/// `cap = 8` refused to activate ANY new title (ghosts ate 5/8).
///
/// Ghosts are still retained in the output (they contribute to the
/// UI's `already_done_total` metric) but don't consume slot budget
/// for fresh activations.
///
/// Pure: no I/O, deterministic on inputs, safe to unit-test.
pub(super) fn apply_shadow_slot_promotion(title_plans: &mut Vec<TitlePlan>, cap: usize) {
    let has_pending_work = |tp: &TitlePlan| -> bool {
        !tp.user_unlocked.is_empty()
            && tp
                .source
                .iter()
                .any(|e| !tp.user_unlocked.contains(&e.achievement_id))
    };
    let in_progress_count = title_plans.iter().filter(|tp| has_pending_work(tp)).count();
    let available_slots = cap.saturating_sub(in_progress_count);
    let mut kept_not_started = 0usize;
    title_plans.retain(|tp| {
        let has_unlocks = !tp.user_unlocked.is_empty();
        if has_unlocks {
            return true;
        }
        if kept_not_started < available_slots {
            kept_not_started += 1;
            true
        } else {
            false
        }
    });
    tracing::info!(
        in_progress_count,
        available_slots,
        promoted = kept_not_started,
        total_titles = title_plans.len(),
        "shadow-replay slot promotion"
    );
}
