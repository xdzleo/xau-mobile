use std::fs;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, UserDirs};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Equivalent of the C# `XAUSettings` model persisted at
/// `%USERPROFILE%/Documents/XAU/settings.json`.
///
/// Field names match the original JSON on the wire (PascalCase) so that an
/// existing `settings.json` produced by the C# application is loaded as-is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    pub settings_version: Option<String>,
    pub tool_version: Option<String>,
    pub unlock_all_enabled: bool,
    pub auto_spoofer_enabled: bool,
    pub auto_launch_xbox_app_enabled: bool,
    pub launch_hidden: bool,
    pub fake_signature_enabled: bool,
    pub region_override: bool,
    pub use_acrylic: bool,
    pub privacy_mode: bool,
    pub o_auth_login: bool,
    /// Events (telemetry) token used for event-based achievement unlocks.
    /// Separate from the XBL3.0 auth — users paste this from the XAU
    /// events guide. An empty / missing value disables the event-based
    /// unlock path (the app falls back to the direct `/achievements/update`
    /// endpoint, which only works for non-event-based titles).
    ///
    /// Alias list lets the GUI send either `events_token` (snake_case
    /// from the JS payload) or `EventsToken` (the canonical PascalCase
    /// this struct uses on disk).
    #[serde(alias = "events_token", alias = "EventsToken")]
    pub events_token: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settings_version: Some("2".into()),
            tool_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            unlock_all_enabled: false,
            auto_spoofer_enabled: false,
            auto_launch_xbox_app_enabled: false,
            launch_hidden: false,
            fake_signature_enabled: true,
            region_override: false,
            use_acrylic: false,
            privacy_mode: false,
            o_auth_login: false,
            events_token: None,
        }
    }
}

impl Settings {
    pub fn load_or_default() -> Result<Self> {
        let path = settings_path()?;
        if path.exists() {
            // BUG HUNTER serde-MED-1 (wave 4): if the user hand-edits
            // settings.json with a type-mismatched value (e.g.
            // `"UnlockAllEnabled": "yes"` — string instead of bool),
            // serde returns Err on every CLI/GUI startup, the launch
            // code panics, and the user has no way to recover without
            // deleting the file. Mirror `AuthSession::load_from`'s
            // soft-fail: log a tracing warning and fall back to
            // defaults so the app still boots. The user can then fix
            // the file or hit "Save" in the UI to overwrite it.
            match Self::load_from(&path) {
                Ok(s) => Ok(s),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        err = %e,
                        "settings.json failed to deserialize — falling back to defaults so the app still boots"
                    );
                    Ok(Self::default())
                }
            }
        } else {
            let s = Self::default();
            s.save_to(&path)?;
            Ok(s)
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let parsed = serde_json::from_str(&raw)?;
        Ok(parsed)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&settings_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // Wave-10/11 audit cross-cutting #4 (HIGH user data loss):
        // route through the shared `write_atomic` helper. Earlier
        // wave-10 fix inlined the same temp+pid+rename pattern
        // here; wave-11 extracted it to `fs_atomic` so all 4 of
        // the divergent torn-write sites share one implementation.
        crate::fs_atomic::write_atomic(path, json.as_bytes())?;
        Ok(())
    }
}

/// Snake_case wire representation of [`Settings`] for the GUI IPC
/// boundary.
///
/// The on-disk `Settings` uses `rename_all = "PascalCase"` so an
/// existing C# `settings.json` loads as-is — but that made
/// `settings.get` emit PascalCase keys (`UnlockAllEnabled`, …) that
/// the JS front-end (which reads/writes snake_case like every other
/// IPC DTO) saw as `undefined`. Result: the whole Settings panel
/// rendered every toggle OFF and silently dropped saves. This DTO is
/// the snake_case bridge: `settings.get` emits it, `settings.save`
/// parses it, and only disk I/O keeps the PascalCase shape. Audit:
/// settings-ipc-pascalcase-vs-snakecase-drift.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SettingsWire {
    pub settings_version: Option<String>,
    pub tool_version: Option<String>,
    pub unlock_all_enabled: bool,
    pub auto_spoofer_enabled: bool,
    pub auto_launch_xbox_app_enabled: bool,
    pub launch_hidden: bool,
    pub fake_signature_enabled: bool,
    pub region_override: bool,
    pub use_acrylic: bool,
    pub privacy_mode: bool,
    pub o_auth_login: bool,
    pub events_token: Option<String>,
}

impl From<&Settings> for SettingsWire {
    fn from(s: &Settings) -> Self {
        Self {
            settings_version: s.settings_version.clone(),
            tool_version: s.tool_version.clone(),
            unlock_all_enabled: s.unlock_all_enabled,
            auto_spoofer_enabled: s.auto_spoofer_enabled,
            auto_launch_xbox_app_enabled: s.auto_launch_xbox_app_enabled,
            launch_hidden: s.launch_hidden,
            fake_signature_enabled: s.fake_signature_enabled,
            region_override: s.region_override,
            use_acrylic: s.use_acrylic,
            privacy_mode: s.privacy_mode,
            o_auth_login: s.o_auth_login,
            events_token: s.events_token.clone(),
        }
    }
}

impl From<SettingsWire> for Settings {
    fn from(w: SettingsWire) -> Self {
        Self {
            settings_version: w.settings_version,
            tool_version: w.tool_version,
            unlock_all_enabled: w.unlock_all_enabled,
            auto_spoofer_enabled: w.auto_spoofer_enabled,
            auto_launch_xbox_app_enabled: w.auto_launch_xbox_app_enabled,
            launch_hidden: w.launch_hidden,
            fake_signature_enabled: w.fake_signature_enabled,
            region_override: w.region_override,
            use_acrylic: w.use_acrylic,
            privacy_mode: w.privacy_mode,
            o_auth_login: w.o_auth_login,
            events_token: w.events_token,
        }
    }
}

/// Returns `%USERPROFILE%/Documents/XAU/`, creating the directory if missing.
///
/// Matches the C# behavior using `Environment.SpecialFolder.MyDocuments`.
pub fn settings_dir() -> Result<PathBuf> {
    let docs = UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf))
        .ok_or_else(|| Error::other("could not resolve user Documents directory"))?;
    let dir = docs.join("XAU");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(settings_dir()?.join("settings.json"))
}

/// Returns `%LOCALAPPDATA%/XAU/` — the home for SECRET-bearing files
/// (`auth.json`, `token-pool.json`).
///
/// Unlike [`settings_dir`] (Documents/XAU, kept for C# `settings.json`
/// interop), LocalAppData is NOT synced by OneDrive, so live refresh
/// tokens and XBL3.0 bearers never leave the machine in plaintext.
/// Audit: plaintext-creds-in-onedrive-synced-documents. Creates the
/// directory if missing.
pub fn credentials_dir() -> Result<PathBuf> {
    let base = BaseDirs::new()
        .map(|b| b.data_local_dir().to_path_buf())
        .ok_or_else(|| Error::other("could not resolve LocalAppData directory"))?;
    let dir = base.join("XAU");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Resolve a secret-bearing file under [`credentials_dir`], migrating
/// it out of the legacy OneDrive-synced Documents/XAU location on
/// first access. Idempotent and cheap — short-circuits once the file
/// exists at the new path.
pub fn credential_path(file_name: &str) -> Result<PathBuf> {
    let new = credentials_dir()?.join(file_name);
    migrate_legacy_credential(file_name, &new);
    Ok(new)
}

/// Move a credential file from the old Documents/XAU location to its
/// new LocalAppData home if it hasn't been moved yet. Best-effort:
/// any failure falls through to the new (possibly absent) path and the
/// caller creates a fresh file.
fn migrate_legacy_credential(file_name: &str, new_path: &Path) {
    if new_path.exists() {
        return;
    }
    let Ok(old_dir) = settings_dir() else {
        return;
    };
    let old_path = old_dir.join(file_name);
    if !old_path.exists() {
        return;
    }
    // `rename` is atomic but fails across volumes (Documents and
    // LocalAppData can live on different drives); fall back to
    // copy + remove.
    if fs::rename(&old_path, new_path).is_ok() {
        tracing::info!(
            file = file_name,
            "migrated credential Documents/XAU → LocalAppData/XAU"
        );
        return;
    }
    if fs::copy(&old_path, new_path).is_ok() {
        let _ = fs::remove_file(&old_path);
        tracing::info!(
            file = file_name,
            "migrated credential (copy) → LocalAppData/XAU"
        );
    }
}

pub fn auth_path() -> Result<PathBuf> {
    credential_path("auth.json")
}

pub fn events_dir() -> Result<PathBuf> {
    let dir = settings_dir()?.join("Events");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_round_trippable() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert!(parsed.fake_signature_enabled);
        assert_eq!(parsed.settings_version.as_deref(), Some("2"));
    }

    #[test]
    fn deserializes_csharp_written_json() {
        let raw = r#"{
            "SettingsVersion": "2",
            "ToolVersion": "2.8.1",
            "UnlockAllEnabled": false,
            "AutoSpooferEnabled": false,
            "AutoLaunchXboxAppEnabled": true,
            "LaunchHidden": false,
            "FakeSignatureEnabled": true,
            "RegionOverride": true,
            "UseAcrylic": false,
            "PrivacyMode": false,
            "OAuthLogin": false
        }"#;
        let parsed: Settings = serde_json::from_str(raw).unwrap();
        assert!(parsed.auto_launch_xbox_app_enabled);
        assert!(parsed.region_override);
    }

    #[test]
    fn wire_dto_uses_snake_case_keys_and_roundtrips() {
        // The GUI IPC boundary MUST speak snake_case. Regression guard
        // for settings-ipc-pascalcase-vs-snakecase-drift: the disk
        // `Settings` is PascalCase, but `SettingsWire` (what
        // `settings.get`/`settings.save` use) has to match the JS.
        let s = Settings {
            fake_signature_enabled: true,
            launch_hidden: true,
            events_token: Some("tok".into()),
            ..Settings::default()
        };
        let v = serde_json::to_value(SettingsWire::from(&s)).unwrap();
        assert!(
            v.get("fake_signature_enabled").is_some(),
            "must be snake_case"
        );
        assert!(v.get("launch_hidden").is_some());
        assert!(v.get("events_token").is_some());
        assert!(
            v.get("FakeSignatureEnabled").is_none(),
            "must NOT leak PascalCase to the GUI"
        );
        // Round-trip through the wire form preserves values.
        let wire: SettingsWire = serde_json::from_value(v).unwrap();
        let back: Settings = wire.into();
        assert!(back.fake_signature_enabled);
        assert!(back.launch_hidden);
        assert_eq!(back.events_token.as_deref(), Some("tok"));
    }

    #[test]
    fn save_settings_accepts_snake_case_payload() {
        // The JS save path sends snake_case booleans; the wire DTO must
        // parse them (the old code deserialized into PascalCase Settings
        // and silently dropped every one).
        let payload = serde_json::json!({
            "unlock_all_enabled": true,
            "fake_signature_enabled": false,
            "launch_hidden": true,
            "events_token": "abc"
        });
        let wire: SettingsWire = serde_json::from_value(payload).unwrap();
        let s: Settings = wire.into();
        assert!(s.unlock_all_enabled);
        assert!(!s.fake_signature_enabled);
        assert!(s.launch_hidden);
        assert_eq!(s.events_token.as_deref(), Some("abc"));
    }
}
