//! Add a Windows Firewall rule exposing the local HTTP server to the LAN.
//!
//! Port of the `AddFirewallRule` / `RuleExists` helpers in the original
//! `Services/HttpServer/HttpServer.cs`. Shells out to `netsh advfirewall
//! firewall`, which requires an elevated (admin) prompt on Windows 10+.

use std::process::Command;

use crate::error::{Error, Result};

pub const DEFAULT_RULE_NAME: &str = "XAU API Server";

/// Validate a rule name before passing it to `netsh`.
///
/// `netsh advfirewall firewall add rule name=<x> dir=in ...` is parsed
/// by `netsh` itself in a parameter-key=value style. If the `name`
/// contains whitespace and another recognised key (`dir=`, `action=`,
/// `protocol=`, `localport=`, `remoteip=`, `profile=`, etc.), `netsh`
/// can interpret the trailing tokens as overriding the rule's
/// directionality / action / protocol. `Command::args(...)` per-arg
/// shell-escapes for the OS but does NOT prevent in-arg `netsh`-level
/// parsing.
///
/// Wave-5 audit IPC-H1: an IPC caller (or a future XSS in the
/// WebView) could call `system.firewall_add` with a crafted `name`
/// like `XAU dir=out action=block remoteip=any` to invert the rule's
/// intent. Restrict the name to a printable ASCII subset that
/// excludes whitespace, `=`, and a few control characters. Rejects
/// empty input; caps length at 60 (`netsh` accepts up to ~256, but
/// short labels keep the rules legible in the Firewall UI).
pub fn validate_rule_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid_argument(
            "firewall rule name cannot be empty",
        ));
    }
    if name.len() > 60 {
        return Err(Error::invalid_argument(
            "firewall rule name too long (max 60 chars)",
        ));
    }
    for ch in name.chars() {
        let ok = matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | ' ' | '-' | '_' | '.')
            // Allow non-leading whitespace via the explicit space
            // case above, but block tabs / newlines / `=` which are
            // the netsh-key delimiters that enable the injection.
            && ch != '\t' && ch != '\n' && ch != '\r' && ch != '=';
        if !ok {
            return Err(Error::invalid_argument(format!(
                "firewall rule name contains disallowed character `{ch}` \
                 (allowed: ASCII letters, digits, space, `-`, `_`, `.`)"
            )));
        }
    }
    // Defensive — even though `=` is already excluded above, double-
    // check that the name doesn't accidentally form a `key=value`
    // shape that `netsh` could parse (e.g. `dir`, `action`, etc.).
    let lower = name.to_ascii_lowercase();
    for forbidden in [
        "dir=",
        "action=",
        "protocol=",
        "localport=",
        "remoteip=",
        "profile=",
        "interface=",
        "edge=",
    ] {
        if lower.contains(forbidden) {
            return Err(Error::invalid_argument(format!(
                "firewall rule name contains netsh keyword `{forbidden}` \
                 — refusing to forward to avoid intent injection"
            )));
        }
    }
    Ok(())
}

/// Returns `true` if `port`'s decimal form appears in `haystack` as a
/// standalone number — i.e. NOT as a substring of a longer digit run.
///
/// BUG HUNTER firewall-port-substring: `rule_exists` previously did
/// `stdout.contains(&port.to_string())`, so checking port `80` matched
/// `8080`, a `LocalPort: 8000` line, the rule index, a profile bitmask,
/// byte counters, etc. in the locale-formatted `netsh show rule` dump.
/// A false positive made `add_rule` SKIP creating the rule the server
/// actually needs — leaving it silently unreachable on the LAN with no
/// error. Requiring a non-digit (or a string boundary) on both sides of
/// the match eliminates the `80 ⊂ 8080` class of false positives.
/// (Port numbers are ASCII digits, so byte-level adjacency checks are
/// UTF-8-safe.)
fn port_appears_as_token(haystack: &str, port: u16) -> bool {
    let needle = port.to_string();
    let nb = needle.as_bytes();
    let bytes = haystack.as_bytes();
    if nb.is_empty() || bytes.len() < nb.len() {
        return false;
    }
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            let prev_is_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_idx = i + nb.len();
            let next_is_digit = next_idx < bytes.len() && bytes[next_idx].is_ascii_digit();
            if !prev_is_digit && !next_is_digit {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub fn rule_exists(name: &str, port: u16) -> Result<bool> {
    // Wave-5 audit IPC-H1: same validation as `add_rule` so a caller
    // checking existence with a crafted name can't trigger the
    // injection via `netsh advfirewall firewall show rule name=<x>`.
    validate_rule_name(name)?;
    #[cfg(windows)]
    {
        let out = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={name}"),
            ])
            .output()
            .map_err(|e| Error::other(format!("netsh failed: {e}")))?;
        if !out.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(port_appears_as_token(&stdout, port))
    }
    #[cfg(not(windows))]
    {
        let _ = (name, port);
        Err(Error::Unsupported("firewall integration is Windows-only"))
    }
}

/// Adds an inbound TCP allow rule for `port`. Requires running elevated;
/// otherwise `netsh` returns a non-zero exit code.
pub fn add_rule(name: &str, port: u16) -> Result<()> {
    // Wave-5 audit IPC-H1: validate before the cfg-windows split so
    // both the real path and the future cross-platform stub honour
    // the same input contract.
    validate_rule_name(name)?;
    #[cfg(windows)]
    {
        if rule_exists(name, port)? {
            return Ok(());
        }
        let status = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={name}"),
                "dir=in",
                "action=allow",
                "protocol=TCP",
                &format!("localport={port}"),
            ])
            .status()
            .map_err(|e| Error::other(format!("netsh failed: {e}")))?;
        if !status.success() {
            return Err(Error::other(
                "adding firewall rule failed — re-run as Administrator",
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (name, port);
        Err(Error::Unsupported("firewall integration is Windows-only"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rule_name_accepts_default_and_friendly() {
        assert!(validate_rule_name(DEFAULT_RULE_NAME).is_ok());
        assert!(validate_rule_name("XAU.API.Server").is_ok());
        assert!(validate_rule_name("XAU-Server-2026").is_ok());
        assert!(validate_rule_name("a").is_ok());
    }

    #[test]
    fn validate_rule_name_rejects_empty_and_too_long() {
        assert!(validate_rule_name("").is_err());
        assert!(validate_rule_name(&"a".repeat(61)).is_err());
        assert!(validate_rule_name(&"a".repeat(60)).is_ok());
    }

    /// Wave-5 audit IPC-H1: smoking gun. A name smuggling
    /// `dir=out action=block` through the netsh parameter parser
    /// MUST be rejected before reaching the shell.
    #[test]
    fn validate_rule_name_rejects_netsh_keyword_injection() {
        assert!(validate_rule_name("XAU dir=out action=block").is_err());
        assert!(validate_rule_name("foo action=block").is_err());
        assert!(validate_rule_name("name=evil").is_err());
        assert!(validate_rule_name("legit dir=in").is_err());
        assert!(validate_rule_name("a remoteip=any").is_err());
        assert!(validate_rule_name("ok protocol=UDP").is_err());
        assert!(validate_rule_name("z localport=22").is_err());
    }

    #[test]
    fn validate_rule_name_rejects_control_chars_and_punctuation() {
        assert!(validate_rule_name("a\tb").is_err());
        assert!(validate_rule_name("a\nb").is_err());
        assert!(validate_rule_name("a\rb").is_err());
        assert!(validate_rule_name("a\"b").is_err());
        assert!(validate_rule_name("a;b").is_err());
        assert!(validate_rule_name("a|b").is_err());
        assert!(validate_rule_name("a&b").is_err());
        // Unicode rejected — netsh-on-Windows is code-page sensitive.
        assert!(validate_rule_name("XAU\u{2028}Server").is_err());
    }

    /// BUG HUNTER firewall-port-substring regression: the existence check
    /// must match the port as a STANDALONE number, never as a substring
    /// of a longer digit run, otherwise `rule_exists` false-positives and
    /// `add_rule` skips creating the rule the server needs.
    #[test]
    fn port_token_match_ignores_substring_collisions() {
        // Realistic-ish `netsh show rule` excerpt for a rule on port 80.
        let dump = "Rule Name:    XAU API Server\r\n\
                    Enabled:      Yes\r\n\
                    Direction:    In\r\n\
                    Profiles:     Domain,Private,Public\r\n\
                    Protocol:     TCP\r\n\
                    LocalPort:    80\r\n\
                    RemotePort:   Any\r\n";
        assert!(
            port_appears_as_token(dump, 80),
            "should match standalone 80"
        );

        // The classic false positive: looking for 80 in a dump that only
        // mentions 8080 must NOT match.
        let other = "LocalPort:    8080\r\nRemotePort:   Any\r\n";
        assert!(
            !port_appears_as_token(other, 80),
            "80 must not match inside 8080"
        );
        // Symmetric: 8080 present should match when that's the port.
        assert!(port_appears_as_token(other, 8080));

        // Adjacent digits on either side block a spurious match.
        assert!(!port_appears_as_token("x44300y", 443));
        assert!(!port_appears_as_token("x12443y", 443));
        // Bounded by non-digits (or string edges) matches.
        assert!(port_appears_as_token("443", 443));
        assert!(port_appears_as_token("port=443 ", 443));
    }
}
