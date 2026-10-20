//! Cheap probes, never the original command. Counts are estimates, not bytes.
use redis::Connection;

pub fn range_count(length: i64, start: i64, stop: i64) -> u64 {
    if length <= 0 {
        return 0;
    }
    let start = if start < 0 {
        length.saturating_add(start)
    } else {
        start
    }
    .max(0);
    let stop = if stop < 0 {
        length.saturating_add(stop)
    } else {
        stop
    }
    .min(length - 1);
    u64::try_from(stop.saturating_sub(start).saturating_add(1).max(0))
        .unwrap_or(0)
}

pub fn estimate(connection: &mut Connection, args: &[Vec<u8>]) -> Option<u64> {
    if args.len() != 4 || !args[0].eq_ignore_ascii_case(b"LRANGE") {
        return None;
    }
    let start = std::str::from_utf8(&args[2]).ok()?.parse().ok()?;
    let stop = std::str::from_utf8(&args[3]).ok()?.parse().ok()?;
    let length = redis::cmd("LLEN").arg(&args[1]).query(connection).ok()?;
    Some(range_count(length, start, stop))
}

pub const fn needs_confirmation(
    estimate: Option<u64>,
    threshold: u64,
    yes: bool,
) -> bool {
    if yes {
        return false;
    }
    match estimate {
        Some(count) => count > threshold,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inclusive_negative_and_out_of_range_bounds() {
        for (len, start, stop, count) in [
            (10, 0, -1, 10),
            (10, 1, 3, 3),
            (10, -3, -1, 3),
            (10, -100, 100, 10),
            (10, 20, 30, 0),
            (10, 0, -20, 0),
            (0, 0, -1, 0),
            (10, 4, 2, 0),
            (10, i64::MIN, i64::MAX, 10),
        ] {
            assert_eq!(range_count(len, start, stop), count);
        }
    }
    #[test]
    fn unknown_is_not_zero_and_threshold_is_strict() {
        assert!(!needs_confirmation(None, 10, false));
        assert!(!needs_confirmation(Some(10), 10, false));
        assert!(needs_confirmation(Some(11), 10, false));
        assert!(!needs_confirmation(Some(11), 10, true));
    }
}
