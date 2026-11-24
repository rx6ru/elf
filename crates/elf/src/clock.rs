//! Wall-clock helpers: unix milliseconds and local ISO timestamps.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `2026-11-23T14:05:11+05:30` in the local time zone.
pub fn iso_local(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as libc::time_t;
    // SAFETY: `tm` is plain data, `localtime_r` only writes into it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format_iso(
        [tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec],
        tm.tm_gmtoff as i32,
    )
}

fn format_iso([y, mo, d, h, mi, s]: [i32; 6], offset_secs: i32) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let off = offset_secs.abs();
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{sign}{:02}:{:02}", off / 3600, off % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_format_has_the_offset() {
        assert_eq!(format_iso([2026, 11, 23, 14, 5, 11], 19800), "2026-11-23T14:05:11+05:30");
        assert_eq!(format_iso([2026, 1, 2, 3, 4, 5], -3600), "2026-01-02T03:04:05-01:00");
    }

    #[test]
    fn local_iso_has_the_right_shape() {
        let s = iso_local(1_763_900_711_000);
        assert_eq!(s.len(), 25, "{s}");
        assert_eq!(&s[10..11], "T");
    }
}
