#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rmycli-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for dir in ["home/.config", "user", "system1", "system2"] {
            std::fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(path)
    }

    fn write(&self, path: &str, source: &str) -> PathBuf {
        let path = self.0.join(path);
        std::fs::write(&path, source).unwrap();
        path
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rmycli"));
        command
            .env_remove("REDIS_URL")
            .env_remove("REDISCLI_AUTH")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("user"))
            .env(
                "XDG_CONFIG_DIRS",
                std::env::join_paths([
                    self.0.join("system1"),
                    self.0.join("system2"),
                ])
                .unwrap(),
            );
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        // Fails before opening a connection, after loading settings.
        command.args(["--instance", "missing"]);
        command
    }

    fn config(&self, args: &[&str]) -> Command {
        let mut command = self.base_command();
        command.arg("config").args(args);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn error(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

fn success(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_path_error(command: &mut Command, path: &Path, expected: &str) {
    let error = error(command);
    assert!(error.contains(path.to_str().unwrap()), "{error}");
    assert!(error.contains(expected), "{error}");
}

#[test]
fn xdg_search_stops_at_first_file_and_never_skips_errors() {
    let fixture = Fixture::new();
    let system2 = fixture.write("system2/rmycli.toml", "second = true");
    assert_path_error(&mut fixture.command(), &system2, "second");
    let system1 = fixture.write("system1/rmycli.toml", "first = true");
    assert_path_error(&mut fixture.command(), &system1, "first");
    let user = fixture.write("user/rmycli.toml", "user = true");
    assert_path_error(&mut fixture.command(), &user, "user");
    // An empty user file selects built-in defaults, not system settings.
    fixture.write("user/rmycli.toml", "");
    assert!(error(&mut fixture.command()).contains("unknown instance"));
    std::fs::remove_file(&user).unwrap();
    std::fs::create_dir(&user).unwrap();
    assert_path_error(&mut fixture.command(), &user, "cannot read config");
}

#[test]
fn xdg_home_fallback_ignores_empty_and_relative_values() {
    let fixture = Fixture::new();
    let path = fixture.write("home/.config/rmycli.toml", "fallback = true");
    assert_path_error(
        fixture.command().env_remove("XDG_CONFIG_HOME"),
        &path,
        "fallback",
    );
    for value in ["", "relative/path"] {
        assert_path_error(
            fixture.command().env("XDG_CONFIG_HOME", value),
            &path,
            "fallback",
        );
    }
    let user = fixture.write("user/rmycli.toml", "override = true");
    assert_path_error(&mut fixture.command(), &user, "override");
}

#[test]
fn explicit_config_missing_files_and_disable_switch() {
    let fixture = Fixture::new();
    assert!(error(&mut fixture.command()).contains("unknown instance"));
    fixture.write("user/rmycli.toml", "invalid = true");
    let explicit = fixture.write("custom.toml", "explicit = true");
    assert_path_error(
        fixture.command().arg("--config").arg(&explicit),
        &explicit,
        "explicit",
    );
    let missing = fixture.0.join("missing.toml");
    assert_path_error(
        fixture.command().arg("--config").arg(&missing),
        &missing,
        "cannot read config",
    );
    assert!(
        error(fixture.command().arg("--no-config"))
            .contains("unknown instance")
    );
    assert!(
        error(fixture.command().args(["--no-config", "--config", "x"]))
            .contains("cannot be used with")
    );
}

#[test]
fn help_does_not_require_valid_configuration() {
    let fixture = Fixture::new();
    fixture.write("user/rmycli.toml", "[invalid");
    let output = fixture.command().arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--instance")
    );
}

#[test]
fn config_get_reads_defaults_and_inheritance_without_resolving_auth() {
    let fixture = Fixture::new();
    assert_eq!(
        success(&mut fixture.config(&["get", "theme.inline-hint"])),
        "dark_grey\n"
    );
    fixture.write(
        "user/rmycli.toml",
        r##"
[theme.commands]
xgroup = "#112233"
[instances.qa]
password-env = "RMYCLI_UNSET_AUTH"
"##,
    );
    assert_eq!(
        success(
            &mut fixture.config(&["get", "theme.commands.\"XGROUP CREATE\""])
        ),
        "rgb_(17,34,51)\n"
    );
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.host"])),
        "127.0.0.1\n"
    );
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.port"])),
        "6379\n"
    );
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.db"])),
        "0\n"
    );
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.tls"])),
        "false\n"
    );
    assert!(
        error(&mut fixture.config(&["get", "instances.qa.password"]))
            .contains("not set")
    );
    assert!(
        error(&mut fixture.config(&["get", "instances.missing.host"]))
            .contains("unknown instance")
    );
    assert!(!fixture.0.join("home/.config/rmycli.toml").exists());
}

#[test]
fn config_set_preserves_comments_and_normalized_keys() {
    let fixture = Fixture::new();
    let path = fixture.write(
        "user/rmycli.toml",
        r"# My theme
[theme]
inline-hint = 'dark_grey' # keep this
[theme.commands]
get = 'green' # lower-case spelling
[instances.qa]
port = 6380 # private endpoint
",
    );
    assert_eq!(
        success(&mut fixture.config(&["set", "theme.inline-hint", "#12abef"])),
        "#12abef\n"
    );
    success(&mut fixture.config(&["set", "theme.commands.GET", "cyan"]));
    success(&mut fixture.config(&["set", "instances.qa.port", "6381"]));
    success(&mut fixture.config(&["set", "instances.qa.tls", "true"]));
    success(&mut fixture.config(&[
        "set",
        "instances.commands.host",
        "commands.example",
    ]));
    let source = std::fs::read_to_string(&path).unwrap();
    for comment in [
        "# My theme",
        "# keep this",
        "# lower-case spelling",
        "# private endpoint",
    ] {
        assert!(source.contains(comment), "{source}");
    }
    let parsed: toml::Value = toml::from_str(&source).unwrap();
    assert_eq!(parsed["theme"]["commands"]["get"].as_str(), Some("cyan"));
    assert!(parsed["theme"]["commands"].get("GET").is_none());
    assert_eq!(parsed["instances"]["qa"]["port"].as_integer(), Some(6381));
    assert_eq!(parsed["instances"]["qa"]["tls"].as_bool(), Some(true));
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.port"])),
        "6381\n"
    );
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.commands.host"])),
        "commands.example\n"
    );
    success(&mut fixture.config(&["unset", "theme.commands.GET"]));
    success(&mut fixture.config(&["unset", "theme.inline-hint"]));
    assert_eq!(
        success(&mut fixture.config(&["get", "theme.commands.GET"])),
        "dark_grey\n"
    );
    assert!(
        error(&mut fixture.config(&["unset", "theme.commands.GET"]))
            .contains("not explicitly set")
    );
}

#[test]
fn invalid_config_updates_leave_files_unchanged() {
    let fixture = Fixture::new();
    let source = "[instances.qa]\npassword-env = 'QA_AUTH'\n";
    let path = fixture.write("user/rmycli.toml", source);
    for (key, value) in [
        ("theme.inline-hnit", "red"),
        ("theme.inline-hint", "not-a-color"),
        ("instances.qa.port", "-1"),
        ("instances.qa.port", "65536"),
        ("instances.qa.port", "zero"),
        ("instances.qa.db", "-1"),
        ("instances.qa.tls", "yes"),
        ("instances.qa.host", ""),
        ("instances.qa.password", "secret"),
    ] {
        error(&mut fixture.config(&["set", key, value]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
    fixture.write("user/rmycli.toml", "[broken");
    assert!(
        error(&mut fixture.config(&["set", "theme.inline-hint", "red"]))
            .contains("invalid config")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "[broken");
}

#[test]
fn config_edits_copy_system_settings_into_user_file() {
    let fixture = Fixture::new();
    let source = "# System defaults\n[instances.qa]\nhost = 'qa.example'\n";
    let system = fixture.write("system1/rmycli.toml", source);
    success(&mut fixture.config(&["set", "theme.inline-hint", "blue"]));
    assert_eq!(std::fs::read_to_string(system).unwrap(), source);
    let user = fixture.0.join("user/rmycli.toml");
    let saved = std::fs::read_to_string(&user).unwrap();
    assert!(saved.contains("# System defaults"));
    assert_eq!(
        success(&mut fixture.config(&["get", "instances.qa.host"])),
        "qa.example\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(user).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn explicit_new_files_quoted_names_and_literal_passwords() {
    let fixture = Fixture::new();
    let path = fixture.0.join("nested/custom.toml");
    let path_arg = path.to_str().unwrap();
    let secret = "spaces ' quotes \" backslash \\ \n new line";
    assert_eq!(
        success(&mut fixture.config(&[
            "--config",
            path_arg,
            "set",
            "instances.\"qa.eu\".password",
            secret,
        ])),
        "[redacted]\n"
    );
    assert_eq!(
        success(&mut fixture.config(&[
            "get",
            "instances.\"qa.eu\".password",
            "--config",
            path_arg,
        ])),
        format!("{secret}\n")
    );
    success(fixture.base_command().args([
        "--config",
        path_arg,
        "config",
        "set",
        "theme.commands.\"XGROUP CREATE\"",
        "red",
    ]));
    assert_eq!(
        success(&mut fixture.config(&[
            "--config",
            path_arg,
            "get",
            "theme.commands.\"xgroup create\"",
        ])),
        "red\n"
    );
    assert!(!fixture.0.join("user/rmycli.toml").exists());
    assert!(
        error(fixture.base_command().args([
            "--no-config",
            "config",
            "set",
            "theme.inline-hint",
            "red",
        ]))
        .contains("--no-config")
    );
}

#[test]
fn hex_colors_from_toml_render_as_truecolor() {
    let fixture = Fixture::new();
    fixture.write(
        "user/rmycli.toml",
        "[theme]\ninline-hint = '#ffffff'\n\
         [theme.commands]\nGET = '#12abEF'\nSET = 'cyan'\n",
    );
    for (key, color) in [
        ("theme.inline-hint", "38;2;255;255;255"),
        ("theme.commands.GET", "38;2;18;171;239"),
        ("theme.commands.SET", "38;5;14"),
    ] {
        let preview =
            success(&mut fixture.config(&["get", key, "--color", "always"]));
        assert!(preview.contains(&format!("\x1b[{color}msample text\x1b[0m")));
    }
}

#[test]
fn color_previews_are_optional_and_reset_terminal_style() {
    let fixture = Fixture::new();
    success(&mut fixture.config(&["set", "theme.inline-hint", "#12abef"]));
    let plain = success(&mut fixture.config(&["get", "theme.inline-hint"]));
    assert_eq!(plain, "#12abef\n");
    let preview = success(&mut fixture.config(&[
        "get",
        "theme.inline-hint",
        "--color",
        "always",
    ]));
    assert!(
        preview.starts_with("#12abef  \x1b[38;2;18;171;239m"),
        "{preview:?}"
    );
    assert!(preview.ends_with("sample text\x1b[0m\n"), "{preview:?}");
    assert_eq!(
        success(
            fixture
                .config(&["get", "theme.inline-hint", "--color", "never",])
                .env("NO_COLOR", "1")
        ),
        plain
    );
    assert_eq!(
        success(&mut fixture.config(&[
            "set",
            "theme.inline-hint",
            "cyan",
            "--color",
            "always",
        ])),
        "cyan  \x1b[38;5;14msample text\x1b[0m\n"
    );
}

#[test]
fn inline_tables_can_be_edited_and_symlinks_are_preserved() {
    let fixture = Fixture::new();
    let target = fixture.write(
        "target.toml",
        "theme = { inline-hint = 'green', commands = { get = 'cyan' } }\n",
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, fixture.0.join("user/rmycli.toml"))
            .unwrap();
        success(&mut fixture.config(&["set", "theme.commands.GET", "red"]));
        success(&mut fixture.config(&["set", "theme.commands.SET", "blue"]));
        success(&mut fixture.config(&["unset", "theme.commands.GET"]));
        assert!(fixture.0.join("user/rmycli.toml").is_symlink());
        let parsed: toml::Value =
            toml::from_str(&std::fs::read_to_string(target).unwrap()).unwrap();
        assert_eq!(parsed["theme"]["commands"]["SET"].as_str(), Some("blue"));
        assert!(parsed["theme"]["commands"].get("get").is_none());
    }
}

#[test]
fn auth_saving_policy_can_be_read_set_and_unset() {
    let fixture = Fixture::new();
    assert_eq!(
        success(&mut fixture.config(&["get", "save-auth"])),
        "false\n"
    );
    for value in ["local", "always", "false"] {
        assert_eq!(
            success(&mut fixture.config(&["set", "save-auth", value])),
            format!("{value}\n")
        );
        assert_eq!(
            success(&mut fixture.config(&["get", "save-auth"])),
            format!("{value}\n")
        );
    }
    let path = fixture.0.join("user/rmycli.toml");
    let before = std::fs::read_to_string(&path).unwrap();
    for value in ["true", "never", "1"] {
        assert!(
            error(&mut fixture.config(&["set", "save-auth", value]))
                .contains("save-auth")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
    success(&mut fixture.config(&["unset", "save-auth"]));
    assert_eq!(
        success(&mut fixture.config(&["get", "save-auth"])),
        "false\n"
    );
}
