//! What happened to a presence heartbeat, in words a caller can act on.
//!
//! Every presence path (single-title spoof, farm, bit-by-bit keepalive)
//! used to collapse a failed heartbeat into "an error string" or a
//! `debug!` line, so a farm whose every ping was refused with 403 still
//! looked like it was farming. [`PresenceStatus::classify`] sorts the
//! outcome of one heartbeat into:
//!
//! - `Accepted` — Xbox answered 2xx.
//! - `Denied`   — 401 / 403: the token was refused (expired, revoked, or
//!   without presence scope). Nothing was registered. Re-login / refresh.
//! - `Rejected` — any other 4xx (400, 404, 409, 429…): Xbox received the
//!   heartbeat and refused it. Nothing was registered.
//! - `NotSent`  — the request never reached Xbox (connect failure, DNS,
//!   missing xuid / title id).
//! - `Unknown`  — 5xx, or the connection dropped after the request was
//!   on the wire: it may or may not have been registered.
//!
//! The 401/403 split mirrors jnetu/Xbox-Achievement-Unlocker's
//! `SpoofResult.AuthRejected`.
//!
//! Mobile cut: the desktop search through the Xbox PC app's tokens
//! (process memory) is not here — a phone has no such tokens.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    /// No heartbeat has been sent yet.
    #[default]
    Pending,
    Accepted,
    Denied,
    Rejected,
    NotSent,
    Unknown,
}

impl PresenceStatus {
    pub fn classify<T>(result: &Result<T>) -> Self {
        match result {
            Ok(_) => Self::Accepted,
            Err(e) => Self::classify_error(e),
        }
    }

    pub fn classify_error(e: &Error) -> Self {
        match e {
            Error::Unauthorized => Self::Denied,
            Error::HttpStatus { status, .. } => match status.as_u16() {
                401 | 403 => Self::Denied,
                s if status.is_client_error() || s == 429 => Self::Rejected,
                _ if status.is_server_error() => Self::Unknown,
                _ => Self::Rejected,
            },
            // A connect error never left the machine; anything later
            // (timeout, reset, lost body) may have been applied.
            Error::Http(e) if e.is_connect() || e.is_builder() => Self::NotSent,
            Error::Http(_) => Self::Unknown,
            Error::InvalidArgument(_) | Error::Url(_) | Error::Io(_) => Self::NotSent,
            _ => Self::Unknown,
        }
    }

    /// True when Xbox definitely did NOT register this heartbeat.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Denied | Self::Rejected | Self::NotSent)
    }

    /// Short, user-facing explanation of the status.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Pending => "presence not sent yet",
            Self::Accepted => "presence accepted by Xbox",
            Self::Denied => {
                "presence DENIED (401/403) — token expired or without presence scope; refresh or log in again"
            }
            Self::Rejected => "presence REJECTED by Xbox — the heartbeat was refused",
            Self::NotSent => "presence NOT SENT — the request never reached Xbox",
            Self::Unknown => "presence outcome unknown — Xbox errored or the connection dropped",
        }
    }
}

/// The last presence heartbeat's outcome, for status snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceReport {
    pub status: PresenceStatus,
    /// HTTP status Xbox answered with, when it answered.
    pub http_status: Option<u16>,
    /// Error text for anything but `Accepted`.
    pub detail: Option<String>,
    pub at_unix: Option<i64>,
}

impl PresenceReport {
    pub fn from_result<T>(result: &Result<T>) -> Self {
        let status = PresenceStatus::classify(result);
        let (http_status, detail) = match result {
            Ok(_) => (Some(200), None),
            Err(e) => (http_status_of(e), Some(e.to_string())),
        };
        Self {
            status,
            http_status,
            detail,
            at_unix: Some(unix_now()),
        }
    }

    /// One line for logs / toasts: description plus the raw error.
    pub fn message(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}: {d}", self.status.describe()),
            None => self.status.describe().to_string(),
        }
    }
}

fn http_status_of(e: &Error) -> Option<u16> {
    match e {
        Error::Unauthorized => Some(401),
        Error::HttpStatus { status, .. } => Some(status.as_u16()),
        Error::Http(e) => e.status().map(|s| s.as_u16()),
        _ => None,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Title ids an `userpresence` read (`GET /users/xuid(..)?level=all`)
/// shows as ACTIVE, on any device. Shape:
/// `{"devices":[{"type":"..","titles":[{"id":"123","state":"Active",..}]}]}`.
/// Ids come as strings or numbers; a title without a `state` counts as
/// active (older contract versions omit it).
pub fn active_title_ids(body: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let devices = body
        .get("devices")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    for device in devices {
        let titles = device
            .get("titles")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten();
        for title in titles {
            let active = title
                .get("state")
                .and_then(serde_json::Value::as_str)
                .map_or(true, |s| s.eq_ignore_ascii_case("active"));
            let id = match title.get("id") {
                Some(serde_json::Value::String(s)) => s.trim().to_string(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            if active && !id.is_empty() && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn read_back_lists_only_active_titles() {
        let body = serde_json::json!({
            "xuid": "1", "state": "Online",
            "devices": [
                {"type": "WindowsOneCore", "titles": [
                    {"id": "1915865634", "state": "Active", "placement": "Full"},
                    {"id": 750323071, "state": "Inactive"},
                ]},
                {"type": "Scarlett", "titles": [{"id": 1915865634u64}, {"id": "219630713"}]},
            ]
        });
        assert_eq!(active_title_ids(&body), vec!["1915865634", "219630713"]);
        assert!(active_title_ids(&serde_json::json!({"state": "Offline"})).is_empty());
    }

    fn status(code: u16) -> Error {
        Error::HttpStatus {
            status: StatusCode::from_u16(code).unwrap(),
            body: String::new(),
        }
    }

    #[test]
    fn ok_is_accepted() {
        assert_eq!(PresenceStatus::classify(&Ok(())), PresenceStatus::Accepted);
    }

    #[test]
    fn auth_refusals_are_denied() {
        assert_eq!(
            PresenceStatus::classify_error(&Error::Unauthorized),
            PresenceStatus::Denied
        );
        assert_eq!(
            PresenceStatus::classify_error(&status(401)),
            PresenceStatus::Denied
        );
        assert_eq!(
            PresenceStatus::classify_error(&status(403)),
            PresenceStatus::Denied
        );
    }

    #[test]
    fn other_client_errors_are_rejected() {
        for code in [400, 404, 409, 429] {
            assert_eq!(
                PresenceStatus::classify_error(&status(code)),
                PresenceStatus::Rejected,
                "{code}"
            );
        }
    }

    #[test]
    fn server_errors_are_unknown() {
        assert_eq!(
            PresenceStatus::classify_error(&status(500)),
            PresenceStatus::Unknown
        );
        assert_eq!(
            PresenceStatus::classify_error(&status(503)),
            PresenceStatus::Unknown
        );
    }

    #[test]
    fn local_failures_are_not_sent() {
        let e = Error::invalid_argument("`xuid` must not be empty");
        assert_eq!(PresenceStatus::classify_error(&e), PresenceStatus::NotSent);
    }

    #[tokio::test]
    async fn connect_failure_is_not_sent() {
        // Port 9 on localhost: nothing listens, connect is refused.
        let err = reqwest::Client::new()
            .post("http://127.0.0.1:9/")
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            PresenceStatus::classify_error(&Error::Http(err)),
            PresenceStatus::NotSent
        );
    }

    #[test]
    fn report_carries_http_status_and_detail() {
        let r = PresenceReport::from_result::<()>(&Err(status(403)));
        assert_eq!(r.status, PresenceStatus::Denied);
        assert_eq!(r.http_status, Some(403));
        assert!(r.message().contains("DENIED"));
        assert!(r.status.is_failure());
    }
}
