//! Noninteractive command construction, repetition, and streaming SCAN.
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use crate::{cli, input, session::Session};

type Error = Box<dyn std::error::Error>;

#[derive(Debug, clap::Args)]
#[group(id = "BatchOptions", multiple = true)]
pub struct Options {
    /// Read the last command argument from stdin, preserving every byte
    #[arg(short = 'x', conflicts_with_all = ["stdin_tag", "scan"])]
    pub stdin_last: bool,
    /// Replace an argument equal to TAG with all of stdin
    #[arg(short = 'X', value_name = "TAG", conflicts_with = "scan")]
    pub stdin_tag: Option<OsString>,
    /// Decode each command argument using Redis quoting and byte escapes
    #[arg(long)]
    pub quoted_input: bool,
    /// Execute a Lua file; separate KEYS from ARGV with a standalone comma
    #[arg(long, value_name = "FILE", conflicts_with = "scan")]
    pub eval: Option<PathBuf>,
    /// Execute each command N times; -1 repeats until interrupted
    #[arg(short = 'r', default_value_t = 1, allow_hyphen_values = true, value_parser = clap::value_parser!(i64).range(-1..))]
    pub repeat: i64,
    /// Pause between repetitions or SCAN requests, in seconds (e.g. 0.1)
    #[arg(short = 'i', default_value = "0", value_parser = seconds, value_name = "SECONDS")]
    pub interval: Duration,
    /// Stream keys using SCAN until cursor zero; Ctrl-C stops early
    #[arg(long, conflicts_with = "command")]
    pub scan: bool,
    /// Match a Redis glob when using --scan (default: *)
    #[arg(long, requires = "scan", conflicts_with = "quoted_pattern")]
    pub pattern: Option<OsString>,
    /// Decode a quoted, byte-escaped glob for --scan
    #[arg(long, requires = "scan", value_name = "PATTERN")]
    pub quoted_pattern: Option<String>,
    /// SCAN work hint per request (not a result limit)
    #[arg(long, requires = "scan", value_parser = clap::value_parser!(u64).range(1..))]
    pub count: Option<u64>,
}

pub fn seconds(text: &str) -> Result<Duration, String> {
    text.parse::<f64>()
        .ok()
        .and_then(|n| Duration::try_from_secs_f64(n).ok())
        .ok_or_else(|| {
            "expected a finite, nonnegative number of seconds".into()
        })
}

fn unquote(text: &str) -> Result<Vec<u8>, Error> {
    let mut parsed = input::parse(text);
    if let Some(error) = parsed.error {
        return Err(error.into());
    }
    if parsed.words.len() != 1 {
        return Err("quoted input must decode to exactly one argument".into());
    }
    Ok(parsed.words.remove(0).bytes)
}

impl Options {
    pub fn command(
        &self,
        words: &[OsString],
        stdin: &mut impl Read,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut args = words
            .iter()
            .map(|word| {
                if self.quoted_input {
                    unquote(word.to_str().ok_or(
                        "quoted input must be UTF-8; use byte escapes",
                    )?)
                } else {
                    Ok(word.as_encoded_bytes().to_vec())
                }
            })
            .collect::<Result<Vec<_>, Error>>()?;
        if self.stdin_last || self.stdin_tag.is_some() {
            if args.is_empty() {
                return Err("-x/-X requires a command".into());
            }
            let index = if let Some(tag) = &self.stdin_tag {
                Some(args.iter().position(|arg| arg == tag.as_encoded_bytes()).ok_or("-X tag was not found in the command arguments")?)
            } else {
                None
            };
            if index == Some(0) {
                return Err("-X cannot replace the command name".into());
            }
            let mut bytes = Vec::new();
            stdin.read_to_end(&mut bytes)?;
            if let Some(index) = index {
                args[index] = bytes;
            } else {
                args.push(bytes);
            }
        }
        if let Some(path) = &self.eval {
            let script = std::fs::read(path)?;
            let key_count = args
                .iter()
                .position(|arg| arg == b",")
                .unwrap_or(args.len());
            if key_count < args.len() {
                args.remove(key_count);
            }
            args.splice(
                0..0,
                [b"EVAL".to_vec(), script, key_count.to_string().into_bytes()],
            );
        }
        Ok(args)
    }
}

pub async fn execute(
    session: &mut Session,
    args: &[Vec<u8>],
    options: &cli::Options,
) -> Result<(), Error> {
    let mut remaining = options.batch.repeat;
    while remaining != 0 {
        crate::batch_command(session, args, options).await?;
        if remaining > 0 {
            remaining -= 1;
        }
        if remaining != 0 {
            tokio::time::sleep(options.batch.interval).await;
        }
    }
    Ok(())
}

pub async fn scan(
    session: &mut Session,
    options: &cli::Options,
) -> Result<(), Error> {
    let batch = &options.batch;
    let pattern = if let Some(quoted) = &batch.quoted_pattern {
        unquote(quoted)?
    } else {
        batch
            .pattern
            .as_deref()
            .map_or_else(|| b"*".to_vec(), |s| s.as_encoded_bytes().to_vec())
    };
    let mut cursor = 0_u64;
    loop {
        // Stream each batch without retaining an unbounded keyspace in memory.
        let reply = session
            .execute_with_push(
                &[
                    b"SCAN".to_vec(),
                    cursor.to_string().into_bytes(),
                    b"MATCH".to_vec(),
                    pattern.clone(),
                    b"COUNT".to_vec(),
                    batch.count.unwrap_or(10).to_string().into_bytes(),
                ],
                |_| {},
            )
            .await?;
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::from_redis_value(reply)?;
        for key in keys {
            options.output.print(&redis::Value::BulkString(key))?;
        }
        cursor = next;
        if cursor == 0 {
            return Ok(());
        }
        tokio::time::sleep(batch.interval).await;
    }
}

pub fn stdin_command(options: &cli::Options) -> Result<Vec<Vec<u8>>, Error> {
    options
        .batch
        .command(&options.command, &mut io::stdin().lock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn options(args: &[&str]) -> cli::Options {
        cli::Options::parse_from(args)
    }

    #[test]
    fn stdin_is_binary_and_tag_replacement_does_not_change_other_arguments() {
        let opts = options(&["rmycli", "-X", "tag", "SET", "key", "tag", "NX"]);
        let args = opts
            .batch
            .command(&opts.command, &mut b"a\0\xff\n".as_slice())
            .unwrap();
        assert_eq!(
            args,
            [
                b"SET".to_vec(),
                b"key".to_vec(),
                b"a\0\xff\n".to_vec(),
                b"NX".to_vec()
            ]
        );
        let opts = options(&["rmycli", "-x", "SET", "key"]);
        assert_eq!(
            opts.batch
                .command(&opts.command, &mut b"".as_slice())
                .unwrap()[2],
            b""
        );
        let opts = options(&["rmycli", "-X", "missing", "SET", "key"]);
        assert!(
            opts.batch
                .command(&opts.command, &mut b"".as_slice())
                .is_err()
        );
    }

    #[test]
    fn eval_counts_keys_and_preserves_arguments_and_negative_indexes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"return ARGV[1]").unwrap();
        let opts = options(&[
            "rmycli",
            "--eval",
            file.path().to_str().unwrap(),
            "key",
            ",",
            "-1",
            "--raw",
        ]);
        assert_eq!(
            opts.batch
                .command(&opts.command, &mut b"".as_slice())
                .unwrap(),
            [
                b"EVAL".to_vec(),
                b"return ARGV[1]".to_vec(),
                b"1".to_vec(),
                b"key".to_vec(),
                b"-1".to_vec(),
                b"--raw".to_vec()
            ]
        );
        let opts =
            options(&["rmycli", "--eval", file.path().to_str().unwrap()]);
        assert_eq!(
            opts.batch.command(&[], &mut b"".as_slice()).unwrap()[2],
            b"0"
        );
    }

    #[test]
    fn quoted_arguments_and_interval_validation() {
        assert_eq!(unquote(r#""a\x00\xff b""#).unwrap(), b"a\0\xff b");
        for text in ["a b", "\"broken", r"\xZZ"] {
            assert!(unquote(text).is_err());
        }
        for text in ["NaN", "inf", "-0.1", "1e100"] {
            assert!(seconds(text).is_err());
        }
        assert_eq!(seconds("0.125").unwrap(), Duration::from_millis(125));
    }
}
