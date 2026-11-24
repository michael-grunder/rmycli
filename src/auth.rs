//! Opt-in credential persistence, matched to an exact transport and endpoint.
use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::auth_vault::Vault;
use redis::{ConnectionAddr, ConnectionInfo};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::sync::{Arc, Mutex};
use toml_edit::{DocumentMut, Item, Table, value};
use zeroize::Zeroizing;

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
    vault: Option<Arc<Mutex<Vault>>>,
    prompt: Arc<Mutex<Option<std::sync::mpsc::Sender<crate::secret::Request>>>>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthFile {
    servers: BTreeMap<String, Server>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct Server {
    user: Option<String>,
    password: Option<Zeroizing<String>>,
    users: BTreeMap<String, Zeroizing<String>>,
    default_user: Option<String>,
}

impl AuthFile {
    /// Number of saved accounts across all endpoints.
    pub fn accounts(&self) -> usize {
        self.servers
            .values()
            .map(|server| server.credentials().len())
            .sum()
    }

    /// Add accounts from `other` that `self` lacks. An account saved in both
    /// with different passwords is a conflict and leaves `self` unchanged;
    /// `self` keeps its default user when both name one.
    pub fn merge(&mut self, other: Self) -> io::Result<()> {
        let conflicts = other
            .servers
            .iter()
            .filter_map(|(key, server)| {
                Some((key, server, self.servers.get(key)?.credentials()))
            })
            .flat_map(|(key, server, saved)| {
                server
                    .credentials()
                    .into_iter()
                    .filter(move |(name, (_, password))| {
                        saved
                            .get(name)
                            .is_some_and(|(_, existing)| existing != password)
                    })
                    .map(move |(name, _)| {
                        format!(
                            "{} (user '{}')",
                            key.escape_default(),
                            name.escape_default()
                        )
                    })
            })
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            return Err(io::Error::other(format!(
                "both credential stores have different passwords for {}; remove one copy and retry",
                conflicts.join(", ")
            )));
        }
        let mut merged = self.clone();
        for (key, server) in other.servers {
            let Some(target) = merged.servers.get_mut(&key) else {
                merged.servers.insert(key, server);
                continue;
            };
            for (name, (user, password)) in server.credentials() {
                if !target.credentials().contains_key(name) {
                    target.add(user, password);
                }
            }
            if target.default_user.is_none() {
                target.default_user.clone_from(&server.default_user);
            }
        }
        merged.validate()?;
        *self = merged;
        Ok(())
    }

    /// Render as a plaintext auth file using the documented layout.
    fn to_document(&self) -> DocumentMut {
        let mut servers = Table::new();
        servers.set_implicit(true);
        for (key, server) in &self.servers {
            let mut table = Table::new();
            if let Some(user) = &server.user {
                table.insert("user", value(user.as_str()));
            }
            if let Some(password) = &server.password {
                table.insert("password", value(password.as_str()));
            }
            if let Some(user) = &server.default_user {
                table.insert("default-user", value(user.as_str()));
            }
            if !server.users.is_empty() {
                let mut users = Table::new();
                for (user, password) in &server.users {
                    users.insert(user, value(password.as_str()));
                }
                table.insert("users", Item::Table(users));
            }
            servers.insert(key, Item::Table(table));
        }
        let mut document = DocumentMut::new();
        if !servers.is_empty() {
            document.insert("servers", Item::Table(servers));
        }
        document
    }

    pub fn validate(&self) -> io::Result<()> {
        if self
            .servers
            .iter()
            .any(|(key, server)| key.trim().is_empty() || !server.valid())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid saved credential accounts or default user",
            ));
        }
        Ok(())
    }
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

    /// Add an account whose name is not yet saved for this endpoint.
    fn add(&mut self, user: Option<&str>, password: &str) {
        let password = Zeroizing::new(password.to_owned());
        if let Some(user) = user {
            self.users.insert(user.to_owned(), password);
            return;
        }
        // A password-only account replaces the legacy single-account slot;
        // its previous named account moves to the users table.
        if let Some(old) = self.password.take() {
            let old_user = self.user.take().unwrap_or_else(|| "default".into());
            self.users.insert(old_user, old);
        }
        self.password = Some(password);
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
    pub fn plaintext(path: PathBuf, policy: SaveAuth) -> Self {
        Self {
            path,
            policy,
            vault: None,
            prompt: Arc::default(),
        }
    }

    pub fn encrypted(path: PathBuf, policy: SaveAuth) -> Self {
        Self {
            path,
            policy,
            vault: Some(Arc::new(Mutex::new(Vault::default()))),
            prompt: Arc::default(),
        }
    }

    pub fn discover(
        policy: SaveAuth,
        encrypted: bool,
    ) -> io::Result<Option<Self>> {
        let Some(dir) = xdg::BaseDirectories::new().get_config_home() else {
            return Ok(None);
        };
        let path = dir.join("rmycli/auth.enc");
        let exists = match std::fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        Ok(Some(if encrypted || exists {
            Self::encrypted(path, policy)
        } else {
            Self::plaintext(dir.join("rmycli-auth.toml"), policy)
        }))
    }

    pub fn prompt_channel(
        &self,
    ) -> io::Result<std::sync::mpsc::Receiver<crate::secret::Request>> {
        let (sender, receiver) = crate::secret::channel();
        *self
            .prompt
            .lock()
            .map_err(|_| io::Error::other("credential prompt unavailable"))? =
            Some(sender);
        Ok(receiver)
    }

    fn prompt(&self, create: bool) -> io::Result<Zeroizing<String>> {
        let sender = self
            .prompt
            .lock()
            .map_err(|_| io::Error::other("credential prompt unavailable"))?
            .clone();
        crate::secret::request(sender.as_ref(), create)
    }

    pub fn unlock(&self) -> io::Result<()> {
        if let Some(vault) = &self.vault {
            vault
                .lock()
                .map_err(|_| io::Error::other("credential store unavailable"))?
                .unlock(&self.path, |create| self.prompt(create))?;
        }
        Ok(())
    }

    pub fn lock(&self) -> io::Result<()> {
        let vault = self.vault.as_ref().ok_or_else(|| {
            io::Error::other("credential storage is not encrypted")
        })?;
        vault
            .lock()
            .map_err(|_| io::Error::other("credential store unavailable"))?
            .lock();
        Ok(())
    }

    pub fn change_passphrase(&self) -> io::Result<()> {
        let vault = self.vault.as_ref().ok_or_else(|| {
            io::Error::other("credential storage is not encrypted")
        })?;
        self.unlock()?;
        let passphrase = self.prompt(true)?;
        vault
            .lock()
            .map_err(|_| io::Error::other("credential store unavailable"))?
            .change_passphrase(&self.path, passphrase.as_bytes())
    }

    fn vault(&self) -> io::Result<std::sync::MutexGuard<'_, Vault>> {
        self.vault
            .as_ref()
            .ok_or_else(|| {
                io::Error::other("credential storage is not encrypted")
            })?
            .lock()
            .map_err(|_| io::Error::other("credential store unavailable"))
    }

    /// Merge the plaintext `source` into this encrypted store and verify the
    /// written file. Returns the source text that was migrated, so callers
    /// can confirm it is unchanged before deleting it.
    pub fn migrate(&self, source: &Self) -> io::Result<Zeroizing<String>> {
        let (file, text) = source.read_file()?;
        self.unlock()?;
        let mut vault = self.vault()?;
        vault.merge(&self.path, file)?;
        vault.verify(&self.path)?;
        drop(vault);
        Ok(text)
    }

    /// Unlock this encrypted store and copy its credentials.
    pub fn export(&self) -> io::Result<AuthFile> {
        self.unlock()?;
        Ok(self.vault()?.file()?.clone())
    }

    /// Merge `file` into this plaintext store. `file`'s default users win
    /// over existing ones; conflicting passwords write nothing.
    pub fn import(&self, file: AuthFile) -> io::Result<()> {
        crate::locked_file::replace(&self.path, "toml.lock", |_| {
            let (existing, _) = self.read_file()?;
            let mut merged = file;
            merged.merge(existing)?;
            let text = Zeroizing::new(merged.to_document().to_string());
            Ok(Some(text.as_bytes().to_vec()))
        })?;
        // The rename must be durable before the encrypted copy is deleted.
        sync_parent(&self.path)
    }

    /// Delete this plaintext store if it still holds exactly `expected`.
    pub fn remove_plaintext(&self, expected: &str) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            result => result?,
        };
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(io::Error::other(
                "the plaintext auth file is a symlink or has other hard links; delete every copy yourself",
            ));
        }
        crate::locked_file::replace(&self.path, "toml.lock", |path| {
            if *Zeroizing::new(std::fs::read_to_string(path)?) != expected {
                return Err(io::Error::other(
                    "the plaintext auth file changed during migration; run rmycli auth encrypt again",
                ));
            }
            std::fs::remove_file(path)?;
            Ok(None)
        })?;
        sync_parent(&self.path)
    }

    /// Delete this encrypted store if it is unchanged since unlocking.
    pub fn remove_encrypted(&self) -> io::Result<()> {
        self.vault()?.remove(&self.path)
    }

    fn read_file(&self) -> io::Result<(AuthFile, Zeroizing<String>)> {
        let source =
            Zeroizing::new(match std::fs::read_to_string(&self.path) {
                Ok(source) => source,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    String::new()
                }
                Err(error) => return Err(error),
            });
        // Parser excerpts and field errors may contain passwords.
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid auth file; expected server tables with password and optional user, a users password table, and an optional default-user naming a saved account; duplicate accounts are not allowed",
            )
        };
        let parsed: AuthFile =
            toml::from_str(&source).map_err(|_| invalid())?;
        parsed.validate()?;
        Ok((parsed, source))
    }

    fn read(&self) -> io::Result<(AuthFile, DocumentMut)> {
        let (file, source) = self.read_file()?;
        let document = source.parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid auth file")
        })?;
        Ok((file, document))
    }

    pub fn usernames(&self, addr: &ConnectionAddr) -> io::Result<Vec<String>> {
        // Shell completion must never unlock the vault or prompt for secrets.
        if self.vault.is_some() {
            return Ok(Vec::new());
        }
        let (file, _) = self.read_file()?;
        Ok(file
            .servers
            .get(&endpoint(addr)?)
            .map(|server| {
                server
                    .credentials()
                    .keys()
                    .map(|user| (*user).to_owned())
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn apply(&self, info: &mut ConnectionInfo) -> io::Result<()> {
        if info.redis_settings().password().is_some() {
            return Ok(());
        }
        if let Some(vault) = &self.vault {
            // A missing encrypted file has no credentials to unlock yet.
            match std::fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            self.unlock()?;
            let vault = vault.lock().map_err(|_| {
                io::Error::other("credential store unavailable")
            })?;
            Self::apply_file(vault.file()?, info);
            drop(vault);
            return Ok(());
        }
        let (file, _) = self.read_file()?;
        Self::apply_file(&file, info);
        Ok(())
    }

    fn apply_file(file: &AuthFile, info: &mut ConnectionInfo) {
        let settings = info.redis_settings();
        let Ok(key) = endpoint(info.addr()) else {
            return;
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
                return;
            };
            let mut settings = settings.clone().set_password(*password);
            if let Some(user) = user {
                settings = settings.set_username(user);
            }
            *info = info.clone().set_redis_settings(settings);
        }
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
        if let Some(vault) = &self.vault {
            self.unlock()?;
            let mut vault = vault.lock().map_err(|_| {
                io::Error::other("credential store unavailable")
            })?;
            if vault.file()?.servers.get(&key).is_some_and(|server| {
                server.credentials().get(user).is_some_and(|account| {
                    *account == (settings.username(), password)
                })
            }) {
                return Ok(());
            }
            return vault.update(&self.path, |file| {
                let server = file.servers.entry(key).or_default();
                if let Some(old_password) = server.password.take() {
                    let old_user =
                        server.user.take().unwrap_or_else(|| "default".into());
                    server.users.insert(old_user, old_password);
                }
                if settings.username().is_none() {
                    server.users.remove("default");
                    server.password = Some(Zeroizing::new(password.to_owned()));
                } else {
                    server.users.insert(
                        user.to_owned(),
                        Zeroizing::new(password.to_owned()),
                    );
                }
                Ok(())
            });
        }
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
        if let Some(vault) = &self.vault {
            self.unlock()?;
            return vault.lock().map_err(|_| io::Error::other("credential store unavailable"))?
                .update(&self.path, |file| {
                    let server = file.servers.get_mut(&key)
                        .filter(|server| server.credentials().contains_key(user))
                        .ok_or_else(|| io::Error::other("no saved credentials for that user at this endpoint"))?;
                    server.default_user = Some(user.to_owned());
                    Ok(())
                });
        }
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
        crate::locked_file::replace(&self.path, "toml.lock", |_| {
            let (file, mut document) = self.read()?;
            Ok(edit(&file, &mut document)?
                .then(|| document.to_string().into_bytes()))
        })
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    Lock,
    Unlock,
    Passwd,
}

impl Action {
    pub fn run(self, store: &Store) -> io::Result<&'static str> {
        if store.vault.is_none() {
            return Err(io::Error::other(
                "credential storage is not encrypted",
            ));
        }
        match self {
            Self::Lock => {
                store.lock()?;
                Ok(
                    "Credential store locked; active Redis connections retain their credentials",
                )
            }
            Self::Unlock => {
                store.unlock()?;
                Ok("Credential store unlocked")
            }
            Self::Passwd => {
                store.change_passphrase()?;
                Ok("Master passphrase changed")
            }
        }
    }
}

type CliResult<T> = Result<T, Box<dyn std::error::Error>>;

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

/// Both store locations under the user config directory.
fn stores(dir: &Path) -> (Store, Store) {
    (
        Store::plaintext(dir.join("rmycli-auth.toml"), SaveAuth::Never),
        Store::encrypted(dir.join("rmycli/auth.enc"), SaveAuth::Never),
    )
}

pub fn run(
    args: &[std::ffi::OsString],
    disabled: bool,
    config: Option<&Path>,
) -> CliResult<()> {
    let action = match args {
        [_, action] => action.to_str(),
        _ => None,
    };
    if !matches!(action, Some("encrypt" | "decrypt" | "passwd")) {
        return Err(
            "usage: rmycli auth encrypt | rmycli auth decrypt | rmycli auth passwd"
                .into(),
        );
    }
    if disabled {
        return Err("auth commands cannot be used with --no-config".into());
    }
    let dir = xdg::BaseDirectories::new()
        .get_config_home()
        .ok_or("cannot locate the user config directory")?;
    let (plaintext, encrypted) = stores(&dir);
    match action {
        Some("encrypt") => encrypt(&plaintext, &encrypted, config),
        Some("decrypt") => decrypt(&plaintext, &encrypted, config),
        _ => {
            if !encrypted.path.try_exists()? {
                return Err(
                    "no encrypted auth file exists; use auth encrypt first"
                        .into(),
                );
            }
            encrypted.change_passphrase()?;
            println!("Master passphrase changed");
            Ok(())
        }
    }
}

/// Plaintext is deleted last, so an interruption leaves the encrypted
/// store (which takes precedence) in use.
fn encrypt(
    plaintext: &Store,
    encrypted: &Store,
    config: Option<&Path>,
) -> CliResult<()> {
    let migrated = encrypted.migrate(plaintext)?;
    println!(
        "Encrypted credentials saved to {} and verified",
        encrypted.path.display()
    );
    if crate::config_cli::set_encrypt_auth(config, true)? {
        println!("Set encrypt-auth = true");
    }
    if !plaintext.path.try_exists()? {
        return Ok(());
    }
    let question = format!(
        "Delete the plaintext auth file {}?",
        plaintext.path.display()
    );
    if !crate::secret::confirm(&question, true)? {
        eprintln!(
            "Warning: the unencrypted auth file is still at {}. Delete it after verifying encrypted authentication works.",
            plaintext.path.display()
        );
        return Ok(());
    }
    plaintext.remove_plaintext(&migrated).map_err(|error| {
        format!(
            "{error}. Encrypted credentials are in use, but {} still exists",
            plaintext.path.display()
        )
    })?;
    println!("Deleted {}", plaintext.path.display());
    eprintln!(
        "Deleting a file does not securely erase it from disks or backups. If the plaintext file may have been copied, change those Redis passwords."
    );
    Ok(())
}

/// The encrypted file is deleted last, so an interruption leaves it in use
/// and the command can simply be run again.
fn decrypt(
    plaintext: &Store,
    encrypted: &Store,
    config: Option<&Path>,
) -> CliResult<()> {
    if !encrypted.path.try_exists()? {
        return Err("no encrypted auth file exists".into());
    }
    let file = encrypted.export()?;
    let question = format!(
        "Write {} saved password(s) as plaintext to {}?",
        file.accounts(),
        plaintext.path.display()
    );
    if !crate::secret::confirm(&question, false)? {
        return Err("decryption cancelled; nothing changed".into());
    }
    plaintext.import(file)?;
    println!(
        "Plaintext credentials saved to {}",
        plaintext.path.display()
    );
    if crate::config_cli::set_encrypt_auth(config, false)? {
        println!("Removed encrypt-auth = true from the config");
    }
    encrypted.remove_encrypted().map_err(|error| {
        format!(
            "{error}. Run rmycli auth decrypt again to merge newer credentials"
        )
    })?;
    println!("Deleted {}", encrypted.path.display());
    Ok(())
}

/// Explain credentials that an `encrypt-auth` change leaves behind.
pub fn storage_hint(encrypt: bool) -> Option<String> {
    storage_hint_in(&xdg::BaseDirectories::new().get_config_home()?, encrypt)
}

fn storage_hint_in(dir: &Path, encrypt: bool) -> Option<String> {
    let (plaintext, encrypted) = stores(dir);
    if encrypt {
        return match plaintext.read_file() {
            Ok((file, _)) if file.accounts() == 0 => None,
            Ok((file, _)) => Some(format!(
                "Note: {} saved account(s) in {} were not encrypted; run `rmycli auth encrypt` to move them.",
                file.accounts(),
                plaintext.path.display()
            )),
            Err(_) => Some(format!(
                "Note: {} could not be read and was not encrypted.",
                plaintext.path.display()
            )),
        };
    }
    std::fs::symlink_metadata(&encrypted.path).is_ok().then(|| {
        format!(
            "Note: {} still exists and takes precedence over this setting; run `rmycli auth decrypt` to switch to plaintext storage.",
            encrypted.path.display()
        )
    })
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
    fn encrypted_backend_preserves_accounts_defaults_and_shared_lock_state() {
        let dir = tempfile::tempdir().unwrap();
        let source =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Never);
        let text = "[servers.'redis://localhost:7000']\nuser = 'alice'\npassword = 'alice-secret'\ndefault-user = 'alice'\n";
        std::fs::write(&source.path, text).unwrap();
        let store = Store::encrypted(
            dir.path().join("private/auth.enc"),
            SaveAuth::Always,
        );
        store
            .vault
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .unlock(&store.path, |_| Ok(Zeroizing::new("master".into())))
            .unwrap();
        store.migrate(&source).unwrap();
        assert_eq!(std::fs::read_to_string(&source.path).unwrap(), text);
        store
            .save(&info("localhost", Some("default-secret")))
            .unwrap();
        let mut selected = info("localhost", None);
        store.apply(&mut selected).unwrap();
        assert_eq!(selected.redis_settings().username(), Some("alice"));
        assert_eq!(selected.redis_settings().password(), Some("alice-secret"));
        store.set_default_user(selected.addr(), "default").unwrap();
        selected = info("localhost", None);
        store.apply(&mut selected).unwrap();
        assert_eq!(
            selected.redis_settings().password(),
            Some("default-secret")
        );
        let before = std::fs::read(&store.path).unwrap();
        store.save(&selected).unwrap();
        assert_eq!(before, std::fs::read(&store.path).unwrap());
        assert!(store.set_default_user(selected.addr(), "missing").is_err());
        assert_eq!(before, std::fs::read(&store.path).unwrap());
        store.clone().lock().unwrap();
        assert!(!store.vault.as_ref().unwrap().lock().unwrap().is_unlocked());
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
        let mut store = Store::plaintext(
            dir.path().join("nested/auth.toml"),
            SaveAuth::Never,
        );
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Never);
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Always);
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Always);
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Always);
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Always);
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
        let store =
            Store::plaintext(dir.path().join("auth.toml"), SaveAuth::Always);
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

    fn writable(store: &Store) -> Store {
        Store {
            policy: SaveAuth::Always,
            ..store.clone()
        }
    }

    fn passphrase(store: &Store) {
        store
            .vault()
            .unwrap()
            .unlock(&store.path, |_| Ok(Zeroizing::new("master".into())))
            .unwrap();
    }

    fn parse(text: &str) -> AuthFile {
        let file: AuthFile = toml::from_str(text).unwrap();
        file.validate().unwrap();
        file
    }

    fn json(file: &AuthFile) -> serde_json::Value {
        serde_json::to_value(file).unwrap()
    }

    #[test]
    fn merging_adds_missing_accounts_and_rejects_conflicts() {
        let mut target =
            parse("[servers.'redis://a:1']\nuser = 'alice'\npassword = 'a1'\n");
        let source = parse(concat!(
            "[servers.'redis://a:1']\npassword = 'default-pw'\n",
            "default-user = 'default'\nusers = { alice = 'a1', bob = 'b1' }\n",
            "[servers.'redis://b:2']\npassword = 'b2'\n",
        ));
        target.merge(source.clone()).unwrap();
        assert_eq!(target.accounts(), 4);
        let a = &target.servers["redis://a:1"];
        assert_eq!(a.user, None);
        assert_eq!(
            a.password.as_deref().map(String::as_str),
            Some("default-pw")
        );
        assert_eq!(a.users["alice"].as_str(), "a1");
        assert_eq!(a.users["bob"].as_str(), "b1");
        assert_eq!(a.default_user.as_deref(), Some("default"));
        // Merging the same accounts again is a no-op.
        let before = json(&target);
        target.merge(source).unwrap();
        assert_eq!(json(&target), before);

        // The target's default user wins over the source's.
        let mut chosen = parse(
            "[servers.'redis://a:1']\ndefault-user = 'bob'\nusers = { bob = 'b1' }\n",
        );
        chosen
            .merge(parse(
                "[servers.'redis://a:1']\ndefault-user = 'carol'\nusers = { carol = 'c1' }\n",
            ))
            .unwrap();
        assert_eq!(
            chosen.servers["redis://a:1"].default_user.as_deref(),
            Some("bob")
        );

        let conflict = parse(concat!(
            "[servers.'redis://a:1']\nusers = { alice = 'other-secret' }\n",
            "[servers.'redis://c:3']\npassword = 'c3'\n",
        ));
        let error = target.merge(conflict).unwrap_err().to_string();
        assert!(error.contains("redis://a:1 (user 'alice')"), "{error}");
        assert!(!error.contains("secret"), "{error}");
        assert_eq!(json(&target), before);
    }

    #[test]
    fn plaintext_rendering_round_trips() {
        let file = parse(concat!(
            "[servers.'redis://localhost:6379']\nuser = 'alice'\n",
            "password = 'a \"quoted\" secret'\ndefault-user = 'bob'\n",
            "users = { bob = 'b1', 'dotted.name' = 'd1' }\n",
            "[servers.'unix:/tmp/redis.sock']\npassword = 'sock'\n",
        ));
        let text = file.to_document().to_string();
        assert_eq!(json(&parse(&text)), json(&file));
        assert_eq!(AuthFile::default().to_document().to_string(), "");
    }

    #[test]
    fn encrypt_merges_into_an_existing_store_and_deletes_only_unchanged_plaintext()
     {
        let dir = tempfile::tempdir().unwrap();
        let (plaintext, encrypted) = stores(dir.path());
        let text = "[servers.'redis://localhost:7000']\npassword = 'old'\n";
        std::fs::write(&plaintext.path, text).unwrap();
        passphrase(&encrypted);
        writable(&encrypted)
            .save(&info("localhost", Some("old")))
            .unwrap();
        writable(&encrypted)
            .save(&info("127.0.0.1", Some("loopback")))
            .unwrap();
        let migrated = encrypted.migrate(&plaintext).unwrap();
        assert_eq!(*migrated, text);
        assert_eq!(encrypted.export().unwrap().accounts(), 2);

        std::fs::write(&plaintext.path, format!("{text}# edited\n")).unwrap();
        assert!(
            plaintext
                .remove_plaintext(&migrated)
                .unwrap_err()
                .to_string()
                .contains("changed during migration")
        );
        assert!(plaintext.path.exists());
        let copy = dir.path().join("copy.toml");
        std::fs::hard_link(&plaintext.path, &copy).unwrap();
        assert!(plaintext.remove_plaintext("unused").is_err());
        std::fs::remove_file(&copy).unwrap();
        let mut current = Zeroizing::new(String::new());
        current.push_str(&std::fs::read_to_string(&plaintext.path).unwrap());
        plaintext.remove_plaintext(&current).unwrap();
        assert!(!plaintext.path.exists());
        plaintext.remove_plaintext(&current).unwrap();

        // A conflicting plaintext password leaves the encrypted file alone.
        std::fs::write(
            &plaintext.path,
            "[servers.'redis://localhost:7000']\npassword = 'new'\n",
        )
        .unwrap();
        let before = std::fs::read(&encrypted.path).unwrap();
        assert!(encrypted.migrate(&plaintext).is_err());
        assert_eq!(before, std::fs::read(&encrypted.path).unwrap());
    }

    #[test]
    fn decrypt_writes_plaintext_before_removing_the_encrypted_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (plaintext, encrypted) = stores(dir.path());
        passphrase(&encrypted);
        writable(&encrypted)
            .save(&info("localhost", Some("secret")))
            .unwrap();
        let mut named = info("localhost", Some("alice-pw"));
        named = named.clone().set_redis_settings(
            named.redis_settings().clone().set_username("alice"),
        );
        writable(&encrypted).save(&named).unwrap();
        encrypted.set_default_user(named.addr(), "alice").unwrap();
        std::fs::write(
            &plaintext.path,
            "[servers.'redis://other:7000']\npassword = 'kept'\n",
        )
        .unwrap();
        let file = encrypted.export().unwrap();

        // Another session's write makes removal refuse after plaintext exists.
        let racing = Store::encrypted(encrypted.path.clone(), SaveAuth::Always);
        passphrase(&racing);
        racing.save(&info("127.0.0.1", Some("late"))).unwrap();
        plaintext.import(file).unwrap();
        assert!(encrypted.remove_encrypted().is_err());
        assert!(encrypted.path.exists());
        // Rerunning merges the newer account; the identical ones are skipped.
        encrypted.lock().unwrap();
        passphrase(&encrypted);
        plaintext.import(encrypted.export().unwrap()).unwrap();
        encrypted.remove_encrypted().unwrap();
        assert!(!encrypted.path.exists());

        let mut selected = info("localhost", None);
        plaintext.apply(&mut selected).unwrap();
        assert_eq!(selected.redis_settings().username(), Some("alice"));
        assert_eq!(selected.redis_settings().password(), Some("alice-pw"));
        let mut late = info("127.0.0.1", None);
        plaintext.apply(&mut late).unwrap();
        assert_eq!(late.redis_settings().password(), Some("late"));
        let mut other = info("other", None);
        plaintext.apply(&mut other).unwrap();
        assert_eq!(other.redis_settings().password(), Some("kept"));
        assert_eq!(
            std::fs::metadata(&plaintext.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // Conflicts write nothing.
        let before = std::fs::read(&plaintext.path).unwrap();
        let conflict =
            parse("[servers.'redis://other:7000']\npassword = 'x'\n");
        assert!(plaintext.import(conflict).is_err());
        assert_eq!(before, std::fs::read(&plaintext.path).unwrap());
    }

    #[test]
    fn setting_hints_describe_credentials_left_in_the_other_store() {
        let dir = tempfile::tempdir().unwrap();
        let (plaintext, encrypted) = stores(dir.path());
        assert_eq!(storage_hint_in(dir.path(), true), None);
        assert_eq!(storage_hint_in(dir.path(), false), None);
        std::fs::write(
            &plaintext.path,
            "[servers.'redis://a:1']\npassword = 'p'\nusers = { bob = 'b' }\n",
        )
        .unwrap();
        let hint = storage_hint_in(dir.path(), true).unwrap();
        assert!(hint.contains("2 saved account(s)"), "{hint}");
        assert!(hint.contains("rmycli auth encrypt"), "{hint}");
        std::fs::write(&plaintext.path, "password = 'leaked'").unwrap();
        let hint = storage_hint_in(dir.path(), true).unwrap();
        assert!(!hint.contains("leaked"), "{hint}");
        passphrase(&encrypted);
        writable(&encrypted)
            .save(&info("localhost", Some("p")))
            .unwrap();
        let hint = storage_hint_in(dir.path(), false).unwrap();
        assert!(hint.contains("rmycli auth decrypt"), "{hint}");
    }
}
