//! Persistent session file at `%USERPROFILE%/Documents/XAU/auth.json`.
//!
//! Stores the Microsoft refresh token + the last XAUTH so subsequent `xau`
//! runs can reuse them without going through the full device-code flow again.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::oauth::device_code::MicrosoftOAuthToken;
use crate::settings;

/// Convenience tuple exposed via `pub use` — the CLI reads it directly.
pub const XAUTH_AND_SESSION: (&str, &str) = ("xauth", "session");

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSession {
    pub refresh_token: Option<String>,
    pub access_token: Option<String>,
    pub xauth: Option<String>,
    pub events_xauth: Option<String>,
    pub issued_at_unix: i64,
    pub expires_in: i64,
}

// Manual Debug — never print raw tokens. A future
// `tracing::debug!(?session, ...)` would otherwise dump every
// credential into logs.
impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field(
                "refresh_token",
                &self.refresh_token.as_deref().map(|_| "<redacted>"),
            )
            .field(
                "access_token",
                &self.access_token.as_deref().map(|_| "<redacted>"),
            )
            .field("xauth", &self.xauth.as_deref().map(|_| "<redacted>"))
            .field(
                "events_xauth",
                &self.events_xauth.as_deref().map(|_| "<redacted>"),
            )
            .field("issued_at_unix", &self.issued_at_unix)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl AuthSession {
    pub fn from_oauth(token: &MicrosoftOAuthToken) -> Self {
        Self {
            refresh_token: if token.refresh_token.is_empty() {
                None
            } else {
                Some(token.refresh_token.clone())
            },
            access_token: if token.access_token.is_empty() {
                None
            } else {
                Some(token.access_token.clone())
            },
            xauth: None,
            events_xauth: None,
            issued_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
                .unwrap_or(0),
            expires_in: token.expires_in,
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = settings::auth_path()?;
        self.save_to(&path)
    }

    /// Path-explicit sibling of [`Self::save`] — mirrors [`Self::load_from`].
    /// Exists so the encrypt/decrypt round trip is testable without
    /// touching the user's real `auth.json`.
    ///
    /// # Errors
    /// Propagates directory-creation, serialization and write failures.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Encrypt at rest when the platform can. A DPAPI failure is NOT
        // fatal — falling back to plaintext keeps a user logged in on a
        // machine with a broken crypto provider, which beats bricking
        // sign-in over a hardening measure. `load_from` reads both.
        let bytes: Vec<u8> = match crate::oauth::dpapi::protect(json.as_bytes()) {
            Ok(blob) => {
                let mut out = Vec::with_capacity(crate::oauth::dpapi::MAGIC.len() + blob.len());
                out.extend_from_slice(crate::oauth::dpapi::MAGIC);
                out.extend_from_slice(&blob);
                out
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "could not DPAPI-encrypt auth.json; writing plaintext"
                );
                json.into_bytes()
            }
        };
        // Atomic write: temp + rename. A bare `fs::write` truncates the
        // existing file before writing — a crash, BSOD, or concurrent
        // `save()` mid-flight leaves `auth.json` empty/half-written and
        // the next `load()` reports `Ok(None)` (the `from_str(...).ok()`
        // in `load_from`), forcing a full device-code re-login. Same
        // pattern + retry policy as `token_pool::save_to_disk`.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        let mut last_err: Option<std::io::Error> = None;
        for attempt in 0..6u32 {
            match fs::rename(&tmp, &path) {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(50u64 << attempt.min(4)));
                }
            }
        }
        if let Some(e) = last_err {
            // Canonical file is unchanged (rename never landed); clean
            // the temp so it doesn't sync to OneDrive plaintext.
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    pub fn load() -> Result<Option<Self>> {
        let path = settings::auth_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        // Read as BYTES: an encrypted file is not valid UTF-8, so the
        // old `read_to_string` would have failed outright on it.
        let bytes = fs::read(path)?;
        let magic = crate::oauth::dpapi::MAGIC;
        let raw = if bytes.starts_with(magic) {
            match crate::oauth::dpapi::unprotect(&bytes[magic.len()..]) {
                Ok(plain) => String::from_utf8_lossy(&plain).into_owned(),
                Err(e) => {
                    // Wrong user, wrong machine, or a corrupt blob. Same
                    // contract as a parse failure below: no session, with
                    // a breadcrumb — never a hard error that blocks login.
                    tracing::warn!(
                        path = %path.display(),
                        err = %e,
                        "auth.json could not be decrypted (copied from another                          user or machine?) — treating as no-session"
                    );
                    return Ok(None);
                }
            }
        } else if bytes.starts_with(crate::oauth::dpapi::UPSTREAM_MAGIC) {
            // The upstream C# XAU's session file. Both apps use
            // `%LOCALAPPDATA%/XAU/auth.json`, so on a machine running
            // both this is what we find. Its payload is a different
            // struct sealed with different entropy — unreadable here,
            // and NOT corrupt. Report no-session with an honest reason
            // instead of the misleading "format may have changed".
            tracing::info!(
                path = %path.display(),
                "auth.json belongs to the upstream XAU app — sign in here to                  create a separate session"
            );
            return Ok(None);
        } else {
            // Legacy plaintext from before encryption existed. Read it;
            // the next `save()` rewrites it encrypted.
            String::from_utf8_lossy(&bytes).into_owned()
        };
        // BUG HUNTER M-auth-corrupt-loud: previously `serde_json::from_str(&raw).ok()`
        // silently discarded parse errors, so a stale-format auth.json
        // (e.g. after a Rust-side schema change) presented as
        // "no session, please re-login" with NO diagnostic. Users
        // hit infinite re-login loops on corrupted files.
        // Now: log the parse error at warn level + return Ok(None)
        // so the CLI / GUI re-login path still works, but the user
        // gets a breadcrumb in the trace log explaining why.
        match serde_json::from_str::<Self>(&raw) {
            Ok(s) => {
                // Readable but USELESS: no refresh token means silent
                // re-auth can never succeed, so this file is dead weight
                // that still holds an access token and two XSTS bearers.
                // Delete it rather than leave credentials lying around
                // for a session we can't revive (upstream XAU#480 calls
                // this "lingering data from expired auth.json").
                //
                // Deliberately NARROWER than upstream, which also deletes
                // when the file merely fails to READ. A decrypt failure
                // can be transient (or a DPAPI hiccup), and destroying a
                // recoverable session over it is worse than keeping a
                // file we ignore this once.
                if s.refresh_token.as_deref().unwrap_or_default().is_empty() {
                    tracing::info!(
                        path = %path.display(),
                        "auth.json has no refresh token — removing the dead session file"
                    );
                    let _ = fs::remove_file(path);
                    return Ok(None);
                }
                Ok(Some(s))
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err = %e,
                    raw_len = raw.len(),
                    "auth.json failed to deserialize — treating as no-session; \
                     re-login required. (file format may have changed across \
                     versions, or a previous write was truncated.)"
                );
                Ok(None)
            }
        }
    }

    pub fn delete() -> Result<()> {
        let path = settings::auth_path()?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AuthSession {
        AuthSession {
            refresh_token: Some("M.C543_SN1.0.U.REFRESH-SECRET".into()),
            access_token: Some("EwAoA8l6BAAUO9chh8cJ".into()),
            xauth: Some("XBL3.0 x=123456;eyJhbGciOi".into()),
            events_xauth: Some("XBL3.0 x=123456;eyJlbmMiOi".into()),
            issued_at_unix: 1_700_000_000,
            expires_in: 86_400,
        }
    }

    #[test]
    fn save_then_load_round_trips_every_field() {
        let dir = std::env::temp_dir().join(format!("xau-sess-rt-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        sample().save_to(&path).expect("save");

        let back = AuthSession::load_from(&path)
            .expect("load must not error")
            .expect("a session must be present");
        assert_eq!(back.refresh_token, sample().refresh_token);
        assert_eq!(back.access_token, sample().access_token);
        assert_eq!(back.xauth, sample().xauth);
        assert_eq!(back.events_xauth, sample().events_xauth);
        assert_eq!(back.issued_at_unix, 1_700_000_000);
        assert_eq!(back.expires_in, 86_400);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The point of the whole change: a refresh token must not be
    /// recoverable by reading the file. On Windows the bytes are DPAPI
    /// ciphertext; elsewhere encryption is unavailable and we
    /// deliberately keep working in plaintext, so only assert there.
    #[cfg(windows)]
    #[test]
    fn the_written_file_does_not_contain_the_token_in_the_clear() {
        let dir = std::env::temp_dir().join(format!("xau-sess-enc-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        sample().save_to(&path).expect("save");

        let bytes = fs::read(&path).expect("read back");
        assert!(
            bytes.starts_with(crate::oauth::dpapi::MAGIC),
            "encrypted files must carry the magic header"
        );
        assert!(
            !bytes
                .windows(b"REFRESH-SECRET".len())
                .any(|w| w == b"REFRESH-SECRET"),
            "the refresh token is sitting in auth.json in plaintext"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Upgrade path. An install predating encryption has a plain JSON
    /// auth.json; if this regresses, every existing user silently logs
    /// out on update — the exact failure that makes people re-login in
    /// a loop and blame the app.
    #[test]
    fn a_legacy_plaintext_file_still_loads() {
        let dir = std::env::temp_dir().join(format!("xau-sess-legacy-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(&path, serde_json::to_vec(&sample()).expect("json")).expect("write plaintext");

        let back = AuthSession::load_from(&path)
            .expect("load must not error")
            .expect("legacy plaintext must still produce a session");
        assert_eq!(back.refresh_token, sample().refresh_token);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A session with no refresh token can never be silently revived,
    /// so it must not be left on disk still holding bearer tokens.
    #[test]
    fn a_session_without_a_refresh_token_is_purged() {
        let dir = std::env::temp_dir().join(format!("xau-sess-purge-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        let mut dead = sample();
        dead.refresh_token = None;
        dead.save_to(&path).expect("save");

        let got = AuthSession::load_from(&path).expect("must not error");
        assert!(got.is_none(), "an unrevivable session must read as none");
        assert!(
            !path.exists(),
            "the dead session file must be deleted, not left holding tokens"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A machine with the upstream C# XAU installed has ITS session at
    /// the same path. Reading it must not be reported as corruption,
    /// and must not produce a bogus session — the real captured file
    /// starts with `XAU1` followed by a DPAPI blob.
    #[test]
    fn the_upstream_apps_session_file_is_recognised_not_mistaken_for_corruption() {
        let dir = std::env::temp_dir().join(format!("xau-sess-upstream-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut bytes = crate::oauth::dpapi::UPSTREAM_MAGIC.to_vec();
        // First bytes of a real DPAPI blob (the provider GUID).
        bytes.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0xd0, 0x8c, 0x9d, 0xdf, 0x01, 0x15, 0xd1, 0x11,
        ]);
        fs::write(&path, &bytes).expect("write");

        let got = AuthSession::load_from(&path).expect("must not be a hard error");
        assert!(got.is_none(), "another app's session must read as none");
        assert!(
            path.exists(),
            "the other app's session file must be left alone, not deleted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file we cannot decrypt (copied from another machine/user) must
    /// degrade to "please log in", never to a hard error that blocks
    /// the app from starting.
    #[cfg(windows)]
    #[test]
    fn an_undecryptable_file_reports_no_session_rather_than_erroring() {
        let dir = std::env::temp_dir().join(format!("xau-sess-bad-{}", std::process::id()));
        let path = dir.join("auth.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut bytes = crate::oauth::dpapi::MAGIC.to_vec();
        bytes.extend_from_slice(b"this is not a valid DPAPI blob at all");
        fs::write(&path, &bytes).expect("write");

        let got = AuthSession::load_from(&path).expect("must not be a hard error");
        assert!(got.is_none(), "undecryptable file must read as no-session");
        let _ = fs::remove_dir_all(&dir);
    }
}
