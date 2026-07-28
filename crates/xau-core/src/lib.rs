//! Core library for the Xbox Achievement Unlocker (XAU) Rust port.
//!
//! Provides the Xbox Live REST client, domain models, persistent settings,
//! GitHub / dbox.tools helpers, and a Windows-only memory reader that
//! extracts an `XBL3.0 x=…;…` token from the Xbox PC app process.

#![deny(unused_must_use)]

pub mod community;
pub mod dbox;
pub mod error;
pub mod events;
pub mod fs_atomic;
pub mod github;
pub mod memory;
pub mod models;
pub mod oauth;
pub mod plan;
pub mod settings;
pub mod system;
pub mod types;
pub mod xbox;

pub use error::{Error, Result};
pub use settings::{settings_dir, settings_path, Settings};
pub use types::{AchievementId, Gamertag, ServiceConfigId, TitleId, ValidationError, Xuid};
pub use xbox::{constants, XboxClient};
