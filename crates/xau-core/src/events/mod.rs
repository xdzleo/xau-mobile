//! Event-based (telemetry) achievement unlock path.
//!
//! Most modern Xbox achievements are **stat-based** — they track gameplay
//! counters (kills, distances, quest completions, …). Xbox Live refuses to
//! update these via the straightforward `/achievements/{scid}/update`
//! endpoint ("code 49 · None of the submitted achievements may be updated
//! in this fashion"). The only way to actually unlock them without playing
//! the game is to POST a synthetic *telemetry event* to
//! `v20.events.data.microsoft.com/OneCollector/1.0/`, the same pipe the
//! real game client uses.
//!
//! The Fumo-Unlockers community project maintains an `Events.zip` bundle
//! containing one JSON template per supported title plus a `Data.json`
//! index mapping `(title_id, achievement_id)` → the text replacements
//! needed to populate that template. This module downloads the bundle,
//! parses the index, applies replacements to the template, and POSTs the
//! resulting event body to OneCollector with the user's **events token**
//! (a separate short-lived credential they paste into settings — it is
//! not the same as the `XBL3.0` auth header).
//!
//! The flow mirrors the C# original (`UnlockEventBasedAchievement`) byte
//! for byte so bundles built for the upstream XAU also work here.
//!
//! Usage:
//!
//! ```no_run
//! use std::path::PathBuf;
//! # async fn demo(xauth: &str, events_token: &str) -> xau_core::Result<()> {
//! let bundle = xau_core::events::EventsBundle::load_or_download(
//!     &PathBuf::from(r"C:\Users\admin\Documents\XAU\Events"),
//! ).await?;
//! if bundle.is_event_based("1144039928") {
//!     bundle.unlock("1144039928", "127", "2535464746664274", xauth, events_token).await?;
//! }
//! # Ok(()) }
//! ```

pub mod bundle;
pub mod etw;
pub mod replacement;
pub mod unlock;

pub use bundle::{EventsBundle, TitleData};
pub use etw::is_events_rp_token;
pub use replacement::Replacement;
