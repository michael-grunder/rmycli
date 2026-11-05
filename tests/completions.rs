#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use std::process::Command;

const SHELLS: [&str; 5] = ["bash", "zsh", "fish", "powershell", "elvish"];

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        fixture.write(
            "rmycli-auth.toml",
            r"
[servers.'redis://127.0.0.1:7000'.users]
alice = 'alice-secret'
mike = 'mike-secret'
[servers.'redis://127.0.0.1:6379']
password = 'default-secret'
[servers.'rediss://127.0.0.1:7000'.users]
tls-user = 'tls-secret'
[servers.'redis://localhost:7000']
user = 'legacy-user'
password = 'legacy-secret'
[servers.'unix:/tmp/rmycli-completion.sock'.users]
socket-user = 'socket-secret'
",
        );
        fixture.write(
            "rmycli.toml",
            r"
[instances.local]
port = 7000
user = 'mike'
password-env = 'RMYCLI_COMPLETION_UNSET_PASSWORD'
[instances.secure]
port = 7000
tls = true
user = 'configured-user'
",
        );
        fixture
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.0.path().join(name), contents).unwrap();
    }

    fn command(&self, shell: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rmycli"));
        command
            .current_dir(self.0.path())
            .env("XDG_CONFIG_HOME", self.0.path())
            .env("XDG_CONFIG_DIRS", self.0.path())
            .env_remove("REDIS_URL")
            .env_remove("REDISCLI_AUTH")
            .env_remove("RMYCLI_COMPLETION_UNSET_PASSWORD")
            .env_remove("_CLAP_COMPLETE_INDEX")
            .env_remove("_CLAP_COMPLETE_SPACE")
            .env("_CLAP_IFS", "\n")
            .env("COMPLETE", shell);
        command
    }

    fn request(&self, shell: &str, args: &[&str]) -> Command {
        let mut command = self.command(shell);
        command
            .env("_CLAP_COMPLETE_INDEX", args.len().to_string())
            .args(["--", "rmycli"])
            .args(args);
        command
    }

    fn complete(&self, shell: &str, args: &[&str]) -> Vec<String> {
        output(&mut self.request(shell, args))
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn every_shell_registers_without_loading_settings_or_connecting() {
    let fixture = Fixture::new();
    fixture.write("rmycli.toml", "not valid TOML");
    for shell in SHELLS {
        let script = output(
            fixture
                .command(shell)
                .env("REDIS_URL", "not a valid URI")
                .env("REDISCLI_AUTH", "do-not-print-me"),
        );
        assert!(script.contains("rmycli"), "{shell}: {script}");
        assert!(script.contains("COMPLETE"), "{shell}: {script}");
        assert!(!script.contains("do-not-print-me"));
    }
}

#[test]
fn every_shell_completes_flags_values_and_endpoint_users() {
    let fixture = Fixture::new();
    for shell in SHELLS {
        let flags = fixture.complete(shell, &["--us"]);
        assert!(flags.iter().any(|flag| flag.starts_with("--user")));
        assert_eq!(fixture.complete(shell, &["--show-pushes", "y"]), ["yes"]);
        assert_eq!(
            fixture.complete(shell, &["-p", "7000", "--user", ""]),
            ["alice", "mike"],
            "{shell}"
        );
        assert_eq!(
            fixture.complete(shell, &["-p7000", "--user", "m"]),
            ["mike"]
        );
        assert_eq!(
            fixture.complete(shell, &["-p7000", "--user", "nobody"]),
            Vec::<String>::new()
        );
        assert_eq!(
            fixture
                .complete(shell, &["--port=7000", "--set-default-user", "a"]),
            ["alice"]
        );
        assert_eq!(fixture.complete(shell, &["--instance", "l"]), ["local"]);
        assert_eq!(
            fixture.complete(shell, &["--config", "rmycli.t"]),
            ["rmycli.toml"]
        );
        assert_eq!(
            fixture.complete(shell, &["-p7000", "SET", "key", "--user", ""]),
            Vec::<String>::new()
        );
    }
}

#[test]
fn users_follow_normal_endpoint_and_instance_precedence() {
    let fixture = Fixture::new();
    for (args, expected) in [
        (vec!["--user", ""], vec!["default"]),
        (vec!["-p7001", "--user", ""], vec![]),
        (
            vec!["-h", "LOCALHOST.", "-p7000", "--user", ""],
            vec!["legacy-user"],
        ),
        (vec!["--tls", "-p7000", "--user", ""], vec!["tls-user"]),
        (
            vec!["-u", "redis://127.0.0.1:7000/3", "-p1", "--user", ""],
            vec!["alice", "mike"],
        ),
        (
            vec!["-u", "redis://127.0.0.1:7000", "--tls", "--user", ""],
            vec!["tls-user"],
        ),
        (
            vec!["-s", "/tmp/rmycli-completion.sock", "-p1", "--user", ""],
            vec!["socket-user"],
        ),
        (
            vec!["--instance", "local", "--user", ""],
            vec!["alice", "mike"],
        ),
        (
            vec!["--instance", "secure", "--user", ""],
            vec!["configured-user", "tls-user"],
        ),
        (
            vec!["--instance", "secure", "--set-default-user", ""],
            vec!["tls-user"],
        ),
        (
            vec!["--instance", "local", "-p7001", "--user", ""],
            vec!["mike"],
        ),
        (vec!["--instance", "unknown", "--user", ""], vec![]),
        (vec!["--no-config", "-p7000", "--user", ""], vec![]),
        (vec!["--no-config", "--instance", ""], vec![]),
        (vec!["-p", "bad", "--user", ""], vec![]),
        (vec!["-u", "invalid", "--user", ""], vec![]),
        (
            vec!["--askpass", "-p7000", "--user", ""],
            vec!["alice", "mike"],
        ),
        (
            vec![
                "--tls", "--cert", "missing", "--key", "missing", "-p7000",
                "--user", "",
            ],
            vec!["tls-user"],
        ),
    ] {
        assert_eq!(fixture.complete("bash", &args), expected, "{args:?}");
    }
    let result = output(
        fixture
            .request("bash", &["--user", ""])
            .env("REDIS_URL", "redis://127.0.0.1:7000"),
    );
    assert_eq!(result, "alice\nmike");
}

#[test]
fn explicit_config_and_cursor_position_are_respected() {
    let fixture = Fixture::new();
    fixture.write("custom.toml", "[instances.alternate]\nport = 7000");
    assert_eq!(
        fixture
            .complete("bash", &["--config", "custom.toml", "--instance", ""]),
        ["alternate"]
    );
    assert_eq!(
        fixture.complete(
            "bash",
            &[
                "--config=custom.toml",
                "--instance",
                "alternate",
                "--user=m"
            ]
        ),
        ["--user=mike"]
    );
    let result = output(
        fixture
            .request("zsh", &["-p7000", "--user"])
            .env("_CLAP_COMPLETE_INDEX", "3"),
    );
    assert_eq!(result, "alice\nmike");
    let result = output(
        fixture
            .request("bash", &["--user", "m", "-p7000"])
            .env("_CLAP_COMPLETE_INDEX", "2"),
    );
    assert_eq!(result, "mike");
    let result = output(
        fixture
            .request("bash", &["-p7000", "--user", "m", "PING"])
            .env("_CLAP_COMPLETE_INDEX", "3"),
    );
    assert_eq!(result, "mike");
}

#[test]
fn missing_invalid_and_encrypted_stores_are_quiet_and_never_modified() {
    let fixture = Fixture::new();
    let auth_path = fixture.0.path().join("rmycli-auth.toml");
    let original = std::fs::read(&auth_path).unwrap();
    std::fs::create_dir(fixture.0.path().join("rmycli")).unwrap();
    fixture.write("rmycli/auth.enc", "not a real encrypted vault");
    assert_eq!(
        fixture.complete("bash", &["-p7000", "--user", ""]),
        Vec::<String>::new()
    );
    assert_eq!(std::fs::read(&auth_path).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(fixture.0.path().join("rmycli/auth.enc"))
            .unwrap(),
        "not a real encrypted vault"
    );
    std::fs::remove_file(fixture.0.path().join("rmycli/auth.enc")).unwrap();
    fixture.write("rmycli.toml", "encrypt-auth = true");
    assert_eq!(
        fixture.complete("bash", &["-p7000", "--user", ""]),
        Vec::<String>::new()
    );
    fixture.write("rmycli.toml", "");
    fixture.write("rmycli-auth.toml", "password = 'do-not-print-me'");
    assert_eq!(
        fixture.complete("bash", &["-p7000", "--user", ""]),
        Vec::<String>::new()
    );
    std::fs::remove_file(auth_path).unwrap();
    assert_eq!(
        fixture.complete("bash", &["-p7000", "--user", ""]),
        Vec::<String>::new()
    );
    fixture.write("rmycli.toml", "not valid TOML");
    assert_eq!(
        fixture.complete("bash", &["--instance", ""]),
        Vec::<String>::new()
    );
    assert!(
        fixture
            .complete("bash", &["--us"])
            .iter()
            .any(|value| value.starts_with("--user"))
    );
}

#[test]
fn usernames_preserve_spaces_and_omit_protocol_control_characters() {
    let fixture = Fixture::new();
    fixture.write(
        "rmycli-auth.toml",
        r#"[servers.'redis://127.0.0.1:7000'.users]
"two words" = 'secret'
"quote'user" = 'secret'
"line\nbreak" = 'secret'
"tab\tuser" = 'secret'
"escape\u001buser" = 'secret'
"#,
    );
    assert_eq!(
        fixture.complete("bash", &["-p7000", "--user", ""]),
        ["quote'user", "two words"]
    );
    assert_eq!(
        fixture.complete("fish", &["-p7000", "--user", "two"]),
        ["two words"]
    );
}
