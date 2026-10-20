use std::time::Duration;

use redis::{Client, Connection, ConnectionInfo, RedisResult, Value};

use crate::input::quote;

#[derive(Clone, Debug)]
pub struct Config {
    pub info: ConnectionInfo,
    pub timeout: Duration,
}

impl Config {
    pub fn connect(&self, auxiliary: bool) -> RedisResult<Connection> {
        let client = Client::open(self.info.clone())?;
        let connection = client.get_connection_with_timeout(self.timeout)?;
        connection.set_write_timeout(Some(self.timeout))?;
        // User commands may intentionally block. Auxiliary work has a timeout.
        connection.set_read_timeout(auxiliary.then_some(self.timeout))?;
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

    pub fn execute(&mut self, args: &[Vec<u8>]) -> RedisResult<Value> {
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
                        Value::Push { .. } if !subscription => {}
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
            "HELLO" => {
                // Auxiliary connections keep RESP2 independently of the main protocol.
                if let Some(index) =
                    args.iter().position(|a| a.eq_ignore_ascii_case(b"AUTH"))
                    && let Some(auth) = args.get(index + 1..index + 3)
                {
                    self.update_auth(auth);
                }
            }
            "RESET" => {
                self.config.info.redis.db = 0;
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
            quote(format!("{error:?}").as_bytes())
        )],
    }
}
