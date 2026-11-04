//! Opt-in credential persistence, matched to an exact transport and endpoint.
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;

use redis::{ConnectionAddr, ConnectionInfo};
use serde::{Deserialize, Deserializer, de::Error};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SaveAuth {
    #[default]
    Never,
    Local,
    Always,
}

impl<'de> Deserialize<'de> for SaveAuth {
    fn deserialize<D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Setting {
            Boolean(bool),
            Text(String),
        }
        match Setting::deserialize(deserializer)? {
            Setting::Boolean(false) => Ok(Self::Never),
            Setting::Text(text) if text == "local" => Ok(Self::Local),
            Setting::Text(text) if text == "always" => Ok(Self::Always),
            _ => Err(D::Error::custom(
                "save-auth requires false, 'local', or 'always'",
            )),
        }
    }
}

impl SaveAuth {
    fn allows(self, addr: &ConnectionAddr) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Local => match addr {
                ConnectionAddr::Unix(_) => true,
                ConnectionAddr::Tcp(host, _)
                | ConnectionAddr::TcpTls { host, .. } => {
                    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                        || host.parse::<IpAddr>().is_ok_and(|ip| match ip {
                            IpAddr::V4(ip) => ip.is_loopback(),
                            IpAddr::V6(ip) => {
                                ip.is_loopback()
                                    || ip
                                        .to_ipv4_mapped()
                                        .is_some_and(|ip| ip.is_loopback())
                            }
                        })
                }
                _ => false,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    pub path: PathBuf,
    pub policy: SaveAuth,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthFile {
    servers: BTreeMap<String, Server>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct Server {
    user: Option<String>,
    password: Option<String>,
    users: BTreeMap<String, String>,
    default_user: Option<String>,
}

impl Server {
    fn credentials(&self) -> BTreeMap<&str, (Option<&str>, &str)> {
        let mut accounts = self
            .users
            .iter()
            .map(|(user, password)| {
                (user.as_str(), (Some(user.as_str()), password.as_str()))
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(password) = &self.password {
            accounts.insert(
                self.user.as_deref().unwrap_or("default"),
                (self.user.as_deref(), password),
            );
        }
        accounts
    }

    fn valid(&self) -> bool {
        (self.user.is_none() || self.password.is_some())
            && !(self.password.is_some()
                && self
                    .users
                    .contains_key(self.user.as_deref().unwrap_or("default")))
            && self.default_user.as_ref().is_none_or(|user| {
                self.credentials().contains_key(user.as_str())
            })
    }
}

pub fn endpoint(addr: &ConnectionAddr) -> io::Result<String> {
    match addr {
        ConnectionAddr::Unix(path) => path
            .to_str()
            .map(|path| format!("unix:{path}"))
            .ok_or_else(|| {
                io::Error::other("cannot store a non-UTF-8 socket path")
            }),
        ConnectionAddr::Tcp(host, port)
        | ConnectionAddr::TcpTls { host, port, .. } => {
            let scheme = if matches!(addr, ConnectionAddr::TcpTls { .. }) {
                "rediss"
            } else {
                "redis"
            };
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host
            };
            Ok(format!("{scheme}://{host}:{port}"))
        }
        _ => Err(io::Error::other("unsupported authentication transport")),
    }
}

impl Store {
    pub fn discover(policy: SaveAuth) -> Option<Self> {
        xdg::BaseDirectories::new()
            .get_config_home()
            .map(|dir| Self {
                path: dir.join("rmycli-auth.toml"),
                policy,
            })
    }

    fn read(&self) -> io::Result<(AuthFile, DocumentMut)> {
        let source = match std::fs::read_to_string(&self.path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                String::new()
            }
            Err(error) => return Err(error),
        };
        // Parser excerpts and field errors may contain passwords.
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid auth file; expected server tables with password and optional user, a users password table, and an optional default-user naming a saved account; duplicate accounts are not allowed",
            )
        };
        let parsed: AuthFile =
            toml::from_str(&source).map_err(|_| invalid())?;
        if parsed
            .servers
            .iter()
            .any(|(key, server)| key.trim().is_empty() || !server.valid())
        {
            return Err(invalid());
        }
        let document = source.parse().map_err(|_| invalid())?;
        Ok((parsed, document))
    }

    pub fn apply(&self, info: &mut ConnectionInfo) -> io::Result<()> {
        let (file, _) = self.read()?;
        let settings = info.redis_settings();
        if settings.password().is_some() {
            return Ok(());
        }
        let Ok(key) = endpoint(info.addr()) else {
            return Ok(());
        };
        if let Some(saved) = file.servers.get(&key) {
            let accounts = saved.credentials();
            let selected =
                settings.username().or(saved.default_user.as_deref());
            let account = selected.map_or_else(
                || accounts.values().next(),
                |user| accounts.get(user),
            );
            let Some((user, password)) = account else {
                return Ok(());
            };
            let mut settings = settings.clone().set_password(*password);
            if let Some(user) = user {
                settings = settings.set_username(user);
            }
            *info = info.clone().set_redis_settings(settings);
        }
        Ok(())
    }

    pub fn save(&self, info: &ConnectionInfo) -> io::Result<()> {
        if !self.policy.allows(info.addr()) {
            return Ok(());
        }
        let settings = info.redis_settings();
        let Some(password) = settings.password() else {
            return Ok(());
        };
        let key = endpoint(info.addr())?;
        let user = settings.username().unwrap_or("default");
        self.update(|file, document| {
            let saved = file.servers.get(&key);
            if saved.is_some_and(|saved| {
                saved.credentials().get(user).is_some_and(|account| {
                    *account == (settings.username(), password)
                })
            }) {
                return Ok(false);
            }
            if !document.contains_key("servers") {
                document["servers"] = Item::Table(Table::new());
            }
            let servers = document["servers"]
                .as_table_like_mut()
                .ok_or_else(|| io::Error::other("servers must be a table"))?;
            if !servers.contains_key(&key) {
                servers.insert(&key, Item::Table(Table::new()));
            }
            let entry = servers
                .get_mut(&key)
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| io::Error::other("server must be a table"))?;
            // Keep legacy entries intact when updating the same account.
            if settings.username().is_none()
                || saved.is_some_and(|saved| {
                    saved.password.is_some()
                        && saved.user.as_deref().unwrap_or("default") == user
                })
            {
                if let Some(users) =
                    entry.get_mut("users").and_then(Item::as_table_like_mut)
                {
                    users.remove(user);
                }
                if let Some(saved) = saved
                    && let (Some(old_user), Some(old_password)) =
                        (&saved.user, &saved.password)
                    && old_user != user
                {
                    insert_user(entry, old_user, old_password)?;
                }
                if let Some(user) = settings.username() {
                    entry.insert("user", value(user));
                } else {
                    entry.remove("user");
                }
                entry.insert("password", value(password));
            } else {
                insert_user(entry, user, password)?;
            }
            Ok(true)
        })
    }

    pub fn set_default_user(
        &self,
        addr: &ConnectionAddr,
        user: &str,
    ) -> io::Result<()> {
        let key = endpoint(addr)?;
        self.update(|file, document| {
            if !file
                .servers
                .get(&key)
                .is_some_and(|server| server.credentials().contains_key(user))
            {
                return Err(io::Error::other(format!(
                    "no saved credentials for user '{}' at {}",
                    user.escape_default(),
                    key.escape_default()
                )));
            }
            document["servers"][&key]["default-user"] = value(user);
            Ok(true)
        })
    }

    fn update(
        &self,
        edit: impl FnOnce(&AuthFile, &mut DocumentMut) -> io::Result<bool>,
    ) -> io::Result<()> {
        // Follow managed dotfile links while replacing their target atomically.
        let path = if self.path.try_exists()? {
            self.path.canonicalize()?
        } else {
            self.path.clone()
        };
        let parent = path.parent().ok_or_else(|| {
            io::Error::other("auth file has no parent directory")
        })?;
        std::fs::create_dir_all(parent)?;
        let lock_path = path.with_extension("toml.lock");
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "auth file is busy; check for a stale lock at {}",
                            lock_path.display()
                        ),
                    )
                } else {
                    error
                }
            })?;
        let _lock = Lock(lock_path);
        drop(lock_file);
        let (file, mut document) = self.read()?;
        if !edit(&file, &mut document)? {
            return Ok(());
        }
        // NamedTempFile starts owner-only on Unix, including replacements.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

fn insert_user(
    entry: &mut dyn toml_edit::TableLike,
    user: &str,
    password: &str,
) -> io::Result<()> {
    if !entry.contains_key("users") {
        entry.insert("users", Item::Table(Table::new()));
    }
    entry
        .get_mut("users")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| io::Error::other("users must be a table"))?
        .insert(user, value(password));
    Ok(())
}

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::IntoConnectionInfo;

    fn info(host: &str, password: Option<&str>) -> ConnectionInfo {
        let mut info = (host, 7000).into_connection_info().unwrap();
        if let Some(password) = password {
            info = info.clone().set_redis_settings(
                info.redis_settings().clone().set_password(password),
            );
        }
        info
    }

    #[test]
    fn save_policy_is_strict_and_defaults_to_disabled() {
        for (value, expected) in [
            ("false", SaveAuth::Never),
            ("'local'", SaveAuth::Local),
            ("'always'", SaveAuth::Always),
        ] {
            let settings =
                crate::config::Settings::parse(&format!("save-auth = {value}"))
                    .unwrap();
            assert_eq!(settings.save_auth, expected);
        }
        assert_eq!(
            crate::config::Settings::default().save_auth,
            SaveAuth::Never
        );
        for value in ["true", "'false'", "'remote'", "1", "[]"] {
            assert!(
                crate::config::Settings::parse(&format!("save-auth = {value}"))
                    .is_err()
            );
        }
    }

    #[test]
    fn local_policy_never_uses_dns_or_private_networks() {
        for host in [
            "localhost",
            "LOCALHOST.",
            "127.0.0.1",
            "127.12.0.2",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(SaveAuth::Local.allows(info(host, None).addr()), "{host}");
        }
        for host in [
            "localhost.example",
            "10.0.0.1",
            "192.168.0.1",
            "0.0.0.0",
            "::",
            "redis.example",
        ] {
            assert!(!SaveAuth::Local.allows(info(host, None).addr()), "{host}");
        }
        assert!(
            SaveAuth::Local
                .allows(&ConnectionAddr::Unix("/tmp/redis.sock".into()))
        );
    }

    #[test]
    fn disabled_and_remote_local_saves_create_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store {
            path: dir.path().join("nested/auth.toml"),
            policy: SaveAuth::Never,
        };
        store.save(&info("localhost", Some("secret"))).unwrap();
        store.policy = SaveAuth::Local;
        store.save(&info("remote.example", Some("secret"))).unwrap();
        store.policy = SaveAuth::Always;
        store.save(&info("localhost", None)).unwrap();
        assert!(!store.path.parent().unwrap().exists());
    }

    #[test]
    fn exact_endpoint_and_user_matching_respect_explicit_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Never,
        };
        std::fs::write(&store.path, "[servers.\"redis://localhost:7000\"]\nuser = 'alice'\npassword = 'stored'").unwrap();
        let mut connection = info("localhost", None);
        store.apply(&mut connection).unwrap();
        assert_eq!(connection.redis_settings().username(), Some("alice"));
        assert_eq!(connection.redis_settings().password(), Some("stored"));
        for mut connection in [
            info("localhost", Some("explicit")),
            info("127.0.0.1", None),
            "redis://localhost:7001".into_connection_info().unwrap(),
            "rediss://localhost:7000".into_connection_info().unwrap(),
            info("localhost", None).set_redis_settings(
                redis::RedisConnectionInfo::default().set_username("bob"),
            ),
        ] {
            let before = connection.clone();
            store.apply(&mut connection).unwrap();
            assert_eq!(
                connection.redis_settings().password(),
                before.redis_settings().password()
            );
            assert_eq!(
                connection.redis_settings().username(),
                before.redis_settings().username()
            );
        }
        let mut connection = info("localhost", None).set_redis_settings(
            redis::RedisConnectionInfo::default()
                .set_username("alice")
                .set_db(3),
        );
        store.apply(&mut connection).unwrap();
        assert_eq!(connection.redis_settings().password(), Some("stored"));
        assert_eq!(connection.redis_settings().db(), 3);
    }

    #[test]
    fn saves_round_trip_and_preserve_other_servers_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Always,
        };
        let source = "# my servers\n[servers.\"redis://remote:7000\"]\npassword = 'remote-secret' # keep\n";
        std::fs::write(&store.path, source).unwrap();
        let password = "spaces \"quotes\" \\ slash\nnewline\0nul";
        let connection = info("localhost", Some(password));
        store.save(&connection).unwrap();
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.starts_with(source));
        let mut restored = info("localhost", None);
        store.apply(&mut restored).unwrap();
        assert_eq!(restored.redis_settings().password(), Some(password));
        store.save(&connection).unwrap();
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&store.path).unwrap().permissions().mode()
                    & 0o777,
                0o600
            );
        }
        let connection = connection.set_redis_settings(
            redis::RedisConnectionInfo::default()
                .set_username("alice")
                .set_password("next"),
        );
        store.save(&connection).unwrap();
        store
            .save(&info("localhost", Some("default-user")))
            .unwrap();
        assert!(
            store.read().unwrap().0.servers["redis://localhost:7000"]
                .user
                .is_none()
        );
    }

    #[test]
    fn corrupt_or_locked_files_are_not_overwritten_or_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Always,
        };
        for source in [
            "password = 'do-not-echo'",
            "[servers.\"redis://localhost:7000\"]\npassword = do-not-echo",
        ] {
            std::fs::write(&store.path, source).unwrap();
            let error = store
                .save(&info("localhost", Some("new")))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("do-not-echo"));
            assert_eq!(std::fs::read_to_string(&store.path).unwrap(), source);
            assert!(!store.path.with_extension("toml.lock").exists());
        }
        std::fs::write(&store.path, "").unwrap();
        std::fs::write(store.path.with_extension("toml.lock"), "").unwrap();
        assert!(store.save(&info("localhost", Some("new"))).is_err());
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "");
        assert!(store.path.with_extension("toml.lock").exists());
    }

    #[test]
    fn accounts_are_selected_explicitly_first_or_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Always,
        };
        let account = |user: &str, password: Option<&str>| {
            let connection = info("localhost", password);
            let settings =
                connection.redis_settings().clone().set_username(user);
            connection.set_redis_settings(settings)
        };
        store.save(&account("bob", Some("bob-secret"))).unwrap();
        let mut single = info("localhost", None);
        store.apply(&mut single).unwrap();
        assert_eq!(single.redis_settings().username(), Some("bob"));
        assert_eq!(single.redis_settings().password(), Some("bob-secret"));

        store.save(&account("alice", Some("alice-secret"))).unwrap();
        store
            .save(&info("localhost", Some("default-secret")))
            .unwrap();
        let mut first = info("localhost", None);
        store.apply(&mut first).unwrap();
        assert_eq!(first.redis_settings().username(), Some("alice"));
        assert_eq!(first.redis_settings().password(), Some("alice-secret"));
        for (user, password) in [
            ("alice", "alice-secret"),
            ("bob", "bob-secret"),
            ("default", "default-secret"),
        ] {
            let mut selected = account(user, None);
            store.apply(&mut selected).unwrap();
            assert_eq!(selected.redis_settings().password(), Some(password));
        }
        store.set_default_user(single.addr(), "bob").unwrap();
        store.save(&account("bob", Some("updated"))).unwrap();
        let mut selected = info("localhost", None);
        store.apply(&mut selected).unwrap();
        assert_eq!(selected.redis_settings().username(), Some("bob"));
        assert_eq!(selected.redis_settings().password(), Some("updated"));
        let mut explicit = account("alice", None);
        store.apply(&mut explicit).unwrap();
        assert_eq!(explicit.redis_settings().password(), Some("alice-secret"));
        let mut explicit = info("localhost", Some("explicit"));
        store.apply(&mut explicit).unwrap();
        assert!(explicit.redis_settings().username().is_none());
        assert_eq!(explicit.redis_settings().password(), Some("explicit"));

        store.set_default_user(single.addr(), "default").unwrap();
        let mut selected = info("localhost", None);
        store.apply(&mut selected).unwrap();
        assert!(selected.redis_settings().username().is_none());
        assert_eq!(
            selected.redis_settings().password(),
            Some("default-secret")
        );
        let before = std::fs::read_to_string(&store.path).unwrap();
        assert!(store.set_default_user(single.addr(), "missing").is_err());
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), before);
    }

    #[test]
    fn legacy_acl_account_survives_saving_password_only_and_named_users() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Always,
        };
        std::fs::write(&store.path, "[servers.\"redis://localhost:7000\"]\nuser = 'alice'\npassword = 'old'\ndefault-user = 'alice'\n").unwrap();
        store
            .save(&info("localhost", Some("password-only")))
            .unwrap();
        let mut selected = info("localhost", None);
        store.apply(&mut selected).unwrap();
        assert_eq!(selected.redis_settings().username(), Some("alice"));
        assert_eq!(selected.redis_settings().password(), Some("old"));
        store
            .save(
                &selected.set_redis_settings(
                    redis::RedisConnectionInfo::default()
                        .set_username("alice")
                        .set_password("new"),
                ),
            )
            .unwrap();
        let (file, _) = store.read().unwrap();
        let accounts = file.servers["redis://localhost:7000"].credentials();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts["alice"], (Some("alice"), "new"));
        assert_eq!(accounts["default"], (None, "password-only"));
    }

    #[test]
    fn invalid_accounts_and_defaults_are_rejected_without_leaking_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("auth.toml"),
            policy: SaveAuth::Always,
        };
        for fields in [
            "user = 'secret'",
            "password = 'secret'\ndefault-user = 'missing'",
            "password = 'secret'\nusers = {default = 'secret'}",
            "user = 'alice'\npassword = 'secret'\nusers = {alice = 'secret'}",
            "users = {alice = 123}",
        ] {
            let source =
                format!("[servers.\"redis://localhost:7000\"]\n{fields}\n");
            std::fs::write(&store.path, &source).unwrap();
            assert!(store.apply(&mut info("localhost", None)).is_err());
            let error =
                store.save(&info("localhost", Some("new"))).unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert!(
                store
                    .set_default_user(info("localhost", None).addr(), "alice")
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&store.path).unwrap(), source);
        }
    }
}
