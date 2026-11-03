#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    child: Child,
    dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("redis.sock");
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
            .args(["--unixsocketperm", "700"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("install redis-server for isolated CLI tests");
        let fixture = Self { child, dir, socket };
        let start = Instant::now();
        while !fixture.run(&["PING"], b"").status.success() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "Redis fixture failed to start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }

    fn command(&self) -> Command {
        let mut command = base();
        command.arg("-s").arg(&self.socket);
        command
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    fn success(&self, args: &[&str], stdin: &[u8]) -> Vec<u8> {
        let result = self.run(args, stdin);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rmycli"));
    command
        .env_remove("REDIS_URL")
        .env_remove("REDISCLI_AUTH")
        .arg("--no-config");
    command
}

#[test]
fn server_errors_are_concise_and_still_fail() {
    let fixture = Fixture::new();
    for protocol in ["-2", "-3"] {
        let result = fixture.run(&[protocol, "SET", "d", "sd", "d"], b"");
        assert!(!result.status.success());
        assert_eq!(result.stderr, b"rmycli: syntax error\n");
    }
}

#[test]
fn piped_commands_preserve_line_boundaries_and_escape_errors() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.success(&[], b"SET piped value\r\nGET piped\nPING"),
        b"OK\nvalue\nPONG\n"
    );
    for ending in [b"\n".as_slice(), b"\r\n", b""] {
        let input = [b"SET unfinished \\".as_slice(), ending].concat();
        let result = fixture.run(&[], &input);
        assert!(!result.status.success());
        assert_eq!(result.stderr, b"rmycli: unfinished escape\n");
    }
    assert_eq!(fixture.success(&["EXISTS", "unfinished"], b""), b"0\n");
}

#[test]
fn help_examples_version_and_invalid_switches_need_no_server() {
    let output = base()
        .arg("--help")
        .env("REDISCLI_AUTH", "do-not-print-me")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Examples:",
        "Connection:",
        "TLS:",
        "-3",
        "--pass",
        "--eval script.lua",
        "Compatibility:",
    ] {
        assert!(help.contains(expected), "{expected}");
    }
    assert!(!help.contains("do-not-print-me"));
    for flag in ["--version", "-v"] {
        assert!(base().arg(flag).output().unwrap().status.success());
    }
    for args in [
        vec!["--unknown"],
        vec!["-2", "-3"],
        vec!["-t", "-1"],
        vec!["--cluster", "help"],
        vec!["-r", "-2"],
        vec!["-i", "NaN"],
        vec!["--show-pushes", "maybe"],
        vec!["--cert", "missing"],
        vec!["--count", "0"],
        vec!["-x"],
        vec!["--askpass"],
    ] {
        let result = base().args(&args).output().unwrap();
        assert!(!result.status.success(), "{args:?}");
        assert!(
            !String::from_utf8_lossy(&result.stderr)
                .contains("Connection refused"),
            "{args:?}"
        );
    }
}

#[test]
fn protocols_auth_database_and_client_name() {
    let f = Fixture::new();
    for (flags, expected) in [
        (vec!["-3"], "3"),
        (vec!["-2"], "2"),
        (vec!["--json"], "3"),
        (vec!["--json", "-2"], "2"),
    ] {
        let mut args = flags;
        args.extend(["--name", "cli-test", "CLIENT", "INFO"]);
        let reply = String::from_utf8(f.success(&args, b"")).unwrap();
        assert!(reply.contains(&format!("resp={expected}")), "{reply}");
        assert!(reply.contains("name=cli-test"));
    }
    f.success(&["-n", "2", "SET", "isolated", "yes"], b"");
    assert_eq!(f.success(&["GET", "isolated"], b""), b"\n");
    assert_eq!(f.success(&["-n", "2", "GET", "isolated"], b""), b"yes\n");
    f.success(
        &["ACL", "SETUSER", "alice", "on", ">secret", "~*", "+@all"],
        b"",
    );
    for flag in ["-a", "--pass", "--password"] {
        assert_eq!(
            f.success(
                &[
                    "--no-auth-warning",
                    "--user",
                    "alice",
                    flag,
                    "secret",
                    "ACL",
                    "WHOAMI"
                ],
                b""
            ),
            b"alice\n"
        );
    }
    let result = f
        .command()
        .env("REDISCLI_AUTH", "wrong")
        .args(["--user", "alice", "--pass", "secret", "PING"])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Warning:"));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("secret"));
}

#[test]
fn binary_stdin_output_modes_repetition_and_eval() {
    let f = Fixture::new();
    let bytes = b"a\0b\xff\n";
    f.success(&["-x", "SET", "binary"], bytes);
    assert_eq!(f.success(&["-D", "", "GET", "binary"], b""), bytes);
    f.success(&["-X", "payload", "SET", "binary", "payload", "XX"], b"new");
    assert_eq!(f.success(&["GET", "binary"], b""), b"new\n");
    assert_eq!(
        f.success(&["-r", "3", "-i", "0.001", "INCR", "counter"], b""),
        b"1\n2\n3\n"
    );
    assert_eq!(f.success(&["-r", "0", "INCR", "counter"], b""), b"");
    assert_eq!(
        f.success(&["--no-raw", "INCR", "counter"], b""),
        b"(integer) 4\n"
    );
    assert_eq!(
        f.success(&["--csv", "MGET", "binary", "missing"], b""),
        b"\"new\",NULL\n"
    );
    assert_eq!(
        f.success(&["-d", "|", "-D", "!", "MGET", "binary", "missing"], b""),
        b"new|!"
    );
    f.success(&["--quoted-input", "SET", "\"key\\x00\"", "value"], b"");
    assert_eq!(
        f.success(&["--quoted-input", "GET", "\"key\\x00\""], b""),
        b"value\n"
    );
    let script = f.dir.path().join("test.lua");
    std::fs::write(&script, "return {KEYS[1], ARGV[1]}").unwrap();
    assert_eq!(
        f.success(
            &[
                "--json",
                "--eval",
                script.to_str().unwrap(),
                "key",
                ",",
                "--literal"
            ],
            b""
        ),
        b"[\"key\",\"--literal\"]\n"
    );
    let result = f.run(&["-e", "UNKNOWN-COMMAND"], b"");
    assert!(!result.status.success());
}

#[test]
fn scan_streams_all_batches_and_respects_quoted_globs() {
    let f = Fixture::new();
    for i in 0..80 {
        f.success(&["SET", &format!("match:{i}"), "v"], b"");
    }
    f.success(&["SET", "other", "v"], b"");
    let output = f.success(
        &[
            "--scan",
            "--pattern",
            "match:*",
            "--count",
            "1",
            "-i",
            "0.001",
        ],
        b"",
    );
    let keys = output
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), 80);
    f.success(&["--quoted-input", "SET", "\"binary\\x00key\"", "v"], b"");
    assert_eq!(
        f.success(&["--scan", "--quoted-pattern", "\"binary\\x00*\""], b""),
        b"binary\0key\n"
    );
}

#[test]
fn tls_trust_client_certificates_and_insecure_are_applied() {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;

    let fixtures =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
    let ca = fixtures.join("ca.pem");
    let cert = fixtures.join("client-server.pem");
    let key = fixtures.join("client-server.key");
    let ca_dir = tempfile::tempdir().unwrap();
    std::fs::copy(&ca, ca_dir.path().join("ca.pem")).unwrap();
    let trusted = ["--cacert", ca.to_str().unwrap()];
    let mutual = [
        "--cacert",
        ca.to_str().unwrap(),
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ];
    let directory = ["--cacertdir", ca_dir.path().to_str().unwrap()];
    for (flags, require_client, success) in [
        (trusted.as_slice(), false, true),
        (directory.as_slice(), false, true),
        (mutual.as_slice(), true, true),
        (trusted.as_slice(), true, false),
        (["--insecure"].as_slice(), false, true),
        ([].as_slice(), false, false),
    ] {
        let roots = CertificateDer::pem_file_iter(&ca)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut root_store = rustls::RootCertStore::empty();
        for root in roots {
            root_store.add(root).unwrap();
        }
        let builder = rustls::ServerConfig::builder();
        let builder = if require_client {
            builder.with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder(Arc::new(
                    root_store,
                ))
                .build()
                .unwrap(),
            )
        } else {
            builder.with_no_client_auth()
        };
        let chain = CertificateDer::pem_file_iter(&cert)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let config = builder
            .with_single_cert(
                chain,
                PrivateKeyDer::from_pem_file(&key).unwrap(),
            )
            .unwrap();
        let (port, server) = tls_server(config);
        let result = base()
            .args(["-u", &format!("rediss://localhost:{port}"), "--tls"])
            .args(flags)
            .arg("PING")
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            success,
            "{flags:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if success {
            assert_eq!(result.stdout, b"PONG\n");
        }
        server.join().unwrap();
    }
}

fn tls_server(
    config: rustls::ServerConfig,
) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::BufReader;
    use std::net::TcpListener;
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let tls = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
            stream,
        );
        let mut reader = BufReader::new(tls);
        let mut parser = redis::Parser::new();
        while let Ok(value) = parser.parse_value(&mut reader) {
            let args: Vec<String> = redis::from_redis_value(value).unwrap();
            let reply = if args[0] == "PING" {
                b"+PONG\r\n".as_slice()
            } else {
                b"+OK\r\n"
            };
            if reader.get_mut().write_all(reply).is_err() {
                break;
            }
            reader.get_mut().flush().unwrap();
        }
    });
    (port, server)
}
