use std::time::Duration;

use redis::{Client, Connection, ConnectionInfo, RedisResult, Value};

use crate::input::quote;

#[derive(Clone, Debug)]
pub struct Config {
    pub info: ConnectionInfo,
    pub tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    pub timeout: Duration,
    pub connect_timeout: Option<Duration>,
    pub name: Option<String>,
    pub cluster: bool,
    pub output: crate::output::Options,
    pub show_pushes: bool,
}

impl Config {
    pub fn connect_auxiliary(&self) -> RedisResult<Connection> {
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
    queued: Vec<Vec<Vec<u8>>>,
}

impl Session {
    pub async fn open(config: Config) -> RedisResult<Self> {
        Ok(Self {
            connection: crate::transport::Connection::open(&config).await?,
            config,
            transaction: false,
            watched: false,
            streaming: false,
            queued: Vec::new(),
        })
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
            let Err(error) = &result else {
                return result;
            };
            if !self.config.cluster || !can_redirect || redirects == 5 {
                return result;
            }
            let Some((target, _)) = error.redirect_node() else {
                return result;
            };
            let mut config = self.config.clone();
            let mut addr = config.info.addr().clone();
            redirect_address(&mut addr, target)?;
            config.info = config.info.set_addr(addr);
            let mut connection =
                crate::transport::Connection::open(&config).await?;
            if error.kind()
                == redis::ErrorKind::Server(redis::ServerErrorKind::Ask)
            {
                connection.query(&redis::cmd("ASKING")).await?;
            }
            self.connection = connection;
            self.config = config;
        }
        unreachable!("the last redirect returns its error")
    }

    async fn execute_once(
        &mut self,
        args: &[Vec<u8>],
        push: &mut impl FnMut(&Value),
    ) -> RedisResult<Value> {
        let subscription =
            [b"SUBSCRIBE".as_slice(), b"PSUBSCRIBE", b"SSUBSCRIBE"]
                .iter()
                .any(|name| args[0].eq_ignore_ascii_case(name));
        let result: RedisResult<Value> = async {
            self.connection
                .send_packed_command(&command(args).get_packed_command())
                .await?;
            loop {
                match self.connection.recv_response().await? {
                    value @ Value::Push { .. } if !subscription => push(&value),
                    Value::ServerError(error) => break Err(error.into()),
                    value => break Ok(value),
                }
            }
        }
        .await;
        match &result {
            Ok(value) => self.observe(args, value),
            Err(error)
                if args[0].eq_ignore_ascii_case(b"EXEC")
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
        if matches!(reply, Value::SimpleString(s) if s == "QUEUED") {
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
                    self.config.info =
                        self.config.info.clone().set_redis_settings(
                            self.config
                                .info
                                .redis_settings()
                                .clone()
                                .set_db(db),
                        );
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
                self.config.info =
                    self.config.info.clone().set_redis_settings(
                        redis::RedisConnectionInfo::default(),
                    );
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
        if let Some(index) = args
            .iter()
            .position(|arg| arg.eq_ignore_ascii_case(b"SETNAME"))
        {
            self.config.name = args
                .get(index + 1)
                .map(|arg| String::from_utf8_lossy(arg).into_owned());
        }
        if let Some(protocol) = args.get(1) {
            if protocol == b"3" {
                self.config.info = self.config.info.clone().set_redis_settings(
                    self.config
                        .info
                        .redis_settings()
                        .clone()
                        .set_protocol(redis::ProtocolVersion::RESP3),
                );
            }
            if protocol == b"2" {
                self.config.info = self.config.info.clone().set_redis_settings(
                    self.config
                        .info
                        .redis_settings()
                        .clone()
                        .set_protocol(redis::ProtocolVersion::RESP2),
                );
            }
        }
        if let Some(index) =
            args.iter().position(|a| a.eq_ignore_ascii_case(b"AUTH"))
            && let Some(auth) = args.get(index + 1..index + 3)
        {
            self.update_auth(auth);
        }
    }

    fn update_auth(&mut self, auth: &[Vec<u8>]) {
        // Redis connection credentials are UTF-8. Binary AUTH still executes,
        // but auxiliary authentication will fail gracefully for such accounts.
        let (user, password) = match auth {
            [password] => (None, password),
            [user, password] => {
                (Some(String::from_utf8_lossy(user).into_owned()), password)
            }
            _ => return,
        };
        let mut settings = redis::RedisConnectionInfo::default()
            .set_db(self.config.info.redis_settings().db())
            .set_protocol(self.config.info.redis_settings().protocol())
            .set_password(String::from_utf8_lossy(password));
        if let Some(user) = user {
            settings = settings.set_username(user);
        }
        self.config.info =
            self.config.info.clone().set_redis_settings(settings);
    }
}

fn redirect_address(
    addr: &mut redis::ConnectionAddr,
    target: &str,
) -> RedisResult<()> {
    use redis::{ConnectionAddr, ErrorKind, RedisError};
    let invalid = || {
        RedisError::from((
            ErrorKind::Client,
            "invalid cluster redirect address",
        ))
    };
    let (host, port) = target.rsplit_once(':').ok_or_else(invalid)?;
    let port = port.parse::<u16>().map_err(|_| invalid())?;
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
            "cannot follow a cluster redirect from this transport",
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
                    format_reply(value).into_iter().enumerate().map(
                        move |(line, text)| {
                            if line == 0 {
                                format!("{}) {text}", index + 1)
                            } else {
                                format!("   {text}")
                            }
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
                let response = if args[0] == "CLIENT" {
                    b"+OK\r\n".to_vec()
                } else {
                    reply(&args)
                };
                reader.get_mut().write_all(&response).unwrap();
            }
        });
        (port, thread)
    }

    fn config(port: u16) -> Config {
        use redis::IntoConnectionInfo;
        Config {
            info: ("127.0.0.1", port).into_connection_info().unwrap(),
            tls: None,
            timeout: Duration::from_secs(1),
            connect_timeout: Some(Duration::from_secs(1)),
            name: None,
            cluster: true,
            output: crate::output::Options::default(),
            show_pushes: true,
        }
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
            let (source_port, source) = server(move |_| {
                format!("-{kind} 1 127.0.0.1:{port}\r\n").into_bytes()
            });
            let mut session = Session::open(config(source_port)).await.unwrap();
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
            session.config.cluster = state != "disabled";
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
    }
}
