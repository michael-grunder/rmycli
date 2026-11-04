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
    servers: BTreeMap<String, Credentials>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    user: Option<String>,
    password: String,
}

fn endpoint(addr: &ConnectionAddr) -> io::Result<String> {
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
                "invalid auth file; expected [servers.\"redis://host:port\"] with user (optional) and password",
            )
        };
        let parsed: AuthFile =
            toml::from_str(&source).map_err(|_| invalid())?;
        if parsed.servers.keys().any(|key| key.trim().is_empty()) {
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
            if settings.username().is_some_and(|user| {
                user != saved.user.as_deref().unwrap_or("default")
            }) {
                return Ok(());
            }
            let mut settings = settings.clone().set_password(&saved.password);
            if let Some(user) = &saved.user {
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
        let key = endpoint(info.addr())?;
        if file.servers.get(&key).is_some_and(|saved| {
            saved.user.as_deref() == settings.username()
                && saved.password == password
        }) {
            return Ok(());
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
        if let Some(user) = settings.username() {
            entry.insert("user", value(user));
        } else {
            entry.remove("user");
        }
        entry.insert("password", value(password));
        // NamedTempFile starts owner-only on Unix, including replacements.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
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
}
