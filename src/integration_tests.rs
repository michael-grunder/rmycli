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
                auth_store: None,
                binary_auth: false,
                info,
                tls: None,
                timeout: Duration::from_millis(300),
                connect_timeout: Some(Duration::from_millis(300)),
                name: None,
                cluster: crate::session::ClusterMode::Auto,
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
    use crate::worker::{Event, Workers};

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
        worker_command(&workers, "MULTI");
        assert_eq!(worker_command(&workers, "INCR abandoned"), ["QUEUED"]);
        // Idle disconnection reconnects without a failure event or command replay.
        redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id.unwrap())
            .query::<i64>(&mut writer)
            .unwrap();
        assert!(matches!(
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Connected { .. }
        ));
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
        assert_eq!(worker_command(&workers, "GET abandoned"), ["(nil)"]);
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

#[test]
fn size_hints_use_live_key_specs_and_typed_counts() {
    let fixture = RedisFixture::new();
    let mut connection = fixture.connection();
    let catalog = Catalog::load(&mut connection);
    redis::cmd("SADD")
        .arg("s1")
        .arg(&["a", "b", "c"])
        .query::<i64>(&mut connection)
        .unwrap();
    redis::cmd("SADD")
        .arg("s2")
        .arg(&["c", "d"])
        .query::<i64>(&mut connection)
        .unwrap();
    redis::cmd("SET")
        .arg("str")
        .arg("hello")
        .exec(&mut connection)
        .unwrap();
    for (line, expected) in [
        ("SMEMBERS s1", Some("[3 members]")),
        ("SUNION s1 s2 missing", Some("[≤5 members]")),
        ("SINTER s1 s2", Some("[≤2 members]")),
        ("GET str", Some("[5 bytes]")),
        ("SMEMBERS str", None),
    ] {
        let args: Vec<_> = input::parse(line)
            .words
            .into_iter()
            .map(|w| w.bytes)
            .collect();
        let (command, _) = catalog.resolve(&args).unwrap();
        let keys = command.accessed_keys(&args).unwrap();
        let hint =
            estimate::size_hint(&mut connection, &command.name, &args, &keys);
        assert_eq!(hint.as_deref(), expected, "{line}");
    }
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
async fn unsubscribe_acknowledgments_stay_aligned_with_later_replies() {
    let fixture = RedisFixture::new();
    for protocol in ["HELLO 2", "HELLO 3"] {
        let mut session = Session::open(fixture.config.clone()).await.unwrap();
        execute(&mut session, protocol).await;
        for line in [
            "UNSUBSCRIBE",
            "PUNSUBSCRIBE",
            "SUNSUBSCRIBE",
            "UNSUBSCRIBE a b c",
        ] {
            // A RESP3 acknowledgment is a push, but it is still this reply.
            tokio::time::timeout(
                Duration::from_secs(2),
                execute(&mut session, line),
            )
            .await
            .expect("unsubscribe must not wait for another reply");
            assert_eq!(
                execute(&mut session, "ECHO next").await,
                Value::BulkString(b"next".to_vec()),
                "{protocol}: {line}"
            );
        }
        assert!(session.assistance_allowed());
    }
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
        let mut replies = Vec::new();
        let last = session
            .execute_with_push(
                &[b"SUBSCRIBE".to_vec(), b"first".to_vec(), b"second".to_vec()],
                |reply| replies.push(reply.clone()),
            )
            .await
            .unwrap();
        replies.push(last);
        assert_eq!(replies.len(), 2);
        for (reply, channel, count) in replies
            .into_iter()
            .zip([b"first".as_slice(), b"second"])
            .zip(1..)
            .map(|((reply, channel), count)| (reply, channel, count))
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
async fn subscribed_commands_and_messages_keep_their_replies() {
    let fixture = RedisFixture::new();
    let mut publisher = fixture.connection();
    for protocol in [2, 3] {
        for (subscribe, unsubscribe, publish) in [
            ("SUBSCRIBE", "UNSUBSCRIBE", "PUBLISH"),
            ("PSUBSCRIBE", "PUNSUBSCRIBE", "PUBLISH"),
            ("SSUBSCRIBE", "SUNSUBSCRIBE", "SPUBLISH"),
        ] {
            let mut session =
                Session::open(fixture.config.clone()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                execute(&mut session, &format!("HELLO {protocol}")).await;
                execute(
                    &mut session,
                    &format!(r"{subscribe} a b a '\x00\xff'"),
                )
                .await;
                assert!(session.subscribed());
                assert!(!session.streaming);
                assert!(!session.assistance_allowed());
                redis::cmd(publish)
                    .arg("a")
                    .arg("before-subscribe")
                    .query::<i64>(&mut publisher)
                    .unwrap();
                let mut pushes = Vec::new();
                session
                    .execute_with_push(
                        &[
                            subscribe.as_bytes().to_vec(),
                            b"c".to_vec(),
                            b"d".to_vec(),
                        ],
                        |reply| pushes.push(reply.clone()),
                    )
                    .await
                    .unwrap();
                assert_eq!(pushes.len(), 2, "message and first acknowledgment");
                let data = match &pushes[0] {
                    Value::Array(data) | Value::Push { data, .. } => data,
                    other => panic!("unexpected message: {other:?}"),
                };
                assert_eq!(
                    data.last(),
                    Some(&Value::BulkString(b"before-subscribe".to_vec()))
                );
                let reply = session
                    .execute(&[b"GET".to_vec(), b"missing".to_vec()])
                    .await;
                if protocol == 2 {
                    assert_eq!(reply.unwrap_err().code(), Some("ERR"));
                } else {
                    assert_eq!(reply.unwrap(), Value::Nil);
                }
                let pong = execute(&mut session, "PING marker").await;
                if protocol == 2 {
                    assert_eq!(
                        pong,
                        Value::Array(vec![
                            Value::BulkString(b"pong".to_vec()),
                            Value::BulkString(b"marker".to_vec()),
                        ])
                    );
                } else {
                    assert_eq!(pong, Value::BulkString(b"marker".to_vec()));
                }
                execute(&mut session, &format!("{unsubscribe} a absent a"))
                    .await;
                assert!(session.subscribed());
                execute(&mut session, unsubscribe).await;
                assert!(!session.subscribed());
                assert!(session.assistance_allowed());
                assert_eq!(
                    execute(&mut session, "ECHO next").await,
                    Value::BulkString(b"next".to_vec())
                );
            })
            .await
            .expect("subscription acknowledgments must not hang");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unsubscribe_preserves_other_subscription_types_and_reset_clears_them()
{
    let fixture = RedisFixture::new();
    for protocol in [2, 3] {
        let mut session = Session::open(fixture.config.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            execute(&mut session, &format!("HELLO {protocol}")).await;
            for final_command in ["SUNSUBSCRIBE", "RESET"] {
                for line in
                    ["SUBSCRIBE a b", "PSUBSCRIBE a* b*", "SSUBSCRIBE c d"]
                {
                    execute(&mut session, line).await;
                }
                for line in ["UNSUBSCRIBE", "UNSUBSCRIBE", "PUNSUBSCRIBE"] {
                    execute(&mut session, line).await;
                    assert!(session.subscribed());
                    assert!(!session.assistance_allowed());
                }
                execute(&mut session, final_command).await;
                assert!(!session.subscribed());
                assert!(session.assistance_allowed());
                assert_eq!(
                    execute(&mut session, "PING").await,
                    Value::SimpleString("PONG".into())
                );
            }
            assert!(session.execute(&[b"SUBSCRIBE".to_vec()]).await.is_err());
            assert!(!session.subscribed());
        })
        .await
        .expect("mixed subscriptions must not hang");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn queued_subscriptions_drain_all_acknowledgments() {
    let fixture = RedisFixture::new();
    for protocol in [2, 3] {
        let mut session = Session::open(fixture.config.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            execute(&mut session, &format!("HELLO {protocol}")).await;
            execute(&mut session, "MULTI").await;
            for line in [
                "SUBSCRIBE a b a",
                "PSUBSCRIBE p q",
                "UNSUBSCRIBE",
                "PUNSUBSCRIBE",
                "SUBSCRIBE c d",
            ] {
                assert_eq!(
                    execute(&mut session, line).await,
                    Value::SimpleString("QUEUED".into())
                );
                assert!(!session.subscribed());
            }
            let Value::Array(replies) = execute(&mut session, "EXEC").await
            else {
                panic!("expected EXEC replies");
            };
            assert_eq!(replies.len(), 5);
            assert!(session.subscribed());
            assert!(!session.transaction);
            execute(&mut session, "UNSUBSCRIBE").await;
            assert!(session.assistance_allowed());
            assert_eq!(
                execute(&mut session, "ECHO aligned").await,
                Value::BulkString(b"aligned".to_vec())
            );
            if protocol == 3 {
                execute(&mut session, "SUBSCRIBE a b").await;
                execute(&mut session, "MULTI").await;
                execute(&mut session, "UNSUBSCRIBE").await;
                execute(&mut session, "GET absent").await;
                execute(&mut session, "PSUBSCRIBE x y").await;
                let Value::Array(replies) = execute(&mut session, "EXEC").await
                else {
                    panic!("expected subscribed EXEC replies");
                };
                assert_eq!(replies.len(), 3);
                assert_eq!(replies[1], Value::Nil);
                assert!(session.subscribed());
                execute(&mut session, "PUNSUBSCRIBE").await;
                assert!(session.assistance_allowed());
                assert_eq!(
                    execute(&mut session, "PING").await,
                    Value::SimpleString("PONG".into())
                );
            }
        })
        .await
        .expect("queued subscriptions must not hang");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn monitor_output_and_cancellation_reconnect_quietly() {
    use crate::worker::{Event, Workers};

    let fixture = RedisFixture::new();
    let mut publisher = fixture.connection();
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg("default")
        .arg("-client|id")
        .arg("-client|kill")
        .query::<()>(&mut publisher)
        .unwrap();
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
            Event::Connected {
                client_id: None,
                ..
            }
        ));
        assert_eq!(worker_command(&workers, "SELECT 2"), ["OK"]);
        assert_eq!(worker_command(&workers, "MONITOR"), ["OK"]);
        redis::cmd("ECHO")
            .arg(b"monitor \"quote\"\\\n\xff".as_slice())
            .query::<Vec<u8>>(&mut publisher)
            .unwrap();
        let Event::Output(lines) =
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected monitor output");
        };
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with(|c: char| c.is_ascii_digit()),
            "{lines:?}"
        );
        assert!(
            lines[0].ends_with(r#""ECHO" "monitor \"quote\"\\\n\xff""#),
            "{lines:?}"
        );
        workers.cancel.notify_one();
        let Event::Connected { config, .. } =
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!(
                "cancellation must reconnect without errors or handshake output"
            );
        };
        assert_eq!(config.info.redis_settings().db(), 2);
        assert_eq!(config.info.redis_settings().protocol(), protocol);
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn interrupted_commands_reconnect_without_replay() {
    use crate::worker::{Event, Execution, Workers};

    let fixture = RedisFixture::new();
    let mut writer = fixture.connection();
    for cancel in [true, false] {
        let workers = Workers::start(
            Session::open(fixture.config.clone()).await.unwrap(),
        );
        let Event::Connected { client_id, .. } =
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected connection");
        };
        workers
            .execution
            .send(Execution::Run(vec![
                b"BLPOP".to_vec(),
                b"pending".to_vec(),
                b"0".to_vec(),
            ]))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let clients: String =
                redis::cmd("CLIENT").arg("LIST").query(&mut writer).unwrap();
            if clients.lines().any(|line| {
                line.starts_with(&format!("id={} ", client_id.unwrap()))
                    && line.contains("cmd=blpop")
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "BLPOP did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if cancel {
            workers.cancel.notify_one();
        } else {
            redis::cmd("CLIENT")
                .arg("KILL")
                .arg("ID")
                .arg(client_id.unwrap())
                .query::<i64>(&mut writer)
                .unwrap();
            assert!(matches!(
                workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
                Event::Executed {
                    connected: false,
                    ..
                }
            ));
        }
        assert!(matches!(
            workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Connected { .. }
        ));
        redis::cmd("RPUSH")
            .arg("pending")
            .arg("untouched")
            .query::<i64>(&mut writer)
            .unwrap();
        assert_eq!(worker_command(&workers, "PING"), ["PONG"]);
        let item: String = redis::cmd("LPOP")
            .arg("pending")
            .query(&mut writer)
            .unwrap();
        assert_eq!(item, "untouched");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn failed_reconnect_retries_only_on_new_requests() {
    use crate::worker::{Event, Execution, Workers};

    let fixture = RedisFixture::new();
    let mut writer = fixture.connection();
    let workers =
        Workers::start(Session::open(fixture.config.clone()).await.unwrap());
    let Event::Connected { client_id, .. } =
        workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
    else {
        panic!("expected connection");
    };
    let socket = fixture.path.join("redis.sock");
    let unavailable = fixture.path.join("unavailable.sock");
    std::fs::rename(&socket, &unavailable).unwrap();
    redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(client_id.unwrap())
        .query::<i64>(&mut writer)
        .unwrap();
    assert!(
        matches!(workers.events.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Failed(message) if message.contains("Reconnect failed"))
    );
    assert!(
        workers
            .events
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    workers
        .execution
        .send(Execution::Run(vec![b"INCR".to_vec(), b"counter".to_vec()]))
        .unwrap();
    assert!(matches!(
        workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
        Event::Failed(_)
    ));
    std::fs::rename(&unavailable, &socket).unwrap();
    workers
        .execution
        .send(Execution::Run(vec![b"PING".to_vec()]))
        .unwrap();
    assert!(matches!(
        workers.events.recv_timeout(Duration::from_secs(3)).unwrap(),
        Event::Connected { .. }
    ));
    assert!(
        matches!(workers.events.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Executed { lines, .. } if lines == ["PONG"])
    );
    assert_eq!(worker_command(&workers, "GET counter"), ["(nil)"]);
    assert_eq!(worker_command(&workers, "INCR counter"), ["(integer) 1"]);
}

#[test]
fn tls_has_a_crypto_provider() {
    let _ = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn authentication_reloads_metadata_and_defers_refresh_during_watch() {
    use crate::worker::{Event, Workers};
    let fixture = RedisFixture::new();
    let mut admin = fixture.connection();
    redis::cmd("CONFIG")
        .arg(&["SET", "requirepass", "secret"])
        .query::<()>(&mut admin)
        .unwrap();
    redis::cmd("ACL")
        .arg(&[
            "SETUSER", "limited", "on", ">secret", "~*", "+@all", "-command",
        ])
        .query::<()>(&mut admin)
        .unwrap();
    let session = Session::open(fixture.config.clone()).await.unwrap();
    let workers = Workers::start(session);
    let Event::Connected { catalog, .. } =
        workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
    else {
        panic!("expected startup catalog")
    };
    assert_eq!(catalog.notices.len(), 1);
    assert!(catalog.notices[0].contains("Authentication required"));
    assert!(!catalog.commands["GET"].live_docs);
    assert!(
        worker_command(&workers, "AUTH wrong")[0]
            .contains("invalid username-password")
    );
    assert!(workers.events.try_recv().is_err());
    for command in ["AUTH secret", "HELLO 3 AUTH default secret"] {
        worker_command(&workers, command);
        let Event::Connected {
            catalog,
            client_id,
            config,
        } = workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected refreshed catalog")
        };
        assert!(catalog.notices.is_empty(), "{:?}", catalog.notices);
        assert!(catalog.commands["GET"].live_docs);
        assert!(client_id.is_some());
        assert_eq!(
            redis::cmd("PING")
                .query::<String>(&mut config.connect_auxiliary().unwrap())
                .unwrap(),
            "PONG"
        );
    }
    worker_command(&workers, "WATCH key");
    worker_command(&workers, "AUTH limited secret");
    worker_command(&workers, "PING");
    worker_command(&workers, "UNWATCH");
    let Event::Connected { catalog, .. } =
        workers.events.recv_timeout(Duration::from_secs(3)).unwrap()
    else {
        panic!("expected deferred catalog")
    };
    assert!(!catalog.commands["GET"].live_docs);
    assert!(
        catalog
            .notices
            .iter()
            .any(|notice| notice.contains("COMMAND unavailable"))
    );
    assert!(
        !catalog
            .notices
            .iter()
            .any(|notice| notice.contains("Authentication required"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn only_successful_authentication_is_saved_including_exec_and_hello() {
    use crate::auth::{SaveAuth, Store};
    let fixture = RedisFixture::new();
    let mut admin = fixture.connection();
    redis::cmd("ACL")
        .arg(&["SETUSER", "alice", "on", ">AUTH", "~*", "+@all"])
        .query::<()>(&mut admin)
        .unwrap();
    let path = fixture.path.join("auth.toml");
    let mut config = fixture.config.clone();
    config.auth_store = Some(Store::plaintext(path.clone(), SaveAuth::Local));
    let mut session = Session::open(config).await.unwrap();
    assert!(!path.exists());
    let args = input::parse("AUTH alice wrong")
        .words
        .into_iter()
        .map(|word| word.bytes)
        .collect::<Vec<_>>();
    assert!(session.execute(&args).await.is_err());
    assert_eq!(session.auth_revision, 0);
    assert!(!path.exists());
    execute(&mut session, "MULTI").await;
    execute(&mut session, "AUTH alice AUTH").await;
    assert!(!path.exists());
    execute(&mut session, "DISCARD").await;
    assert!(!path.exists());
    execute(&mut session, "MULTI").await;
    execute(&mut session, "AUTH alice AUTH").await;
    execute(&mut session, "EXEC").await;
    assert_eq!(session.auth_revision, 1);
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(before.contains("alice"));
    execute(&mut session, "HELLO 3 SETNAME AUTH AUTH alice AUTH").await;
    assert_eq!(session.auth_revision, 2);
    assert_eq!(session.config.name.as_deref(), Some("AUTH"));
    assert_eq!(
        session.config.info.redis_settings().username(),
        Some("alice")
    );
    assert_eq!(
        session.config.info.redis_settings().password(),
        Some("AUTH")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    execute(&mut session, "HELLO 3 SETNAME AUTH").await;
    assert_eq!(session.auth_revision, 2);
    execute(&mut session, "RESET").await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert!(session.config.info.redis_settings().password().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn failed_saves_and_binary_credentials_do_not_break_authentication() {
    use crate::auth::{SaveAuth, Store};
    let fixture = RedisFixture::new();
    let mut admin = fixture.connection();
    redis::cmd("ACL")
        .arg(&["SETUSER", "alice", "on", ">secret", "~*", "+@all"])
        .arg(b">\xff")
        .query::<()>(&mut admin)
        .unwrap();
    let path = fixture.path.join("auth.toml");
    std::fs::write(&path, "[broken").unwrap();
    let mut config = fixture.config.clone();
    config.auth_store = Some(Store::plaintext(path.clone(), SaveAuth::Always));
    let mut session = Session::open(config).await.unwrap();
    execute(&mut session, "AUTH alice secret").await;
    assert!(
        session
            .notices
            .pop()
            .unwrap()
            .contains("could not be saved")
    );
    assert_eq!(
        execute(&mut session, "PING").await,
        Value::SimpleString("PONG".into())
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
    std::fs::remove_file(&path).unwrap();
    session
        .execute(&[b"AUTH".to_vec(), b"alice".to_vec(), vec![0xff]])
        .await
        .unwrap();
    assert!(!path.exists());
    assert!(session.config.info.redis_settings().password().is_none());
    assert!(session.config.connect_auxiliary().is_err());
    assert!(Session::open(session.config.clone()).await.is_err());
    assert_eq!(
        execute(&mut session, "PING").await,
        Value::SimpleString("PONG".into())
    );
}
