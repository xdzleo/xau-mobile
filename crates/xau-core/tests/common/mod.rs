//! Shared test-only infrastructure. Compiled into each integration
//! test binary that adds `mod common;` at the top.
//!
//! Why this layout: Cargo's integration test model compiles each
//! `tests/*.rs` as a separate crate. Shared helpers go in
//! `tests/common/mod.rs` + sub-files so multiple test files can
//! pull them in via `mod common;`. Files under `common/` are NOT
//! compiled as test crates themselves — they're only included by
//! siblings that explicitly `mod common;`.

#![allow(dead_code)] // not every test uses every helper

pub mod mock_xbox;
