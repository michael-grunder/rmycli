#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

mod config;
mod config_cli;
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
    #[command(flatten)]
    settings: ConfigOptions,
    /// Redis host (default: 127.0.0.1, or selected instance's host)
    #[arg(short = 'h', long)]
    host: Option<String>,
    /// Redis TCP port (default: 6379, or selected instance's port)
    #[arg(short = 'p', long)]
    port: Option<u16>,
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
    /// Redis command, or `config get/set/unset` for local settings
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
struct ConfigOptions {
    /// Read this TOML config instead of searching XDG directories
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    config: Option<PathBuf>,
    /// Use built-in settings without reading a config file
    #[arg(long)]
    no_config: bool,
    /// Connect using a named instance from the config file
    #[arg(long, value_name = "NAME", conflicts_with_all = ["uri", "socket"])]
    instance: Option<String>,
}

impl Options {
    fn config(
        &self,
        settings: &config::Settings,
    ) -> Result<Config, Box<dyn std::error::Error>> {
        let default_instance = config::Instance::default();
        let instance = if let Some(name) = &self.settings.instance {
            settings
                .instances
                .get(name)
                .ok_or_else(|| format!("unknown instance {name:?}"))?
        } else {
            &default_instance
        };
        let host = self
            .host
            .as_deref()
            .or(instance.host.as_deref())
            .unwrap_or("127.0.0.1");
        let port = self.port.or(instance.port).unwrap_or(6379);
        let mut info = if let Some(uri) = &self.uri {
            uri.as_str().into_connection_info()?
        } else {
            (host, port).into_connection_info()?
        };
        if let Some(socket) = &self.socket {
            info.addr = ConnectionAddr::Unix(socket.clone());
        }
        if self.tls || instance.tls {
            info.addr = ConnectionAddr::TcpTls {
                host: host.into(),
                port,
                insecure: false,
                tls_params: None,
            };
        }
        if let Some(db) = self.db.or(instance.db) {
            if db < 0 {
                return Err("database must be nonnegative".into());
            }
            info.redis.db = db;
        }
        if let Some(user) = self.user.as_ref().or(instance.user.as_ref()) {
            info.redis.username = Some(user.clone());
        }
        let password = match &self.password {
            Some(password) => Some(password.clone()),
            None => instance.password(|name| std::env::var(name).ok())?,
        };
        if password.is_some() {
            info.redis.password = password;
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
    if options
        .command
        .first()
        .is_some_and(|command| command == "config")
    {
        return config_cli::run(
            &options.command,
            options.settings.config.as_deref(),
            options.settings.no_config,
        );
    }
    let settings = config::Settings::load(
        options.settings.config.as_deref(),
        options.settings.no_config,
    )?;
    let mut session = Session::open(options.config(&settings)?)?;
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
                theme: settings.theme,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn options(args: &[&str]) -> Options {
        let matches = Options::command()
            .mut_arg("uri", |arg| arg.env(None::<&str>))
            .mut_arg("password", |arg| arg.env(None::<&str>))
            .get_matches_from(args);
        Options::from_arg_matches(&matches).unwrap()
    }

    fn settings() -> config::Settings {
        toml::from_str(
            "[instances.qa]\nhost = 'qa.example'\nport = 6380\n\
             db = 2\nuser = 'alice'\npassword = 'qa-secret'\ntls = true",
        )
        .unwrap()
    }

    #[test]
    fn selected_instance_supplies_connection_and_authentication() {
        let config = options(&["rmycli", "--instance", "qa"])
            .config(&settings())
            .unwrap();
        assert!(matches!(config.info.addr,
            ConnectionAddr::TcpTls { ref host, port: 6380, insecure: false, .. }
                if host == "qa.example"));
        assert_eq!(config.info.redis.db, 2);
        assert_eq!(config.info.redis.username.as_deref(), Some("alice"));
        assert_eq!(config.info.redis.password.as_deref(), Some("qa-secret"));
    }

    #[test]
    fn explicit_options_override_selected_instance() {
        let mut settings = settings();
        let qa = settings.instances.get_mut("qa").unwrap();
        qa.password = None;
        qa.password_env = Some("RMYCLI_TEST_UNSET_PASSWORD".into());
        let config = options(&[
            "rmycli",
            "--instance",
            "qa",
            "-h",
            "override.example",
            "-p",
            "6381",
            "-n",
            "3",
            "--user",
            "bob",
            "-a",
            "override",
        ])
        .config(&settings)
        .unwrap();
        assert!(matches!(config.info.addr,
            ConnectionAddr::TcpTls { ref host, port: 6381, .. }
                if host == "override.example"));
        assert_eq!(config.info.redis.db, 3);
        assert_eq!(config.info.redis.username.as_deref(), Some("bob"));
        assert_eq!(config.info.redis.password.as_deref(), Some("override"));
    }

    #[test]
    fn unselected_instances_do_not_change_defaults_or_url_credentials() {
        let config = options(&["rmycli"]).config(&settings()).unwrap();
        assert!(matches!(config.info.addr,
            ConnectionAddr::Tcp(ref host, 6379) if host == "127.0.0.1"));
        assert_eq!(config.info.redis.db, 0);
        assert!(config.info.redis.username.is_none());
        assert!(config.info.redis.password.is_none());
        let config = options(&[
            "rmycli",
            "-u",
            "redis://url-user:url-secret@localhost:6382/4",
        ])
        .config(&settings())
        .unwrap();
        assert_eq!(config.info.redis.db, 4);
        assert_eq!(config.info.redis.username.as_deref(), Some("url-user"));
        assert_eq!(config.info.redis.password.as_deref(), Some("url-secret"));
    }
}
