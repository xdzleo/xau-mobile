//! Several accounts signed in at once.
//!
//! The desktop app assumes one identity: a single `auth.json`, a single
//! client. The phone is a switcher — you add accounts and see all of
//! them at a glance — so sessions are keyed by xuid and stored together.
//!
//! # What each row can show, and what it cannot
//!
//! Gamertag, gamerpic and gamerscore all come from one profile call.
//! **Email does not.** The token this app holds is scoped
//! `service::user.auth.xboxlive.com::MBI_SSL`, which is Xbox Live only —
//! the profile behind it has no address on it. Showing one would mean
//! requesting a different scope at sign-in and risking the SISU exchange
//! that scope exists for, so this reports what it actually knows.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Session;
use xau_core::error::{Error, Result};

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
}

fn store_path() -> Result<PathBuf> {
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

    pub fn remove(&mut self, xuid: &str) -> bool {
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
}
