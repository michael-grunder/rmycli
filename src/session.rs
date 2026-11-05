use std::time::Duration;

use redis::{Client, Connection, ConnectionInfo, RedisResult, Value};

use crate::input::quote;

#[derive(Clone, Debug)]
pub struct Config {
    pub auth_store: Option<crate::auth::Store>,
    pub binary_auth: bool,
    pub info: ConnectionInfo,
    pub tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    pub timeout: Duration,
    pub connect_timeout: Option<Duration>,
    pub name: Option<String>,
    pub cluster: ClusterMode,
    pub output: crate::output::Options,
    pub show_pushes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterMode {
    Auto,
    Enabled,
    Disabled,
}

impl Config {
    pub fn check_auth(&self) -> RedisResult<()> {
        if self.binary_auth {
            return Err((
                redis::ErrorKind::Client,
                "Binary credentials cannot be reused on another connection",
            )
                .into());
        }
        Ok(())
    }

    pub fn connect_auxiliary(&self) -> RedisResult<Connection> {
        self.check_auth()?;
        let info = self.info.clone().set_redis_settings(
            self.info
                .redis_settings()
                .clone()
                .set_protocol(redis::ProtocolVersion::RESP2),
        );
        let timeout = self
            .connect_timeout
            .map_or(self.timeout, |timeout| timeout.min(self.timeout));
        let connection =
            Client::open(info)?.get_connection_with_timeout(timeout)?;
        connection.set_write_timeout(Some(self.timeout))?;
        connection.set_read_timeout(Some(self.timeout))?;
        Ok(connection)
    }
}

pub fn command(args: &[Vec<u8>]) -> redis::Cmd {
    let mut command = redis::Cmd::new();
    for arg in args {
        command.arg(arg);
    }
    command
}

pub struct Session {
    pub connection: crate::transport::Connection,
    pub config: Config,
    pub transaction: bool,
    pub watched: bool,
    pub streaming: bool,
    pub auth_revision: u64,
    pub notices: Vec<String>,
    queued: Vec<Vec<Vec<u8>>>,
    routing_revision: u64,
}

impl Session {
    pub async fn open(config: Config) -> RedisResult<Self> {
        let mut session = Self {
            connection: crate::transport::Connection::open(&config).await?,
            config,
            transaction: false,
            watched: false,
            streaming: false,
            auth_revision: 0,
            notices: Vec::new(),
            queued: Vec::new(),
            routing_revision: 0,
        };
        session.save_auth();
        Ok(session)
    }

    pub const fn assistance_allowed(&self) -> bool {
        !self.transaction && !self.watched && !self.streaming
    }

    #[cfg(test)]
    async fn poll_message(&mut self) -> RedisResult<Option<Value>> {
        tokio::time::timeout(
            Duration::from_millis(100),
            self.connection.recv_response(),
        )
        .await
        .map_or_else(|_| Ok(None), |value| value.map(Some))
    }

    #[cfg(test)]
    pub async fn execute(&mut self, args: &[Vec<u8>]) -> RedisResult<Value> {
        self.execute_with_push(args, |_| {}).await
    }

    pub async fn execute_with_push(
        &mut self,
        args: &[Vec<u8>],
        mut push: impl FnMut(&Value),
    ) -> RedisResult<Value> {
        for redirects in 0..=5 {
            let can_redirect = self.assistance_allowed();
            let result = self.execute_once(args, &mut push).await;
            if self.assistance_allowed()
                && self.routing_revision != self.auth_revision
            {
                self.routing_revision = self.auth_revision;
                if let Err(error) =
                    self.connection.detect_server(&self.config, &mut push).await
                {
                    self.notices.push(format!(
                        "Server detection failed: {}",
                        crate::output::error_message(&error)
                    ));
                }
            }
            let Err(error) = &result else {
                return result;
            };
            if !can_redirect || redirects == 5 {
                return result;
            }
            let target = if self.connection.redirect
                && error.code() == Some("REDIRECT")
            {
                error.detail()
            } else if self.config.cluster == ClusterMode::Enabled
                || self.connection.cluster
            {
                error.redirect_node().map(|(target, _)| target)
            } else {
                None
            };
            let Some(target) = target else {
                return result;
            };
            let mut config = self.config.clone();
            let mut addr = config.info.addr().clone();
            redirect_address(&mut addr, target)?;
            config.info = config.info.set_addr(addr);
            let mut connection =
                crate::transport::Connection::open(&config).await?;
            connection.cluster |= self.connection.cluster;
            if error.kind()
                == redis::ErrorKind::Server(redis::ServerErrorKind::Ask)
            {
                connection.query(&redis::cmd("ASKING")).await?;
            }
            self.connection = connection;
            self.config = config;
            self.save_auth();
        }
        unreachable!("the last redirect returns its error")
    }

    async fn execute_once(
        &mut self,
        args: &[Vec<u8>],
        push: &mut impl FnMut(&Value),
    ) -> RedisResult<Value> {
        let unsubscription =
            [b"UNSUBSCRIBE".as_slice(), b"PUNSUBSCRIBE", b"SUNSUBSCRIBE"]
                .iter()
                .any(|name| args[0].eq_ignore_ascii_case(name));
        let subscription = unsubscription
            || [b"SUBSCRIBE".as_slice(), b"PSUBSCRIBE", b"SSUBSCRIBE"]
                .iter()
                .any(|name| args[0].eq_ignore_ascii_case(name));
        // Each named channel is acknowledged separately. Unread acknowledgments
        // would otherwise be taken for the replies to the following commands.
        let mut extra_acks = if unsubscription && !self.transaction {
            args.len().saturating_sub(2)
        } else {
            0
        };
        let result: RedisResult<Value> = async {
            self.connection
                .send_packed_command(&command(args).get_packed_command())
                .await?;
            loop {
                match self.connection.recv_response().await? {
                    value @ Value::Push { .. } if !subscription => push(&value),
                    Value::ServerError(error) => break Err(error.into()),
                    value if extra_acks > 0 => {
                        extra_acks -= 1;
                        push(&value);
                    }
                    value => break Ok(value),
                }
            }
        }
        .await;
        match &result {
            Ok(value) => self.observe(args, value),
            // EXEC without MULTI fails without clearing the server's WATCH.
            Err(error)
                if self.transaction
                    && args[0].eq_ignore_ascii_case(b"EXEC")
                    && !error.is_io_error() =>
            {
                self.transaction = false;
                self.watched = false;
                self.queued.clear();
            }
            _ => {}
        }
        result
    }

    fn observe(&mut self, args: &[Vec<u8>], reply: &Value) {
        if self.transaction
            && matches!(reply, Value::SimpleString(s) if s == "QUEUED")
        {
            self.queued.push(args.to_vec());
            return;
        }
        if matches!(reply, Value::ServerError(_)) {
            return;
        }
        let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        match name.as_str() {
            "MULTI" => self.transaction = true,
            "WATCH" => self.watched = true,
            "UNWATCH" => self.watched = false,
            "DISCARD" | "EXEC" => {
                self.transaction = false;
                self.watched = false;
                let queued = std::mem::take(&mut self.queued);
                if let Value::Array(replies) = reply {
                    for (args, reply) in queued.iter().zip(replies) {
                        self.observe(args, reply);
                    }
                }
            }
            "SELECT" => {
                if let Some(db) = args
                    .get(1)
                    .and_then(|a| std::str::from_utf8(a).ok())
                    .and_then(|a| a.parse().ok())
                {
                    self.update_settings(|settings| settings.set_db(db));
                }
            }
            "AUTH" => self.update_auth(&args[1..]),
            "CLIENT"
                if args.get(1).is_some_and(|arg| {
                    arg.eq_ignore_ascii_case(b"SETNAME")
                }) =>
            {
                self.config.name = args
                    .get(2)
                    .map(|arg| String::from_utf8_lossy(arg).into_owned());
            }
            "HELLO" => self.observe_hello(args),
            "RESET" => {
                self.connection.cluster = false;
                self.connection.redirect = false;
                self.auth_revision += 1;
                self.config.binary_auth = false;
                self.update_settings(|_| redis::RedisConnectionInfo::default());
                self.config.name = None;
                self.transaction = false;
                self.watched = false;
                self.streaming = false;
                self.queued.clear();
            }
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" | "SYNC"
            | "PSYNC" => {
                self.streaming = true;
            }
            _ => {}
        }
    }

    fn observe_hello(&mut self, args: &[Vec<u8>]) {
        let protocol = match args.get(1).map(Vec::as_slice) {
            Some(b"2") => Some(redis::ProtocolVersion::RESP2),
            Some(b"3") => Some(redis::ProtocolVersion::RESP3),
            _ => None,
        };
        if let Some(protocol) = protocol {
            self.update_settings(|settings| settings.set_protocol(protocol));
        }
        let mut options = args.get(2..).unwrap_or_default();
        let mut auth = None;
        while let Some(option) = options.first() {
            if option.eq_ignore_ascii_case(b"AUTH") && options.len() >= 3 {
                auth = Some(&options[1..3]);
                options = &options[3..];
            } else if option.eq_ignore_ascii_case(b"SETNAME")
                && options.len() >= 2
            {
                self.config.name =
                    Some(String::from_utf8_lossy(&options[1]).into_owned());
                options = &options[2..];
            } else {
                break;
            }
        }
        if let Some(auth) = auth {
            self.update_auth(auth);
        }
    }

    fn update_settings(
        &mut self,
        update: impl FnOnce(
            redis::RedisConnectionInfo,
        ) -> redis::RedisConnectionInfo,
    ) {
        let settings = update(self.config.info.redis_settings().clone());
        self.config.info =
            self.config.info.clone().set_redis_settings(settings);
    }

    fn update_auth(&mut self, auth: &[Vec<u8>]) {
        let (user, password) = match auth {
            [password] => (None, password),
            [user, password] => (Some(user.as_slice()), password),
            _ => return,
        };
        self.auth_revision += 1;
        self.config.binary_auth = true;
        let settings = redis::RedisConnectionInfo::default()
            .set_db(self.config.info.redis_settings().db())
            .set_protocol(self.config.info.redis_settings().protocol());
        // Never persist a lossy conversion of successfully submitted bytes.
        let text = std::str::from_utf8(password)
            .ok()
            .zip(user.map(std::str::from_utf8).transpose().ok());
        let Some((password, user)) = text else {
            self.update_settings(|_| settings);
            self.notices.push("Binary credentials cannot be reused for background requests, reconnects, or saved authentication.".into());
            return;
        };
        let mut settings = settings.set_password(password);
        if let Some(user) = user {
            settings = settings.set_username(user);
        }
        self.update_settings(|_| settings);
        self.config.binary_auth = false;
        self.save_auth();
    }

    fn save_auth(&mut self) {
        if let Some(store) = &self.config.auth_store
            && let Err(error) = store.save(&self.config.info)
        {
            self.notices.push(format!("Authentication succeeded, but credentials could not be saved: {error}"));
        }
    }
}

fn redirect_address(
    addr: &mut redis::ConnectionAddr,
    target: &str,
) -> RedisResult<()> {
    use redis::{ConnectionAddr, ErrorKind, RedisError};
    let invalid =
        || RedisError::from((ErrorKind::Client, "invalid redirect address"));
    let (host, port) = target.rsplit_once(':').ok_or_else(invalid)?;
    let port = port.parse::<u16>().map_err(|_| invalid())?;
    if port == 0 || host.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match addr {
        ConnectionAddr::Tcp(current, current_port)
        | ConnectionAddr::TcpTls {
            host: current,
            port: current_port,
            ..
        } => {
            if !host.is_empty() {
                host.clone_into(current);
            }
            *current_port = port;
            Ok(())
        }
        _ => Err(RedisError::from((
            ErrorKind::Client,
            "cannot follow a redirect from this transport",
        ))),
    }
}

pub fn format_reply(value: &Value) -> Vec<String> {
    match value {
        Value::Nil => vec!["(nil)".into()],
        Value::Okay => vec!["OK".into()],
        Value::Int(n) => vec![format!("(integer) {n}")],
        Value::BulkString(bytes) => vec![quote(bytes)],
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            vec![quote(s.as_bytes())]
        }
        Value::Set(values) if values.is_empty() => {
            vec!["(empty set)".into()]
        }
        Value::Map(entries) if entries.is_empty() => {
            vec!["(empty hash)".into()]
        }
        Value::Array(values)
        | Value::Set(values)
        | Value::Push { data: values, .. } => {
            if values.is_empty() {
                return vec!["(empty array)".into()];
            }
            values
                .iter()
                .enumerate()
                .flat_map(|(index, value)| {
                    let label = format!("{}) ", index + 1);
                    let indent = " ".repeat(label.len());
                    format_reply(value).into_iter().enumerate().map(
                        move |(line, text)| {
                            let prefix =
                                if line == 0 { &label } else { &indent };
                            format!("{prefix}{text}")
                        },
                    )
                })
                .collect()
        }
        Value::Map(entries) => entries
            .iter()
            .flat_map(|(k, v)| {
                let mut result = format_reply(k);
                result.extend(
                    format_reply(v).into_iter().map(|line| format!("  {line}")),
                );
                result
            })
            .collect(),
        Value::Attribute { data, .. } => format_reply(data),
        Value::Double(n) => vec![format!("(double) {n}")],
        Value::Boolean(b) => vec![format!("(boolean) {b}")],
        Value::BigNumber(n) => vec![format!("(big number) {n}")],
        Value::ServerError(error) => vec![format!(
            "(error) {}",
            crate::output::server_error_message(error.code(), error.details())
                .chars()
                .map(|c| {
                    if c.is_control() {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        )],
        _ => vec!["(unsupported Redis reply type)".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    fn server(
        mut reply: impl FnMut(&[String]) -> Vec<u8> + Send + 'static,
    ) -> (u16, JoinHandle<()>) {
        mock_server(move |args| match args[0].as_str() {
            "INFO" => b"-ERR unknown command 'INFO'\r\n".to_vec(),
            "CLIENT" => b"+OK\r\n".to_vec(),
            _ => reply(args),
        })
    }

    fn mock_server(
        mut reply: impl FnMut(&[String]) -> Vec<u8> + Send + 'static,
    ) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut parser = redis::Parser::new();
            while let Ok(value) = parser.parse_value(&mut reader) {
                let args: Vec<String> = redis::from_redis_value(value).unwrap();
                let response = reply(&args);
                reader.get_mut().write_all(&response).unwrap();
            }
        });
        (port, thread)
    }

    fn config(port: u16) -> Config {
        use redis::IntoConnectionInfo;
        Config {
            auth_store: None,
            binary_auth: false,
            info: ("127.0.0.1", port).into_connection_info().unwrap(),
            tls: None,
            timeout: Duration::from_secs(1),
            connect_timeout: Some(Duration::from_secs(1)),
            name: None,
            cluster: ClusterMode::Enabled,
            output: crate::output::Options::default(),
            show_pushes: true,
        }
    }

    fn info(mode: &str, valkey: bool) -> Vec<u8> {
        let version = if valkey {
            "valkey_version:8.1.0\r\n"
        } else {
            ""
        };
        let mode_field = if valkey { "server_mode" } else { "redis_mode" };
        let text = format!("# Server\r\n{mode_field}:{mode}\r\n{version}");
        format!("${}\r\n{text}\r\n", text.len()).into_bytes()
    }

    #[tokio::test]
    async fn automatic_cluster_detection_and_opt_out() {
        for (automatic, forced) in
            [(true, false), (false, false), (false, true)]
        {
            let (port, server) = mock_server(|args| match args[0].as_str() {
                "INFO" => info("cluster", false),
                "GET" => b"-MOVED 1 host:not-a-port\r\n".to_vec(),
                _ => panic!("unexpected command: {args:?}"),
            });
            let mut config = config(port);
            config.cluster = if forced {
                ClusterMode::Enabled
            } else if automatic {
                ClusterMode::Auto
            } else {
                ClusterMode::Disabled
            };
            let mut session = Session::open(config).await.unwrap();
            assert_eq!(session.connection.cluster, automatic);
            let error = session.execute(&[b"GET".to_vec()]).await.unwrap_err();
            assert_eq!(
                error.kind(),
                if automatic || forced {
                    redis::ErrorKind::Client
                } else {
                    redis::ErrorKind::Server(redis::ServerErrorKind::Moved)
                }
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn valkey_redirect_negotiates_on_each_connection_and_keeps_settings()
    {
        let mut capable = false;
        let (port, target) = mock_server(move |args| match args[0].as_str() {
            "AUTH" => {
                assert_eq!(args, &["AUTH", "alice", "secret"]);
                b"+OK\r\n".to_vec()
            }
            "SELECT" => {
                assert_eq!(args, &["SELECT", "2"]);
                b"+OK\r\n".to_vec()
            }
            "INFO" => info("standalone", true),
            "CLIENT" => {
                if args[1] == "CAPA" {
                    assert_eq!(args, &["CLIENT", "CAPA", "REDIRECT"]);
                    capable = true;
                } else {
                    assert_eq!(args, &["CLIENT", "SETNAME", "test"]);
                }
                b"+OK\r\n".to_vec()
            }
            "GET" => {
                assert!(capable);
                assert_eq!(args, &["GET", "key with spaces\0"]);
                b"$5\r\nvalue\r\n".to_vec()
            }
            _ => panic!("unexpected command: {args:?}"),
        });
        let mut capable = false;
        let (source_port, source) =
            mock_server(move |args| match args[0].as_str() {
                "INFO" => info("standalone", true),
                "CLIENT" if args[1] == "CAPA" => {
                    capable = true;
                    b"+OK\r\n".to_vec()
                }
                "GET" => {
                    assert!(capable);
                    format!("-REDIRECT 127.0.0.1:{port}\r\n").into_bytes()
                }
                _ => b"+OK\r\n".to_vec(),
            });
        let mut config = config(source_port);
        config.cluster = ClusterMode::Disabled;
        config.name = Some("test".into());
        config.info = config.info.clone().set_redis_settings(
            config
                .info
                .redis_settings()
                .clone()
                .set_username("alice")
                .set_password("secret")
                .set_db(2),
        );
        let mut session = Session::open(config).await.unwrap();
        for _ in 0..2 {
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec(), b"key with spaces\0".to_vec()])
                    .await
                    .unwrap(),
                Value::BulkString(b"value".to_vec())
            );
        }
        assert_eq!(
            session.config.info.addr(),
            &redis::ConnectionAddr::Tcp("127.0.0.1".into(), port)
        );
        drop(session);
        tokio::task::yield_now().await;
        source.join().unwrap();
        target.join().unwrap();
    }

    #[tokio::test]
    async fn unsupported_or_denied_detection_does_not_prevent_commands() {
        for (server_info, capability) in [
            (info("standalone", false), b"+OK\r\n".to_vec()),
            (b"-NOPERM INFO denied\r\n".to_vec(), b"+OK\r\n".to_vec()),
            (
                b"-NOAUTH Authentication required\r\n".to_vec(),
                b"+OK\r\n".to_vec(),
            ),
            (
                info("standalone", true),
                b"-ERR unknown subcommand CAPA\r\n".to_vec(),
            ),
            (
                info("standalone", true),
                b"-NOPERM CAPA denied\r\n".to_vec(),
            ),
        ] {
            let (port, server) =
                mock_server(move |args| match args[0].as_str() {
                    "INFO" => server_info.clone(),
                    "CLIENT" => {
                        assert!(
                            server_info
                                .windows(14)
                                .any(|v| v == b"valkey_version")
                        );
                        capability.clone()
                    }
                    "PING" => b"+PONG\r\n".to_vec(),
                    "GET" => b"-REDIRECT host:not-a-port\r\n".to_vec(),
                    _ => panic!("unexpected command: {args:?}"),
                });
            let mut session = Session::open(config(port)).await.unwrap();
            assert!(!session.connection.cluster);
            assert!(!session.connection.redirect);
            assert_eq!(
                session.execute(&[b"PING".to_vec()]).await.unwrap(),
                Value::SimpleString("PONG".into())
            );
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec()])
                    .await
                    .unwrap_err()
                    .code(),
                Some("REDIRECT")
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn valkey_redirect_does_not_replay_stateful_commands() {
        for state in ["WATCH", "MULTI", "streaming"] {
            let (port, server) = mock_server(|args| match args[0].as_str() {
                "INFO" => info("standalone", true),
                "CLIENT" => b"+OK\r\n".to_vec(),
                _ => b"-REDIRECT host:not-a-port\r\n".to_vec(),
            });
            let mut session = Session::open(config(port)).await.unwrap();
            session.watched = state == "WATCH";
            session.transaction = state == "MULTI";
            session.streaming = state == "streaming";
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec()])
                    .await
                    .unwrap_err()
                    .code(),
                Some("REDIRECT")
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn redirect_chains_are_bounded() {
        for kind in ["REDIRECT", "MOVED 1", "ASK 1"] {
            let mut port = 1;
            let mut servers = Vec::new();
            for index in 0..6 {
                let target = port;
                let (next, server) =
                    mock_server(move |args| match args[0].as_str() {
                        "INFO" if index == 5 || kind == "REDIRECT" => {
                            info("cluster", true)
                        }
                        "INFO" => b"-NOPERM INFO denied\r\n".to_vec(),
                        "CLIENT" | "ASKING" => b"+OK\r\n".to_vec(),
                        _ => format!("-{kind} 127.0.0.1:{target}\r\n")
                            .into_bytes(),
                    });
                port = next;
                servers.push(server);
            }
            let mut config = config(port);
            config.cluster = ClusterMode::Auto;
            let mut session = Session::open(config).await.unwrap();
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec()])
                    .await
                    .unwrap_err()
                    .code(),
                kind.split_whitespace().next()
            );
            drop(session);
            tokio::task::yield_now().await;
            for server in servers {
                server.join().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn detection_after_authentication_waits_for_watch_and_preserves_pushes()
     {
        let mut authenticated = false;
        let mut watched = false;
        let mut restricted = false;
        let (port, server) = mock_server(move |args| match args[0].as_str() {
            "AUTH" => {
                if args[1] == "wrong" {
                    b"-WRONGPASS invalid password\r\n".to_vec()
                } else {
                    authenticated = true;
                    restricted = args[1] == "limited";
                    b"+OK\r\n".to_vec()
                }
            }
            "WATCH" => {
                watched = true;
                b"+OK\r\n".to_vec()
            }
            "UNWATCH" => {
                watched = false;
                b"+OK\r\n".to_vec()
            }
            "RESET" => {
                authenticated = false;
                restricted = false;
                b"+RESET\r\n".to_vec()
            }
            "INFO" => {
                assert!(!watched);
                if restricted {
                    b"-NOPERM INFO denied\r\n".to_vec()
                } else if authenticated {
                    let mut response =
                        b">2\r\n+invalidate\r\n*1\r\n$3\r\nkey\r\n".to_vec();
                    response.extend(info("cluster", true));
                    response
                } else {
                    b"-NOAUTH Authentication required\r\n".to_vec()
                }
            }
            "CLIENT" => {
                assert!(authenticated && !watched);
                b"+OK\r\n".to_vec()
            }
            _ => panic!("unexpected command: {args:?}"),
        });
        let mut config = config(port);
        config.cluster = ClusterMode::Auto;
        let mut session = Session::open(config).await.unwrap();
        assert!(!session.connection.redirect);
        assert!(
            session
                .execute(&[b"AUTH".to_vec(), b"wrong".to_vec()])
                .await
                .is_err()
        );
        assert!(!session.connection.redirect);
        session
            .execute(&[b"WATCH".to_vec(), b"key".to_vec()])
            .await
            .unwrap();
        session
            .execute(&[b"AUTH".to_vec(), b"secret".to_vec()])
            .await
            .unwrap();
        assert!(!session.connection.redirect);
        let mut pushes = Vec::new();
        session
            .execute_with_push(&[b"UNWATCH".to_vec()], |value| {
                pushes.push(value.clone());
            })
            .await
            .unwrap();
        assert!(session.connection.cluster && session.connection.redirect);
        assert_eq!(pushes.len(), 1);
        session
            .execute(&[b"AUTH".to_vec(), b"limited".to_vec()])
            .await
            .unwrap();
        assert!(session.connection.cluster && session.connection.redirect);
        session.execute(&[b"RESET".to_vec()]).await.unwrap();
        assert!(!session.connection.cluster && !session.connection.redirect);
        drop(session);
        tokio::task::yield_now().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn moved_and_ask_follow_redirects_and_keep_command_bytes() {
        for kind in ["MOVED", "ASK"] {
            let mut asking = false;
            let (port, target) = server(move |args| {
                if args[0] == "ASKING" {
                    asking = true;
                    return b"+OK\r\n".to_vec();
                }
                assert_eq!(args, &["GET", "key"]);
                assert_eq!(asking, kind == "ASK");
                b"$5\r\nvalue\r\n".to_vec()
            });
            let (source_port, source) = mock_server(move |args| {
                if args[0] == "INFO" {
                    info("cluster", false)
                } else {
                    format!("-{kind} 1 127.0.0.1:{port}\r\n").into_bytes()
                }
            });
            let mut config = config(source_port);
            config.cluster = ClusterMode::Auto;
            let mut session = Session::open(config).await.unwrap();
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec(), b"key".to_vec()])
                    .await
                    .unwrap(),
                Value::BulkString(b"value".to_vec())
            );
            assert_eq!(
                session.config.info.addr(),
                &redis::ConnectionAddr::Tcp("127.0.0.1".into(), port)
            );
            drop(session);
            tokio::task::yield_now().await;
            source.join().unwrap();
            target.join().unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_save_credentials_only_after_successful_authentication() {
        use crate::auth::{SaveAuth, Store};

        for kind in ["MOVED 1", "ASK 1", "REDIRECT"] {
            for (policy, accepted, broken_store) in [
                (SaveAuth::Local, true, false),
                (SaveAuth::Always, true, false),
                (SaveAuth::Never, true, false),
                (SaveAuth::Local, false, false),
                (SaveAuth::Local, true, true),
            ] {
                let (port, target) = server(move |args| {
                    if args[0] == "AUTH" {
                        assert_eq!(args, &["AUTH", "alice", "secret"]);
                        if !accepted {
                            return b"-WRONGPASS invalid credentials\r\n"
                                .to_vec();
                        }
                    }
                    b"+OK\r\n".to_vec()
                });
                let (source_port, source) = server(move |args| {
                    if args[0] == "GET" {
                        format!("-{kind} 127.0.0.1:{port}\r\n").into_bytes()
                    } else {
                        b"+OK\r\n".to_vec()
                    }
                });
                let dir = tempfile::tempdir().unwrap();
                let store =
                    Store::plaintext(dir.path().join("auth.toml"), policy);
                let mut source_config = config(source_port);
                source_config.auth_store = Some(store.clone());
                source_config.info =
                    source_config.info.clone().set_redis_settings(
                        source_config
                            .info
                            .redis_settings()
                            .clone()
                            .set_username("alice")
                            .set_password("secret"),
                    );
                let mut session = Session::open(source_config).await.unwrap();
                session.connection.redirect = kind == "REDIRECT";
                if broken_store {
                    std::fs::write(&store.path, "[broken").unwrap();
                }
                let result =
                    session.execute(&[b"GET".to_vec(), b"foo".to_vec()]).await;
                if accepted {
                    assert_eq!(result.unwrap(), Value::Okay);
                } else {
                    assert_eq!(result.unwrap_err().code(), Some("WRONGPASS"));
                }
                if broken_store {
                    assert_eq!(
                        std::fs::read_to_string(&store.path).unwrap(),
                        "[broken"
                    );
                    assert!(session.notices.iter().any(|notice| {
                        notice.contains("could not be saved")
                    }));
                } else {
                    assert_eq!(session.notices, Vec::<String>::new());
                    for (endpoint, saved) in
                        [(source_port, true), (port, accepted)]
                    {
                        let mut direct = config(endpoint).info;
                        store.apply(&mut direct).unwrap();
                        let saved = saved && policy != SaveAuth::Never;
                        assert_eq!(
                            direct.redis_settings().username(),
                            saved.then_some("alice")
                        );
                        assert_eq!(
                            direct.redis_settings().password(),
                            saved.then_some("secret")
                        );
                    }
                }
                drop(session);
                tokio::task::yield_now().await;
                source.join().unwrap();
                target.join().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn redirects_never_replay_watched_or_transactional_commands() {
        for (state, command) in [
            ("WATCH", "GET"),
            ("MULTI", "GET"),
            ("MULTI", "EXEC"),
            ("disabled", "GET"),
        ] {
            let (port, server) =
                server(|_| b"-MOVED 1 127.0.0.1:1\r\n".to_vec());
            let mut session = Session::open(config(port)).await.unwrap();
            session.watched = state == "WATCH";
            session.transaction = state == "MULTI";
            session.config.cluster = if state == "disabled" {
                ClusterMode::Disabled
            } else {
                ClusterMode::Enabled
            };
            let error = session
                .execute(&[command.as_bytes().to_vec()])
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                redis::ErrorKind::Server(redis::ServerErrorKind::Moved)
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn pushes_do_not_consume_the_command_reply() {
        let (port, server) = server(|_| {
            let mut replies =
                b">2\r\n+invalidate\r\n*1\r\n$3\r\nkey\r\n".repeat(128);
            replies.extend_from_slice(b"+PONG\r\n");
            replies
        });
        let mut session = Session::open(config(port)).await.unwrap();
        let mut pushes = Vec::new();
        let reply = session
            .execute_with_push(&[b"PING".to_vec()], |value| {
                pushes.push(value.clone());
            })
            .await
            .unwrap();
        assert_eq!(reply, Value::SimpleString("PONG".into()));
        assert_eq!(pushes.len(), 128);
        let mut output = Vec::new();
        session
            .config
            .output
            .write(&mut output, &pushes[0], false)
            .unwrap();
        assert_eq!(output, b"invalidate\nkey\n");
        drop(session);
        tokio::task::yield_now().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancelled_reads_preserve_fragmented_pushes_and_replies() {
        for finish_while_idle in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (release, fragment) = std::sync::mpsc::channel();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut parser = redis::Parser::new();
                let mut pings = 0;
                while let Ok(value) = parser.parse_value(&mut stream) {
                    let args: Vec<String> =
                        redis::from_redis_value(value).unwrap();
                    if args[0] == "PING" {
                        pings += 1;
                        if pings == 1 {
                            stream.write_all(b"+PONG\r\n>2\r\n+invalidate\r\n*1\r\n$3\r\nk").unwrap();
                            fragment
                                .recv_timeout(Duration::from_secs(3))
                                .unwrap();
                            stream.write_all(b"ey\r\n").unwrap();
                        } else {
                            stream.write_all(b"+SECOND\r\n").unwrap();
                        }
                    } else {
                        stream.write_all(b"+OK\r\n").unwrap();
                    }
                }
            });
            let mut session = Session::open(config(port)).await.unwrap();
            // Empty polls must not discard the next command's reply.
            assert_eq!(session.poll_message().await.unwrap(), None);
            assert_eq!(session.poll_message().await.unwrap(), None);
            let args = [b"PING".to_vec()];
            assert_eq!(
                session.execute(&args).await.unwrap(),
                Value::SimpleString("PONG".into())
            );
            assert_eq!(session.poll_message().await.unwrap(), None);
            release.send(()).unwrap();
            let mut pushes = Vec::new();
            if finish_while_idle {
                for _ in 0..10 {
                    if let Some(value) = session.poll_message().await.unwrap() {
                        pushes.push(value);
                        break;
                    }
                }
            }
            assert_eq!(
                session
                    .execute_with_push(&args, |value| pushes
                        .push(value.clone()))
                    .await
                    .unwrap(),
                Value::SimpleString("SECOND".into())
            );
            assert_eq!(
                pushes,
                vec![Value::Push {
                    kind: redis::PushKind::Invalidate,
                    data: vec![Value::Array(vec![Value::BulkString(
                        b"key".to_vec()
                    )])],
                }]
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn legacy_auth_retries_only_for_the_old_server_arity_error() {
        for first_reply in [
            "-ERR wrong number of arguments for 'auth' command\r\n",
            "-WRONGPASS invalid username-password pair\r\n",
            ":1\r\n",
        ] {
            let mut calls = 0;
            let (port, server) = server(move |args| {
                calls += 1;
                assert_eq!(args[0], "AUTH");
                if calls == 1 {
                    assert_eq!(args, &["AUTH", "default", "secret"]);
                    first_reply.as_bytes().to_vec()
                } else {
                    assert!(first_reply.starts_with("-ERR wrong number"));
                    assert_eq!(args, &["AUTH", "secret"]);
                    b"+OK\r\n".to_vec()
                }
            });
            let mut config = config(port);
            config.info = config.info.clone().set_redis_settings(
                config
                    .info
                    .redis_settings()
                    .clone()
                    .set_username("default")
                    .set_password("secret"),
            );
            let session = Session::open(config).await;
            assert_eq!(
                session.is_ok(),
                first_reply.starts_with("-ERR wrong number")
            );
            drop(session);
            tokio::task::yield_now().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn setup_timeout_closes_the_socket_and_reader() {
        use tokio::io::AsyncReadExt;
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(2),
                stream.read_to_end(&mut command),
            )
            .await
            .unwrap()
            .unwrap();
            assert_ne!(command, [] as [u8; 0]);
        });
        let mut config = config(port);
        config.name = Some("timeout-test".into());
        config.connect_timeout = Some(Duration::from_millis(50));
        let error = Session::open(config)
            .await
            .err()
            .expect("handshake must time out");
        assert!(error.is_timeout());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn exec_without_multi_keeps_the_watch_state() {
        let (port, server) = server(|args| match args[0].as_str() {
            "EXEC" => b"-ERR EXEC without MULTI\r\n".to_vec(),
            _ => b"+OK\r\n".to_vec(),
        });
        let mut session = Session::open(config(port)).await.unwrap();
        session
            .execute(&[b"WATCH".to_vec(), b"key".to_vec()])
            .await
            .unwrap();
        session.execute(&[b"EXEC".to_vec()]).await.unwrap_err();
        assert!(session.watched);
        drop(session);
        tokio::task::yield_now().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn queued_reply_outside_multi_is_not_replayed_by_exec() {
        let (port, server) = server(|args| match args[0].as_str() {
            "EVAL" | "SELECT" => b"+QUEUED\r\n".to_vec(),
            "EXEC" => b"*1\r\n+OK\r\n".to_vec(),
            _ => b"+OK\r\n".to_vec(),
        });
        let mut session = Session::open(config(port)).await.unwrap();
        let select = [b"SELECT".to_vec(), b"5".to_vec()];
        session.execute(&[b"EVAL".to_vec()]).await.unwrap();
        session.execute(&[b"MULTI".to_vec()]).await.unwrap();
        session.execute(&select).await.unwrap();
        session.execute(&[b"EXEC".to_vec()]).await.unwrap();
        assert_eq!(session.config.info.redis_settings().db(), 5);
        drop(session);
        tokio::task::yield_now().await;
        server.join().unwrap();
    }

    #[test]
    fn empty_aggregates_and_wide_indexes_format_readably() {
        assert_eq!(format_reply(&Value::Map(vec![])), ["(empty hash)"]);
        assert_eq!(format_reply(&Value::Set(vec![])), ["(empty set)"]);
        assert_eq!(format_reply(&Value::Array(vec![])), ["(empty array)"]);
        let mut values = vec![Value::Nil; 9];
        values.push(Value::Array(vec![Value::Int(1), Value::Int(2)]));
        let lines = format_reply(&Value::Array(values));
        assert_eq!(lines[9], "10) 1) (integer) 1");
        assert_eq!(lines[10], "    2) (integer) 2");
    }

    #[test]
    fn redirect_addresses_preserve_tls_and_support_ipv6_and_empty_hosts() {
        let mut addr = redis::ConnectionAddr::TcpTls {
            host: "old".into(),
            port: 1,
            insecure: false,
            tls_params: None,
        };
        redirect_address(&mut addr, "[::1]:6380").unwrap();
        assert!(
            matches!(&addr, redis::ConnectionAddr::TcpTls { host, port: 6380, insecure: false, .. } if host == "::1")
        );
        redirect_address(&mut addr, ":6381").unwrap();
        assert!(
            matches!(&addr, redis::ConnectionAddr::TcpTls { host, port: 6381, .. } if host == "::1")
        );
        assert!(redirect_address(&mut addr, "host:not-a-port").is_err());
        redirect_address(&mut addr, "::1:6382").unwrap();
        assert!(
            matches!(&addr, redis::ConnectionAddr::TcpTls { host, port: 6382, .. } if host == "::1")
        );
        for target in [
            "host:0",
            "host:65536",
            "host",
            "host :6379",
            "host:6379 extra",
        ] {
            assert!(redirect_address(&mut addr, target).is_err(), "{target}");
        }
    }
}
