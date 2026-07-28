//! Misc system integration helpers — port of the small Windows-specific
//! utilities scattered across `HomeViewModel`, `HttpServer` and
//! `FileDownloader` in the original C# app.
//!
//! Every function either succeeds, returns an informative error, or returns
//! `Error::Unsupported` on non-Windows hosts. None of these invoke GUI
//! prompts.

pub mod firewall;
pub mod launch;
pub mod updater;
