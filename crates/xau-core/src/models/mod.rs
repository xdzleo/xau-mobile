//! Domain models mirroring the Xbox Live REST API request/response shapes.
//!
//! Field naming follows the original C# source for wire-compatibility:
//! - Types that Xbox Live returns in `PascalCase` use `#[serde(rename_all = "PascalCase")]`.
//! - Types that Xbox Live returns in `camelCase` (achievements endpoint, mostly)
//!   use `#[serde(rename_all = "camelCase")]`.

pub mod achievements;
pub mod games;
pub mod github;
pub mod profile;
pub mod spoofing;

pub use achievements::*;
pub use games::*;
pub use github::*;
pub use profile::*;
pub use spoofing::*;
