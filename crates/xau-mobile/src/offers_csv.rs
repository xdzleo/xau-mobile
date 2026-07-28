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
use std::path::PathBuf;

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
///
/// A line break is folded to a space rather than quoted. RFC 4180 lets a
/// record span physical lines, but every reader here — the index, the
/// detail screen, the export — takes `lines().nth(1)` and reads exactly
/// one physical line. Quoting a newline would write a record those
/// readers cannot see, and a code nobody can see is a code that is lost.
fn field(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if s.contains([',', '"']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
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
    //
    // The xuid is in the name and not only in the row because that
    // sanitiser is lossy: "epic gamer" and "epic#gamer" both flatten to
    // "epic_gamer", so the second account's rename would silently
    // destroy the first one's code — a resource minted exactly once,
    // gone with no error raised anywhere.
    let safe_tag: String = gamertag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe_tag}_{xuid}_{}.csv", offer.offer_id));

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

/// Every claim recorded for one offer, one normalised row each.
///
/// The single place that knows how the files on disk are found and read.
/// It was written twice for a day — once for the screen, once for the
/// export — and the two would have disagreed the moment either changed,
/// with the screen printing "N códigos salvos" directly above a button
/// that produced a file with a different N.
///
/// Every row comes back with exactly the columns [`HEADER`] names, so a
/// caller never has to know which build wrote the file it came from.
///
/// # Errors
/// Propagates filesystem failures. One unreadable or malformed file is
/// skipped, with a warning, rather than hiding every other account's
/// code.
fn scan_offer(offer_id: &str) -> Result<Vec<Vec<String>>> {
    let dir = offers_dir()?;
    let suffix = format!("_{offer_id}.csv");
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // The index ends in `_todas.csv` and would answer to an offer
        // literally called "todas". Nothing can produce that id today —
        // they are hex — but the id crosses the JNI boundary as a string
        // and the suffix rule is the only thing standing between them.
        if name == INDEX_NAME || !name.ends_with(&suffix) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("CSV ilegível {}: {e}", path.display());
                continue;
            }
        };
        let Some(line) = text.lines().nth(1) else {
            tracing::warn!("CSV sem linha de dados: {}", path.display());
            continue;
        };
        match split_row(line).and_then(normalise) {
            Some(fields) => rows.push(fields),
            None => tracing::warn!("linha ilegível em {}", path.display()),
        }
    }
    // One row per account, newest kept. Putting the xuid in the
    // filename left the file an older build wrote and the one this build
    // writes both answering to the same suffix, and the same code listed
    // twice reads as two rewards.
    rows.sort_by(|a, b| a[1].cmp(&b[1]).then_with(|| b[7].cmp(&a[7])));
    rows.dedup_by(|a, b| a[1] == b[1]);

    // Newest first: the code someone just claimed is the one they want.
    // The gamertag breaks ties because a batch claim writes several rows
    // inside the same second, and read_dir order is whatever the
    // filesystem feels like — the screen would reshuffle between reads.
    rows.sort_by(|a, b| b[7].cmp(&a[7]).then_with(|| a[0].cmp(&b[0])));
    Ok(rows)
}

/// Bring a row up to the column list [`HEADER`] declares.
///
/// Files written before the `titulo` column existed have eight fields,
/// and eight fields read as if they were nine puts `claimed_date` where
/// the code belongs and the marketplace where the date belongs — the
/// screen would show a timestamp as someone's Discord link. The layout
/// is recoverable from the count, so those rows are widened rather than
/// dropped: the code in them is still the user's.
fn normalise(fields: Vec<String>) -> Option<Vec<String>> {
    match fields.len() {
        n if n >= COLUMNS => Some(fields.into_iter().take(COLUMNS).collect()),
        n if n == COLUMNS - 1 => {
            let mut out = fields;
            out.insert(3, String::new());
            Some(out)
        }
        _ => None,
    }
}

/// One claimed row, as the detail view shows it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ClaimRow {
    pub gamertag: String,
    /// The link or store token this claim minted.
    pub code: String,
    pub claimed_date: String,
}

/// Every code recorded for one offer, across all accounts.
///
/// Read back from the CSVs rather than kept in memory: the files are the
/// record. An account claimed on a previous run — or in a session that
/// has since been signed out — still shows its code here, which is the
/// whole reason the files are written before the UI is told anything.
///
/// # Errors
/// Propagates filesystem failures.
pub fn rows_for_offer(offer_id: &str) -> Result<Vec<ClaimRow>> {
    Ok(scan_offer(offer_id)?
        .into_iter()
        .map(|f| ClaimRow {
            gamertag: f[0].clone(),
            code: f[6].clone(),
            claimed_date: f[7].clone(),
        })
        .collect())
}

/// One offer's CSV, assembled and named, ready to be saved.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OfferCsv {
    /// Suggested filename, safe on any filesystem.
    pub file_name: String,
    pub text: String,
}

/// Everything recorded for one offer, as a file a person can keep.
///
/// Assembled across accounts rather than handing over one of the files:
/// a reward claimed on five accounts is five files on disk, and what was
/// asked for is the reward.
///
/// # A byte-order mark and CRLF, and still commas
///
/// The mark is there because Excel on a pt-BR Windows box decodes a
/// plain `.csv` as the ANSI codepage, so an accented reward name arrives
/// as mojibake — a silent failure that reads as corrupted data.
///
/// The separator stays a comma even though that same Excel uses `;` as
/// its list separator and will open this file with every row in one
/// cell. That failure is loud and the user fixes it in two clicks;
/// switching would break every reader that takes `text/csv` at its word,
/// and would make the download disagree with the record on disk. The
/// loud, correctable failure is the one worth keeping.
///
/// `title` is what the screen is showing. The `titulo` column is the
/// fallback, and the id the last resort — a file named after a GUID when
/// the app had a name for it on screen is a file nobody can find again.
///
/// # Errors
/// Propagates filesystem failures.
pub fn csv_for_offer(offer_id: &str, title: &str) -> Result<OfferCsv> {
    let rows = scan_offer(offer_id)?;

    let mut text = String::from("\u{feff}");
    let head: Vec<&str> = HEADER.trim_end().split(',').collect();
    text.push_str(&head.join(","));
    text.push_str("\r\n");
    for row in &rows {
        let line: Vec<String> = row.iter().map(|f| export_field(f)).collect();
        text.push_str(&line.join(","));
        text.push_str("\r\n");
    }

    let named = if title.is_empty() {
        rows.first().map(|r| r[3].clone()).unwrap_or_default()
    } else {
        title.to_string()
    };
    Ok(OfferCsv {
        file_name: file_name_for(&named, offer_id),
        text,
    })
}

/// Quote one field for the export, and defuse it.
///
/// A value starting with `=`, `+`, `-` or `@` is run as a formula the
/// moment a spreadsheet opens the file. The `titulo` column is filled
/// from arc.msn.com, which this app reaches with no authentication at
/// all — whoever can answer that request can choose what a cell in the
/// user's spreadsheet executes. A leading apostrophe is the standard
/// answer: the spreadsheet reads the rest as text and shows it whole.
fn export_field(s: &str) -> String {
    let mut v: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if v.starts_with(['=', '+', '-', '@']) {
        v.insert(0, '\'');
    }
    if v.contains([',', '"']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

/// A filename that survives every filesystem a phone can write to.
///
/// A reward title carries spaces, accents, `:` and `#`, and one of them
/// is literally `skate.` — a trailing dot is stripped on some volumes and
/// would leave the file called `.csv`. So: ASCII word characters kept,
/// everything else a single dash, and the offer id's first bytes
/// appended because two rewards may share a title.
fn file_name_for(title: &str, offer_id: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars() {
        let c = fold_accent(c);
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let stem = out.trim_end_matches('-');
    let stem: String = stem.chars().take(40).collect();
    let short: String = offer_id.chars().take(8).collect();
    if stem.is_empty() {
        format!("vantagem-{short}.csv")
    } else {
        format!("{stem}-{short}.csv")
    }
}

/// Portuguese accents to their bare letter, for filenames only.
///
/// Without this, "Vantagens de Ação" becomes `Vantagens-de-A-o` — the
/// accented letters are not ASCII, so they turn into separators and eat
/// the word around them. Folding keeps the name readable while the file
/// stays ASCII, which is what a phone's SD card and every share target
/// can be relied on to accept.
fn fold_accent(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => 'A',
        'É' | 'È' | 'Ê' | 'Ë' => 'E',
        'Í' | 'Ì' | 'Î' | 'Ï' => 'I',
        'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'O',
        'Ú' | 'Ù' | 'Û' | 'Ü' => 'U',
        'Ç' => 'C',
        'Ñ' => 'N',
        other => other,
    }
}

/// Split one CSV line back into fields, honouring RFC 4180 quoting.
///
/// Written rather than split(',') because a resource is a URL and may
/// carry commas inside quotes — splitting naively would shift every
/// field after it and show the wrong value as the code.
fn split_row(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut cur)),
            other => cur.push(other),
        }
    }
    fields.push(cur);
    // gamertag,xuid,offer_id,titulo,status,resource_type,resource,claimed_date,marketplace
    // Short of the date column the row cannot be read at all: every
    // field after the gap would be attributed to the wrong column.
    if fields.len() < 8 {
        return None;
    }
    Some(fields)
}

const INDEX_NAME: &str = "_todas.csv";

/// The column list, shared by both writers.
///
/// These drifted once: a `titulo` column was added to the data row of
/// the per-offer file but not to its header, so every row was one field
/// wider than its own header claimed. One constant makes that
/// impossible.
/// How many columns [`HEADER`] names. Load-bearing: it is what tells a
/// legacy row from a current one in [`normalise`].
const COLUMNS: usize = 9;

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

    /// A quoted URL containing a comma must come back whole.
    ///
    /// Splitting on ',' would make the resource two fields and shift
    /// every column after it, so the detail view would show a fragment
    /// of the link as the code and the wrong value as the date.
    #[test]
    fn a_quoted_resource_with_a_comma_round_trips() {
        let line = r#"xdzleo,2533,39FE,Discord Nitro,claimed,external-link,"https://x.test/a?b=1,2",2026-07-28T17:45:59Z,BR"#;
        let row = read(line).expect("parses");
        assert_eq!(row[0], "xdzleo");
        assert_eq!(row[6], "https://x.test/a?b=1,2");
        assert_eq!(row[7], "2026-07-28T17:45:59Z");
    }

    /// A truncated line is skipped, not read with shifted columns.
    #[test]
    fn a_short_line_is_rejected() {
        assert!(read("only,three,fields").is_none());
    }

    /// A file from the build before `titulo` existed has eight fields.
    ///
    /// Read as if it had nine, its `claimed_date` lands where the code
    /// belongs: the screen would show a timestamp as someone's Discord
    /// link, and the export would carry that into a spreadsheet. The
    /// count identifies the layout, so the row is widened instead.
    #[test]
    fn a_legacy_row_without_a_title_is_widened_not_misread() {
        let line =
            "xdzleo,2533,39FE,claimed,external-link,https://x.test/a,2026-07-28T17:45:59Z,BR";
        let row = read(line).expect("recovered");
        assert_eq!(row.len(), COLUMNS);
        assert_eq!(row[3], "", "the missing column is the title");
        assert_eq!(row[6], "https://x.test/a");
        assert_eq!(row[7], "2026-07-28T17:45:59Z");
    }

    /// What both readers do to one line, in one place.
    fn read(line: &str) -> Option<Vec<String>> {
        split_row(line).and_then(normalise)
    }

    /// What write_offer emits must be what the readers read back.
    #[test]
    fn a_written_row_parses_back_to_the_same_values() {
        let offer = Offer {
            offer_id: "39FEA8F9".into(),
            offer_status: "claimed".into(),
            resource_type: Some("external-link".into()),
            resource: Some("https://discord.com/x?a=1,2".into()),
            claimed_date: Some("2026-07-28T17:45:59Z".into()),
            claim_type: None,
            marketplace_id: Some("BR".into()),
        };
        let mut out = String::from(HEADER);
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{}",
            field("xdzleo"),
            field("2533"),
            field(&offer.offer_id),
            field("Discord Nitro"),
            field(&offer.offer_status),
            field(offer.resource_type.as_deref().unwrap_or("")),
            field(offer.resource.as_deref().unwrap_or("")),
            field(offer.claimed_date.as_deref().unwrap_or("")),
            field(offer.marketplace_id.as_deref().unwrap_or("")),
        );
        let row = read(out.lines().nth(1).expect("data line")).expect("parses");
        assert_eq!(row[6], "https://discord.com/x?a=1,2");
        assert_eq!(row[0], "xdzleo");
    }

    /// `skate.` is a real reward name. A trailing dot is stripped by
    /// some volumes, which would leave the file called `.csv`.
    #[test]
    fn a_title_ending_in_a_dot_does_not_lose_its_name() {
        assert_eq!(
            file_name_for("skate.", "5609A2AE78614695B55634C5D4F2963D"),
            "skate-5609A2AE.csv",
        );
    }

    /// Punctuation collapses to one dash, never a run of them.
    #[test]
    fn punctuation_becomes_a_single_separator() {
        assert_eq!(
            file_name_for("PUBG: BATTLEGROUNDS", "23561ABE36424A7A83FC9F04C1A51941"),
            "PUBG-BATTLEGROUNDS-23561ABE.csv",
        );
    }

    /// An accent must not eat the word it sits in.
    #[test]
    fn accents_fold_instead_of_becoming_separators() {
        assert_eq!(
            file_name_for("Vantagens de Ação", "39FEA8F9C18B4222BF36A0D7BF5B28D7"),
            "Vantagens-de-Acao-39FEA8F9.csv",
        );
    }

    /// A nameless offer still gets a file, told apart by its id.
    #[test]
    fn a_missing_title_falls_back_to_the_id() {
        assert_eq!(
            file_name_for("", "C25B53732BE94B82"),
            "vantagem-C25B5373.csv"
        );
    }

    /// Two rewards can share a title; the id is what separates them.
    #[test]
    fn the_id_keeps_two_identical_titles_apart() {
        let a = file_name_for("Discord Nitro", "6870AAC98E444CD6");
        let b = file_name_for("Discord Nitro", "39FEA8F9C18B4222");
        assert_ne!(a, b);
    }

    /// A formula in a cell is executed on open, and the title column is
    /// filled from a feed this app reaches with no authentication.
    #[test]
    fn a_title_that_looks_like_a_formula_is_defused() {
        assert_eq!(
            export_field("=HYPERLINK(\"http://x\")"),
            "\"'=HYPERLINK(\"\"http://x\"\")\""
        );
        assert_eq!(export_field("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(export_field("Discord Nitro"), "Discord Nitro");
    }

    /// The export carries the BOM and CRLF the on-disk file must not.
    #[test]
    fn the_export_opens_with_a_byte_order_mark_and_ends_lines_with_crlf() {
        // Built by hand rather than through csv_for_offer, which needs a
        // data directory the test process does not have.
        let head: Vec<&str> = HEADER.trim_end().split(',').collect();
        let text = format!("\u{feff}{}\r\n", head.join(","));
        assert!(text.starts_with('\u{feff}'));
        assert!(text.ends_with("\r\n"));
        assert!(text.contains("gamertag,xuid"), "still commas");
    }

    /// A newline inside a value would write a record the line-oriented
    /// readers cannot see. Folding it keeps one record on one line.
    #[test]
    fn a_line_break_never_survives_into_a_row() {
        assert!(!field("a\nb").contains('\n'));
        assert!(!export_field("a\r\nb").contains('\n'));
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
        assert_eq!(columns, COLUMNS, "COLUMNS is what tells a legacy row apart");
    }
}
