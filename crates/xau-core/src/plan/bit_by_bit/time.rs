//! Small wall-clock helpers (`now_unix`, `now_unix_ms`, `wallclock_ms`).

pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wall-clock unix millisecond timestamp.
///
/// Used by:
/// - `sleep_until_scheduled_cancellable` to compare a sub-second
///   schedule target (`scheduled_unix * 1000 + scheduled_subsec_ms`)
///   against the current instant with millisecond precision.
/// - The wire-event forensic log (`log_wire_event`) so a post-hoc
///   audit can compute actual inter-POST gaps and confirm the
///   bit-by-bit replay followed the plan's chronology at ms
///   resolution.
///
/// Falls back to `0` on a system clock before 1970 (can't happen on
/// any reachable runtime configuration; defensive).
pub(super) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Alias kept for the wire-event log so existing call sites compile
/// unchanged. Identical to `now_unix_ms`.
pub(super) fn wallclock_ms() -> i64 {
    now_unix_ms()
}
