use std::io;

use reqwest::StatusCode;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Returns a body string safe to include in `Display` / log output.
///
/// `Error::HttpStatus { body }` carries the verbatim upstream response.
/// For the `oauth20_token.srf` failure path that body is JSON containing
/// `access_token` / `refresh_token` fields; a downstream
/// `tracing::error!(%err, ...)` would otherwise spray those bearers into
/// log files. Whenever the body looks like a token-bearing JSON payload
/// we redact the body entirely; otherwise we cap it at 1 KiB so a
/// gigantic Cloudflare HTML doesn't explode the log line either.
///
/// BUG HUNTER H2: also called from `xau-server` when serializing
/// `HttpStatus` errors into the JSON response, since the struct field
/// is otherwise the raw upstream body and would bypass the `Display`
/// redaction.
pub fn redact_http_body_for_display(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let looks_like_secret = lower.contains("access_token")
        || lower.contains("refresh_token")
        || lower.contains("xbl3.0")
        || lower.contains("xsts");
    if looks_like_secret {
        return format!(
            "<{} bytes redacted — body contained token-shaped fields>",
            body.len()
        );
    }
    const MAX: usize = 1024;
    if body.len() > MAX {
        let head: String = body.chars().take(MAX).collect();
        return format!("{head}…(truncated, {} total bytes)", body.len());
    }
    body.to_string()
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("HTTP status {status}: {}", redact_http_body_for_display(body))]
    HttpStatus { status: StatusCode, body: String },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error(
        "unauthorized (401) — XAUTH token is missing, invalid or expired. Run `xau refresh` to obtain a fresh token, or `xau login` if that fails."
    )]
    Unauthorized,

    #[error("resource not found: {0}")]
    NotFound(String),

    #[error("memory reader error: {0}")]
    Memory(String),

    #[error("operation not supported on this platform: {0}")]
    Unsupported(&'static str),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn memory(msg: impl Into<String>) -> Self {
        Self::Memory(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized
                | Self::HttpStatus {
                    status: StatusCode::UNAUTHORIZED,
                    ..
                }
        )
    }

    /// True when the error is a transient server-side condition that
    /// an application-level retry might recover from:
    /// - 429 Too Many Requests
    /// - 5xx server errors (500 / 502 / 503 / 504 etc.)
    ///
    /// The HTTP retry layer in `xbox::client::retry` already retries
    /// 429 / 503 up to `max_retries` with throttle-gate cooperation,
    /// so by the time a caller sees one of these errors the short-
    /// term retry budget is already exhausted. Application code
    /// (e.g. bit-by-bit's per-achievement unlock loop) uses this
    /// predicate to apply a second, longer-backoff retry layer on
    /// top — "maybe the server needs more than 40 s to recover".
    ///
    /// Excludes 401 / 403 / 404 / 400 — those are permanent for the
    /// given request and retrying just wastes quota.
    pub fn is_transient_server_error(&self) -> bool {
        match self {
            Self::HttpStatus { status, .. } => status.as_u16() == 429 || status.is_server_error(),
            // Connection-level errors (timeouts, resets) are handled
            // by the HTTP layer's is_transient_send_err; they don't
            // surface as `Error::HttpStatus` and we don't want to
            // double-retry them at the app layer.
            _ => false,
        }
    }
}
