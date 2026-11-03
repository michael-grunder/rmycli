use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::{config, session::Config};
use clap::Parser;
use redis::{ConnectionAddr, IntoConnectionInfo};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "A Redis shell with live command help and type-aware completion",
    disable_help_flag = true,
    disable_version_flag = true,
    after_help = include_str!("../data/cli-help.txt")
)]
// Clap models independent command-line switches as booleans.
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    /// Print help
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,
    /// Print version
    #[arg(short = 'v', long, action = clap::ArgAction::Version)]
    pub version: Option<bool>,
    #[command(flatten, next_help_heading = "Local settings")]
    pub settings: ConfigOptions,
    /// Redis host (default: 127.0.0.1, or selected instance's host)
    #[arg(short = 'h', long, help_heading = "Connection")]
    pub host: Option<String>,
    /// Redis TCP port (default: 6379, or selected instance's port)
    #[arg(short = 'p', long, help_heading = "Connection")]
    pub port: Option<u16>,
    /// Connection URL (redis://, rediss://, or redis+unix://)
    #[arg(
        short = 'u',
        long,
        env = "REDIS_URL",
        hide_env_values = true,
        help_heading = "Connection"
    )]
    pub uri: Option<String>,
    /// Unix domain socket
    #[arg(
        short = 's',
        long,
        conflicts_with = "tls",
        help_heading = "Connection"
    )]
    pub socket: Option<PathBuf>,
    /// Database number
    #[arg(short = 'n', long, help_heading = "Connection")]
    pub db: Option<i64>,
    /// ACL username
    #[arg(long, help_heading = "Connection")]
    pub user: Option<String>,
    /// Prefer `REDISCLI_AUTH` to putting a password in shell history
    #[arg(
        short = 'a',
        long = "pass",
        visible_alias = "password",
        env = "REDISCLI_AUTH",
        hide_env_values = true,
        help_heading = "Connection"
    )]
    pub password: Option<String>,
    /// Use TLS with certificate verification
    #[arg(long, help_heading = "Connection")]
    pub tls: bool,
    /// Connection and auxiliary-request timeout in milliseconds
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..), help_heading = "Connection")]
    pub timeout_ms: u64,
    /// Connection timeout in seconds (decimals allowed; 0 uses the OS timeout)
    #[arg(short = 't', value_name = "SECONDS", value_parser = crate::batch::seconds, help_heading = "Connection")]
    pub connect_timeout: Option<Duration>,
    /// Ask before showing more than this many completion candidates
    #[arg(long, default_value_t = 100, help_heading = "Shell assistance")]
    pub completion_threshold: usize,
    /// Maximum retained completion keys
    #[arg(long, default_value_t = 10000, help_heading = "Shell assistance")]
    pub scan_max_keys: usize,
    /// Maximum retained completion key bytes
    #[arg(
        long,
        default_value_t = 4_194_304,
        help_heading = "Shell assistance"
    )]
    pub scan_max_bytes: usize,
    /// Maximum SCAN requests per discovery step
    #[arg(long, default_value_t = 32, help_heading = "Shell assistance")]
    pub scan_max_batches: usize,
    /// Time budget per discovery step (plus at most one in-flight request)
    #[arg(long, default_value_t = 750, help_heading = "Shell assistance")]
    pub scan_ms: u64,
    /// Confirm LRANGE replies estimated above this element count
    #[arg(long, default_value_t = 10000, help_heading = "Shell assistance")]
    pub reply_threshold: u64,
    /// Execute without reply-size confirmation (also for noninteractive use)
    #[arg(short = 'y', long, help_heading = "Shell assistance")]
    pub yes: bool,
    #[command(flatten, next_help_heading = "Output")]
    pub output: crate::output::Options,
    /// Start in RESP2 protocol mode (default)
    #[arg(short = '2', conflicts_with = "resp3", help_heading = "Connection")]
    pub resp2: bool,
    /// Start in RESP3 protocol mode (HELLO 3)
    #[arg(short = '3', conflicts_with = "resp2", help_heading = "Connection")]
    pub resp3: bool,
    /// Prompt for a password without echo; overrides all other passwords
    #[arg(long, conflicts_with_all = ["stdin_last", "stdin_tag"], help_heading = "Connection")]
    pub askpass: bool,
    /// Set the main connection's CLIENT SETNAME
    #[arg(long, help_heading = "Connection")]
    pub name: Option<String>,
    /// Follow MOVED/ASK redirects outside transactions and WATCH
    #[arg(short = 'c', conflicts_with = "socket", help_heading = "Connection")]
    pub cluster: bool,
    /// Print RESP3 push messages (yes/no; default: yes on a terminal)
    #[arg(long, value_name = "YN", value_parser = ["yes", "no"], help_heading = "Output")]
    pub show_pushes: Option<String>,
    /// Return failure on Redis errors (already the default in rmycli)
    #[arg(short = 'e', help_heading = "Execution")]
    pub exit_on_error: bool,
    /// Suppress the command-line password warning
    #[arg(long, help_heading = "Connection")]
    pub no_auth_warning: bool,
    /// Print connection status to stderr (without credentials)
    #[arg(long, help_heading = "Connection")]
    pub verbose: bool,
    #[command(flatten, next_help_heading = "TLS")]
    pub certificates: TlsOptions,
    #[command(flatten, next_help_heading = "Execution")]
    pub batch: crate::batch::Options,
    /// Redis command, or `config get/set/unset` for local settings
    #[arg(trailing_var_arg = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
pub struct ConfigOptions {
    /// Read this TOML config instead of searching XDG directories
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    pub config: Option<PathBuf>,
    /// Use built-in settings without reading a config file
    #[arg(long)]
    pub no_config: bool,
    /// Connect using a named instance from the config file
    #[arg(long, value_name = "NAME", conflicts_with_all = ["uri", "socket"])]
    pub instance: Option<String>,
}

impl Options {
    pub fn config(
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
            info = info.set_addr(ConnectionAddr::Unix(socket.clone()));
        }
        if self.tls || instance.tls {
            if let ConnectionAddr::Tcp(host, port) = info.addr().clone() {
                info = info.set_addr(ConnectionAddr::TcpTls {
                    host,
                    port,
                    insecure: false,
                    tls_params: None,
                });
            } else if matches!(info.addr(), ConnectionAddr::Unix(_)) {
                return Err("TLS cannot be used with a Unix socket".into());
            }
        }
        let tls = self.certificates.apply(&mut info)?;
        let mut redis = info.redis_settings().clone();
        if self.resp2 {
            redis = redis.set_protocol(redis::ProtocolVersion::RESP2);
        } else if self.resp3 || self.output.json || self.output.quoted_json {
            redis = redis.set_protocol(redis::ProtocolVersion::RESP3);
        }
        if let Some(db) = self.db.or(instance.db) {
            if db < 0 {
                return Err("database must be nonnegative".into());
            }
            redis = redis.set_db(db);
        }
        if let Some(user) = self.user.as_ref().or(instance.user.as_ref()) {
            redis = redis.set_username(user);
        }
        let password = match &self.password {
            Some(password) => Some(password.clone()),
            None if self.askpass => None,
            None => instance.password(|name| std::env::var(name).ok())?,
        };
        if let Some(password) = password {
            redis = redis.set_password(password);
        }
        if self.askpass {
            redis = redis.set_password(read_password()?);
        }
        info = info.set_redis_settings(redis);
        Ok(Config {
            info,
            tls,
            timeout: Duration::from_millis(self.timeout_ms),
            connect_timeout: self
                .connect_timeout
                .or_else(|| Some(Duration::from_millis(self.timeout_ms)))
                .filter(|duration| !duration.is_zero()),
            name: self.name.clone(),
            cluster: self.cluster,
            output: self.output.clone(),
            show_pushes: self.show_pushes.as_deref().map_or_else(
                || std::io::IsTerminal::is_terminal(&std::io::stdout()),
                |value| value == "yes",
            ),
        })
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

    #[test]
    fn connection_flags_preserve_uri_addresses_and_timeout_units() {
        let config = options(&[
            "rmycli",
            "-u",
            "redis://example.test:6380/2",
            "--tls",
            "-3",
            "-t",
            "0.125",
        ])
        .config(&config::Settings::default())
        .unwrap();
        assert!(
            matches!(config.info.addr(), ConnectionAddr::TcpTls { host, port: 6380, insecure: false, .. } if host == "example.test")
        );
        assert_eq!(config.info.redis_settings().db(), 2);
        assert_eq!(
            config.info.redis_settings().protocol(),
            redis::ProtocolVersion::RESP3
        );
        assert_eq!(config.connect_timeout, Some(Duration::from_millis(125)));
        let config = options(&[
            "rmycli",
            "-t",
            "0",
            "-h",
            "ignored",
            "-p",
            "1",
            "-s",
            "/tmp/redis.sock",
        ])
        .config(&config::Settings::default())
        .unwrap();
        assert!(matches!(config.info.addr(), ConnectionAddr::Unix(_)));
        assert_eq!(config.connect_timeout, None);
        assert_eq!(config.timeout, Duration::from_secs(1));
        assert!(
            options(&["rmycli", "--insecure"])
                .config(&config::Settings::default())
                .is_err()
        );
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
        assert!(matches!(config.info.addr(),
            ConnectionAddr::TcpTls { host, port: 6380, insecure: false, .. }
                if host == "qa.example"));
        assert_eq!(config.info.redis_settings().db(), 2);
        assert_eq!(config.info.redis_settings().username(), Some("alice"));
        assert_eq!(config.info.redis_settings().password(), Some("qa-secret"));
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
        assert!(matches!(config.info.addr(),
            ConnectionAddr::TcpTls { host, port: 6381, .. }
                if host == "override.example"));
        assert_eq!(config.info.redis_settings().db(), 3);
        assert_eq!(config.info.redis_settings().username(), Some("bob"));
        assert_eq!(config.info.redis_settings().password(), Some("override"));
    }

    #[test]
    fn unselected_instances_do_not_change_defaults_or_url_credentials() {
        let config = options(&["rmycli"]).config(&settings()).unwrap();
        assert!(matches!(config.info.addr(),
            ConnectionAddr::Tcp(host, 6379) if host == "127.0.0.1"));
        assert_eq!(config.info.redis_settings().db(), 0);
        assert!(config.info.redis_settings().username().is_none());
        assert!(config.info.redis_settings().password().is_none());
        let config = options(&[
            "rmycli",
            "-u",
            "redis://url-user:url-secret@localhost:6382/4",
        ])
        .config(&settings())
        .unwrap();
        assert_eq!(config.info.redis_settings().db(), 4);
        assert_eq!(config.info.redis_settings().username(), Some("url-user"));
        assert_eq!(config.info.redis_settings().password(), Some("url-secret"));
    }
}

#[derive(Debug, clap::Args)]
#[group(multiple = true)]
pub struct TlsOptions {
    /// PEM CA bundle; otherwise use system trust roots
    #[arg(long, value_name = "FILE")]
    pub cacert: Option<PathBuf>,
    /// Directory of PEM CA certificates
    #[arg(long, value_name = "DIR")]
    pub cacertdir: Option<PathBuf>,
    /// PEM client certificate chain (requires --key)
    #[arg(long, value_name = "FILE", requires = "key")]
    pub cert: Option<PathBuf>,
    /// PEM client private key (requires --cert)
    #[arg(long, value_name = "FILE", requires = "cert")]
    pub key: Option<PathBuf>,
    /// Skip TLS certificate verification (unsafe; TLS connections only)
    #[arg(long)]
    pub insecure: bool,
}

impl TlsOptions {
    fn apply(
        &self,
        info: &mut redis::ConnectionInfo,
    ) -> Result<
        Option<std::sync::Arc<rustls::ClientConfig>>,
        Box<dyn std::error::Error>,
    > {
        let custom = self.cacert.is_some()
            || self.cacertdir.is_some()
            || self.cert.is_some();
        if !custom
            && !self.insecure
            && !matches!(info.addr(), ConnectionAddr::TcpTls { .. })
        {
            return Ok(None);
        }
        let mut addr = info.addr().clone();
        let ConnectionAddr::TcpTls { insecure, .. } = &mut addr else {
            return Err(
                "TLS options require --tls, a rediss:// URI, or a TLS instance"
                    .into(),
            );
        };
        *insecure |= self.insecure;
        let insecure = *insecure;
        *info = info.clone().set_addr(addr);
        let mut certificates = redis::TlsCertificates {
            client_tls: None,
            root_cert: None,
        };
        if custom {
            let mut root_cert =
                self.cacert.as_ref().map(std::fs::read).transpose()?;
            if let Some(dir) = &self.cacertdir {
                let roots = root_cert.get_or_insert_with(Vec::new);
                for entry in std::fs::read_dir(dir)? {
                    let path = entry?.path();
                    if path.is_file() {
                        roots.push(b'\n');
                        roots.extend(std::fs::read(path)?);
                    }
                }
            }
            let client_tls = self
                .cert
                .as_ref()
                .zip(self.key.as_ref())
                .map(|(cert, key)| {
                    Ok::<_, std::io::Error>(redis::ClientTlsConfig {
                        client_cert: std::fs::read(cert)?,
                        client_key: std::fs::read(key)?,
                    })
                })
                .transpose()?;
            certificates = redis::TlsCertificates {
                client_tls,
                root_cert,
            };
            let client = redis::Client::build_with_tls(
                info.clone(),
                certificates.clone(),
            )?;
            *info = client.get_connection_info().clone();
        }
        Ok(Some(crate::transport::tls_config(&certificates, insecure)?))
    }
}

fn read_password() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::{IsTerminal, Write};
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    if !std::io::stdin().is_terminal() {
        return Err("--askpass requires a terminal on stdin; use REDISCLI_AUTH for pipes".into());
    }
    eprint!("Password: ");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let guard = RawMode;
    let mut password = String::new();
    let result = loop {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        if let Event::Key(key) = crossterm::event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => break Ok(password),
                KeyCode::Esc => break Err("password entry cancelled".into()),
                KeyCode::Char('c' | 'd')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    break Err("password entry cancelled".into());
                }
                KeyCode::Backspace => {
                    password.pop();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    password.push(ch);
                }
                _ => {}
            }
        }
    };
    drop(guard);
    eprintln!();
    result
}
