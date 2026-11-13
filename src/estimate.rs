//! Cheap probes, never the original command. Counts are estimates, not bytes.
use redis::Connection;
use std::time::{Duration, Instant};

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

pub struct SizeHint {
    pub count: u64,
    pub label: String,
}

/// One pass over a pattern; count entries without storing or deduplicating keys.
pub fn keys_hint(
    connection: &mut impl redis::ConnectionLike,
    pattern: &[u8],
    count: u64,
    tick: Duration,
    cancelled: impl Fn() -> bool,
    mut publish: impl FnMut(SizeHint) -> bool,
) -> redis::RedisResult<()> {
    if cancelled() {
        return Ok(());
    }
    let dbsize: u64 = redis::cmd("DBSIZE").query(connection)?;
    if cancelled() {
        return Ok(());
    }
    if pattern == b"*" {
        publish(SizeHint {
            count: dbsize,
            label: format!("[~{dbsize} keys]"),
        });
        return Ok(());
    }
    if !publish(SizeHint {
        count: 0,
        label: format!("[~0/{dbsize} keys; scanning]"),
    }) {
        return Ok(());
    }
    let mut cursor = 0_u64;
    let mut total = 0_u64;
    while !cancelled() {
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(count)
            .query(connection)?;
        total = total.saturating_add(keys.len() as u64);
        drop(keys);
        cursor = next;
        let progress = if cursor == 0 { "" } else { "; scanning" };
        if cancelled()
            || !publish(SizeHint {
                count: total,
                label: format!("[~{total}/{dbsize} keys{progress}]"),
            })
            || cursor == 0
        {
            return Ok(());
        }
        // Even a long configured interval must yield promptly to edits.
        let started = Instant::now();
        while !cancelled() {
            let remaining = tick.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    Ok(())
}

/// A short `[N unit]` label, `≤` marking upper bounds. Wrong-type keys and
/// failed probes give `None`; data can change before the command runs.
pub fn size_hint(
    connection: &mut Connection,
    command: &str,
    args: &[Vec<u8>],
    keys: &[Vec<u8>],
) -> Option<SizeHint> {
    let (probe, unit, combine) = probe(command)?;
    let mut pipe = redis::pipe();
    for key in keys {
        pipe.cmd(probe).arg(key);
    }
    let counts: Vec<i64> = pipe.query(connection).ok()?;
    let (count, upper) = total(combine, &counts, args)?;
    Some(SizeHint {
        count,
        label: format!("[{}{count} {unit}]", if upper { "≤" } else { "" }),
    })
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
    use redis::{ConnectionLike, RedisResult, Value};
    use std::cell::Cell;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeConnection {
        replies: VecDeque<RedisResult<Value>>,
        commands: Vec<Vec<u8>>,
        times: Vec<Instant>,
    }

    impl ConnectionLike for FakeConnection {
        fn req_packed_command(&mut self, cmd: &[u8]) -> RedisResult<Value> {
            self.commands.push(cmd.to_vec());
            self.times.push(Instant::now());
            self.replies.pop_front().expect("unexpected request")
        }
        fn req_packed_commands(
            &mut self,
            _: &[u8],
            _: usize,
            _: usize,
        ) -> RedisResult<Vec<Value>> {
            unreachable!("hints do not pipeline")
        }
        fn get_db(&self) -> i64 {
            0
        }
        fn check_connection(&mut self) -> bool {
            true
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    fn batches() -> FakeConnection {
        let batch = |cursor, keys: &[&[u8]]| {
            Ok(Value::Array(vec![
                Value::Int(cursor),
                Value::Array(
                    keys.iter()
                        .map(|key| Value::BulkString(key.to_vec()))
                        .collect(),
                ),
            ]))
        };
        FakeConnection {
            replies: VecDeque::from([
                Ok(Value::Int(500)),
                batch(7, &[b"s:a", b"s:a"]),
                batch(9, &[]),
                batch(0, &[b"s:b"]),
            ]),
            ..FakeConnection::default()
        }
    }

    #[test]
    fn keys_counts_duplicates_and_empty_batches_until_cursor_zero() {
        let mut source = batches();
        let mut hints = Vec::new();
        let tick = Duration::from_millis(2);
        let pattern = b"s[ab]\\*\xff";
        keys_hint(
            &mut source,
            pattern,
            123,
            tick,
            || false,
            |hint| {
                hints.push((hint.count, hint.label));
                true
            },
        )
        .unwrap();
        assert_eq!(
            hints,
            [
                (0, "[~0/500 keys; scanning]".into()),
                (2, "[~2/500 keys; scanning]".into()),
                (2, "[~2/500 keys; scanning]".into()),
                (3, "[~3/500 keys]".into()),
            ]
        );
        assert_eq!(
            source.commands[0],
            redis::cmd("DBSIZE").get_packed_command()
        );
        for (index, cursor) in [0, 7, 9].into_iter().enumerate() {
            assert_eq!(
                source.commands[index + 1],
                redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(pattern)
                    .arg("COUNT")
                    .arg(123)
                    .get_packed_command()
            );
        }
        assert!(source.times[2].duration_since(source.times[1]) >= tick);
        assert!(source.times[3].duration_since(source.times[2]) >= tick);
        assert!(source.replies.is_empty());
    }

    #[test]
    fn keys_star_only_uses_dbsize_and_errors_stay_unknown() {
        let mut source = batches();
        let mut hints = Vec::new();
        keys_hint(
            &mut source,
            b"*",
            10,
            Duration::ZERO,
            || false,
            |hint| {
                hints.push(hint.label);
                true
            },
        )
        .unwrap();
        assert_eq!(hints, ["[~500 keys]"]);
        assert_eq!(source.commands.len(), 1);
        source.replies =
            VecDeque::from([Err((redis::ErrorKind::Client, "denied").into())]);
        assert!(
            keys_hint(
                &mut source,
                b"s*",
                10,
                Duration::ZERO,
                || false,
                |_| {
                    panic!("failed DBSIZE must not publish zero");
                }
            )
            .is_err()
        );
    }

    #[test]
    fn keys_cancellation_before_requests_after_replies_and_between_ticks() {
        let mut source = batches();
        keys_hint(
            &mut source,
            b"s*",
            10,
            Duration::ZERO,
            || true,
            |_| panic!("cancelled"),
        )
        .unwrap();
        assert_eq!(source.commands, Vec::<Vec<u8>>::new());
        let checks = Cell::new(0);
        keys_hint(
            &mut source,
            b"*",
            10,
            Duration::ZERO,
            || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
            |_| panic!("cancelled after DBSIZE"),
        )
        .unwrap();
        assert_eq!(source.commands.len(), 1);

        let mut source = batches();
        let cancelled = Cell::new(false);
        keys_hint(
            &mut source,
            b"s*",
            10,
            Duration::from_secs(60),
            || cancelled.get(),
            |hint| {
                if hint.count > 0 {
                    cancelled.set(true);
                }
                true
            },
        )
        .unwrap();
        assert_eq!(source.commands.len(), 2, "no next tick after cancellation");
    }

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
