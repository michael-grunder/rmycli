use std::time::Duration;

use redis::{Client, Connection, ConnectionInfo, RedisResult, Value};

use crate::input::quote;

#[derive(Clone, Debug)]
pub struct Config {
    pub info: ConnectionInfo,
    pub timeout: Duration,
    pub connect_timeout: Option<Duration>,
    pub name: Option<String>,
    pub cluster: bool,
    pub output: crate::output::Options,
    pub show_pushes: bool,
}

impl Config {
    pub fn connect(&self, auxiliary: bool) -> RedisResult<Connection> {
        let mut info = self.info.clone();
        if auxiliary {
            info.redis.protocol = redis::ProtocolVersion::RESP2;
        }
        let client = Client::open(info)?;
        let timeout = if auxiliary {
            Some(
                self.connect_timeout
                    .map_or(self.timeout, |timeout| timeout.min(self.timeout)),
            )
        } else {
            self.connect_timeout
        };
        let mut connection = if let Some(timeout) = timeout {
            client.get_connection_with_timeout(timeout)?
        } else {
            client.get_connection()?
        };
        connection.set_write_timeout(Some(self.timeout))?;
        // User commands may intentionally block. Auxiliary work has a timeout.
        connection.set_read_timeout(auxiliary.then_some(self.timeout))?;
        if !auxiliary && let Some(name) = &self.name {
            redis::cmd("CLIENT")
                .arg("SETNAME")
                .arg(name)
                .query::<()>(&mut connection)?;
        }
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
    pub connection: Connection,
    pub config: Config,
    pub transaction: bool,
    pub watched: bool,
    pub streaming: bool,
    queued: Vec<Vec<Vec<u8>>>,
}

impl Session {
    pub fn open(config: Config) -> RedisResult<Self> {
        Ok(Self {
            connection: config.connect(false)?,
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

    /// Read idle traffic without changing Redis state or losing partial frames.
    pub fn poll_message(&mut self) -> RedisResult<Option<Value>> {
        self.connection
            .set_read_timeout(Some(Duration::from_millis(100)))?;
        let result = self.connection.recv_message();
        self.connection.set_read_timeout(None)?;
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.is_timeout() => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub fn execute(&mut self, args: &[Vec<u8>]) -> RedisResult<Value> {
        self.execute_with_push(args, |_| {})
    }

    pub fn execute_with_push(
        &mut self,
        args: &[Vec<u8>],
        mut push: impl FnMut(&Value),
    ) -> RedisResult<Value> {
        for redirects in 0..=5 {
            let can_redirect = self.assistance_allowed();
            let result = self.execute_once(args, &mut push);
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
            redirect_address(&mut config.info.addr, target)?;
            let mut connection = config.connect(false)?;
            if error.kind() == redis::ErrorKind::Ask {
                redis::cmd("ASKING").query::<()>(&mut connection)?;
            }
            self.connection = connection;
            self.config = config;
        }
        unreachable!("the last redirect returns its error")
    }

    fn execute_once(
        &mut self,
        args: &[Vec<u8>],
        push: &mut impl FnMut(&Value),
    ) -> RedisResult<Value> {
        let subscription =
            [b"SUBSCRIBE".as_slice(), b"PSUBSCRIBE", b"SSUBSCRIBE"]
                .iter()
                .any(|name| args[0].eq_ignore_ascii_case(name));
        let result = self
            .connection
            .send_packed_command(&command(args).get_packed_command())
            .and_then(|()| {
                loop {
                    match self.connection.recv_response()? {
                        value @ Value::Push { .. } if !subscription => {
                            push(&value);
                        }
                        Value::ServerError(error) => break Err(error.into()),
                        value => break Ok(value),
                    }
                }
            });
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
                    self.config.info.redis.db = db;
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
            "HELLO" => {
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
                        self.config.info.redis.protocol =
                            redis::ProtocolVersion::RESP3;
                    }
                    if protocol == b"2" {
                        self.config.info.redis.protocol =
                            redis::ProtocolVersion::RESP2;
                    }
                }
                if let Some(index) =
                    args.iter().position(|a| a.eq_ignore_ascii_case(b"AUTH"))
                    && let Some(auth) = args.get(index + 1..index + 3)
                {
                    self.update_auth(auth);
                }
            }
            "RESET" => {
                self.config.info.redis.db = 0;
                self.config.info.redis.protocol = redis::ProtocolVersion::RESP2;
                self.config.name = None;
                self.config.info.redis.username = None;
                self.config.info.redis.password = None;
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
        self.config.info.redis.username = user;
        self.config.info.redis.password =
            Some(String::from_utf8_lossy(password).into_owned());
    }
}

fn redirect_address(
    addr: &mut redis::ConnectionAddr,
    target: &str,
) -> RedisResult<()> {
    use redis::{ConnectionAddr, ErrorKind, RedisError};
    let invalid = || {
        RedisError::from((
            ErrorKind::ResponseError,
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
        ConnectionAddr::Unix(_) => Err(RedisError::from((
            ErrorKind::ResponseError,
            "cannot follow a cluster redirect from a Unix socket",
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
                let args: Vec<String> =
                    redis::from_redis_value(&value).unwrap();
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
            timeout: Duration::from_secs(1),
            connect_timeout: Some(Duration::from_secs(1)),
            name: None,
            cluster: true,
            output: crate::output::Options::default(),
            show_pushes: true,
        }
    }

    #[test]
    fn moved_and_ask_follow_redirects_and_keep_command_bytes() {
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
            let mut session = Session::open(config(source_port)).unwrap();
            assert_eq!(
                session
                    .execute(&[b"GET".to_vec(), b"key".to_vec()])
                    .unwrap(),
                Value::BulkString(b"value".to_vec())
            );
            assert_eq!(
                session.config.info.addr,
                redis::ConnectionAddr::Tcp("127.0.0.1".into(), port)
            );
            drop(session);
            source.join().unwrap();
            target.join().unwrap();
        }
    }

    #[test]
    fn redirects_never_replay_watched_or_transactional_commands() {
        for (state, command) in [
            ("WATCH", "GET"),
            ("MULTI", "GET"),
            ("MULTI", "EXEC"),
            ("disabled", "GET"),
        ] {
            let (port, server) =
                server(|_| b"-MOVED 1 127.0.0.1:1\r\n".to_vec());
            let mut session = Session::open(config(port)).unwrap();
            session.watched = state == "WATCH";
            session.transaction = state == "MULTI";
            session.config.cluster = state != "disabled";
            let error =
                session.execute(&[command.as_bytes().to_vec()]).unwrap_err();
            assert_eq!(error.kind(), redis::ErrorKind::Moved);
            drop(session);
            server.join().unwrap();
        }
    }

    #[test]
    fn pushes_do_not_consume_the_command_reply() {
        let (port, server) = server(|_| {
            b">2\r\n+invalidate\r\n*1\r\n$3\r\nkey\r\n+PONG\r\n".to_vec()
        });
        let mut session = Session::open(config(port)).unwrap();
        let mut pushes = Vec::new();
        let reply = session
            .execute_with_push(&[b"PING".to_vec()], |value| {
                pushes.push(value.clone());
            })
            .unwrap();
        assert_eq!(reply, Value::SimpleString("PONG".into()));
        assert_eq!(pushes.len(), 1);
        let mut output = Vec::new();
        session
            .config
            .output
            .write(&mut output, &pushes[0], false)
            .unwrap();
        assert_eq!(output, b"invalidate\nkey\n");
        drop(session);
        server.join().unwrap();
    }

    #[test]
    fn idle_timeouts_preserve_fragmented_pushes_and_later_replies() {
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
                        redis::from_redis_value(&value).unwrap();
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
            let mut session = Session::open(config(port)).unwrap();
            // Empty polls must not discard the next command's reply.
            assert_eq!(session.poll_message().unwrap(), None);
            assert_eq!(session.poll_message().unwrap(), None);
            let args = [b"PING".to_vec()];
            assert_eq!(
                session.execute(&args).unwrap(),
                Value::SimpleString("PONG".into())
            );
            assert_eq!(session.poll_message().unwrap(), None);
            release.send(()).unwrap();
            let mut pushes = Vec::new();
            if finish_while_idle {
                for _ in 0..10 {
                    if let Some(value) = session.poll_message().unwrap() {
                        pushes.push(value);
                        break;
                    }
                }
            }
            assert_eq!(
                session
                    .execute_with_push(&args, |value| pushes
                        .push(value.clone()))
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
            server.join().unwrap();
        }
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
