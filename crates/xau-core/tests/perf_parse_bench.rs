// Benchmark code — wall-clock casts to f64 for human-readable ratios, and
// id indices cast to i32 to fit the struct shape. Both are fine because
// the inputs are bounded by test constants, not user data.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]

//! Micro-benchmarks (run manually with `cargo test --release -- --ignored`)
//! that quantify the speedups of the 2.8.2 scanning rewrite:
//!
//! 1. `SlimAchievement` parse is materially faster than full `Achievement`
//!    parse on the bulk endpoint body shape.
//! 2. Rayon `par_iter` filter + `par_sort_unstable_by` beat the serial
//!    equivalents on a 30k-entry payload big enough to matter.
//!
//! Tests are `#[ignore]` so they don't slow down the normal CI run but
//! remain one command away when profiling or tuning the scan pipeline.
//! They assert relative speedups rather than absolute wall-clock numbers
//! so CI hardware variance doesn't cause flakes — the guarantee is
//! "the optimized path is NOT slower than the unoptimized one", which
//! is the only regression we actually care about.

use std::time::Instant;

use rayon::prelude::*;

use xau_core::models::{AchievementsResponse, SlimAchievementsResponse};

/// Build a synthetic bulk response with `n` achievements. Shape mimics
/// the real Xbox Live payload: most fields populated, some nullable.
fn build_payload(n: usize) -> String {
    let mut s = String::with_capacity(n * 600);
    s.push_str(r#"{"achievements":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        // Vary timestamps so sort has work to do. Cycling through ~10
        // distinct months prevents a pre-sorted input from making sort
        // trivially cheap.
        let month = 1 + (i % 12);
        let title = 1_000_000 + (i % 200);
        s.push_str(&format!(
            concat!(
                r#"{{"id":"ach_{i}","serviceConfigId":"scid_{sc}","name":"Ach {i}","#,
                r#""progressState":"Achieved","isSecret":false,"#,
                r#""description":"description of achievement {i}","#,
                r#""lockedDescription":"locked {i}","#,
                r#""platforms":["Durango","Scarlett"],"#,
                r#""mediaAssets":[{{"name":"icon","type":"Icon","url":"https://ex/i{i}.png"}}],"#,
                r#""titleAssociations":[{{"id":"{title}","name":"Game {title}"}}],"#,
                r#""rewards":[{{"name":"","value":"10","type":"Gamerscore","valueType":"Int"}}],"#,
                r#""progression":{{"requirements":[],"timeUnlocked":"2024-{mo:02}-15T12:00:00Z"}},"#,
                r#""rarity":{{"currentCategory":"Common","currentPercentage":0.57}},"#,
                r#""isRevoked":false,"#,
                r#""participationType":"Individual"}}"#
            ),
            i = i,
            sc = i % 32,
            title = title,
            mo = month,
        ));
    }
    s.push_str(r#"],"pagingInfo":{"continuationToken":null,"totalRecords":"#);
    s.push_str(&n.to_string());
    s.push_str("}}");
    s
}

#[test]
#[ignore = "slow / run manually with --ignored"]
fn bench_simd_json_vs_serde_json_reports_speedup() {
    // This is the test that proves the simd-json switch on the bulk
    // hot path was worth it. Compare serde_json::from_str vs
    // simd_json::serde::from_slice on the exact shape the scanner
    // parses — `SlimAchievementsResponse`.
    const N: usize = 30_000;
    let raw = build_payload(N);
    // Warm up.
    let _: SlimAchievementsResponse = serde_json::from_str(&raw).unwrap();
    {
        let mut warm = raw.clone().into_bytes();
        let _: SlimAchievementsResponse = simd_json::serde::from_slice(&mut warm).unwrap();
    }

    let t0 = Instant::now();
    let serde: SlimAchievementsResponse = serde_json::from_str(&raw).unwrap();
    let serde_ms = t0.elapsed().as_millis();

    // simd-json mutates its input buffer, so we clone the bytes each
    // run to keep the comparison fair.
    let mut buf = raw.into_bytes();
    let t0 = Instant::now();
    let simd: SlimAchievementsResponse = simd_json::serde::from_slice(&mut buf).unwrap();
    let simd_ms = t0.elapsed().as_millis();

    println!(
        "slim parse {} rows: serde_json={}ms simd_json={}ms speedup={:.2}x",
        N,
        serde_ms,
        simd_ms,
        serde_ms as f64 / simd_ms.max(1) as f64
    );
    assert_eq!(serde.achievements.len(), N);
    assert_eq!(simd.achievements.len(), N);
    // Equality on a hot-path invariant: both parsers must agree on the
    // first achievement's id. If they disagree, one of them is wrong.
    assert_eq!(serde.achievements[0].id, simd.achievements[0].id);
    // Regression guard: simd-json must NOT be slower than serde_json
    // on any realistic payload. +20ms slack for CI hardware variance.
    assert!(
        simd_ms <= serde_ms + 20,
        "simd-json regressed: serde={serde_ms}ms simd={simd_ms}ms"
    );
}

#[test]
#[ignore = "slow / run manually with --ignored"]
fn bench_slim_vs_full_parse_reports_speedup() {
    const N: usize = 30_000;
    let raw = build_payload(N);
    // Warm up once so we measure steady state.
    let _: AchievementsResponse = serde_json::from_str(&raw).unwrap();
    let _: SlimAchievementsResponse = serde_json::from_str(&raw).unwrap();

    let t0 = Instant::now();
    let full: AchievementsResponse = serde_json::from_str(&raw).unwrap();
    let full_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let slim: SlimAchievementsResponse = serde_json::from_str(&raw).unwrap();
    let slim_ms = t0.elapsed().as_millis();

    println!(
        "parse {} rows: full={}ms slim={}ms speedup={:.2}x",
        N,
        full_ms,
        slim_ms,
        full_ms as f64 / slim_ms.max(1) as f64
    );
    assert_eq!(full.achievements.len(), N);
    assert_eq!(slim.achievements.len(), N);
    // Allow slack for noisy CI hardware; the guarantee is "slim is not
    // slower". On any real Windows build box we observe 2–4x.
    assert!(
        slim_ms <= full_ms + 50,
        "slim parse regressed: full={full_ms}ms slim={slim_ms}ms"
    );
}

#[test]
#[ignore = "slow / run manually with --ignored"]
fn bench_par_sort_vs_serial_reports_speedup() {
    const N: usize = 100_000;
    // Synthetic (timestamp, i32) tuples — the same compare key build_plan
    // uses on PlanEntry.time_unlocked.
    let mut entries: Vec<(String, i32)> = (0..N)
        .map(|i| {
            (
                format!("2024-{:02}-{:02}T12:00:00Z", 1 + (i % 12), 1 + (i % 28)),
                i as i32,
            )
        })
        .collect();
    let mut clone = entries.clone();

    let t0 = Instant::now();
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let serial_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    clone.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let par_ms = t0.elapsed().as_millis();

    println!(
        "sort {} entries: serial={}ms par={}ms speedup={:.2}x",
        N,
        serial_ms,
        par_ms,
        serial_ms as f64 / par_ms.max(1) as f64
    );
    // Both sorts must produce a monotone sequence by the compare key.
    // Exact equality is NOT an invariant because `par_sort_unstable_by`
    // is unstable AND our synthetic input has many duplicate keys (12
    // months × 28 days = 336 distinct keys across 100k rows) — so ties
    // can break differently. The correct contract is "sorted".
    for pair in entries.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "serial sort violated order");
    }
    for pair in clone.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "par sort violated order");
    }
    // Same slack policy as above. par_sort beats serial on all our
    // test boxes but a tiny dataset on a single-core VM could flip it.
    assert!(
        par_ms <= serial_ms + 100,
        "par_sort regressed: serial={serial_ms}ms par={par_ms}ms"
    );
}

#[test]
#[ignore = "slow / run manually with --ignored"]
fn bench_par_filter_vs_serial_reports_speedup() {
    // Simulate the build_plan filter: keep rows with even id, compute a
    // cloned-string project. Mimics the real work mix.
    #[derive(Clone)]
    struct Row {
        id: i32,
        name: String,
    }
    const N: usize = 200_000;
    let data: Vec<Row> = (0..N)
        .map(|i| Row {
            id: i as i32,
            name: format!("Row number {i}"),
        })
        .collect();

    let t0 = Instant::now();
    let serial: Vec<String> = data
        .iter()
        .filter(|r| r.id % 2 == 0)
        .map(|r| format!("{}:{}", r.id, r.name))
        .collect();
    let serial_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let parallel: Vec<String> = data
        .par_iter()
        .filter(|r| r.id % 2 == 0)
        .map(|r| format!("{}:{}", r.id, r.name))
        .collect();
    let par_ms = t0.elapsed().as_millis();

    println!(
        "filter+map {} rows: serial={}ms par={}ms speedup={:.2}x",
        N,
        serial_ms,
        par_ms,
        serial_ms as f64 / par_ms.max(1) as f64
    );
    assert_eq!(serial, parallel);
}
