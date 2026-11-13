use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum entries in memory and across all servers in the history file.
pub const LIMIT: usize = 500;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    server: String,
    line: String,
}

pub struct Store {
    path: PathBuf,
    server: String,
    pending: VecDeque<String>,
}

impl Store {
    pub fn open(
        path: PathBuf,
        addr: &redis::ConnectionAddr,
        per_server: bool,
    ) -> io::Result<(Self, VecDeque<String>)> {
        let server = crate::auth::endpoint(addr)?;
        let history = read(&path)?
            .into_iter()
            .filter(|entry| !per_server || entry.server == server)
            .map(|entry| entry.line)
            .collect();
        Ok((
            Self {
                path,
                server,
                pending: VecDeque::new(),
            },
            history,
        ))
    }

    pub fn remember(&mut self, line: &str) {
        if !persistable(line) {
            return;
        }
        self.pending.push_back(line.to_owned());
        if self.pending.len() > LIMIT {
            self.pending.pop_front();
        }
    }

    pub fn save(&self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        crate::locked_file::replace(&self.path, "lock", |path| {
            // Merge only this session's new commands with the latest disk
            // contents.
            let mut entries = read(path)?;
            entries.extend(self.pending.iter().map(|line| Entry {
                server: self.server.clone(),
                line: line.clone(),
            }));
            entries.drain(..entries.len().saturating_sub(LIMIT));
            let mut bytes = Vec::new();
            for entry in entries {
                serde_json::to_writer(&mut bytes, &entry)?;
                bytes.push(b'\n');
            }
            Ok(Some(bytes))
        })
    }
}

fn read(path: &Path) -> io::Result<VecDeque<Entry>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(VecDeque::new());
        }
        Err(error) => return Err(error),
    };
    let mut entries = VecDeque::new();
    for line in io::BufReader::new(file).lines() {
        let entry: Entry = serde_json::from_str(&line?).map_err(|_| {
            // Parser errors can include sensitive command arguments.
            io::Error::new(io::ErrorKind::InvalidData, "invalid history file")
        })?;
        if !persistable(&entry.line) {
            continue;
        }
        entries.push_back(entry);
        if entries.len() > LIMIT {
            entries.pop_front();
        }
    }
    Ok(entries)
}

pub fn persistable(line: &str) -> bool {
    let parsed = crate::input::parse(line);
    let Some(command) = parsed.words.first() else {
        return false;
    };
    if parsed.error.is_some()
        || command.bytes.eq_ignore_ascii_case(b"AUTH")
        || command.bytes.eq_ignore_ascii_case(b"HELLO")
    {
        return false;
    }
    if parsed.words.iter().any(|word| {
        [b"redis://".as_slice(), b"rediss://".as_slice()]
            .iter()
            .any(|scheme| {
                word.bytes
                    .windows(scheme.len())
                    .position(|part| part.eq_ignore_ascii_case(scheme))
                    .is_some_and(|start| {
                        word.bytes[start + scheme.len()..].contains(&b'@')
                    })
            })
    }) {
        return false;
    }
    let subcommand = parsed.words.get(1).map(|word| word.bytes.as_slice());
    !((command.bytes.eq_ignore_ascii_case(b"ACL")
        && subcommand
            .is_some_and(|word| word.eq_ignore_ascii_case(b"SETUSER")))
        || (command.bytes.eq_ignore_ascii_case(b"CONFIG")
            && subcommand
                .is_some_and(|word| word.eq_ignore_ascii_case(b"SET")))
        || (command.bytes.eq_ignore_ascii_case(b"MIGRATE")
            && parsed.words.iter().skip(6).any(|word| {
                word.bytes.eq_ignore_ascii_case(b"AUTH")
                    || word.bytes.eq_ignore_ascii_case(b"AUTH2")
            })))
}

/// Search recent commands using fuzzy terms or apostrophe-prefixed substrings.
// shortcut: fuzzy/exact terms only; extend when more fzf operators are needed.
pub fn search(history: &VecDeque<String>, query: &str) -> Vec<String> {
    let terms = terms(query);
    let mut seen = HashSet::new();
    history
        .iter()
        .rev()
        .filter(|line| seen.insert(line.as_str()))
        .filter(|line| {
            terms.iter().all(|term| {
                let (exact, term) = term
                    .strip_prefix('\'')
                    .map_or((false, term.as_str()), |term| (true, term));
                let folded;
                let text = if term.chars().any(char::is_uppercase) {
                    line.as_str()
                } else {
                    folded = line.to_lowercase();
                    &folded
                };
                if exact {
                    text.contains(term)
                } else {
                    let mut chars = text.chars();
                    term.chars().all(|c| chars.any(|next| next == c))
                }
            })
        })
        .cloned()
        .collect()
}

fn terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut term = String::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&' ') {
            chars.next();
            term.push(' ');
        } else if c == ' ' {
            if !term.is_empty() {
                terms.push(std::mem::take(&mut term));
            }
        } else {
            term.push(c);
        }
    }
    if !term.is_empty() {
        terms.push(term);
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &Path, port: u16) -> (Store, VecDeque<String>) {
        Store::open(
            path.to_owned(),
            &redis::ConnectionAddr::Tcp("localhost".into(), port),
            true,
        )
        .unwrap()
    }

    #[test]
    fn round_trip_preserves_commands_and_separates_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/rmycli_history");
        let (mut first, history) = open(&path, 6379);
        assert!(history.is_empty());
        first.save().unwrap();
        assert!(!path.exists());
        let lines = [r#"SET "a b" "\x00\xff""#, "SET café 'a\nb'", "PING"];
        for line in lines {
            first.remember(line);
        }
        first.save().unwrap();
        assert_eq!(open(&path, 6379).1, lines);
        assert!(open(&path, 6380).1.is_empty());
        for addr in [
            redis::ConnectionAddr::TcpTls {
                host: "localhost".into(),
                port: 6379,
                insecure: false,
                tls_params: None,
            },
            redis::ConnectionAddr::Unix("/tmp/redis.sock".into()),
        ] {
            assert!(
                Store::open(path.clone(), &addr, true).unwrap().1.is_empty()
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    }

    #[test]
    fn flat_history_keeps_order_and_server_tags_when_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rmycli_history");
        let (mut first, _) = open(&path, 6379);
        first.remember("GET first");
        first.save().unwrap();
        let (mut second, _) = open(&path, 6380);
        second.remember("GET second");
        second.save().unwrap();

        let before = std::fs::read(&path).unwrap();
        let (mut flat, history) = Store::open(
            path.clone(),
            &redis::ConnectionAddr::Tcp("localhost".into(), 6380),
            false,
        )
        .unwrap();
        assert_eq!(history, ["GET first", "GET second"]);
        flat.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        flat.remember("GET third");
        flat.save().unwrap();
        assert_eq!(open(&path, 6379).1, ["GET first"]);
        assert_eq!(open(&path, 6380).1, ["GET second", "GET third"]);
    }

    #[test]
    fn overlapping_sessions_merge_only_new_entries_and_bound_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rmycli_history");
        let (mut seed, _) = open(&path, 6379);
        seed.remember("PING");
        seed.save().unwrap();
        let (mut first, _) = open(&path, 6379);
        let (mut second, _) = open(&path, 6379);
        let (mut other, _) = open(&path, 6380);
        first.remember("GET first");
        second.remember("GET second");
        other.remember("GET other");
        first.save().unwrap();
        other.save().unwrap();
        second.save().unwrap();
        assert_eq!(open(&path, 6379).1, ["PING", "GET first", "GET second"]);
        assert_eq!(open(&path, 6380).1, ["GET other"]);
        let (mut full, _) = open(&path, 6380);
        for i in 0..LIMIT + 10 {
            full.remember(&format!("GET {i}"));
        }
        assert_eq!(full.pending.len(), LIMIT);
        full.save().unwrap();
        let history = open(&path, 6380).1;
        assert_eq!(history.len(), LIMIT);
        assert_eq!(history.front().unwrap(), "GET 10");
        assert_eq!(history.back().unwrap(), &format!("GET {}", LIMIT + 9));
        assert!(open(&path, 6379).1.is_empty());
        assert_eq!(read(&path).unwrap().len(), LIMIT);
    }

    #[test]
    fn credential_commands_are_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rmycli_history");
        let (mut store, _) = open(&path, 6379);
        for line in [
            "AUTH secret",
            "SET endpoint redis://alice:secret@host:6379",
            "SET endpoint REDISS://:secret@host",
            r"a\x75th user secret",
            "'AUTH' secret",
            "HELLO 3 AUTH user secret",
            "hello 2",
            "ACL SETUSER user >secret",
            "config set requirepass secret",
            "CONFIG SET masterauth secret",
            "MIGRATE host 6379 key 0 100 AUTH secret",
            "MIGRATE host 6379 key 0 100 AUTH2 user secret",
            "",
            "  ",
            "SET 'unfinished",
        ] {
            store.remember(line);
        }
        assert!(store.pending.is_empty());
        store.remember("GET auth");
        store.remember("CONFIG GET maxmemory");
        store.save().unwrap();
        assert_eq!(open(&path, 6379).1, ["GET auth", "CONFIG GET maxmemory"]);
        assert!(!std::fs::read_to_string(path).unwrap().contains("secret"));
    }

    #[test]
    fn old_sensitive_history_is_not_recalled_or_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "{\"server\":\"redis://localhost:6379\",\"line\":\"AUTH old-secret\"}\n{\"server\":\"redis://localhost:6379\",\"line\":\"SET url redis://:old-secret@host\"}\n").unwrap();
        let (mut store, history) = open(&path, 6379);
        assert!(history.is_empty());
        store.remember("PING");
        store.save().unwrap();
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("old-secret")
        );
    }

    #[test]
    fn corrupt_or_locked_history_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rmycli_history");
        let (mut store, _) = open(&path, 6379);
        store.remember("PING");
        let source = "{\"private-command\":\"secret\"}\n";
        std::fs::write(&path, source).unwrap();
        assert!(
            Store::open(
                path.clone(),
                &redis::ConnectionAddr::Tcp("localhost".into(), 6379),
                true,
            )
            .is_err()
        );
        let error = store.save().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("secret"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("lock").exists());
        std::fs::write(&path, "").unwrap();
        std::fs::write(path.with_extension("lock"), "").unwrap();
        assert_eq!(
            store.save().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(path.with_extension("lock").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[cfg(unix)]
    #[test]
    fn saves_are_private_and_follow_existing_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let path = dir.path().join("rmycli_history");
        std::fs::write(&target, "").unwrap();
        std::fs::set_permissions(
            &target,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        symlink(&target, &path).unwrap();
        let (mut store, _) = open(&path, 6379);
        store.remember("PING");
        store.save().unwrap();
        assert!(path.is_symlink());
        assert_eq!(open(&target, 6379).1, ["PING"]);
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn fuzzy_terms_and_exact_substrings_follow_fzf_conventions() {
        let history = ["GET user:42", "SET user:42 hello", "GET upper:42"]
            .map(String::from)
            .into();
        assert_eq!(search(&history, "gusr"), ["GET user:42"]);
        assert_eq!(search(&history, "'gusr"), Vec::<String>::new());
        assert_eq!(search(&history, "'user g"), ["GET user:42"]);
        assert_eq!(search(&history, "'GET\\ user"), ["GET user:42"]);
        assert_eq!(search(&history, "42 set"), ["SET user:42 hello"]);
        assert_eq!(search(&history, "get Hello"), Vec::<String>::new());
    }

    #[test]
    fn smart_case_unicode_and_literal_command_bytes() {
        let history = ["GET café", "get CAFÉ", r"GET \x00", "GET a\x1bb"]
            .map(String::from)
            .into();
        assert_eq!(search(&history, "cfé"), ["get CAFÉ", "GET café"]);
        assert_eq!(search(&history, "'CAFÉ"), ["get CAFÉ"]);
        assert_eq!(search(&history, "Get"), Vec::<String>::new());
        assert_eq!(search(&history, r"'\x00"), [r"GET \x00"]);
        assert_eq!(search(&history, "ab"), ["GET a\x1bb"]);
    }

    #[test]
    fn empty_queries_and_duplicates_keep_the_most_recent_entry() {
        let history = ["PING", "GET a", "PING"].map(String::from).into();
        for query in ["", "   ", "'"] {
            assert_eq!(search(&history, query), ["PING", "GET a"]);
        }
        assert_eq!(search(&VecDeque::new(), ""), Vec::<String>::new());
        assert_eq!(search(&history, "missing"), Vec::<String>::new());
    }
}
