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
        info.addr = ConnectionAddr::Unix(socket);
        let fixture = Self {
            child,
            path,
            config: Config {
                info,
                timeout: Duration::from_millis(300),
                connect_timeout: Some(Duration::from_millis(300)),
                name: None,
                cluster: false,
                output: crate::output::Options::default(),
                show_pushes: false,
            },
        };
        let start = Instant::now();
        while fixture.config.connect(true).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "isolated Redis failed to start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    fn connection(&self) -> Connection {
        self.config.connect(true).unwrap()
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

#[test]
fn worker_shows_server_errors_without_library_context() {
    use crate::worker::{Event, Workers};

    let fixture = RedisFixture::new();
    for protocol in
        [redis::ProtocolVersion::RESP2, redis::ProtocolVersion::RESP3]
    {
        let mut config = fixture.config.clone();
        config.info.redis.protocol = protocol;
        let workers = Workers::start(Session::open(config).unwrap());
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

#[test]
fn worker_displays_idle_invalidations_and_preserves_session_state() {
    use crate::worker::{Event, Execution, Workers};

    let fixture = RedisFixture::new();
    let mut writer = fixture.connection();
    for show_pushes in [true, false] {
        let mut config = fixture.config.clone();
        config.show_pushes = show_pushes;
        let workers = Workers::start(Session::open(config).unwrap());
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
        // Disconnection is noticed while idle, and reconnect restores polling.
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

fn execute(session: &mut Session, line: &str) -> Value {
    let args = input::parse(line)
        .words
        .into_iter()
        .map(|w| w.bytes)
        .collect::<Vec<_>>();
    session.execute(&args).unwrap()
}

#[test]
fn session_database_transaction_and_background_isolation() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).unwrap();
    execute(&mut session, "SELECT 2");
    execute(&mut session, "RPUSH list:db2 x");
    assert_eq!(session.config.info.redis.db, 2);
    let mut auxiliary = session.config.connect(true).unwrap();
    execute(&mut session, "MULTI");
    assert!(!session.assistance_allowed());
    execute(&mut session, "SELECT 3");
    execute(&mut session, "SET k v");
    let scan = scan::discover(
        &mut auxiliary,
        b"list:",
        Some("list"),
        Discovery::default(),
        &Limits::default(),
        || false,
    );
    assert_eq!(scan.keys.len(), 1);
    let Value::Array(replies) = execute(&mut session, "EXEC") else {
        panic!("expected EXEC array")
    };
    assert_eq!(
        replies.len(),
        2,
        "auxiliary SCAN must not enter transaction"
    );
    assert_eq!(session.config.info.redis.db, 3);
    assert!(session.assistance_allowed());
    execute(&mut session, "WATCH k");
    assert!(!session.assistance_allowed());
    execute(&mut session, "UNWATCH");
    assert!(session.assistance_allowed());
    execute(&mut session, "MULTI");
    execute(&mut session, "SELECT 4");
    execute(&mut session, "DISCARD");
    assert_eq!(session.config.info.redis.db, 3);
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
    config.info.redis.username = Some("limited".into());
    config.info.redis.password = Some(String::new());
    let mut connection = config.connect(true).unwrap();
    let catalog = Catalog::load(&mut connection);
    assert_ne!(catalog.notices, [] as [std::string::String; 0]);
    assert!(!catalog.commands["LRANGE"].live_docs);
    assert!(
        command(&[b"PING".to_vec()])
            .query::<String>(&mut connection)
            .is_ok()
    );
}

#[test]
fn exec_runtime_errors_preserve_successful_database_changes() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).unwrap();
    execute(&mut session, "MULTI");
    execute(&mut session, "SELECT 2");
    execute(&mut session, "SET key value");
    execute(&mut session, "LPUSH key wrong-type");
    let Value::Array(replies) = execute(&mut session, "EXEC") else {
        panic!("EXEC must preserve individual replies")
    };
    assert_eq!(replies.len(), 3);
    assert!(matches!(replies[2], Value::ServerError(_)));
    assert_eq!(session.config.info.redis.db, 2);
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

#[test]
fn authentication_changes_and_resp3_subscription_acknowledgment() {
    let fixture = RedisFixture::new();
    let mut session = Session::open(fixture.config.clone()).unwrap();
    execute(&mut session, "ACL SETUSER alice on >secret ~* &* +@all");
    execute(&mut session, "AUTH alice secret");
    assert_eq!(session.config.info.redis.username.as_deref(), Some("alice"));
    assert!(session.config.connect(true).is_ok());
    execute(&mut session, "HELLO 3");
    let reply = execute(&mut session, "SUBSCRIBE updates");
    assert!(matches!(reply, Value::Push { .. }));
    assert!(!session.assistance_allowed());
}

#[test]
fn tls_has_a_crypto_provider() {
    let _ = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}
