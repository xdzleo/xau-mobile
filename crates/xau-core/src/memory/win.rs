//! Windows implementation of the process-memory reader.
//!
//! Port of the memory.dll code vendored in `XAU/Util/Memory/*.cs`, trimmed to
//! what the XAUTH extractor actually needs. Major performance optimisations
//! over the vendored `memory.dll`:
//!
//! - **SIMD pattern search** via `memchr::memmem::Finder` (SSE2/AVX2 on
//!   x86_64, NEON on aarch64) instead of `haystack.windows().position()`.
//!   Empirically 5–10× faster on typical Xbox PC app regions. Pattern is
//!   compiled once and reused across every region.
//! - **Chunked region reads** with overlap so huge regions (hundreds of MB)
//!   don't balloon RAM and tokens straddling chunk boundaries are not lost.
//! - **Rayon region-level parallelism** over `ReadProcessMemory` + scan, so
//!   kernel I/O and SIMD pattern match overlap on multi-core hosts.
//! - **Minimum-required process access rights** (`PROCESS_QUERY_INFORMATION
//!   | PROCESS_VM_READ`) so `OpenProcess` doesn't fail without elevation
//!   on locked-down Windows builds.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::mem::{self, MaybeUninit};
use std::os::windows::ffi::OsStringExt;

use memchr::memmem::Finder;
use rayon::prelude::*;
use tracing::debug;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};

use crate::error::{Error, Result};
use crate::xbox::constants::{XAUTH_SCAN_PATTERN, XAUTH_SCAN_PATTERN_UTF16_LE};

// ---- scan tuning ----------------------------------------------------------

/// Size of the rolling buffer used when reading a region in chunks. 4 MiB is
/// big enough to amortise `ReadProcessMemory` latency and small enough that
/// several concurrent rayon workers don't blow up peak RAM.
const MEMORY_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Maximum XAUTH length we're willing to accept. Real tokens are ~600 bytes,
/// so 4 KiB is comfortably above the upper bound and well below a useful
/// substring-splitter heuristic.
const MAX_TOKEN_LEN: usize = 4096;

/// Overlap between consecutive chunks within one region so a token that
/// straddles the seam is still found by the SIMD finder. Must be ≥ the
/// longest match window we care about (== `MAX_TOKEN_LEN`).
const CHUNK_OVERLAP: usize = MAX_TOKEN_LEN;

// -------------------------------------------------------------------------
// Process handle (RAII)
// -------------------------------------------------------------------------

struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn open(pid: u32) -> Result<Self> {
        // Minimum-required access. The vendored C# memory.dll asks for
        // `0x1F0FFF` (PROCESS_ALL_ACCESS) which fails without elevation on
        // current Windows builds.
        let access = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
        // SAFETY: FFI call returning a HANDLE; we check for null /
        // INVALID_HANDLE_VALUE before using it.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            return Err(Error::memory(format!("OpenProcess({pid}) failed: {e}")));
        }
        Ok(Self(h))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: handle was obtained from `OpenProcess`.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// `HANDLE` is `*mut c_void`, which is not Send/Sync by default. The kernel
// serialises access, so we opt in explicitly for rayon.
unsafe impl Send for ProcessHandle {}
unsafe impl Sync for ProcessHandle {}

/// Wrapper that stores the `HANDLE` as a `usize` so it can cross thread
/// boundaries and survive rayon's capture analysis. The HANDLE is converted
/// back to `*mut c_void` only on FFI calls.
///
/// Safety: the kernel serialises access to process handles, so sharing one
/// across rayon workers for read-only `VirtualQueryEx` / `ReadProcessMemory`
/// calls is sound.
#[derive(Copy, Clone)]
struct SendHandle(usize);

impl SendHandle {
    fn new(h: HANDLE) -> Self {
        Self(h as usize)
    }
    #[inline]
    fn get(self) -> HANDLE {
        self.0 as HANDLE
    }
}

// -------------------------------------------------------------------------
// Process enumeration
// -------------------------------------------------------------------------

pub(super) fn is_process_running_impl(name: &str) -> Result<bool> {
    Ok(find_process_id_by_name(name)?.is_some())
}

/// Returns the first PID matching the given image name (case-insensitive).
///
/// The `.exe` suffix is stripped before comparison, matching the original C#
/// helper in `Util/Memory/memory.cs`.
pub fn find_process_id_by_name(name: &str) -> Result<Option<u32>> {
    Ok(enumerate_processes()?
        .into_iter()
        .find(|(_, n)| normalize_process_name(n) == normalize_process_name(name))
        .map(|(pid, _)| pid))
}

/// Enumerate (pid, image_name) for every running process. Cheap — the
/// ToolHelp snapshot is a single kernel call.
pub fn enumerate_processes() -> Result<Vec<(u32, String)>> {
    // SAFETY: ToolHelp snapshot API — handle closed by `Closer` on drop.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(Error::memory(format!(
            "CreateToolhelp32Snapshot failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let _guard = Closer(snapshot);

    let mut entry: PROCESSENTRY32W = unsafe { mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut out = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return Ok(out);
    }
    loop {
        let name_len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let os = OsString::from_wide(&entry.szExeFile[..name_len]);
        out.push((entry.th32ProcessID, os.to_string_lossy().into_owned()));
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    Ok(out)
}

/// Return the first process whose image name contains `needle`
/// (case-insensitive, `.exe`/`.bin` suffix ignored).
pub(super) fn any_xbox_process_running_impl() -> Result<Option<String>> {
    Ok(enumerate_processes()?
        .into_iter()
        .find(|(_, n)| n.to_ascii_lowercase().contains("xbox"))
        .map(|(_, n)| n))
}

fn normalize_process_name(name: &str) -> String {
    let mut s = name.to_ascii_lowercase();
    for suffix in [".exe", ".bin"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.to_string();
        }
    }
    s
}

struct Closer(HANDLE);
impl Drop for Closer {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// -------------------------------------------------------------------------
// Region walking
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Region {
    base: u64,
    size: usize,
}

fn collect_committed_regions(h: HANDLE) -> Vec<Region> {
    let mut sys: MaybeUninit<SYSTEM_INFO> = MaybeUninit::uninit();
    // SAFETY: FFI call writes into a stack struct we then assume_init.
    unsafe { GetSystemInfo(sys.as_mut_ptr()) };
    let sys = unsafe { sys.assume_init() };

    let min_addr = sys.lpMinimumApplicationAddress as usize as u64;
    let max_addr = sys.lpMaximumApplicationAddress as usize as u64;

    let mut current = min_addr;
    let mut regions = Vec::new();

    while current < max_addr {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { mem::zeroed() };
        // SAFETY: FFI call, `info` is a properly aligned out param.
        let written = unsafe {
            VirtualQueryEx(
                h,
                current as *const _,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            break;
        }

        let base = info.BaseAddress as usize as u64;
        let size = info.RegionSize;
        let protect = info.Protect;
        let state = info.State;

        // Match the simpler Python scanner the user observed working: include
        // every committed region that isn't PAGE_GUARD / PAGE_NOACCESS. Do
        // NOT filter by Type (this excluded MEM_MAPPED regions where some
        // Windows builds of the Xbox app now cache the XAUTH) or by the
        // "readable" protect bits (those aren't a reliable signal on
        // modern builds either). ReadProcessMemory returns 0 on regions we
        // genuinely can't read, which we handle.
        let is_commit = state == MEM_COMMIT;
        let not_guarded = (protect & PAGE_GUARD) == 0;
        let accessible = (protect & PAGE_NOACCESS) == 0;

        if is_commit && not_guarded && accessible && size > 0 {
            // Coalesce with the previous region if contiguous — matches the
            // C# behaviour and halves the number of rayon jobs.
            if let Some(last) = regions.last_mut() {
                let l: &mut Region = last;
                if l.base + l.size as u64 == base {
                    l.size = l.size.saturating_add(size);
                    current = base + size as u64;
                    continue;
                }
            }
            regions.push(Region { base, size });
        }

        let next = base.saturating_add(size as u64);
        if next <= current {
            break;
        }
        current = next;
    }

    debug!(count = regions.len(), "collected committed regions");
    regions
}

// -------------------------------------------------------------------------
// Chunked read + SIMD scan
// -------------------------------------------------------------------------

fn read_chunk(h: HANDLE, base: u64, buf: &mut [u8]) -> usize {
    let mut read: usize = 0;
    // SAFETY: FFI call; `buf` is a properly sized mutable slice and `h` is a
    // non-null handle.
    let ok = unsafe {
        ReadProcessMemory(
            h,
            base as *const std::ffi::c_void,
            buf.as_mut_ptr().cast::<std::ffi::c_void>(),
            buf.len(),
            &mut read,
        )
    };
    if ok == 0 {
        return 0;
    }
    read
}

/// How a match was decoded — controls how `on_hit` interprets the slice.
#[derive(Copy, Clone)]
enum Encoding {
    Ascii,
    Utf16Le,
}

/// Scan a single region in overlapping chunks. `finders` holds one
/// precompiled SIMD Two-Way matcher per encoding; each is `Sync` so they
/// can be reused across rayon workers.
///
/// For every match found, `on_hit` is called with:
///   - the **absolute virtual address** of the first byte of the match
///   - the UTF-8 bytes of the decoded candidate string (ASCII passes
///     through, UTF-16 is decoded — both capped at `MAX_TOKEN_LEN`).
fn scan_region<F>(
    h: HANDLE,
    region: Region,
    finders: &[(Encoding, &Finder<'_>)],
    buf: &mut Vec<u8>,
    mut on_hit: F,
) where
    F: FnMut(u64, Encoding, &[u8]),
{
    if region.size == 0 {
        return;
    }
    if buf.len() < MEMORY_CHUNK_SIZE.min(region.size) {
        buf.resize(MEMORY_CHUNK_SIZE.min(region.size), 0);
    }

    let mut offset: usize = 0;
    // Scratch buffer for UTF-16 → UTF-8 decoding, reused across matches.
    let mut scratch: Vec<u8> = Vec::with_capacity(MAX_TOKEN_LEN);

    while offset < region.size {
        let remaining = region.size - offset;
        let read_size = remaining.min(buf.len());
        let slice = &mut buf[..read_size];

        let bytes_read = read_chunk(h, region.base + offset as u64, slice);
        if bytes_read == 0 {
            return;
        }
        let data = &slice[..bytes_read];

        for (enc, finder) in finders {
            for match_start in finder.find_iter(data) {
                match enc {
                    Encoding::Ascii => {
                        let limit = (match_start + MAX_TOKEN_LEN).min(data.len());
                        let end = if let Some(i) = memchr::memchr(0, &data[match_start..limit]) {
                            match_start + i
                        } else {
                            data[match_start..limit]
                                .iter()
                                .position(|&b| b < 0x20)
                                .map_or(limit, |i| match_start + i)
                        };
                        let addr = region.base + (offset + match_start) as u64;
                        on_hit(addr, Encoding::Ascii, &data[match_start..end]);
                    }
                    Encoding::Utf16Le => {
                        // Walk u16 pairs until a u16 NUL (two consecutive 0
                        // bytes on an even boundary) or MAX_TOKEN_LEN.
                        scratch.clear();
                        let mut cursor = match_start;
                        while cursor + 1 < data.len() && scratch.len() < MAX_TOKEN_LEN {
                            let lo = data[cursor];
                            let hi = data[cursor + 1];
                            if lo == 0 && hi == 0 {
                                break;
                            }
                            // Reject anything outside BMP ASCII — real XAUTH
                            // is printable ASCII, so bail as soon as we see
                            // anything weird. This also correctly terminates
                            // on misalignment.
                            if hi != 0 || !(0x20..=0x7E).contains(&lo) {
                                break;
                            }
                            scratch.push(lo);
                            cursor += 2;
                        }
                        if !scratch.is_empty() {
                            let addr = region.base + (offset + match_start) as u64;
                            on_hit(addr, Encoding::Utf16Le, &scratch);
                        }
                    }
                }
            }
        }

        if read_size >= remaining {
            break;
        }
        // Advance with a small overlap so that a pattern match straddling the
        // seam is picked up on the next iteration.
        let advance = read_size.saturating_sub(CHUNK_OVERLAP);
        if advance == 0 {
            break;
        }
        offset += advance;
    }
}

// -------------------------------------------------------------------------
// Public API
// -------------------------------------------------------------------------

/// Finds every byte offset (absolute virtual address) where `pattern` occurs
/// in the committed regions of the process identified by `pid`.
///
/// The scan is parallelised per region using rayon and uses SIMD substring
/// search (memchr). `max_regions == 0` scans everything.
pub fn scan_process_for_pattern(pid: u32, pattern: &[u8], max_regions: usize) -> Result<Vec<u64>> {
    let handle = ProcessHandle::open(pid)?;
    let mut regions = collect_committed_regions(handle.raw());
    if max_regions > 0 && regions.len() > max_regions {
        regions.truncate(max_regions);
    }

    let finder = Finder::new(pattern);
    let raw = SendHandle::new(handle.raw());

    let finders: &[(Encoding, &Finder<'_>)] = &[(Encoding::Ascii, &finder)];
    let results: Vec<u64> = regions
        .par_iter()
        .flat_map_iter(move |region| {
            let mut buf = Vec::with_capacity(MEMORY_CHUNK_SIZE.min(region.size));
            let mut hits: Vec<u64> = Vec::new();
            scan_region(
                raw.get(),
                *region,
                finders,
                &mut buf,
                |addr, _enc, _slice| {
                    hits.push(addr);
                },
            );
            hits.into_iter()
        })
        .collect();

    Ok(results)
}

/// Read `length` bytes from `pid` at virtual address `address`, interpret
/// them as UTF-8 and cut at the first NUL.
pub fn read_process_string(pid: u32, address: u64, length: usize) -> Result<String> {
    let handle = ProcessHandle::open(pid)?;
    let mut buf = vec![0u8; length];
    let n = read_chunk(handle.raw(), address, &mut buf);
    if n == 0 {
        return Ok(String::new());
    }
    buf.truncate(n);
    let cut = memchr::memchr(0, &buf).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..cut]).into_owned())
}

// -------------------------------------------------------------------------
// XAUTH extraction
// -------------------------------------------------------------------------

pub(super) fn extract_xauth_from_xbox_app_impl(
    process_name: &str,
    min_frequency: usize,
) -> Result<Option<String>> {
    let Some(pid) = find_process_id_by_name(process_name)? else {
        return Ok(None);
    };
    scan_pid_for_xauth(pid, min_frequency).map(|(token, _, _)| token)
}

/// Scan a specific PID and return `(best_token_if_above_threshold, top_frequency)`.
pub(super) fn scan_pid_for_xauth(
    pid: u32,
    min_frequency: usize,
) -> Result<(Option<String>, usize, Option<String>)> {
    scan_pid_detailed(pid, min_frequency).map(|r| (r.token, r.frequency, r.masked))
}

pub(super) struct ScanOutcome {
    pub token: Option<String>,
    pub frequency: usize,
    pub masked: Option<String>,
    pub ascii_hits: usize,
    pub utf16_hits: usize,
    pub raw_candidates: usize,
    /// All unique candidate strings + their frequency in this process.
    pub unique: Vec<(String, usize)>,
}

pub(super) fn scan_pid_detailed(pid: u32, min_frequency: usize) -> Result<ScanOutcome> {
    let handle = ProcessHandle::open(pid)?;
    let regions = collect_committed_regions(handle.raw());
    let finder_ascii = Finder::new(XAUTH_SCAN_PATTERN);
    let finder_utf16 = Finder::new(XAUTH_SCAN_PATTERN_UTF16_LE);
    let raw = SendHandle::new(handle.raw());

    // Collect all candidate token strings across regions, in parallel, using
    // a per-worker scratch buffer so we allocate once per rayon job rather
    // than once per region. Counts ASCII vs UTF-16 hits separately so the
    // diagnostic can report which encoding the Xbox app is using this build.
    #[allow(clippy::items_after_statements)]
    type Acc = (Vec<u8>, Vec<(String, bool /* utf16? */)>);
    let raw_candidates: Vec<(String, bool)> = regions
        .par_iter()
        .fold(
            || -> Acc { (Vec::with_capacity(MEMORY_CHUNK_SIZE), Vec::new()) },
            move |(mut buf, mut local): Acc, region| -> Acc {
                let ascii_pair: (Encoding, &Finder<'_>) = (Encoding::Ascii, &finder_ascii);
                let utf16_pair: (Encoding, &Finder<'_>) = (Encoding::Utf16Le, &finder_utf16);
                let finders: &[(Encoding, &Finder<'_>)] = &[ascii_pair, utf16_pair];
                scan_region(
                    raw.get(),
                    *region,
                    finders,
                    &mut buf,
                    |_addr, enc, slice| {
                        if let Ok(s) = std::str::from_utf8(slice) {
                            if looks_like_xauth(s) {
                                let is_utf16 = matches!(enc, Encoding::Utf16Le);
                                local.push((s.to_string(), is_utf16));
                            }
                        }
                    },
                );
                (buf, local)
            },
        )
        .map(|(_, v): Acc| v)
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });

    let candidates: Vec<String> = raw_candidates.iter().map(|(s, _)| s.clone()).collect();
    let ascii_hits = raw_candidates.iter().filter(|(_, u)| !*u).count();
    let utf16_hits = raw_candidates.iter().filter(|(_, u)| *u).count();

    if candidates.is_empty() {
        return Ok(ScanOutcome {
            token: None,
            frequency: 0,
            masked: None,
            ascii_hits,
            utf16_hits,
            raw_candidates: 0,
            unique: Vec::new(),
        });
    }

    // Voting heuristic: pick the most-frequent string. Filters out stale
    // copy-on-write duplicates and partial fragments. Matches the original
    // C# `HomeViewModel.GetXAUTH` behaviour.
    let mut frequency: HashMap<String, usize> = HashMap::new();
    for s in &candidates {
        *frequency.entry(s.clone()).or_insert(0) += 1;
    }

    let raw_count = candidates.len();
    let (best, count) = frequency
        .into_iter()
        .max_by_key(|(_, v)| *v)
        .expect("non-empty candidates ensured above");

    let masked = best
        .strip_prefix("XBL3.0 x=")
        .and_then(|rest| rest.split_once(';'))
        .map(|(uhs, tok)| {
            format!(
                "XBL3.0 x={uhs};{}…({} chars)",
                &tok.chars().take(12).collect::<String>(),
                tok.len()
            )
        });

    let unique: Vec<(String, usize)> = {
        let mut freq_local: HashMap<String, usize> = HashMap::new();
        for s in &candidates {
            *freq_local.entry(s.clone()).or_insert(0) += 1;
        }
        let mut v: Vec<(String, usize)> = freq_local.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };

    let token = if count >= min_frequency {
        Some(best)
    } else {
        None
    };
    Ok(ScanOutcome {
        token,
        frequency: count,
        masked,
        ascii_hits,
        utf16_hits,
        raw_candidates: raw_count,
        unique,
    })
}

/// Walk every xbox-ish (and optionally every) process, collect every unique
/// XAUTH candidate seen in memory, and return them sorted by the sum of
/// their in-memory frequencies across all processes.
pub(super) fn extract_all_candidates_impl(exhaustive: bool) -> Result<Vec<super::TokenCandidate>> {
    let processes = enumerate_processes()?;
    let mut to_try: Vec<(u32, String)> = Vec::new();

    for candidate in super::XBOX_PROCESS_CANDIDATES {
        let target = normalize_process_name(candidate);
        if let Some((pid, name)) = processes
            .iter()
            .find(|(_, n)| normalize_process_name(n) == target)
            .cloned()
        {
            to_try.push((pid, name));
        }
    }
    for (pid, name) in &processes {
        let lower = name.to_ascii_lowercase();
        if (lower.contains("xbox") || lower.contains("gaming"))
            && !to_try.iter().any(|(p, _)| p == pid)
        {
            to_try.push((*pid, name.clone()));
        }
    }
    if exhaustive {
        for (pid, name) in &processes {
            if !to_try.iter().any(|(p, _)| p == pid) {
                to_try.push((*pid, name.clone()));
            }
        }
    }

    // Scan processes in parallel with rayon. Each process's scan already
    // uses rayon internally for per-region parallelism, but the outer loop
    // over processes was sequential — so a machine with 4 Xbox-related
    // processes would only ever use one process-worth of work at a time.
    // With par_iter here we overlap I/O-bound `VirtualQueryEx` latency
    // across all candidate processes. rayon's work-stealing handles the
    // nested parallelism fine.
    //
    // `par_iter().map()` returns per-process HashMaps; we fold them into
    // the combined map serially (cheap compared to the scans themselves).
    let per_process: Vec<(String, HashMap<String, usize>)> = to_try
        .par_iter()
        .filter_map(|(pid, name)| {
            let outcome = scan_pid_detailed(*pid, 1).ok()?;
            let map: HashMap<String, usize> = outcome.unique.into_iter().collect();
            Some((name.clone(), map))
        })
        .collect();

    // token → (combined_freq, sources)
    let mut combined: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    for (name, uniq) in per_process {
        for (tok, freq) in uniq {
            let entry = combined.entry(tok).or_insert_with(|| (0, Vec::new()));
            entry.0 = entry.0.saturating_add(freq);
            if !entry.1.iter().any(|n| n == &name) {
                entry.1.push(name.clone());
            }
        }
    }

    let mut list: Vec<super::TokenCandidate> = combined
        .into_iter()
        .map(|(token, (frequency, sources))| super::TokenCandidate {
            token,
            frequency,
            sources,
        })
        .collect();
    list.sort_by(|a, b| b.frequency.cmp(&a.frequency));
    Ok(list)
}

/// Implementation behind [`extract_xauth_anywhere`]: tries a curated list of
/// candidate processes first, then (if `exhaustive`) falls back to every
/// process whose name contains "xbox" or "gaming".
pub(super) fn extract_xauth_anywhere_impl(
    min_frequency: usize,
    exhaustive: bool,
) -> Result<super::ScanReport> {
    let processes = enumerate_processes()?;
    let mut to_try: Vec<(u32, String)> = Vec::new();

    // 1. Known image names, preserving the curated order.
    for candidate in super::XBOX_PROCESS_CANDIDATES {
        let target = normalize_process_name(candidate);
        if let Some((pid, name)) = processes
            .iter()
            .find(|(_, n)| normalize_process_name(n) == target)
            .cloned()
        {
            to_try.push((pid, name));
        }
    }

    // 2. Exhaustive fallback: every xbox-ish process not already tried.
    if exhaustive {
        for (pid, name) in &processes {
            let lower = name.to_ascii_lowercase();
            if (lower.contains("xbox") || lower.contains("gaming"))
                && !to_try.iter().any(|(p, _)| p == pid)
            {
                to_try.push((*pid, name.clone()));
            }
        }
    }

    let mut report = super::ScanReport::default();
    for (pid, name) in to_try {
        let attempt = match scan_pid_detailed(pid, min_frequency) {
            Ok(r) => {
                if let Some(t) = r.token.as_ref() {
                    if r.frequency > report.best_frequency {
                        report.best_frequency = r.frequency;
                        report.best_token = Some(t.clone());
                    }
                }
                super::ProcessAttempt {
                    name,
                    pid: Some(pid),
                    candidates: r.raw_candidates,
                    top_frequency: r.frequency,
                    top_token_prefix: r.masked,
                    error: None,
                    raw_hits: r.raw_candidates,
                    ascii_hits: r.ascii_hits,
                    utf16_hits: r.utf16_hits,
                }
            }
            Err(e) => super::ProcessAttempt {
                name,
                pid: Some(pid),
                candidates: 0,
                top_frequency: 0,
                top_token_prefix: None,
                error: Some(e.to_string()),
                raw_hits: 0,
                ascii_hits: 0,
                utf16_hits: 0,
            },
        };
        report.tried.push(attempt);
        if report.best_token.is_some() && !exhaustive {
            break;
        }
    }

    // Last-ditch fallback: if no xbox/gaming process had the token in
    // memory, sweep every running process. This catches the (common) case
    // where the XAUTH lives inside `svchost.exe` because the Xbox Identity
    // Provider service is the actual cache-holder rather than the UI.
    let nothing_found = report.best_token.is_none() && report.tried.iter().all(|a| a.raw_hits == 0);
    if exhaustive && nothing_found {
        if let Ok(full) = scan_every_process(min_frequency) {
            for attempt in full.tried {
                report.tried.push(attempt);
            }
            if full.best_frequency > report.best_frequency {
                report.best_frequency = full.best_frequency;
                report.best_token = full.best_token;
            }
        }
    }

    Ok(report)
}

/// Last-resort scan — walks **every** process the kernel reports, not just
/// xbox/gaming ones. Slow (a few seconds on a busy machine) but catches
/// cases where the XAUTH lives in a process with an unexpected name.
pub fn scan_every_process(min_frequency: usize) -> Result<super::ScanReport> {
    let mut report = super::ScanReport::default();
    let processes = enumerate_processes()?;
    for (pid, name) in processes {
        match scan_pid_detailed(pid, min_frequency) {
            Ok(r) if r.raw_candidates > 0 => {
                if let Some(t) = r.token.as_ref() {
                    if r.frequency > report.best_frequency {
                        report.best_frequency = r.frequency;
                        report.best_token = Some(t.clone());
                    }
                }
                report.tried.push(super::ProcessAttempt {
                    name,
                    pid: Some(pid),
                    candidates: r.raw_candidates,
                    top_frequency: r.frequency,
                    top_token_prefix: r.masked,
                    error: None,
                    raw_hits: r.raw_candidates,
                    ascii_hits: r.ascii_hits,
                    utf16_hits: r.utf16_hits,
                });
            }
            _ => {} // process had zero hits or failed to open — skip
        }
    }
    Ok(report)
}

/// Structural validation of `XBL3.0 x=<uhs>;<xsts>`.
/// - `uhs`: 1..=64 chars
/// - `xsts`: at least 50 chars (real tokens are ~300)
/// - No second `;` anywhere in the payload
/// - Printable ASCII only (no whitespace, no control characters)
#[inline]
fn looks_like_xauth(s: &str) -> bool {
    let Some(payload) = s.strip_prefix("XBL3.0 x=") else {
        return false;
    };
    let Some((uhs, xsts)) = payload.split_once(';') else {
        return false;
    };
    if uhs.is_empty() || uhs.len() > 64 {
        return false;
    }
    if xsts.len() < 50 {
        return false;
    }
    if xsts.contains(';') {
        return false;
    }
    payload.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_extensions() {
        assert_eq!(normalize_process_name("XboxPcApp"), "xboxpcapp");
        assert_eq!(normalize_process_name("XboxPcApp.exe"), "xboxpcapp");
        assert_eq!(normalize_process_name("game.bin"), "game");
    }

    #[test]
    fn finds_pattern_via_memchr() {
        let bytes = b"junk\x00XBL3.0 x=abc;deftoken\x00more";
        let finder = Finder::new(b"XBL3.0 x=");
        let positions: Vec<usize> = finder.find_iter(bytes).collect();
        assert_eq!(positions, vec![5]);
    }

    #[test]
    fn looks_like_xauth_rejects_garbage() {
        assert!(!looks_like_xauth("foo"));
        assert!(!looks_like_xauth("XBL3.0 x=;shorttoken"));
        assert!(!looks_like_xauth("XBL3.0 x=uhs;tooshort"));
    }

    #[test]
    fn looks_like_xauth_accepts_real_shape() {
        let jwt_ish = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsIng1dCI6IjEifQ.abcdefghij.klmnopqrst";
        assert!(looks_like_xauth(&format!("XBL3.0 x=1234567890;{jwt_ish}")));
    }

    #[test]
    fn looks_like_xauth_rejects_extra_semicolon() {
        // Second ';' after the split_once means the XSTS part contains it.
        let bad =
            "XBL3.0 x=uhs;eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.payloadpayloadpayload.sig;more";
        assert!(!looks_like_xauth(bad));
    }

    #[test]
    fn looks_like_xauth_rejects_whitespace() {
        let bad = "XBL3.0 x=uhs; token_with_leading_space_and_50_chars_right_here_yep";
        assert!(!looks_like_xauth(bad));
    }
}
