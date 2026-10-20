#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

mod estimate;
mod grammar;
mod hints;
mod input;
mod metadata;
mod scan;
mod session;
mod terminal;
mod worker;

#[cfg(test)]
mod integration_tests;

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use redis::{ConnectionAddr, IntoConnectionInfo, Value};
use session::{Config, Session};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "A Redis shell with live command help and type-aware completion",
    disable_help_flag = true
)]
struct Options {
    /// Print help
    #[arg(long, action = clap::ArgAction::Help)]
    help: Option<bool>,
    /// Redis host
    #[arg(short = 'h', long, default_value = "127.0.0.1")]
    host: String,
    /// Redis TCP port
    #[arg(short = 'p', long, default_value_t = 6379)]
    port: u16,
    /// Connection URL (redis://, rediss://, or redis+unix://)
    #[arg(short = 'u', long, env = "REDIS_URL", hide_env_values = true)]
    uri: Option<String>,
    /// Unix domain socket
    #[arg(short = 's', long, conflicts_with_all = ["uri", "tls"])]
    socket: Option<PathBuf>,
    /// Database number
    #[arg(short = 'n', long)]
    db: Option<i64>,
    /// ACL username
    #[arg(long)]
    user: Option<String>,
    /// Prefer `REDISCLI_AUTH` to putting a password in shell history
    #[arg(short = 'a', long, env = "REDISCLI_AUTH", hide_env_values = true)]
    password: Option<String>,
    /// Use TLS with certificate verification
    #[arg(long, conflicts_with = "uri")]
    tls: bool,
    /// Connection and auxiliary-request timeout in milliseconds
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_ms: u64,
    /// Ask before showing more than this many completion candidates
    #[arg(long, default_value_t = 100)]
    completion_threshold: usize,
    /// Maximum retained completion keys
    #[arg(long, default_value_t = 10000)]
    scan_max_keys: usize,
    /// Maximum retained completion key bytes
    #[arg(long, default_value_t = 4_194_304)]
    scan_max_bytes: usize,
    /// Maximum SCAN requests per discovery step
    #[arg(long, default_value_t = 32)]
    scan_max_batches: usize,
    /// Time budget per discovery step (plus at most one in-flight request)
    #[arg(long, default_value_t = 750)]
    scan_ms: u64,
    /// Confirm LRANGE replies estimated above this element count
    #[arg(long, default_value_t = 10000)]
    reply_threshold: u64,
    /// Execute without reply-size confirmation (also for noninteractive use)
    #[arg(short = 'y', long)]
    yes: bool,
    /// Write bulk reply bytes verbatim in noninteractive mode
    #[arg(long)]
    raw: bool,
    /// Optional command and arguments; otherwise start a shell or read stdin
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

impl Options {
    fn config(&self) -> Result<Config, Box<dyn std::error::Error>> {
        let mut info = if let Some(uri) = &self.uri {
            uri.as_str().into_connection_info()?
        } else {
            (self.host.as_str(), self.port).into_connection_info()?
        };
        if let Some(socket) = &self.socket {
            info.addr = ConnectionAddr::Unix(socket.clone());
        }
        if self.tls {
            info.addr = ConnectionAddr::TcpTls {
                host: self.host.clone(),
                port: self.port,
                insecure: false,
                tls_params: None,
            };
        }
        if let Some(db) = self.db {
            if db < 0 {
                return Err("database must be nonnegative".into());
            }
            info.redis.db = db;
        }
        if self.user.is_some() {
            info.redis.username.clone_from(&self.user);
        }
        if self.password.is_some() {
            info.redis.password.clone_from(&self.password);
        }
        Ok(Config {
            info,
            timeout: Duration::from_millis(self.timeout_ms),
        })
    }
}

fn main() {
    if let Err(error) = run(&Options::parse()) {
        eprintln!("rmycli: {error}");
        std::process::exit(1);
    }
}

fn run(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::open(options.config()?)?;
    if !options.command.is_empty() {
        let args = options
            .command
            .iter()
            .map(|arg| arg.as_encoded_bytes().to_vec())
            .collect::<Vec<_>>();
        batch_command(&mut session, &args, options)?;
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        terminal::run(
            session,
            terminal::Options {
                limits: scan::Limits {
                    threshold: options.completion_threshold,
                    keys: options.scan_max_keys,
                    bytes: options.scan_max_bytes,
                    batches: options.scan_max_batches,
                    time: Duration::from_millis(options.scan_ms),
                },
                reply_threshold: options.reply_threshold,
                yes: options.yes,
            },
        )?;
    } else {
        for line in io::stdin().lock().lines() {
            let line = line?;
            let parsed = input::parse(&line);
            if let Some(error) = parsed.error {
                return Err(error.into());
            }
            let args = parsed
                .words
                .into_iter()
                .map(|word| word.bytes)
                .collect::<Vec<_>>();
            if !args.is_empty() {
                batch_command(&mut session, &args, options)?;
            }
        }
    }
    Ok(())
}

fn batch_command(
    session: &mut Session,
    args: &[Vec<u8>],
    options: &Options,
) -> Result<(), Box<dyn std::error::Error>> {
    if session.assistance_allowed()
        && !options.yes
        && args[0].eq_ignore_ascii_case(b"LRANGE")
    {
        let estimate =
            session
                .config
                .connect(true)
                .ok()
                .and_then(|mut connection| {
                    estimate::estimate(&mut connection, args)
                });
        if estimate::needs_confirmation(
            estimate,
            options.reply_threshold,
            false,
        ) {
            return Err(format!("reply estimated at {} elements; not executed. Use --yes to allow it in noninteractive mode",
                estimate.unwrap_or_default()).into());
        }
    }
    let reply = session.execute(args)?;
    print_reply(&reply, options.raw)?;
    while session.streaming {
        print_reply(&session.connection.recv_response()?, options.raw)?;
    }
    Ok(())
}

fn print_reply(reply: &Value, raw: bool) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if raw {
        raw_reply(&mut stdout, reply)?;
    } else {
        for line in session::format_reply(reply) {
            writeln!(stdout, "{line}")?;
        }
    }
    stdout.flush()
}

fn raw_reply(out: &mut impl Write, reply: &Value) -> io::Result<()> {
    match reply {
        Value::BulkString(bytes) => {
            out.write_all(bytes)?;
            out.write_all(b"\n")
        }
        Value::Array(values)
        | Value::Set(values)
        | Value::Push { data: values, .. } => {
            for value in values {
                raw_reply(out, value)?;
            }
            Ok(())
        }
        _ => {
            for line in session::format_reply(reply) {
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
    }
}
