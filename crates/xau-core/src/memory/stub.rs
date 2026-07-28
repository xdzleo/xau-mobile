// Non-Windows stub — keeps the module surface stable on Linux/macOS so that
// the rest of the workspace builds cleanly. Every operation returns
// `Error::Unsupported`.

use crate::error::{Error, Result};

pub fn find_process_id_by_name(_name: &str) -> Result<Option<u32>> {
    Err(Error::Unsupported("process enumeration is Windows-only"))
}

pub(super) fn any_xbox_process_running_impl() -> Result<Option<String>> {
    Err(Error::Unsupported("process enumeration is Windows-only"))
}

pub(super) fn extract_xauth_anywhere_impl(
    _min_frequency: usize,
    _exhaustive: bool,
) -> Result<super::ScanReport> {
    Err(Error::Unsupported("process memory reading is Windows-only"))
}

pub fn scan_every_process(_min_frequency: usize) -> Result<super::ScanReport> {
    Err(Error::Unsupported("process memory reading is Windows-only"))
}

pub(super) fn extract_all_candidates_impl(_exhaustive: bool) -> Result<Vec<super::TokenCandidate>> {
    Err(Error::Unsupported("process memory reading is Windows-only"))
}

pub fn scan_process_for_pattern(
    _pid: u32,
    _pattern: &[u8],
    _max_regions: usize,
) -> Result<Vec<u64>> {
    Err(Error::Unsupported("process memory reading is Windows-only"))
}

pub fn read_process_string(_pid: u32, _address: u64, _length: usize) -> Result<String> {
    Err(Error::Unsupported("process memory reading is Windows-only"))
}
