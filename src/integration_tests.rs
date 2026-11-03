//! Each test owns an isolated Redis process, socket, and temporary directory.
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use redis::{Connection, ConnectionAddr, IntoConnectionInfo, Value};

use crate::{
    estimate, input,
    metadata::Catalog,
    scan::{self, Discovery, Limits},
    session::{Config, Session, command},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct RedisFixture {
    child: Child,
    path: PathBuf,
    config: Config,
}

impl RedisFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rmycli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let socket = path.join("redis.sock");
        let child = Command::new("redis-server")
            .args([
                "--port",
                "0",
                "--save",
                "",
                "--appendonly",
                "no",
                "--unixsocket",
            ])
            .arg(&socket)
            .args(["--unixsocketperm", "700", "--dir"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("install redis-server to run isolated integration tests");
        let mut info = ("127.0.0.1", 6379).into_connection_info().unwrap();
        info = info.set_addr(ConnectionAddr::Unix(socket));
        let fixture = Self {
            child,
            path,
            config: Config {
                info,
                tls: None,
                timeout: Duration::from_millis(300),
                connect_timeout: Some(Duration::from_millis(300)),
                name: None,
                cluster: false,
                output: crate::output::Options::default(),
                show_pushes: false,
            },
        };
        let start = Instant::now();
        while fixture.config.connect_auxiliary().is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "isolated Redis failed to start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    fn connection(&self) -> Connection {
        self.config.connect_auxiliary().unwrap()
    }
}
impl Drop for RedisFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn worker_command(
    workers: &crate::worker::Workers,
    command: &str,
) -> Vec<String> {
    workers
        .execution
        .send(crate::worker::Execution::Run(
            input::parse(command)
                .words
                .into_iter()
                .map(|word| word.bytes)
                .collect(),
        ))
        .unwrap();
    match workers.events.recv_timeout(Duration::from_secs(3)).unwrap() {
        crate::worker::Event::Executed { lines, .. } => lines,
        _ => panic!("expected command reply"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn alternate_keys_scans_selected_database_and_empty_batches() {
    use crate::worker::{Event, Execution, Workers};
    use std::sync::{Arc, atomic::AtomicBool};

    let fixture = RedisFixture::new();
    let mut connection = fixture.connection();
    redis::cmd("SET")
        .arg("wrong-db")
        .arg("x")
        .query::<()>(&mut connection)
        .unwrap();
    redis::cmd("SELECT")
        .arg(2)
        .query::<()>(&mut connection)
        .unwrap();
    let mut expected = Vec::new();
    let mut pipeline = redis::pipe();
    for n in 0..1000 {
        let key = format!("foo:{n}").into_bytes();
        expected.extend(
            fixture.config.output.lines(&Value::BulkString(key.clone())),
        );
        pipeline.cmd("SET").arg(key).arg("x").ignore();
    }
    let binary = b"foo:\0\xff\x1b";
    expected.extend(
        fixture
            .config
            .output
            .lines(&Value::BulkString(binary.to_vec())),
    );
    pipeline.cmd("SET").arg(binary.as_slice()).arg("x").ignore();
    pipeline.query::<()>(&mut connection).unwrap();
    expected.sort();

    let workers =
        Workers::start(Session::open(fixture.config.clone()).await.unwrap());
    assert!(matches!(
        workers.events.recv().unwrap(),
        Event::Connected { .. }
    ));
    worker_command(&workers, "SELECT 2");
    for pattern in [b"foo:*".as_slice(), b"missing:*", b"foo:\0\xff*"] {
        workers
            .execution
            .send(Execution::Scan {
                pattern: pattern.to_vec(),
                cancelled: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let mut lines = Vec::new();
        let mut batches = 0;
        loop {
            match workers.events.recv_timeout(Duration::from_secs(3)).unwrap() {
                Event::Output(batch) => {
                    batches += 1;
                    lines.extend(batch);
                }
                Event::Executed {
                    lines,
                    connected,
                    allowed,
                    ..
                } => {
                    assert_eq!(lines, Vec::<String>::new());
                    assert!(connected && allowed);
                    break;
                }
                _ => panic!("unexpected scan event"),
            }
        }
        assert!(batches > 1);
        lines.sort();
        lines.dedup();
        match pattern {
            b"foo:*" => assert_eq!(lines, expected),
            b"missing:*" => assert_eq!(lines, Vec::<String>::new()),
            _ => assert_eq!(
                lines,
                fixture
                    .config
                    .output
                    .lines(&Value::BulkString(binary.to_vec()))
            ),
        }
    }
    assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn alternate_keys_cancellation_and_state_guards_preserve_connection() {
    use crate::worker::{Event, Execution, Workers};
    use std::sync::{Arc, atomic::AtomicBool};

    let fixture = RedisFixture::new();
    let mut pipeline = redis::pipe();
    for n in 0..20_000 {
        pipeline
            .cmd("SET")
            .arg(format!("key:{n}"))
            .arg("x")
            .ignore();
    }
    pipeline.query::<()>(&mut fixture.connection()).unwrap();
    let workers =
        Workers::start(Session::open(fixture.config.clone()).await.unwrap());
    let Event::Connected { client_id, .. } = workers.events.recv().unwrap()
    else {
        panic!("expected connection");
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    workers
        .execution
        .send(Execution::Scan {
            pattern: b"*".to_vec(),
            cancelled: Arc::clone(&cancelled),
        })
        .unwrap();
    let mut keys = 0;
    loop {
        match workers.events.recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Output(lines) => {
                keys += lines.len();
                cancelled.store(true, Ordering::Relaxed);
            }
            Event::Executed {
                lines,
                client_id: after,
                connected,
                allowed,
                ..
            } => {
                assert_eq!(lines, Vec::<String>::new());
                assert!(connected && allowed);
                assert_eq!(after, client_id);
                break;
            }
            _ => panic!("unexpected scan event"),
        }
    }
    assert!(keys > 0 && keys < 20_000);
    assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
    assert_eq!(
        worker_command(&workers, "CLIENT ID"),
        [format!("(integer) {}", client_id.unwrap())]
    );

    for (start, stop) in [("MULTI", "DISCARD"), ("WATCH key:0", "UNWATCH")] {
        worker_command(&workers, start);
        workers
            .execution
            .send(Execution::Scan {
                pattern: b"*".to_vec(),
                cancelled: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let Event::Executed {
            lines,
            connected,
            allowed,
            ..
        } = workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected scan rejection");
        };
        assert!(connected && !allowed);
        assert!(lines[0].contains("unavailable"));
        assert_eq!(worker_command(&workers, stop), ["OK"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn worker_shows_server_errors_without_library_context() {
    use crate::worker::{Event, Workers};

    let fixture = RedisFixture::new();
    for protocol in
        [redis::ProtocolVersion::RESP2, redis::ProtocolVersion::RESP3]
    {
        let mut config = fixture.config.clone();
        config.info = config.info.clone().set_redis_settings(
            config.info.redis_settings().clone().set_protocol(protocol),
        );
        let workers = Workers::start(Session::open(config).await.unwrap());
        assert!(matches!(
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Connected { .. }
        ));
        assert_eq!(
            worker_command(&workers, "SET d sd d d d d"),
            ["(error) syntax error"]
        );
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn worker_displays_idle_invalidations_and_preserves_session_state() {
    use crate::worker::{Event, Execution, Workers};

    let fixture = RedisFixture::new();
    let mut writer = fixture.connection();
    for show_pushes in [true, false] {
        let mut config = fixture.config.clone();
        config.show_pushes = show_pushes;
        let workers = Workers::start(Session::open(config).await.unwrap());
        let Event::Connected { client_id, .. } =
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected connected event");
        };
        // Exercise a protocol change on the existing connection too.
        worker_command(&workers, "HELLO 3");
        worker_command(&workers, "SET foo bar");
        worker_command(&workers, "CLIENT TRACKING ON");
        worker_command(&workers, "GET foo");
        worker_command(&workers, "WATCH unrelated");
        redis::cmd("SET")
            .arg("foo")
            .arg("updated")
            .query::<()>(&mut writer)
            .unwrap();
        if show_pushes {
            let Event::Push(lines) =
                workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected invalidation without submitting a command");
            };
            assert_eq!(lines, ["-> invalidate: 'foo'"]);
        } else {
            assert!(
                workers
                    .events
                    .recv_timeout(Duration::from_millis(350))
                    .is_err()
            );
        }
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
        worker_command(&workers, "MULTI");
        assert!(
            workers
                .events
                .recv_timeout(Duration::from_millis(350))
                .is_err()
        );
        assert_eq!(worker_command(&workers, "PING"), ["QUEUED"]);
        assert_eq!(worker_command(&workers, "EXEC"), ["1) PONG"]);
        if show_pushes {
            redis::cmd("FLUSHDB").query::<()>(&mut writer).unwrap();
            let Event::Push(lines) =
                workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected cache-wide invalidation");
            };
            assert_eq!(lines, ["-> invalidate: (nil)"]);
        }
        // Disconnection is noticed while idle, and reconnect restores reading.
        redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id.unwrap())
            .query::<i64>(&mut writer)
            .unwrap();
        assert!(matches!(
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Failed(_)
        ));
        workers.execution.send(Execution::Reconnect).unwrap();
        assert!(matches!(
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Connected { .. }
        ));
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
    }
}

#[test]
fn live_metadata_typed_scan_binary_keys_and_estimates() {
    let fixture = RedisFixture::new();
    let mut connection = fixture.connection();
    let catalog = Catalog::load(&mut connection);
    assert!(catalog.commands["LRANGE"].live_docs);
    assert!(catalog.commands.contains_key("XGROUP CREATE"));
    let help = catalog.help(&input::context("LRANGE k ", 9));
    assert!(help.iter().any(|line| line.contains("start")));
    for key in [b"list:a".as_slice(), b"list: b\xff", b"list:[x]*"] {
        redis::cmd("RPUSH")
            .arg(key)
            .arg(&["a", "b", "c"])
            .query::<i64>(&mut connection)
            .unwrap();
    }
    redis::cmd("SET")
        .arg("list:string")
        .arg("v")
        .query::<()>(&mut connection)
        .unwrap();
    let state = scan::discover(
        &mut connection,
        b"list:",
        Some("list"),
        Discovery::default(),
        &Limits::default(),
        || false,
    );
    assert!(state.complete);
    assert_eq!(state.keys.len(), 3);
    assert!(state.keys.contains(b"list: b\xff".as_slice()));
    let state = scan::discover(
        &mut connection,
        b"list:[x]*",
        Some("list"),
        Discovery::default(),
        &Limits::default(),
        || false,
    );
    assert_eq!(state.keys.len(), 1);
    let args = input::parse("LRANGE list:a 0 -1")
        .words
        .into_iter()
        .map(|w| w.bytes)
        .collect::<Vec<_>>();
    assert_eq!(estimate::estimate(&mut connection, &args), Some(3));
    let unknown = input::parse("LRANGE list:string 0 -1")
        .words
        .into_iter()
        .map(|w| w.bytes)
        .collect::<Vec<_>>();
    assert_eq!(estimate::estimate(&mut connection, &unknown), None);
}

async fn execute(session: &mut Session, line: &str) -> Value {
    let args = input::parse(line)
        .words
        .into_iter()
        .map(|w| w.bytes)
        .collect::<Vec<_>>();
    session.execute(&args).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn session_database_transaction_and_background_isolation() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).await.unwrap();
    execute(&mut session, "SELECT 2").await;
    execute(&mut session, "RPUSH list:db2 x").await;
    assert_eq!(session.config.info.redis_settings().db(), 2);
    let mut auxiliary = session.config.connect_auxiliary().unwrap();
    execute(&mut session, "MULTI").await;
    assert!(!session.assistance_allowed());
    execute(&mut session, "SELECT 3").await;
    execute(&mut session, "SET k v").await;
    let scan = scan::discover(
        &mut auxiliary,
        b"list:",
        Some("list"),
        Discovery::default(),
        &Limits::default(),
        || false,
    );
    assert_eq!(scan.keys.len(), 1);
    let Value::Array(replies) = execute(&mut session, "EXEC").await else {
        panic!("expected EXEC array")
    };
    assert_eq!(
        replies.len(),
        2,
        "auxiliary SCAN must not enter transaction"
    );
    assert_eq!(session.config.info.redis_settings().db(), 3);
    assert!(session.assistance_allowed());
    execute(&mut session, "WATCH k").await;
    assert!(!session.assistance_allowed());
    execute(&mut session, "UNWATCH").await;
    assert!(session.assistance_allowed());
    execute(&mut session, "MULTI").await;
    execute(&mut session, "SELECT 4").await;
    execute(&mut session, "DISCARD").await;
    assert_eq!(session.config.info.redis_settings().db(), 3);
}

#[test]
fn restricted_introspection_keeps_manual_execution_available() {
    let fixture = RedisFixture::new();
    let mut connection = fixture.connection();
    redis::cmd("ACL")
        .arg(&[
            "SETUSER", "limited", "on", "nopass", "~*", "+@all", "-command",
        ])
        .query::<()>(&mut connection)
        .unwrap();
    let mut config = fixture.config.clone();
    config.info = config.info.clone().set_redis_settings(
        config.info.redis_settings().clone().set_username("limited"),
    );
    config.info = config.info.clone().set_redis_settings(
        config
            .info
            .redis_settings()
            .clone()
            .set_password(String::new()),
    );
    let mut connection = config.connect_auxiliary().unwrap();
    let catalog = Catalog::load(&mut connection);
    assert_ne!(catalog.notices, [] as [std::string::String; 0]);
    assert!(!catalog.commands["LRANGE"].live_docs);
    assert!(
        command(&[b"PING".to_vec()])
            .query::<String>(&mut connection)
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn exec_runtime_errors_preserve_successful_database_changes() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).await.unwrap();
    execute(&mut session, "MULTI").await;
    execute(&mut session, "SELECT 2").await;
    execute(&mut session, "SET key value").await;
    execute(&mut session, "LPUSH key wrong-type").await;
    let Value::Array(replies) = execute(&mut session, "EXEC").await else {
        panic!("EXEC must preserve individual replies")
    };
    assert_eq!(replies.len(), 3);
    assert!(matches!(replies[2], Value::ServerError(_)));
    assert_eq!(session.config.info.redis_settings().db(), 2);
    assert!(session.assistance_allowed());
}

#[test]
fn getkeys_fallback_does_not_execute_and_distinguishes_equal_values() {
    let fixture = RedisFixture::new();
    let mut connection = fixture.connection();
    let args = input::parse("MSET same same")
        .words
        .into_iter()
        .map(|w| w.bytes)
        .collect::<Vec<_>>();
    assert_eq!(
        crate::metadata::probe_key_position(&mut connection, &args, 1),
        Some(true)
    );
    assert_eq!(
        crate::metadata::probe_key_position(&mut connection, &args, 2),
        Some(false)
    );
    assert_eq!(
        redis::cmd("DBSIZE").query::<i64>(&mut connection).unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn authentication_changes_and_resp3_subscription_acknowledgment() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).await.unwrap();
    execute(&mut session, "ACL SETUSER alice on >secret ~* &* +@all").await;
    execute(&mut session, "AUTH alice secret").await;
    assert_eq!(
        session.config.info.redis_settings().username(),
        Some("alice")
    );
    assert!(session.config.connect_auxiliary().is_ok());
    execute(&mut session, "HELLO 3").await;
    let reply = execute(&mut session, "SUBSCRIBE updates").await;
    assert!(matches!(reply, Value::Push { .. }));
    assert!(!session.assistance_allowed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn subscriptions_keep_the_session_and_all_replies() {
    let fixture = RedisFixture::new();
    let mut publisher = fixture.connection();
    for protocol in [2, 3] {
        let mut session = Session::open(fixture.config.clone()).await.unwrap();
        execute(&mut session, "CLIENT SETNAME streaming-test").await;
        execute(&mut session, "SELECT 2").await;
        let Value::Int(id) = execute(&mut session, "CLIENT ID").await else {
            panic!("expected client id");
        };
        execute(&mut session, &format!("HELLO {protocol}")).await;
        let first = execute(&mut session, "SUBSCRIBE first second").await;
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            session.connection.recv_response(),
        )
        .await
        .unwrap()
        .unwrap();
        for (reply, channel, count) in
            [(first, b"first".as_slice(), 1), (second, b"second", 2)]
        {
            let data = match reply {
                Value::Array(values) => {
                    assert_eq!(protocol, 2);
                    values
                }
                Value::Push {
                    kind: redis::PushKind::Subscribe,
                    data,
                } => {
                    assert_eq!(protocol, 3);
                    data
                }
                other => panic!("unexpected subscription reply: {other:?}"),
            };
            assert_eq!(
                &data[data.len() - 2..],
                &[Value::BulkString(channel.to_vec()), Value::Int(count)]
            );
        }
        let clients: String = redis::cmd("CLIENT")
            .arg("LIST")
            .query(&mut publisher)
            .unwrap();
        let client = clients
            .lines()
            .find(|line| line.starts_with(&format!("id={id} ")))
            .unwrap();
        assert!(
            client.contains("name=streaming-test ")
                && client.contains("db=2 ")
                && client.contains("sub=2 ")
        );
        redis::cmd("PUBLISH")
            .arg("second")
            .arg(b"a\x00\xff")
            .query::<i64>(&mut publisher)
            .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            session.connection.recv_response(),
        )
        .await
        .unwrap()
        .unwrap();
        let data = match reply {
            Value::Array(data) | Value::Push { data, .. } => data,
            other => panic!("unexpected message: {other:?}"),
        };
        assert_eq!(
            data.last(),
            Some(&Value::BulkString(b"a\x00\xff".to_vec()))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn monitor_keeps_the_session() {
    let fixture = RedisFixture::new();
    let mut publisher = fixture.connection();
    let mut session = Session::open(fixture.config.clone()).await.unwrap();
    execute(&mut session, "CLIENT SETNAME monitor-test").await;
    let Value::Int(id) = execute(&mut session, "CLIENT ID").await else {
        panic!("expected client id");
    };
    assert_eq!(execute(&mut session, "MONITOR").await, Value::Okay);
    redis::cmd("ECHO")
        .arg("monitor-marker")
        .query::<String>(&mut publisher)
        .unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        session.connection.recv_response(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(reply, Value::SimpleString(line) if line.contains("monitor-marker"))
    );
    let clients: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query(&mut publisher)
        .unwrap();
    let client = clients
        .lines()
        .find(|line| line.starts_with(&format!("id={id} ")))
        .unwrap();
    assert!(
        client.contains("name=monitor-test ")
            && client.split_whitespace().any(|field| field
                .strip_prefix("flags=")
                .is_some_and(|flags| flags.contains('O'))),
        "{client}"
    );
}

#[test]
fn tls_has_a_crypto_provider() {
    let _ = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}
