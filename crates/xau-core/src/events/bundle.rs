//! Download + parse the Fumo-Unlockers `Events.zip` bundle.
//!
//! The bundle contains:
//! - `Events/Data.json` — per-title index of supported achievements +
//!   their replacement rules
//! - `Events/<titleId>.json` — one JSON template per supported title,
//!   with literal placeholder tokens like `REPLACEEVENT`, `REPLACEDATA`,
//!   `REPLACETIME`, `REPLACESEQ`, `REPLACEXUID`
//!
//! We cache the extracted bundle under a caller-provided directory (the
//! GUI / CLI pass `%USERPROFILE%\Documents\XAU\Events` to match the
//! upstream layout) so the scan isn't redone on every launch.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use reqwest::Client;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::xbox::constants::events_urls;

use super::replacement::Replacement;

/// Parsed replacement rules for one achievement.
#[derive(Debug, Clone)]
pub struct AchievementRules {
    /// The rules are stored in insertion order so `apply_all` can run
    /// them deterministically (matches the C# `foreach` iteration over
    /// a JObject).
    pub rules: Vec<Replacement>,
}

/// Per-title data loaded from `Data.json`.
#[derive(Debug, Clone, Default)]
pub struct TitleData {
    pub fully_supported: bool,
    pub achievements: HashMap<String, AchievementRules>,
}

/// In-memory view of the whole bundle.
pub struct EventsBundle {
    events_dir: PathBuf,
    supported_titles: HashSet<String>,
    per_title: HashMap<String, TitleData>,
}

impl EventsBundle {
    /// Directory where the template JSONs live.
    pub fn events_dir(&self) -> &Path {
        &self.events_dir
    }

    /// Title IDs that have at least one entry in `Data.json`.
    pub fn supported_titles(&self) -> &HashSet<String> {
        &self.supported_titles
    }

    /// `true` if an event-based template is available for this title.
    pub fn is_event_based(&self, title_id: &str) -> bool {
        self.supported_titles.contains(title_id)
    }

    /// Load the bundle from `events_dir` if it already exists there,
    /// otherwise download `Events.zip` + extract it first. Safe to call
    /// on every app start — subsequent calls only re-read `Data.json`.
    pub async fn load_or_download(events_dir: &Path) -> Result<Self> {
        let data_json_path = events_dir.join("Data.json");
        if !data_json_path.exists() {
            tracing::info!(dir = %events_dir.display(), "events bundle missing, downloading");
            download_and_extract(events_dir).await?;
        }
        Self::load(events_dir)
    }

    /// Load a bundle that's already been extracted to `events_dir`.
    pub fn load(events_dir: &Path) -> Result<Self> {
        let data_json_path = events_dir.join("Data.json");
        if !data_json_path.exists() {
            return Err(Error::not_found(format!(
                "events bundle not found at {}",
                data_json_path.display()
            )));
        }
        let raw = std::fs::read_to_string(&data_json_path)
            .map_err(|e| Error::other(format!("reading Data.json: {e}")))?;
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|e| Error::other(format!("parsing Data.json: {e}")))?;

        let obj = parsed
            .as_object()
            .ok_or_else(|| Error::other("Data.json must be a JSON object at the top level"))?;

        // Keep the legacy `SupportedTitleIDs` array if present as a
        // source of truth — and also any other object-keyed entry that
        // looks like a numeric title id. Newer bundles drop the array
        // and rely on keys, so we merge both.
        let mut supported: HashSet<String> = HashSet::new();
        if let Some(arr) = obj.get("SupportedTitleIDs").and_then(Value::as_array) {
            for v in arr {
                if let Some(n) = v.as_u64() {
                    supported.insert(n.to_string());
                } else if let Some(s) = v.as_str() {
                    supported.insert(s.to_string());
                }
            }
        }

        let mut per_title: HashMap<String, TitleData> = HashMap::new();
        for (k, v) in obj {
            if k == "SupportedTitleIDs" {
                continue;
            }
            // Keys that aren't numeric aren't title data — skip any
            // top-level metadata the bundle authors add in the future.
            if k.chars().all(|c| c.is_ascii_digit()) {
                if let Some(td) = parse_title_data(v) {
                    supported.insert(k.clone());
                    per_title.insert(k.clone(), td);
                }
            }
        }

        tracing::info!(
            dir = %events_dir.display(),
            titles_supported = supported.len(),
            titles_with_rules = per_title.len(),
            "events bundle loaded"
        );

        Ok(Self {
            events_dir: events_dir.to_path_buf(),
            supported_titles: supported,
            per_title,
        })
    }

    /// Apply every replacement rule for `(title_id, achievement_id)` to
    /// the on-disk template `events_dir/{title_id}.json`, injecting
    /// `xuid`, a fresh UTC timestamp and sequence number, and return the
    /// final POST body. Errors cleanly if the template file is missing
    /// or the (title, achievement) tuple isn't indexed by `Data.json`.
    pub fn render_event(&self, title_id: &str, achievement_id: &str, xuid: &str) -> Result<String> {
        let td = self.per_title.get(title_id).ok_or_else(|| {
            Error::not_found(format!("events bundle has no entry for title {title_id}"))
        })?;
        let rules = td.achievements.get(achievement_id).ok_or_else(|| {
            Error::not_found(format!(
                "events bundle has no entry for achievement {achievement_id} on title {title_id}"
            ))
        })?;

        let template_path = self.events_dir.join(format!("{title_id}.json"));
        let mut body = std::fs::read_to_string(&template_path).map_err(|e| {
            Error::other(format!("reading template {}: {e}", template_path.display()))
        })?;

        // Apply rules in their original order (EventReplacement first,
        // DataReplacement second, etc.) so that RangeInt / composite
        // substitutions see the already-expanded data payload.
        for rule in &rules.rules {
            rule.apply(&mut body)?;
        }

        // Final shared substitutions handled by the C# original AFTER
        // the per-achievement rules run.
        let now_iso = iso_utc_fractional_now();
        let seq = unix_seconds_now();
        body = body
            .replace("REPLACETIME", &now_iso)
            .replace("REPLACESEQ", &seq.to_string())
            .replace("REPLACEXUID", xuid);

        // Compact the JSON — OneCollector rejects bodies with template
        // whitespace left in them. Parse + re-serialise to prove the
        // output is syntactically valid.
        let compact: Value = serde_json::from_str(&body).map_err(|e| {
            Error::other(format!(
                "template produced invalid JSON: {e}\n---- body ----\n{body}"
            ))
        })?;
        serde_json::to_string(&compact).map_err(|e| Error::other(format!("recompact JSON: {e}")))
    }
}

fn parse_title_data(v: &Value) -> Option<TitleData> {
    let obj = v.as_object()?;
    let fully_supported = obj
        .get("FullySupported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let achievements_obj = obj.get("Achievements")?.as_object()?;

    let mut achievements = HashMap::with_capacity(achievements_obj.len());
    for (ach_id, rules_val) in achievements_obj {
        // Each achievement value is a bag of named replacement-rule
        // objects (EventReplacement, DataReplacement, Replacement1, …).
        // We only care about the VALUES, but we need them in the insertion
        // order the bundle authors chose, so we iterate a BTreeMap —
        // serde_json preserves insertion order by default for Object but
        // we sort the Replacement* suffixes naturally via BTreeMap keys.
        let rules_obj = rules_val.as_object()?;
        // BUG HUNTER events-rule-order: the previous `BTreeMap<String, _>`
        // sorted keys LEXICOGRAPHICALLY, so `Replacement10` ordered BEFORE
        // `Replacement2` ("1" < "2"). Any title with ≥10 numbered
        // replacements applied its rules out of order, producing a wrong
        // event body. Sort with a structured key instead: DataReplacement,
        // then EventReplacement (the C# "expand the big chunks first"
        // order), then `Replacement<N>` by NUMERIC N, then anything else
        // alphabetically.
        let mut ordered: Vec<(&String, &Value)> = rules_obj.iter().collect();
        ordered.sort_by(|a, b| replacement_rule_rank(a.0).cmp(&replacement_rule_rank(b.0)));
        let mut rules = Vec::with_capacity(ordered.len());
        for (name, val) in ordered {
            // BUG HUNTER events-rule-drop: a rule that fails to parse
            // (unknown ReplacementType, malformed remote Data.json) was
            // SILENTLY discarded, leaving an unexpanded `REPLACE…` token in
            // the rendered body that then fails JSON re-parse with an
            // opaque downstream error. Log it loudly so the cause is
            // visible instead of swallowed.
            match serde_json::from_value::<Replacement>(val.clone()) {
                Ok(rule) => rules.push(rule),
                Err(e) => tracing::warn!(
                    rule = %name,
                    err = %e,
                    "skipping unparseable replacement rule in events bundle"
                ),
            }
        }
        achievements.insert(ach_id.clone(), AchievementRules { rules });
    }
    Some(TitleData {
        fully_supported,
        achievements,
    })
}

/// Sort key for a replacement-rule name. Orders the big structural
/// expansions first (`DataReplacement`, then `EventReplacement`), then
/// the numbered `Replacement<N>` rules by NUMERIC N (so `Replacement2`
/// precedes `Replacement10`), then any other key alphabetically.
fn replacement_rule_rank(name: &str) -> (u8, u64, String) {
    match name {
        "DataReplacement" => (0, 0, String::new()),
        "EventReplacement" => (1, 0, String::new()),
        other => match other
            .strip_prefix("Replacement")
            .and_then(|n| n.parse::<u64>().ok())
        {
            Some(n) => (2, n, String::new()),
            None => (3, 0, other.to_string()),
        },
    }
}

/// Flatten a zip entry path to its location relative to the bundle's
/// `Events/` root.
///
/// BUG HUNTER events-zip-layout: the old `name.strip_prefix("Events")`
/// only matched when the FIRST path component was literally `Events`. A
/// zip whose entries are wrapped in a top-level directory (e.g. GitHub's
/// "Download ZIP" / branch tarballs name it `<repo>-<ref>/Events/123.json`)
/// failed the strip, fell through to the full nested path, and wrote
/// files to the wrong place — `render_event` later couldn't find
/// `<events_dir>/<title_id>.json`. Find the LAST `Events` component
/// (case-insensitive) and take everything after it; with no `Events`
/// component at all, fall back to the bare file name so we never write
/// outside `events_dir`.
fn flatten_events_zip_path(name: &Path) -> PathBuf {
    let comps: Vec<_> = name.components().collect();
    let events_pos = comps.iter().rposition(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("Events"))
    });
    match events_pos {
        Some(pos) => {
            let mut rel = PathBuf::new();
            for c in &comps[pos + 1..] {
                rel.push(c.as_os_str());
            }
            rel
        }
        None => name
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| name.to_path_buf()),
    }
}

async fn download_and_extract(events_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(events_dir)
        .map_err(|e| Error::other(format!("creating events dir: {e}")))?;

    let client = Client::builder()
        .gzip(true)
        .build()
        .map_err(|e| Error::other(format!("http client: {e}")))?;

    let url = events_urls::ZIP;
    tracing::info!(%url, "downloading events bundle");
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::other(format!("download events zip: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "downloading events zip failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::other(format!("download events zip body: {e}")))?;

    let reader = Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| Error::other(format!("open events zip: {e}")))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| Error::other(format!("zip entry {i}: {e}")))?;
        let name = match file.enclosed_name() {
            Some(p) => p.clone(),
            None => continue,
        };
        // The zip stores files under `Events/...` — we flatten them
        // into `events_dir` directly because the caller already named
        // the dir `...\Events`.
        let rel = flatten_events_zip_path(&name);
        let out_path = events_dir.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out_path).ok();
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|e| Error::other(format!("read zip entry: {e}")))?;
        std::fs::write(&out_path, bytes)
            .map_err(|e| Error::other(format!("write {}: {e}", out_path.display())))?;
    }
    Ok(())
}

fn iso_utc_fractional_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    // The C# original emits `yyyy-MM-ddTHH:mm:ss.fffffffZ` (7-digit
    // fraction = 100-ns precision). Rust's SystemTime gives us 9-digit
    // nanosecond precision — truncate to 7 by dividing and zero-padding.
    let ticks100 = nanos / 100;
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ticks100:07}Z")
}

fn unix_seconds_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// Same Hinnant civil-from-days algorithm used by `plan::builder`. Copied
// here instead of exported so the events module can stand alone.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the Data.json parser against a minimal fixture we construct
    /// in-memory (mirrors the real bundle's shape, just smaller).
    #[test]
    fn parses_minimal_data_json() {
        let fixture = r#"{
            "SupportedTitleIDs": [1144039928],
            "1144039928": {
                "FullySupported": false,
                "Achievements": {
                    "127": {
                        "EventReplacement": {
                            "ReplacementType": "Replace",
                            "Target": "REPLACEEVENT",
                            "Replacement": "Microsoft.XboxLive.T1144039928.AchievementEarned"
                        },
                        "DataReplacement": {
                            "ReplacementType": "Replace",
                            "Target": "REPLACEDATA",
                            "Replacement": "{\"id\":REPLACECRITERIA1}"
                        },
                        "Replacement1": {
                            "ReplacementType": "Replace",
                            "Target": "REPLACECRITERIA1",
                            "Replacement": 42
                        }
                    }
                }
            }
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path();
        std::fs::write(events_dir.join("Data.json"), fixture).unwrap();

        let bundle = EventsBundle::load(events_dir).unwrap();
        assert!(bundle.is_event_based("1144039928"));
        let td = bundle.per_title.get("1144039928").unwrap();
        assert!(!td.fully_supported);
        let ach = td.achievements.get("127").unwrap();
        assert_eq!(ach.rules.len(), 3);
    }

    #[test]
    fn replacement_rule_rank_orders_numerically_not_lexically() {
        // BUG HUNTER events-rule-order regression: `Replacement10` must
        // sort AFTER `Replacement2` (numeric), with the structural
        // expansions first. The old BTreeMap-of-strings got this wrong
        // ("Replacement10" < "Replacement2" lexically).
        let mut names = vec![
            "Replacement10",
            "Replacement2",
            "EventReplacement",
            "Replacement1",
            "DataReplacement",
            "Replacement11",
            "WeirdCustom",
        ];
        names.sort_by(|a, b| replacement_rule_rank(a).cmp(&replacement_rule_rank(b)));
        assert_eq!(
            names,
            vec![
                "DataReplacement",
                "EventReplacement",
                "Replacement1",
                "Replacement2",
                "Replacement10",
                "Replacement11",
                "WeirdCustom",
            ]
        );
    }

    #[test]
    fn flatten_events_zip_path_handles_layouts() {
        // Direct `Events/` root.
        assert_eq!(
            flatten_events_zip_path(Path::new("Events/123.json")),
            PathBuf::from("123.json")
        );
        // Wrapped in a top-level repo dir (GitHub "Download ZIP" layout).
        assert_eq!(
            flatten_events_zip_path(Path::new("Xbox-Achievement-Unlocker-main/Events/123.json")),
            PathBuf::from("123.json")
        );
        // Nested below Events — keep the sub-path.
        assert_eq!(
            flatten_events_zip_path(Path::new("Events/sub/x.json")),
            PathBuf::from("sub/x.json")
        );
        // No `Events` component at all → bare file name, never escapes
        // events_dir.
        assert_eq!(
            flatten_events_zip_path(Path::new("random/deep/file.json")),
            PathBuf::from("file.json")
        );
    }
}
