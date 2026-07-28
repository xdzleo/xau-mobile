//! Smoke-tests the events bundle rendering pipeline against the real
//! on-disk bundle at `%USERPROFILE%\Documents\XAU\Events`. Skipped unless
//! the bundle is present (which is the case on a dev box that's run the
//! download at least once).

use std::path::PathBuf;

use xau_core::events::EventsBundle;

fn events_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.document_dir().map(std::path::Path::to_path_buf))
        .expect("home dir")
        .join("XAU")
        .join("Events")
}

#[test]
fn bundle_loads_and_renders_halo_mcc_event() {
    let dir = events_dir();
    if !dir.join("Data.json").exists() {
        eprintln!("skipping: bundle not installed at {}", dir.display());
        return;
    }

    let bundle = EventsBundle::load(&dir).expect("load bundle");
    assert!(
        bundle.is_event_based("1144039928"),
        "Halo MCC should be supported"
    );

    // Render Halo MCC achievement 127 for a dummy XUID.
    let rendered = bundle
        .render_event("1144039928", "127", "2535464746664274")
        .expect("render Halo MCC ach 127");

    // Must parse back as JSON (we compact via serde at the end of render).
    let parsed: serde_json::Value =
        serde_json::from_str(&rendered).expect("rendered event is valid JSON");

    // No leftover REPLACE* tokens.
    let s = serde_json::to_string(&parsed).unwrap();
    for token in [
        "REPLACEEVENT",
        "REPLACEEVENTORI",
        "REPLACENAME",
        "REPLACENAMEORI",
        "REPLACEDATA",
        "REPLACETIME",
        "REPLACESEQ",
        "REPLACEXUID",
        "REPLACECRITERIA1",
        "REPLACECRITERIA2",
        "REPLACECRITERIA3",
    ] {
        assert!(
            !s.contains(token),
            "rendered event still has `{token}` placeholder:\n{s}"
        );
    }

    // The XUID placeholder must have been substituted with our dummy.
    assert!(
        s.contains("2535464746664274"),
        "rendered event is missing the XUID we passed in:\n{s}"
    );

    eprintln!("Rendered Halo MCC ach 127:\n{s}");
}

#[test]
fn bundle_is_supported_reports_true_only_for_known_titles() {
    let dir = events_dir();
    if !dir.join("Data.json").exists() {
        return;
    }
    let bundle = EventsBundle::load(&dir).unwrap();
    assert!(bundle.is_event_based("1144039928"));
    assert!(!bundle.is_event_based("999999999999"));
    // Spot-check we picked up a reasonable number of titles.
    assert!(bundle.supported_titles().len() > 50);
}
