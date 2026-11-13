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

#[derive(Clone, Copy)]
enum Combine {
    /// Union-like: every key contributes, so the total is an upper bound.
    Sum,
    /// Intersection: no larger than the smallest key.
    Min,
    /// Difference: no larger than the first key.
    First,
    /// `key start stop` range over a single key.
    Range,
}

/// How to size commands that return collection contents. Which arguments are
/// keys comes from server key specs; this table only says how to count them.
fn probe(command: &str) -> Option<(&'static str, &'static str, Combine)> {
    use Combine::{First, Min, Range, Sum};
    Some(match command {
        "SMEMBERS" | "SUNION" => ("SCARD", "members", Sum),
        "SINTER" => ("SCARD", "members", Min),
        "SDIFF" => ("SCARD", "members", First),
        "ZUNION" => ("ZCARD", "members", Sum),
        "ZINTER" => ("ZCARD", "members", Min),
        "ZDIFF" => ("ZCARD", "members", First),
        "HGETALL" | "HKEYS" | "HVALS" => ("HLEN", "fields", Sum),
        "LRANGE" => ("LLEN", "elements", Range),
        "GET" => ("STRLEN", "bytes", Sum),
        _ => return None,
    })
}

pub fn sized(command: &str) -> bool {
    probe(command).is_some()
}

/// Returns the count and whether it is only an upper bound.
fn total(
    combine: Combine,
    counts: &[i64],
    args: &[Vec<u8>],
) -> Option<(u64, bool)> {
    let upper = counts.len() > 1;
    let count = match combine {
        Combine::Sum => counts.iter().map(|&c| c.max(0)).sum(),
        Combine::Min => counts.iter().copied().min()?,
        Combine::First => *counts.first()?,
        Combine::Range => {
            let [_, _, start, stop] = args else {
                return None;
            };
            let start = std::str::from_utf8(start).ok()?.parse().ok()?;
            let stop = std::str::from_utf8(stop).ok()?.parse().ok()?;
            return Some((range_count(*counts.first()?, start, stop), false));
        }
    };
    Some((u64::try_from(count).ok()?, upper))
}

/// A short `[N unit]` label, `≤` marking upper bounds. Wrong-type keys and
/// failed probes give `None`; data can change before the command runs.
pub fn size_hint(
    connection: &mut Connection,
    command: &str,
    args: &[Vec<u8>],
    keys: &[Vec<u8>],
) -> Option<String> {
    let (probe, unit, combine) = probe(command)?;
    let mut pipe = redis::pipe();
    for key in keys {
        pipe.cmd(probe).arg(key);
    }
    let counts: Vec<i64> = pipe.query(connection).ok()?;
    let (count, upper) = total(combine, &counts, args)?;
    Some(format!("[{}{count} {unit}]", if upper { "≤" } else { "" }))
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
    fn size_totals_mark_multi_key_bounds() {
        let args = |line: &str| -> Vec<Vec<u8>> {
            line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
        };
        let none = args("X");
        assert_eq!(total(Combine::Sum, &[5], &none), Some((5, false)));
        assert_eq!(total(Combine::Sum, &[5, 3, 0], &none), Some((8, true)));
        assert_eq!(total(Combine::Min, &[5, 3], &none), Some((3, true)));
        assert_eq!(total(Combine::First, &[5, 9], &none), Some((5, true)));
        assert_eq!(total(Combine::Sum, &[], &none), Some((0, false)));
        let range = args("LRANGE k 1 -1");
        assert_eq!(total(Combine::Range, &[10], &range), Some((9, false)));
        assert_eq!(total(Combine::Range, &[10], &args("LRANGE k 1")), None);
        assert_eq!(total(Combine::Range, &[10], &args("LRANGE k a -1")), None);
        assert!(sized("SUNION") && !sized("SUNIONSTORE"));
    }

    #[test]
    fn unknown_is_not_zero_and_threshold_is_strict() {
        assert!(!needs_confirmation(None, 10, false));
        assert!(!needs_confirmation(Some(10), 10, false));
        assert!(needs_confirmation(Some(11), 10, false));
        assert!(!needs_confirmation(Some(11), 10, true));
    }
}
