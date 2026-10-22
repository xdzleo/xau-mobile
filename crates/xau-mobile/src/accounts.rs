//! Several accounts signed in at once.
//!
//! The desktop app assumes one identity: a single `auth.json`, a single
//! client. The phone is a switcher — you add accounts and see all of
//! them at a glance — so sessions are keyed by xuid and stored together.
//!
//! # What each row can show, and what it cannot
//!
//! Gamertag, gamerpic and gamerscore all come from one profile call.
//!
//! **Email comes from somewhere else entirely.** No Xbox Live endpoint
//! carries it: the token is scoped
//! `service::user.auth.xboxlive.com::MBI_SSL`, and widening that scope
//! would change the token the SISU handshake is signed against — which
//! is not a trade worth making for a display string. It is instead read
//! from the sign-in itself, where the user types it into Microsoft's own
//! page and it travels in the query string the login WebView is already
//! walking. See `sniff_email` in the crate root.
//!
//! The consequence is that only accounts added on this device have one.
//! A session restored from a stored token has no address, because nobody
//! typed anything — and that is reported as absent rather than guessed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::Session;
use xau_core::error::{Error, Result};

/// Where accounts are stored, when the host tells us.
///
/// `xau_core::settings::credential_path` resolves through the
/// `directories` crate, which on desktop means `%LOCALAPPDATA%` or
/// `$XDG_DATA_HOME`. **On Android that resolution is wrong**: there is
/// no `$HOME` for an app process, so it lands on `/.local/share`, which
/// is not writable — every save fails, and an app that treats a failed
/// save as a failed login then throws away a session that authenticated
/// perfectly well.
///
/// Android has exactly one correct answer and only the Java side knows
/// it: `Context.getFilesDir()`. The host passes it in at startup.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Point the account store at a host-supplied directory.
///
/// Call once, before anything reads or writes the store. Later calls
/// are ignored rather than racing a path change past an open file.
pub fn set_data_dir(dir: impl Into<PathBuf>) {
    let _ = DATA_DIR.set(dir.into());
}

/// One signed-in account as the UI sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Account {
    pub xuid: String,
    pub gamertag: String,
    /// Avatar URL from `GameDisplayPicRaw`. Empty when Xbox has none.
    pub gamerpic: String,
    pub gamerscore: i64,
    /// Rewards balance. `None` until the first read for this account
    /// lands — a row that shows 0 before it has asked is a lie.
    pub balance: Option<i64>,
    pub unclaimed: Option<i64>,
}

/// Stored credentials, one entry per account.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// xuid → xauth. A map, not a list, because "sign in again as an
    /// account already present" has to replace rather than duplicate.
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    /// xuid -> the address seen at sign-in. Separate map so an existing
    /// accounts.json without it still loads.
    #[serde(default)]
    pub emails: BTreeMap<String, String>,
    /// The single shared MSA device of builds before every sign-in got
    /// its own. Only the accounts signed in then still use it, through
    /// [`Store::device_of`]; nothing writes it any more.
    #[serde(default)]
    pub device: Option<msa_device::Device>,
    /// xuid -> the MSA device account (a "Windows PC") that account's
    /// sign-in registered. Every login registers a new one.
    #[serde(default)]
    pub devices: BTreeMap<String, msa_device::Device>,
    /// xuid -> the Windows sign-in behind it, from which a fresh xauth
    /// is issued silently. Absent for accounts added the old way.
    #[serde(default)]
    pub windows: BTreeMap<String, msa_device::UserSession>,
    /// Accounts the user froze: not a single request goes out for them
    /// (no sign-in check, no balance, no claim, no farm) until unfrozen.
    #[serde(default)]
    pub frozen: BTreeSet<String>,
    /// xuid -> the last profile Xbox returned, so a frozen account still
    /// shows its name and picture without asking for them.
    #[serde(default)]
    pub profiles: BTreeMap<String, CachedProfile>,
    /// xuid -> the last Rewards figures read while the account was live,
    /// shown (as such) on its card once it is frozen.
    #[serde(default)]
    pub rewards: BTreeMap<String, CachedRewards>,
}

/// The last Rewards read of an account, kept for when it may not ask.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedRewards {
    pub balance: i64,
    pub unclaimed: i64,
    pub level: Option<i64>,
}

/// What a row needs to draw an account it may not ask Xbox about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedProfile {
    pub gamertag: String,
    pub gamerpic: String,
    pub gamerscore: i64,
}

/// The directory this app may write to.
///
/// The host supplies it (Android's `Context.getFilesDir()`); the
/// platform default is the fallback for desktop and tests.
///
/// # Errors
/// Propagates whatever the platform resolver reports.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = DATA_DIR.get() {
        return Ok(dir.clone());
    }
    Ok(xau_core::settings::credentials_dir()?)
}

fn store_path() -> Result<PathBuf> {
    if let Some(dir) = DATA_DIR.get() {
        return Ok(dir.join("accounts.json"));
    }
    xau_core::settings::credential_path("accounts.json")
}

impl Store {
    /// Read the stored set. A missing or unreadable file is an empty
    /// set, not an error — the app must still start.
    #[must_use]
    pub fn load() -> Self {
        store_path()
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// # Errors
    /// Propagates filesystem and serialization failures.
    pub fn save(&self) -> Result<()> {
        let path = store_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Same temp+rename discipline as the desktop session file: a
        // crash mid-write must not leave every account unreadable.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn put(&mut self, xuid: impl Into<String>, xauth: impl Into<String>) {
        self.tokens.insert(xuid.into(), xauth.into());
    }

    pub fn put_email(&mut self, xuid: impl Into<String>, email: impl Into<String>) {
        self.emails.insert(xuid.into(), email.into());
    }

    #[must_use]
    pub fn email_of(&self, xuid: &str) -> Option<&str> {
        self.emails.get(xuid).map(String::as_str)
    }

    /// The device an account's Windows sign-in is bound to: its own, or
    /// the shared one of older builds.
    #[must_use]
    pub fn device_of(&self, xuid: &str) -> Option<&msa_device::Device> {
        self.devices.get(xuid).or(self.device.as_ref())
    }

    pub fn remove(&mut self, xuid: &str) -> bool {
        self.emails.remove(xuid);
        self.windows.remove(xuid);
        self.devices.remove(xuid);
        self.frozen.remove(xuid);
        self.profiles.remove(xuid);
        self.rewards.remove(xuid);
        self.tokens.remove(xuid).is_some()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Fetch the display details for a signed-in session.
///
/// # Errors
/// Propagates transport failures. A profile that simply lacks a picture
/// or gamerscore yields empty/zero rather than an error.
pub async fn describe(session: &Session) -> Result<Account> {
    let (gamerpic, gamerscore) = session
        .profile_display()
        .await
        .unwrap_or_else(|_| (String::new(), 0));
    Ok(Account {
        xuid: session.xuid().to_string(),
        gamertag: session.gamertag().to_string(),
        gamerpic,
        gamerscore,
        balance: None,
        unclaimed: None,
    })
}

/// The device's public IP, shown on the home screen so the user can see
/// which address their presence and claims are coming from.
///
/// # Errors
/// Fails when the lookup service is unreachable.
pub async fn public_ip() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| Error::other(e.to_string()))?;
    let text = client
        .get("https://api.ipify.org")
        .send()
        .await
        .map_err(|e| Error::other(format!("não foi possível obter o IP: {e}")))?
        .text()
        .await
        .map_err(|e| Error::other(e.to_string()))?;
    let ip = text.trim().to_string();
    // The service returns a bare address; anything else means we reached
    // a captive portal or an error page, and echoing that as "your IP"
    // would be worse than saying nothing.
    if ip.is_empty()
        || ip.len() > 45
        || !ip
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
    {
        return Err(Error::other("resposta inesperada do serviço de IP"));
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_in_as_an_existing_account_replaces_rather_than_duplicates() {
        let mut s = Store::default();
        s.put("2535", "XBL3.0 x=a;old");
        s.put("2535", "XBL3.0 x=a;new");
        assert_eq!(s.tokens.len(), 1);
        assert_eq!(
            s.tokens.get("2535").map(String::as_str),
            Some("XBL3.0 x=a;new")
        );
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let mut s = Store::default();
        s.put("1", "t");
        assert!(s.remove("1"));
        assert!(!s.remove("1"));
        assert!(s.is_empty());
    }

    /// The host-supplied directory must win over the `directories`
    /// crate. On Android that crate resolves to an unwritable path, and
    /// the resulting save failure once took a perfectly good login down
    /// with it — the screen showed "no accounts connected" after a
    /// successful sign-in.
    ///
    /// This is the only test that touches the global override, which is
    /// a `OnceLock` and therefore one-shot per process.
    #[test]
    fn a_host_supplied_directory_overrides_the_platform_default() {
        let dir = std::env::temp_dir().join("xau-store-test");
        set_data_dir(dir.clone());
        let path = store_path().expect("path");
        assert_eq!(path, dir.join("accounts.json"));

        // And a second call cannot move the store out from under an
        // already-open file.
        set_data_dir(std::env::temp_dir().join("somewhere-else"));
        assert_eq!(store_path().expect("path"), dir.join("accounts.json"));
    }

    /// A row must not claim a balance it has never fetched — `None`
    /// renders as a placeholder, `Some(0)` renders as a real zero.
    #[test]
    fn a_fresh_account_has_no_balance_yet() {
        let a = Account {
            xuid: "1".into(),
            gamertag: "x".into(),
            ..Default::default()
        };
        assert!(a.balance.is_none() && a.unclaimed.is_none());
    }

    fn device(name: &str) -> msa_device::Device {
        msa_device::Device {
            puid: String::new(),
            hwid: String::new(),
            device_id: String::new(),
            splicense: String::new(),
            username: name.into(),
            password: String::new(),
        }
    }

    /// Every sign-in has its own device; only accounts from the builds
    /// with one shared device fall back to it, and removing an account
    /// drops its device so signing it in again registers a new one.
    #[test]
    fn each_account_keeps_its_own_device() {
        let mut s = Store {
            device: Some(device("shared")),
            devices: [("new".to_string(), device("own"))].into(),
            ..Default::default()
        };
        assert_eq!(s.device_of("new").map(|d| d.username.as_str()), Some("own"));
        assert_eq!(s.device_of("old").map(|d| d.username.as_str()), Some("shared"));
        s.put("new", "x");
        s.remove("new");
        assert!(!s.devices.contains_key("new"));
    }

    /// Stores written before the Windows sign-in must still load, with
    /// their accounts intact and no device yet.
    #[test]
    fn a_store_from_before_the_windows_sign_in_still_loads() {
        let old = r#"{"tokens":{"1":"XBL3.0 x=a;b"},"emails":{"1":"a@b.c"}}"#;
        let s: Store = serde_json::from_str(old).expect("old store");
        assert_eq!(s.tokens.len(), 1);
        assert!(s.device.is_none() && s.windows.is_empty());
    }

    #[test]
    fn removing_an_account_forgets_its_windows_sign_in() {
        let mut s = Store::default();
        s.put("1", "x");
        s.windows.insert(
            "1".into(),
            msa_device::UserSession {
                username: "a@b.c".into(),
                puid: "p".into(),
                sts: msa_device::StsToken {
                    key_name: None,
                    token: "t".into(),
                    binary_secret: None,
                    tpm_key: None,
                    lifetime: msa_device::models::soap::Timestamp {
                        id: None,
                        created: String::new(),
                        expires: String::new(),
                    },
                },
            },
        );
        assert!(s.remove("1"));
        assert!(s.windows.is_empty());
    }
}
