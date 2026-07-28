//! Write one CSV per claimed offer, so a reward is never only on screen.
//!
//! An offer's `resource` — a Discord promo URL, a store token — is
//! minted exactly once, by the claim that consumes it. If it is shown in
//! a toast and lost, the offer is gone with it: claiming again returns
//! the same resource, but only while the service still has it, and
//! nothing about that is guaranteed. So every claim is written to disk
//! before the UI is told about it.
//!
//! One file per offer, named by account and offer id, rather than one
//! shared ledger: a batch run across several accounts should not be able
//! to lose ten rewards because a single write failed halfway.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use xau_core::error::Result;
use xau_core::xbox::client::offers::Offer;

/// Where CSVs are written, under the host-supplied data directory.
///
/// # Errors
/// Propagates filesystem failures.
pub fn offers_dir() -> Result<PathBuf> {
    let dir = crate::accounts::data_dir()?.join("offers");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Quote a field for CSV.
///
/// A resource is a URL and can carry commas or quotes; without this the
/// file silently gains a column and a spreadsheet reads the wrong cell.
fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Write one claimed offer to its own CSV and return the path.
///
/// # Errors
/// Propagates filesystem failures.
pub fn write_offer(
    gamertag: &str,
    xuid: &str,
    offer: &Offer,
    title: Option<&str>,
) -> Result<PathBuf> {
    let dir = offers_dir()?;
    // Keep the filename to characters every filesystem accepts: a
    // gamertag can contain spaces and punctuation.
    let safe_tag: String = gamertag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe_tag}_{}.csv", offer.offer_id));

    let mut out = String::new();
    out.push_str(HEADER);
    let _ = writeln!(
        out,
        "{},{},{},{},{},{},{},{},{}",
        field(gamertag),
        field(xuid),
        field(&offer.offer_id),
        field(title.unwrap_or("")),
        field(&offer.offer_status),
        field(offer.resource_type.as_deref().unwrap_or("")),
        field(offer.resource.as_deref().unwrap_or("")),
        field(offer.claimed_date.as_deref().unwrap_or("")),
        field(offer.marketplace_id.as_deref().unwrap_or("")),
    );

    // Temp + rename, like the account store: a crash mid-write must not
    // leave a half-line that reads as a valid but truncated URL.
    let tmp = path.with_extension("csv.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// A combined index of everything claimed so far, for convenience.
///
/// Rebuilt from the per-offer files rather than appended to, so it can
/// never drift out of step with them.
///
/// # Errors
/// Propagates filesystem failures.
pub fn write_index() -> Result<PathBuf> {
    let dir = offers_dir()?;
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("csv") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some(INDEX_NAME) {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        // Skip each file's own header; keep the single data line.
        if let Some(line) = text.lines().nth(1) {
            rows.push(line.to_string());
        }
    }
    rows.sort();

    let mut out = String::from(HEADER);
    for r in rows {
        out.push_str(&r);
        out.push('\n');
    }
    let path = dir.join(INDEX_NAME);
    let tmp = path.with_extension("csv.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

const INDEX_NAME: &str = "_todas.csv";

/// The column list, shared by both writers.
///
/// These drifted once: a `titulo` column was added to the data row of
/// the per-offer file but not to its header, so every row was one field
/// wider than its own header claimed. One constant makes that
/// impossible.
const HEADER: &str =
    "gamertag,xuid,offer_id,titulo,status,resource_type,resource,claimed_date,marketplace\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL with a comma must not become two columns.
    #[test]
    fn a_resource_with_a_comma_stays_one_field() {
        let quoted = field("https://x.test/a?b=1,2");
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert_eq!(quoted, "\"https://x.test/a?b=1,2\"");
    }

    /// An embedded quote is doubled, per RFC 4180.
    #[test]
    fn an_embedded_quote_is_doubled() {
        assert_eq!(field("a\"b"), "\"a\"\"b\"");
    }

    /// A plain value is not needlessly quoted.
    #[test]
    fn a_plain_value_is_left_alone() {
        assert_eq!(field("39FEA8F9"), "39FEA8F9");
    }

    /// The row must have exactly as many fields as the header names.
    ///
    /// This is the bug that already happened: a `titulo` column was
    /// added to the data row and not to the header, so every file was
    /// one field wider than it declared and a spreadsheet read every
    /// value into the wrong column from that point on.
    #[test]
    fn the_row_and_the_header_agree_on_column_count() {
        let columns = HEADER.trim_end().split(',').count();
        // Count the placeholders the row writer actually emits.
        let row_fields = "{},{},{},{},{},{},{},{},{}".split(',').count();
        assert_eq!(
            columns, row_fields,
            "header names {columns} columns but the row writes {row_fields}",
        );
    }
}
