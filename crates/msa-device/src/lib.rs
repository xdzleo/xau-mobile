//! Windows-device Microsoft account sign-in, vendored from xodus.
//!
//! Xbox only accepts the multi-title `presence-heartbeat` from a token
//! whose device is a registered Windows device: an MSA *device account*
//! (`deviceaddcredential.srf`) authenticated over RST2, as Windows itself
//! does. An anonymous ProofOfPossession device token is refused with 403
//! whatever the client id. This crate is that device side plus the user
//! side of the same protocol (the "inline" login and the silent RST2
//! ticket exchange), everything the xodus / xodus-nexus Linux launcher
//! uses to sign games in.
//!
//! Source: <https://github.com/xodus-gaming/xodus> (`crates/xodus`), the
//! backend xodus-nexus builds on, GPL-3.0-or-later like this project.
//! `models`, `clep`, `licensing` and `api::live` are copied as-is so they
//! can be diffed against upstream; `hardware`, `device` and `flow` are the
//! parts rewritten for a phone (no SMBIOS, no keychain, no panics on
//! network input).

// Upstream style in the vendored modules, kept so they diff cleanly.
#![allow(clippy::new_without_default, clippy::items_after_test_module)]

pub mod api;
pub mod clep;
pub mod device;
pub mod flow;
pub mod hardware;
pub mod licensing;
pub mod models;

pub use flow::*;
