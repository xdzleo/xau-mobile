//! Process memory reader, used to extract an `XBL3.0 x=…;…` token from the
//! Xbox PC app by scanning its user-mode address space for the `XBL3.0 x=`
//! byte pattern.
//!
//! The implementation is Windows-only. On other platforms the public API still
//! compiles, but every call returns [`Error::Unsupported`]. This matches the
//! spirit of the original C# code which relied on `kernel32.dll`.

#[cfg(not(windows))]
use crate::error::Error;
use crate::error::Result;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::*;

/// Image-names the Xbox identity may live in on current Windows builds. We
/// try each in order and take the first match. Modern Windows 11 ships the
/// Xbox app as `XboxPcApp.exe` but older machines (and the Game Pass
/// variants) use different names — and sometimes the XAUTH is cached by
/// ancillary services instead of the main app.
pub const XBOX_PROCESS_CANDIDATES: &[&str] = &[
    "XboxPcApp",
    "Xbox",
    "XboxApp",
    "Microsoft.GamingApp",
    "GamingServices",
    "XboxGameBar",
    "XboxGamingOverlay",
];

/// Result of a diagnostic scan across multiple processes. Useful when the
/// regular extraction fails so the user can see which processes existed and
/// how many matches each had.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub tried: Vec<ProcessAttempt>,
    pub best_token: Option<String>,
    pub best_frequency: usize,
}

#[derive(Debug, Clone)]
pub struct ProcessAttempt {
    pub name: String,
    pub pid: Option<u32>,
    pub candidates: usize,
    pub top_frequency: usize,
    pub top_token_prefix: Option<String>,
    pub error: Option<String>,
    /// Raw pattern matches before deduping and validation. Useful to know
    /// whether the pattern is in memory at all.
    pub raw_hits: usize,
    /// How many raw hits were ASCII (UTF-8) vs UTF-16 LE.
    pub ascii_hits: usize,
    pub utf16_hits: usize,
}

/// A deduped XAUTH candidate with the processes it was seen in.
#[derive(Debug, Clone)]
pub struct TokenCandidate {
    pub token: String,
    /// Combined frequency across every process that held this exact string.
    pub frequency: usize,
    /// Names of the processes this token was extracted from.
    pub sources: Vec<String>,
}

/// Returns every unique XAUTH candidate across every xbox/gaming process
/// (and, if `exhaustive`, every running process), sorted by frequency descending.
/// Multiple tokens often coexist in memory — partial, expired, cached — and
/// only one is currently valid. Use [`validate_best_token`] to pick it.
pub fn extract_all_candidates(exhaustive: bool) -> Result<Vec<TokenCandidate>> {
    #[cfg(windows)]
    {
        extract_all_candidates_impl(exhaustive)
    }
    #[cfg(not(windows))]
    {
        let _ = exhaustive;
        Err(Error::Unsupported("process memory reading is Windows-only"))
    }
}

/// High-level entry point: find the Xbox PC app process and extract the most
/// frequently-seen `XBL3.0 x=…;…` string from its committed memory regions.
///
/// - `process_name` defaults to `XboxPcApp` (no extension).
/// - `min_frequency` is the minimum number of times the *same* string must
///   appear across scanned regions before it is considered a valid XAUTH.
///   The original C# code used `> 3`, but recent Windows builds sometimes
///   cache the token only once or twice in memory, so the CLI/GUI default
///   was lowered to `2`.
///
/// Returns `Ok(None)` if the process was not found or no candidate met the
/// frequency threshold.
pub fn extract_xauth_from_xbox_app(
    process_name: &str,
    min_frequency: usize,
) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        extract_xauth_from_xbox_app_impl(process_name, min_frequency)
    }
    #[cfg(not(windows))]
    {
        let _ = (process_name, min_frequency);
        Err(Error::Unsupported("process memory reading is Windows-only"))
    }
}

/// Try every candidate in [`XBOX_PROCESS_CANDIDATES`] until one yields a
/// valid-looking XAUTH, or scan every process matching "xbox" / "gaming" if
/// `exhaustive` is set. Prefer this entry point over `extract_xauth_from_xbox_app`
/// for the CLI/GUI flow — it falls back across image-name changes and
/// down-ranks processes that returned garbage.
pub fn extract_xauth_anywhere(min_frequency: usize, exhaustive: bool) -> Result<ScanReport> {
    #[cfg(windows)]
    {
        extract_xauth_anywhere_impl(min_frequency, exhaustive)
    }
    #[cfg(not(windows))]
    {
        let _ = (min_frequency, exhaustive);
        Err(Error::Unsupported("process memory reading is Windows-only"))
    }
}

/// Last-resort scan — try to find the pattern in **every** running process
/// (not just ones matching "xbox"/"gaming"). Use sparingly; takes seconds.
pub fn scan_every_process(min_frequency: usize) -> Result<ScanReport> {
    #[cfg(windows)]
    {
        win::scan_every_process(min_frequency)
    }
    #[cfg(not(windows))]
    {
        let _ = min_frequency;
        Err(Error::Unsupported("process memory reading is Windows-only"))
    }
}

/// Returns whether the Xbox PC app (or the process given by `name`) is running.
pub fn is_process_running(name: &str) -> Result<bool> {
    #[cfg(windows)]
    {
        is_process_running_impl(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Err(Error::Unsupported("process enumeration is Windows-only"))
    }
}

/// Returns whether *any* process whose image name contains "xbox" (case
/// insensitive) is currently running.
pub fn any_xbox_process_running() -> Result<Option<String>> {
    #[cfg(windows)]
    {
        any_xbox_process_running_impl()
    }
    #[cfg(not(windows))]
    {
        Err(Error::Unsupported("process enumeration is Windows-only"))
    }
}
