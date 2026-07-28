//! Self-updater: port of `HomeViewModel.UpdateTool`.
//!
//! Downloads a new binary to a temp path next to the current exe, writes a
//! short `.bat` that waits for us to exit, moves the new binary into place
//! and relaunches. On non-Windows this is a no-op that returns `Unsupported`.
//!
//! # Integrity (BUG HUNTER C1)
//!
//! The previous implementation downloaded whatever bytes the URL handed
//! over, wrote them to disk, and relaunched — no hash check, no signature,
//! no restriction on which host the redirect chain landed on. A poisoned
//! GitHub release, a hijacked S3 object, or a MITM on the user's network
//! would have meant trivial RCE as the user.
//!
//! We now:
//! 1. Pin allowed hosts: `github.com` (the asset URL) and its content
//!    delivery hosts (`objects.githubusercontent.com`,
//!    `release-assets.githubusercontent.com`,
//!    `*.s3.amazonaws.com` for the AWS-backed redirect target). Any
//!    redirect outside the allowlist is refused.
//! 2. Require a sibling `.sha256` file alongside the asset (e.g.
//!    `XAU.exe` → `XAU.exe.sha256`). The file MUST contain a 64-char
//!    lowercase hex SHA-256 of the asset bytes. Without it, the
//!    update is refused with a loud error rather than blindly applied.
//! 3. Compute SHA-256 of the staged bytes and compare. Mismatch ⇒
//!    delete the stage and refuse to apply.
//!
//! Real signing (codesign, ed25519 detached signature) would be even
//! better, but a published-hash-from-the-same-repo scheme already
//! defeats every practical attack short of "attacker fully owns the
//! release publishing flow", which is the upper bound of what any
//! GitHub-hosted updater can defend against without a separate trust
//! root.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use reqwest::redirect::Policy;
use reqwest::Client;

use crate::error::{Error, Result};

const UPDATE_FILENAME: &str = "XAU-new.exe";
const UPDATER_SCRIPT: &str = "XAU-Updater.bat";

/// Hosts the updater is willing to talk to (initial URL + every
/// redirect hop). Anything outside this list is a hard refusal.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "raw.githubusercontent.com",
];

/// Hosts whose suffix we tolerate (covers AWS-backed redirect targets
/// GitHub historically used: `*.s3.amazonaws.com`).
const ALLOWED_HOST_SUFFIXES: &[&str] = &[".s3.amazonaws.com"];

fn is_allowed_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    ALLOWED_HOSTS.iter().any(|a| *a == h) || ALLOWED_HOST_SUFFIXES.iter().any(|s| h.ends_with(s))
}

fn build_locked_down_client() -> Result<Client> {
    // Custom redirect policy: refuse any hop whose host isn't in the
    // allowlist. Keeps the default 10-hop cap to avoid loops.
    let policy = Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        // Snapshot the host into an owned String so the borrow on
        // `attempt.url()` is dropped before we call `attempt.error(_)`
        // (which moves `attempt`).
        let host = attempt.url().host_str().map(str::to_string);
        match host {
            None => attempt.error("redirect URL has no host"),
            Some(h) if is_allowed_host(&h) => attempt.follow(),
            Some(h) => attempt.error(format!(
                "self-updater: refusing redirect to disallowed host `{h}`"
            )),
        }
    });
    Client::builder()
        .redirect(policy)
        .gzip(true)
        .deflate(true)
        .build()
        .map_err(Error::from)
}

fn require_allowed_initial_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| Error::other(format!("self-updater: invalid asset URL `{url}`: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(Error::other(format!(
            "self-updater: asset URL must be https, got `{}`",
            parsed.scheme()
        )));
    }
    let Some(host) = parsed.host_str() else {
        return Err(Error::other("self-updater: asset URL has no host"));
    };
    if !is_allowed_host(host) {
        return Err(Error::other(format!(
            "self-updater: refusing asset URL on disallowed host `{host}`"
        )));
    }
    Ok(())
}

pub struct UpdatePlan {
    pub current_exe: PathBuf,
    pub staged_path: PathBuf,
    pub updater_path: PathBuf,
    /// SHA-256 (lowercase hex) the staged binary must STILL match at
    /// apply time. Set from the verified digest `download` returns; left
    /// `None` skips the apply-time re-check (manual staging / tests).
    pub expected_sha256: Option<String>,
}

impl UpdatePlan {
    pub fn current() -> Result<Self> {
        let current_exe = std::env::current_exe()?;
        let dir = current_exe
            .parent()
            .ok_or_else(|| Error::other("current exe has no parent directory"))?;
        Ok(Self {
            staged_path: dir.join(UPDATE_FILENAME),
            updater_path: dir.join(UPDATER_SCRIPT),
            current_exe,
            expected_sha256: None,
        })
    }
}

/// Download `url` to `destination` and verify its SHA-256 against the
/// sibling `{url}.sha256` published alongside the release. Both fetches
/// go through a redirect-locked client (see `build_locked_down_client`).
///
/// On hash mismatch or on inability to fetch the hash file, the staged
/// file is removed and an `Err` is returned — the caller MUST NOT fall
/// back to applying the unverified bytes.
///
/// Returns the verified lowercase-hex SHA-256 of the staged bytes. The
/// caller should stash it in [`UpdatePlan::expected_sha256`] so
/// [`apply_update`] can re-verify the on-disk file just before swapping
/// it in (closes the staged-file TOCTOU — see `apply_update`).
pub async fn download(url: &str, destination: &Path) -> Result<String> {
    require_allowed_initial_url(url)?;
    let client = build_locked_down_client()?;

    // 1. Fetch the expected hash first. Doing it before the binary
    // download means a poisoned mirror that serves the binary but no
    // hash file gets caught at zero cost.
    let hash_url = format!("{url}.sha256");
    require_allowed_initial_url(&hash_url)?;
    let expected_hex = fetch_expected_sha256(&client, &hash_url).await?;

    // 2. Download the binary itself.
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;

    // 3. Verify before touching disk.
    let actual_hex = crate::community::sha256_hex(&bytes);
    if !ct_eq_hex(&actual_hex, &expected_hex) {
        return Err(Error::other(format!(
            "self-updater: SHA-256 mismatch — refusing to install. \
             expected={expected_hex} got={actual_hex} \
             (asset URL: {url})"
        )));
    }

    // Wave-11 audit cross-cutting #4 (HIGH corrupt-binary): route
    // through `write_atomic`. Power loss / kill -9 mid-write would
    // otherwise leave the user with a half-written executable —
    // next launch loads a corrupt PE and the user has no recourse
    // short of redownloading. With temp+rename, power loss leaves
    // either the OLD binary or the FULL new content. The bytes
    // come from a SHA-256-verified download (line 152 above), so
    // no further integrity work is needed.
    crate::fs_atomic::write_atomic(destination, &bytes)?;
    Ok(expected_hex)
}

/// GET the sibling `.sha256` file and parse the first whitespace-
/// separated token as a 64-char lowercase hex digest.
///
/// The shape `{hex}  {filename}` produced by `sha256sum` is accepted
/// (only the first token matters). Anything that doesn't reduce to a
/// 64-char `[0-9a-f]` string is rejected — better to fail loudly than
/// auto-update on garbage.
async fn fetch_expected_sha256(client: &Client, url: &str) -> Result<String> {
    let text = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| {
            Error::other(format!(
                "self-updater: cannot fetch SHA-256 file `{url}` — \
                 release must publish a `<asset>.sha256` sibling. ({e})"
            ))
        })?
        .text()
        .await?;
    let token = text.split_whitespace().next().unwrap_or("").trim();
    let normalized = token.to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::other(format!(
            "self-updater: SHA-256 file at `{url}` is not a 64-char hex digest \
             (got `{token}`); refusing to update"
        )));
    }
    Ok(normalized)
}

/// Length-checked equality on two pre-validated lowercase hex digests.
/// Both inputs come from this module after explicit hex-shape checks,
/// so a plain `==` is fine — but routing it through a named helper
/// keeps the verification step obvious in `download`.
fn ct_eq_hex(a: &str, b: &str) -> bool {
    a.len() == b.len() && a == b
}

/// Returns `true` if the file at `path` still hashes to `expected_hex`
/// (lowercase hex SHA-256). Used by `apply_update` to re-verify the
/// staged binary immediately before swapping it in.
fn staged_hash_matches(path: &Path, expected_hex: &str) -> Result<bool> {
    let bytes = fs::read(path)?;
    let actual = crate::community::sha256_hex(&bytes);
    Ok(ct_eq_hex(&actual, expected_hex))
}

/// Render the self-relaunch batch script.
///
/// BUG HUNTER updater-brick: the previous script did
/// `del "<current_exe>"` then `ren "XAU-new.exe" "<name>"` with NO check
/// that the rename succeeded. If `ren` failed (e.g. antivirus held a lock
/// on the freshly-downloaded file), the running exe was ALREADY deleted
/// and nothing replaced it — the app was bricked. It also mixed an
/// absolute `del` with bare-filename `del`/`ren`/`start` that silently
/// depended on the batch's CWD. This version does a SINGLE `move /y` of
/// the staged file over the current exe using ABSOLUTE paths
/// (CWD-independent): on success the new binary is in place, on failure
/// the ORIGINAL is left untouched — either way the current exe still
/// exists and is relaunched, so the brick window is gone. A same-volume
/// `move` on NTFS is an atomic rename, so the destination is never empty.
fn render_updater_batch(staged: &Path, current_exe: &Path) -> String {
    // `(goto) 2>nul & del "%~f0"` is the canonical self-deleting-batch
    // idiom — cmd has the current line buffered, so the script can delete
    // itself as its final act.
    format!(
        "@echo off\r\n\
         timeout 1 > nul\r\n\
         move /y \"{staged}\" \"{current}\" > nul 2>&1\r\n\
         start \"\" \"{current}\"\r\n\
         (goto) 2>nul & del \"%~f0\"\r\n",
        staged = staged.display(),
        current = current_exe.display(),
    )
}

/// Write the updater batch and relaunch ourselves. Exits the current process
/// on success.
#[cfg(windows)]
pub fn apply_update(plan: &UpdatePlan) -> Result<()> {
    if !plan.staged_path.exists() {
        return Err(Error::other(format!(
            "staged binary not found at {}",
            plan.staged_path.display()
        )));
    }

    // BUG HUNTER updater-apply-toctou: re-verify the staged binary's
    // SHA-256 immediately before swapping it in. `download` verified the
    // bytes IN MEMORY, but they then sat on disk under a predictable name
    // (`XAU-new.exe`) in the install dir; any same-user process could swap
    // the file between download and apply. Re-hashing here closes that
    // window — a mismatch deletes the stage and refuses to apply.
    if let Some(expected) = &plan.expected_sha256 {
        if !staged_hash_matches(&plan.staged_path, expected)? {
            let _ = fs::remove_file(&plan.staged_path);
            return Err(Error::other(
                "self-updater: staged binary SHA-256 changed since download \
                 — refusing to apply (possible tampering)",
            ));
        }
    }

    let batch_body = render_updater_batch(&plan.staged_path, &plan.current_exe);
    let mut batch = fs::File::create(&plan.updater_path)?;
    batch.write_all(batch_body.as_bytes())?;
    batch.flush()?;
    drop(batch);

    Command::new(&plan.updater_path)
        .current_dir(plan.current_exe.parent().unwrap_or_else(|| Path::new(".")))
        .spawn()
        .map_err(|e| Error::other(format!("failed to start updater: {e}")))?;

    // Give the detached batch a moment, then exit.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Legitimate exception: the self-updater spawned a batch script
    // that will OVERWRITE the running binary. Returning to main()
    // normally risks a race where the batch tries to replace the
    // exe while Rust's Drop chain is still running; a clean exit(0)
    // gets out of the way. The Result in this function's signature
    // doesn't apply once the batch is detached.
    #[allow(clippy::disallowed_methods)]
    {
        std::process::exit(0);
    }
}

#[cfg(not(windows))]
pub fn apply_update(_plan: &UpdatePlan) -> Result<()> {
    Err(Error::Unsupported("the self-updater is Windows-only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_batch_is_brick_safe_move() {
        // BUG HUNTER updater-brick regression: the relaunch script must do
        // a single `move /y` of the staged file over the current exe —
        // never `del` the running exe before an unchecked `ren`. On a
        // failed move the original survives and is relaunched.
        let staged = Path::new(r"C:\Apps\XAU\XAU-new.exe");
        let current = Path::new(r"C:\Apps\XAU\XAU.exe");
        let script = render_updater_batch(staged, current);

        assert!(script.contains("move /y"), "must use move /y: {script}");
        assert!(script.contains(r"C:\Apps\XAU\XAU-new.exe"));
        assert!(script.contains(r"C:\Apps\XAU\XAU.exe"));
        assert!(script.contains("start \"\""), "must relaunch the exe");
        assert!(
            script.contains("del \"%~f0\""),
            "must self-delete the batch"
        );
        // The brick patterns must be gone: no outright delete of the
        // current exe, no unchecked rename.
        assert!(
            !script.contains("del \"C:\\Apps\\XAU\\XAU.exe\""),
            "must NOT delete the current exe (brick risk): {script}"
        );
        assert!(
            !script.contains(" ren "),
            "must not use an unchecked ren: {script}"
        );
    }

    #[test]
    fn staged_hash_matches_detects_tampering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("XAU-new.exe");
        let bytes = b"the real verified binary bytes";
        std::fs::write(&path, bytes).expect("write staged");

        let expected = crate::community::sha256_hex(bytes);
        assert!(
            staged_hash_matches(&path, &expected).expect("hash ok"),
            "an untampered staged file should match its digest"
        );

        // A digest that doesn't match the on-disk bytes → false: a
        // post-download swap is detected and apply_update refuses.
        let wrong = "0".repeat(64);
        assert!(
            !staged_hash_matches(&path, &wrong).expect("hash ok"),
            "a mismatched digest must be rejected"
        );
    }
}
