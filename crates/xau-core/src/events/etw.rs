//! Recover an **events token** by tracing Windows network activity.
//!
//! # Why this exists, and why it is a FALLBACK
//!
//! Event-based ("stat") achievements are unlocked by POSTing synthetic
//! telemetry to OneCollector, which authenticates with a token minted
//! for the `http://events.xboxlive.com` relying party — NOT the ordinary
//! `xboxlive.com` XSTS bearer.
//!
//! Our primary path mints that token directly: a second SISU call at
//! sign-in (`ipc/auth.rs`, stored as `AuthSession::events_xauth`). That
//! needs no privileges, no other app, and no tracing. **Use it.**
//!
//! Upstream XAU cannot do that, so it recovers the token by tracing HTTP
//! traffic while Microsoft Solitaire talks to Xbox Live, then scraping
//! the trace (Fumo-Unlockers XAU#483). This module ports that technique
//! for the cases our SISU mint cannot cover:
//!
//! - the mint failed (Microsoft declined the events relying party for
//!   this account) and the user would otherwise have to paste a token by
//!   hand, and
//! - verifying that whatever token we hold really is an events-RP token
//!   — see [`is_events_rp_token`], which is useful on its own.
//!
//! # Cost of this path
//!
//! - **Requires Administrator.** `netsh trace` / `logman` create a
//!   kernel trace session; without elevation every start method fails.
//! - Requires Microsoft Solitaire installed, as the traffic source.
//! - Writes a multi-megabyte `.etl` to the temp dir, which is deleted
//!   afterwards. That file contains captured network metadata.
//!
//! Nothing here runs unless the caller explicitly asks for it.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::error::{Error, Result};

/// Trace session name. Matching upstream's makes a stuck session from
/// either tool recoverable by the other.
const SESSION_NAME: &str = "XAU_EventsTokenCapture";

/// Package family launch URI for Microsoft Solitaire Collection — the
/// traffic source. It contacts Xbox Live within seconds of launch,
/// which is the window this capture exploits.
pub const SOLITAIRE_LAUNCH_URI: &str =
    r"shell:appsFolder\Microsoft.MicrosoftSolitaireCollection_8wekyb3d8bbwe!App";

/// Certificate thumbprint prefix that appears in the JWE header of a
/// token minted for the **events** relying party. This is what
/// distinguishes an events token from an ordinary XAUTH: both look like
/// `XBL3.0 x=<uhs>;<jwe>`, but only the events one is encrypted for this
/// certificate.
const EVENTS_RP_X5T: &str = "9wLGzMJDNz";

/// How the trace was started, so it can be stopped the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraceMethod {
    Netsh,
    Logman,
}

/// True when `token` was minted for the events relying party.
///
/// Decodes the JWE protected header (the part before the first `.`
/// after the `;`) and looks for the events certificate thumbprint.
/// Purely local: no network, no allocation beyond the header.
///
/// Accepts both `x:XBL3.0 x=…` (as it appears on the wire in the
/// `tickets` header) and a bare `XBL3.0 x=…`.
///
/// Use it to tell a *usable* events token from an XAUTH that will be
/// accepted with a 200 OK and then silently ignored by the telemetry
/// backend — the failure mode that makes event unlocks look like they
/// worked when they did not.
#[must_use]
pub fn is_events_rp_token(token: &str) -> bool {
    let Some((_, jwe)) = token.split_once(';') else {
        return false;
    };
    let header_b64 = jwe.split('.').next().unwrap_or_default();
    if header_b64.is_empty() {
        return false;
    }
    let Some(json) = base64url_decode(header_b64) else {
        return false;
    };
    String::from_utf8_lossy(&json).contains(EVENTS_RP_X5T)
}

/// Minimal base64url decoder (no padding required).
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    const fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' | b'+' => Some(62),
            b'_' | b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = u32::from(val(c)?);
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

fn etl_dir() -> PathBuf {
    std::env::temp_dir().join("XAU_ETW")
}

fn etl_path() -> PathBuf {
    etl_dir().join("capture.etl")
}

fn run(cmd: &str, args: &[&str], timeout: Duration) -> (i32, String) {
    // `Command` has no built-in timeout; these tools are quick, and the
    // callers below are already on a blocking worker. The timeout is
    // carried for parity with upstream and to document intent.
    let _ = timeout;
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(e) => (-1, e.to_string()),
    }
}

/// Stop any trace session left behind by a previous run (ours or
/// upstream's). Safe to call when nothing is running.
pub fn cleanup_sessions() {
    let _ = run("netsh", &["trace", "stop"], Duration::from_secs(15));
    let _ = run(
        "logman",
        &["stop", SESSION_NAME, "-ets"],
        Duration::from_secs(10),
    );
}

/// Start a trace, trying the same three methods upstream does, in the
/// same order: `netsh` first (widest coverage), then the WinHttp and
/// WinINet providers via `logman`.
fn start_trace() -> Option<TraceMethod> {
    let _ = std::fs::create_dir_all(etl_dir());
    let etl = etl_path();
    let etl_s = etl.to_string_lossy().to_string();

    let (code, _) = run(
        "netsh",
        &[
            "trace",
            "start",
            "scenario=InternetClient_dbg",
            "capture=no",
            &format!("tracefile={etl_s}"),
            "maxsize=256",
            "overwrite=yes",
            "report=disabled",
        ],
        Duration::from_secs(15),
    );
    if code == 0 {
        return Some(TraceMethod::Netsh);
    }
    for provider in ["Microsoft-Windows-WinHttp", "Microsoft-Windows-WinINet"] {
        let (code, _) = run(
            "logman",
            &[
                "start",
                SESSION_NAME,
                "-p",
                provider,
                "0xFFFFFFFF",
                "0xFF",
                "-o",
                &etl_s,
                "-ets",
            ],
            Duration::from_secs(10),
        );
        if code == 0 {
            return Some(TraceMethod::Logman);
        }
    }
    None
}

fn stop_trace(method: TraceMethod) {
    match method {
        TraceMethod::Netsh => {
            let _ = run("netsh", &["trace", "stop"], Duration::from_secs(30));
        }
        TraceMethod::Logman => {
            let _ = run(
                "logman",
                &["stop", SESSION_NAME, "-ets"],
                Duration::from_secs(15),
            );
        }
    }
}

/// Launch Microsoft Solitaire so it generates Xbox Live traffic.
///
/// # Errors
/// Returns an error when the shell cannot start the package (typically
/// "not installed").
pub fn launch_solitaire() -> Result<()> {
    let (code, out) = run(
        "cmd",
        &["/C", "start", "", SOLITAIRE_LAUNCH_URI],
        Duration::from_secs(15),
    );
    if code == 0 {
        Ok(())
    } else {
        Err(Error::other(format!(
            "could not launch Microsoft Solitaire (is it installed?): {out}"
        )))
    }
}

/// A token found in the trace, with how much we believe it.
#[derive(Debug, Clone)]
struct Candidate {
    token: String,
    score: i64,
}

/// Scan a captured trace for events tokens and return the best one.
///
/// Exposed (rather than inlined into [`capture`]) so the scoring and
/// validation are testable against a synthetic buffer without needing
/// Administrator, Solitaire, or a real trace.
#[must_use]
pub fn extract_token_from_bytes(data: &[u8]) -> Option<String> {
    let mut candidates: Vec<Candidate> = Vec::new();
    // A trace stores strings as both ASCII and UTF-16. Scan the raw
    // bytes, then again with the interleaved NULs removed, so a token
    // written as UTF-16 is still found.
    let ascii = printable_only(data, false);
    collect_candidates(&ascii, &mut candidates);
    let stripped = printable_only(data, true);
    collect_candidates(&stripped, &mut candidates);

    candidates.sort_by(|a, b| b.score.cmp(&a.score));
    // Only a token actually minted for the events RP is worth handing
    // back: an XAUTH would be accepted by OneCollector and then quietly
    // do nothing.
    candidates
        .into_iter()
        .find(|c| is_events_rp_token(&c.token))
        .map(|c| c.token)
}

/// Keep printable ASCII. With `drop_nuls` the NUL bytes of UTF-16 text
/// are removed first, which turns wide strings back into readable ASCII.
fn printable_only(data: &[u8], drop_nuls: bool) -> String {
    let mut s = String::with_capacity(data.len() / if drop_nuls { 2 } else { 1 } + 16);
    for &b in data {
        if b == 0 {
            if !drop_nuls {
                // Keep a separator so tokens can't be spliced together
                // across unrelated records.
                s.push('\u{1}');
            }
            continue;
        }
        if (0x20..=0x7E).contains(&b) {
            s.push(b as char);
        } else if !drop_nuls {
            s.push('\u{1}');
        }
    }
    s
}

const TOKEN_MARKER: &str = "x:XBL3.0 x=";

fn collect_candidates(text: &str, out: &mut Vec<Candidate>) {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(TOKEN_MARKER) {
        let idx = from + rel;
        let Some(token) = clean_token(&text[idx..]) else {
            from = idx + TOKEN_MARKER.len();
            continue;
        };
        if !out.iter().any(|c| c.token == token) {
            let score = score_candidate(text, idx, &token);
            out.push(Candidate { token, score });
        }
        from = idx + TOKEN_MARKER.len();
    }
}

/// Trim a raw slice at the first character that cannot belong to a
/// token, and reject anything too short or structurally wrong.
fn clean_token(raw: &str) -> Option<String> {
    let end = raw
        .find(|c: char| {
            !c.is_ascii() || c.is_ascii_control() || matches!(c, '"' | '\'' | '<' | '>' | '{' | '}')
        })
        .unwrap_or(raw.len());
    let token = raw[..end].trim_end().to_string();
    if !token.starts_with(TOKEN_MARKER) || token.len() < 100 || !token.contains(';') {
        return None;
    }
    Some(token)
}

fn score_candidate(text: &str, idx: usize, token: &str) -> i64 {
    let mut score = 0i64;
    // Proximity to a OneCollector endpoint is the strongest signal that
    // this token was on its way to the telemetry pipe.
    let start = idx.saturating_sub(2000);
    let end = (idx + 2000).min(text.len());
    let vicinity = &text[floor_char_boundary(text, start)..floor_char_boundary(text, end)];
    if vicinity.contains("v20.events.data.microsoft.com") || vicinity.contains("OneCollector") {
        score += 5000;
    }
    // A ticket header carries `"<titleid>" = "x:XBL3.0 …"`, i.e. real
    // per-title auth rather than an incidental copy.
    if vicinity.contains("tickets") {
        score += 2000;
    }
    score += i64::try_from(token.len()).unwrap_or(0) / 10;
    score
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Delete the captured trace. The `.etl` holds captured network
/// metadata, so it does not get to linger in the temp dir.
pub fn cleanup_files() {
    let _ = std::fs::remove_dir_all(etl_dir());
}

/// One-shot capture: start a trace, wait `capture` seconds while the
/// traffic source runs, stop, scan, clean up.
///
/// Returns `Ok(None)` when the trace ran but held no events token —
/// a normal outcome if Solitaire did not talk to Xbox Live in the
/// window. Returns `Err` when the trace could not be started at all,
/// which almost always means the process is not elevated.
///
/// # Errors
/// Fails when no trace method could start, or the trace file is unreadable.
pub fn capture(capture_secs: u64) -> Result<Option<String>> {
    if !cfg!(windows) {
        return Err(Error::Unsupported("ETW capture is Windows-only".into()));
    }
    cleanup_sessions();
    let Some(method) = start_trace() else {
        cleanup_files();
        return Err(Error::other(
            "could not start a trace session — this needs Administrator",
        ));
    };
    std::thread::sleep(Duration::from_secs(capture_secs));
    stop_trace(method);
    // Let the session flush to disk before reading.
    std::thread::sleep(Duration::from_secs(2));

    let path = etl_path();
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            cleanup_files();
            return Err(Error::other(format!("trace file unreadable: {e}")));
        }
    };
    let found = extract_token_from_bytes(&data);
    cleanup_files();
    Ok(found)
}

#[cfg(test)]
mod tests_support {
    /// Build a JWE-shaped token whose protected header carries `x5t`.
    pub fn make_token(x5t: &str, body_len: usize) -> String {
        let header = format!(r#"{{"enc":"A128CBC","alg":"RSA-OAEP","x5t":"{x5t}"}}"#);
        let mut b64 = String::new();
        // Minimal base64url encoder for the fixture.
        const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = header.as_bytes();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
            let keep = chunk.len() + 1;
            for &i in idx.iter().take(keep) {
                b64.push(TBL[i as usize] as char);
            }
        }
        format!(
            "x:XBL3.0 x=1234567890123456;{}.{}",
            b64,
            "Q".repeat(body_len)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::make_token as token_with_x5t;
    use super::*;

    #[test]
    fn an_events_token_is_recognised_by_its_certificate_thumbprint() {
        let tok = token_with_x5t(EVENTS_RP_X5T, 120);
        assert!(is_events_rp_token(&tok));
    }

    /// The whole point of the x5t check: an ordinary XAUTH looks
    /// identical in shape, and OneCollector accepts it with a 200 while
    /// silently dropping the unlock.
    #[test]
    fn an_ordinary_xauth_is_rejected() {
        let tok = token_with_x5t("SOMEOTHERTHUMB", 120);
        assert!(!is_events_rp_token(&tok));
    }

    #[test]
    fn malformed_tokens_do_not_panic() {
        for bad in [
            "",
            "x:XBL3.0 x=",
            "no-semicolon",
            "x:XBL3.0 x=1;",
            "x:XBL3.0 x=1;!!!!.!!!!",
            "x:XBL3.0 x=1;....",
        ] {
            assert!(!is_events_rp_token(bad), "{bad:?} should not validate");
        }
    }

    #[test]
    fn a_token_is_recovered_from_surrounding_trace_noise() {
        let tok = token_with_x5t(EVENTS_RP_X5T, 200);
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&[0u8; 64]);
        buf.extend_from_slice(b"POST https://v20.events.data.microsoft.com/OneCollector/1.0/ ");
        buf.extend_from_slice(b"tickets: {\"1144039928\"=\"");
        buf.extend_from_slice(tok.as_bytes());
        buf.extend_from_slice(b"\"}\x00\x00trailing junk");
        let got = extract_token_from_bytes(&buf).expect("token must be recovered");
        assert_eq!(got, tok);
    }

    /// A trace containing only a plain XAUTH must yield nothing rather
    /// than handing back a token that cannot unlock anything.
    #[test]
    fn a_trace_without_an_events_token_yields_none() {
        let tok = token_with_x5t("NOTEVENTSRP", 200);
        let mut buf = b"GET https://xboxlive.com/ Authorization: ".to_vec();
        buf.extend_from_slice(tok.as_bytes());
        assert!(extract_token_from_bytes(&buf).is_none());
    }

    /// Traces store strings as UTF-16 too; a token written that way has
    /// to survive the NUL-stripping pass.
    #[test]
    fn a_utf16_encoded_token_is_still_found() {
        let tok = token_with_x5t(EVENTS_RP_X5T, 150);
        let mut buf: Vec<u8> = Vec::new();
        for b in b"OneCollector ".iter().chain(tok.as_bytes()) {
            buf.push(*b);
            buf.push(0);
        }
        let got = extract_token_from_bytes(&buf).expect("UTF-16 token must be recovered");
        assert_eq!(got, tok);
    }

    #[test]
    fn a_truncated_token_is_not_offered() {
        // Under the 100-char floor — a fragment, not a usable token.
        let mut buf = b"noise ".to_vec();
        buf.extend_from_slice(b"x:XBL3.0 x=123;abc.def");
        assert!(extract_token_from_bytes(&buf).is_none());
    }
}

/// Tests for the resolution rule that `XboxClient::reload_events_token_from_auth`
/// applies. Kept next to `is_events_rp_token` because that predicate is
/// the whole basis of the decision.
#[cfg(test)]
mod resolution_tests {
    use super::tests_support::make_token;
    use super::*;

    /// Pick whichever source is provably an events token. This is the
    /// case the ETW/manual fallback exists for: the sign-in mint
    /// produced something unusable, so blindly preferring the session
    /// would throw away the very token that was fetched to fix it.
    fn resolve(session: Option<&str>, settings: Option<&str>) -> Option<String> {
        let s_ok = session.is_some_and(is_events_rp_token);
        let t_ok = settings.is_some_and(is_events_rp_token);
        match (s_ok, t_ok) {
            (true, false) => session.map(str::to_string),
            (false, true) => settings.map(str::to_string),
            _ => session.or(settings).map(str::to_string),
        }
    }

    #[test]
    fn a_valid_captured_token_beats_an_invalid_minted_one() {
        let bad_session = make_token("NOTEVENTS", 120);
        let good_settings = make_token(EVENTS_RP_X5T, 120);
        assert_eq!(
            resolve(Some(&bad_session), Some(&good_settings)).as_deref(),
            Some(good_settings.as_str()),
            "the fallback token must win when the minted one is not an events token"
        );
    }

    #[test]
    fn the_minted_token_still_wins_when_both_are_valid() {
        let session = make_token(EVENTS_RP_X5T, 120);
        let settings = make_token(EVENTS_RP_X5T, 200);
        assert_eq!(
            resolve(Some(&session), Some(&settings)).as_deref(),
            Some(session.as_str()),
            "behaviour must be unchanged for accounts where the mint works"
        );
    }

    #[test]
    fn with_neither_provably_valid_the_historical_order_holds() {
        let session = make_token("A", 120);
        let settings = make_token("B", 120);
        assert_eq!(
            resolve(Some(&session), Some(&settings)).as_deref(),
            Some(session.as_str())
        );
        assert_eq!(
            resolve(None, Some(&settings)).as_deref(),
            Some(settings.as_str())
        );
        assert_eq!(resolve(None, None), None);
    }
}
