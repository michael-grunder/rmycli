use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use redis::Connection;

use crate::input::glob_prefix;

#[derive(Clone, Debug)]
pub struct Limits {
    pub batches: usize,
    pub keys: usize,
    pub bytes: usize,
    pub time: Duration,
    pub threshold: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            batches: 32,
            keys: 10_000,
            bytes: 4 * 1024 * 1024,
            time: Duration::from_millis(750),
            threshold: 100,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub cursor: u64,
    pub keys: BTreeSet<Vec<u8>>,
    pub bytes: usize,
    pub complete: bool,
    pub hard_limit: bool,
    pub cancelled: bool,
    pub notice: String,
}

impl Discovery {
    pub fn count_description(&self) -> String {
        if self.complete {
            format!("{} matches", self.keys.len())
        } else {
            format!("at least {} matches (partial scan)", self.keys.len())
        }
    }
}

pub trait Source {
    fn batch(
        &mut self,
        cursor: u64,
        pattern: &[u8],
        kind: Option<&str>,
    ) -> Result<(u64, Vec<Vec<u8>>), String>;
}

impl Source for Connection {
    fn batch(
        &mut self,
        cursor: u64,
        pattern: &[u8],
        kind: Option<&str>,
    ) -> Result<(u64, Vec<Vec<u8>>), String> {
        let mut command = redis::cmd("SCAN");
        command
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(128);
        if let Some(kind) = kind {
            command.arg("TYPE").arg(kind);
        }
        command.query(self).map_err(|error| error.to_string())
    }
}

pub fn discover(
    source: &mut impl Source,
    prefix: &[u8],
    kind: Option<&str>,
    mut state: Discovery,
    limits: &Limits,
    cancelled: impl Fn() -> bool,
) -> Discovery {
    let pattern = glob_prefix(prefix);
    let started = Instant::now();
    let mut kind = kind;
    for _ in 0..limits.batches {
        if cancelled() {
            state.cancelled = true;
            return state;
        }
        if started.elapsed() >= limits.time {
            break;
        }
        let batch = match source.batch(state.cursor, &pattern, kind) {
            Ok(batch) => batch,
            Err(error)
                if kind.is_some()
                    && error.to_ascii_lowercase().contains("syntax") =>
            {
                kind = None;
                state.notice =
                    "Server lacks SCAN TYPE; suggestions are unfiltered".into();
                continue;
            }
            Err(error) => {
                state.notice = error;
                return state;
            }
        };
        if cancelled() {
            state.cancelled = true;
            return state;
        }
        state.cursor = batch.0;
        for key in batch.1 {
            if !key.starts_with(prefix) || state.keys.contains(&key) {
                continue;
            }
            if state.keys.len() >= limits.keys
                || key.len() > limits.bytes.saturating_sub(state.bytes)
            {
                state.hard_limit = true;
                state.notice = "Completion memory limit reached".into();
                return state;
            }
            state.bytes += key.len();
            state.keys.insert(key);
        }
        state.complete = state.cursor == 0;
        if state.complete || state.keys.len() > limits.threshold {
            return state;
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake {
        batches: VecDeque<(u64, Vec<Vec<u8>>)>,
        calls: usize,
    }
    impl Source for Fake {
        fn batch(
            &mut self,
            _: u64,
            pattern: &[u8],
            kind: Option<&str>,
        ) -> Result<(u64, Vec<Vec<u8>>), String> {
            self.calls += 1;
            assert_eq!(pattern, b"list:*");
            assert_eq!(kind, Some("list"));
            Ok(self.batches.pop_front().expect("unexpected SCAN"))
        }
    }
    fn source() -> Fake {
        Fake {
            calls: 0,
            batches: VecDeque::from([
                (9, vec![]),
                (8, vec![b"list:a".to_vec(), b"list:a".to_vec()]),
                (0, vec![b"list:b".to_vec()]),
            ]),
        }
    }

    #[test]
    fn empty_batches_duplicates_and_completion() {
        let mut source = source();
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits::default(),
            || false,
        );
        assert_eq!(source.calls, 3);
        assert!(state.complete);
        assert_eq!(state.keys.len(), 2);
    }

    #[test]
    fn partial_single_match_is_not_unique_and_can_resume() {
        let mut source = source();
        let limits = Limits {
            batches: 2,
            ..Limits::default()
        };
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            Discovery::default(),
            &limits,
            || false,
        );
        assert!(!state.complete);
        assert!(state.count_description().starts_with("at least 1"));
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            state,
            &limits,
            || false,
        );
        assert!(state.complete);
        assert_eq!(state.keys.len(), 2);
    }

    #[test]
    fn cancellation_threshold_time_and_memory_bounds() {
        let mut source = source();
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits::default(),
            || true,
        );
        assert!(state.cancelled);
        assert_eq!(source.calls, 0);
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits {
                threshold: 0,
                ..Limits::default()
            },
            || false,
        );
        assert!(!state.complete);
        assert_eq!(state.keys.len(), 1);
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            state,
            &Limits {
                keys: 1,
                ..Limits::default()
            },
            || false,
        );
        assert!(state.hard_limit);
        assert!(!state.complete);
        let state = discover(
            &mut source,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits {
                time: Duration::ZERO,
                ..Limits::default()
            },
            || false,
        );
        assert!(!state.complete);
    }

    #[test]
    fn cancellation_after_network_work_and_byte_limit() {
        let checks = std::cell::Cell::new(0);
        let mut network = source();
        let cancelled = discover(
            &mut network,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits::default(),
            || {
                let previous = checks.get();
                checks.set(previous + 1);
                previous > 0
            },
        );
        assert!(cancelled.cancelled);
        assert_eq!(network.calls, 1);
        let partial = discover(
            &mut network,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits {
                bytes: 1,
                ..Limits::default()
            },
            || false,
        );
        assert!(partial.hard_limit);
        assert!(!partial.complete);
        assert!(partial.keys.is_empty());
    }

    #[test]
    fn older_scan_type_syntax_falls_back_without_filter() {
        struct OldServer {
            calls: Vec<Option<String>>,
        }
        impl Source for OldServer {
            fn batch(
                &mut self,
                _: u64,
                _: &[u8],
                kind: Option<&str>,
            ) -> Result<(u64, Vec<Vec<u8>>), String> {
                self.calls.push(kind.map(str::to_owned));
                if kind.is_some() {
                    Err("ERR syntax error".into())
                } else {
                    Ok((0, vec![b"list:a".to_vec()]))
                }
            }
        }
        let mut server = OldServer { calls: Vec::new() };
        let state = discover(
            &mut server,
            b"list:",
            Some("list"),
            Discovery::default(),
            &Limits::default(),
            || false,
        );
        assert_eq!(server.calls, vec![Some("list".into()), None]);
        assert!(state.complete);
        assert!(state.notice.contains("unfiltered"));
    }
}
