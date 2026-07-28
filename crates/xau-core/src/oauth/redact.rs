//! Token-redaction helpers shared by the CLI and GUI.
//!
//! Logging an `XBL3.0 x=<uhs>;<token>` header verbatim leaks a credential
//! that is valid for ~16 hours. `mask_token` keeps the user hash (which is
//! useful for "is this the right account?" diagnostics) and a 12-char
//! prefix of the token (enough to distinguish two tokens at a glance) and
//! drops the rest, replacing it with the token's length so wave-style
//! "did the same token come back?" debugging still works.
//!
//! Wave-10/13 audit cross-cutting #7: `mask_token` was previously a
//! byte-identical copy in `xau-gui/src/ipc.rs` AND `xau-cli/src/commands/auth.rs`.
//! Promoted here so any future tweak to the redaction policy lands in
//! one place.

/// Redact an `XBL3.0 x=<uhs>;<token>` header for logging.
///
/// Non-XBL3.0 strings are truncated to 48 chars; the prefix shape is
/// preserved so callers can tell whether what came in was a header or
/// some other opaque blob.
#[must_use]
pub fn mask_token(token: &str) -> String {
    token
        .strip_prefix("XBL3.0 x=")
        .and_then(|rest| rest.split_once(';'))
        .map_or_else(
            || token.chars().take(48).collect::<String>(),
            |(uhs, tok)| {
                format!(
                    "XBL3.0 x={uhs};{}…({} chars)",
                    tok.chars().take(12).collect::<String>(),
                    tok.len()
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::mask_token;

    #[test]
    fn masks_xbl3_header_keeps_uhs_and_length() {
        let m = mask_token("XBL3.0 x=1234567890;abcdefghijklmnopqrstuv");
        assert!(
            m.starts_with("XBL3.0 x=1234567890;abcdefghijkl"),
            "expected uhs+12 prefix, got: {m}"
        );
        assert!(
            m.contains("(22 chars)"),
            "expected token length suffix, got: {m}"
        );
    }

    #[test]
    fn truncates_non_xbl3_to_48() {
        let s = "x".repeat(200);
        assert_eq!(mask_token(&s).len(), 48);
    }

    #[test]
    fn leaves_short_non_xbl3_unchanged() {
        // Below the 48-char ceiling: returned verbatim (well, via
        // `chars().take(48)` which is a no-op for short input).
        let s = "short string with spaces";
        assert_eq!(mask_token(s), s);
    }

    #[test]
    fn handles_xbl3_with_empty_token() {
        // Edge case: header shape correct but token portion empty.
        let m = mask_token("XBL3.0 x=uhs;");
        assert_eq!(m, "XBL3.0 x=uhs;…(0 chars)");
    }
}
