//! The reference second-precision UTC timestamp format
//! (`YYYY-MM-DDTHH:MM:SSZ`) — one home (audit 2026-07-17, D3). Six sites
//! carried their own copy of the format literal; staleness checks parse
//! what `now_iso_utc` wrote, so the copies had to round-trip each other
//! and a drift in one would have broken the pair silently.

/// The one format literal. Callers use the helpers; the const exists so
/// there is exactly one string to change.
const ISO_UTC: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// Formats a timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn format_iso(timestamp: time::OffsetDateTime) -> Option<String> {
    timestamp.format(ISO_UTC).ok()
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn now_iso() -> String {
    format_iso(time::OffsetDateTime::now_utc())
        .expect("formatting a UTC timestamp with a const format never fails")
}

/// Parses `YYYY-MM-DDTHH:MM:SSZ` back (assumed UTC). `None` for anything
/// that is not exactly the reference format.
pub(crate) fn parse_iso(text: &str) -> Option<time::OffsetDateTime> {
    time::PrimitiveDateTime::parse(text, ISO_UTC)
        .ok()
        .map(|dt| dt.assume_utc())
}

/// The date part of an ISO-8601 timestamp (`2026-07-15T…` → `2026-07-15`).
/// One home (audit 2026-07-18, C2): two `date_only` copies with different
/// behavior (`&s[..10]` vs `split_once('T')`) drifted. Returns the input
/// unchanged when it carries no `T` separator.
pub(crate) fn date_only(timestamp: &str) -> &str {
    timestamp
        .split_once('T')
        .map_or(timestamp, |(date, _)| date)
}

/// Current Unix time in fractional seconds. One home (audit 2026-07-18,
/// D7): seven inline `SystemTime::now().duration_since(UNIX_EPOCH)`
/// re-spellings disagreed on the pre-1970 case (`.expect` panic vs
/// `unwrap_or(0.0)`). A clock before the epoch is treated as the epoch —
/// never a panic; downstream that reads as "expired/stale", which is
/// fail-safe.
pub(crate) fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Current Unix time in whole milliseconds ([`now_unix`] scaled).
pub(crate) fn now_unix_ms() -> i64 {
    (now_unix() * 1000.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trips() {
        let now = now_iso();
        let parsed = parse_iso(&now).expect("now_iso output parses");
        assert_eq!(format_iso(parsed).as_deref(), Some(now.as_str()));
        assert!(parse_iso("2026-07-17T12:00:00Z").is_some());
        assert!(parse_iso("garbage").is_none());
        assert!(parse_iso("2026-07-17 12:00:00").is_none());
    }
}
