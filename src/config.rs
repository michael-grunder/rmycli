//! User settings, separate from the active Redis session's connection state.
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::style::Color;
use serde::{Deserialize, Deserializer, de::Error};

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    pub auto_cluster: bool,
    pub history_per_server: bool,
    pub save_auth: crate::auth::SaveAuth,
    pub theme: Theme,
    pub instances: BTreeMap<String, Instance>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_cluster: true,
            history_per_server: true,
            save_auth: crate::auth::SaveAuth::default(),
            theme: Theme::default(),
            instances: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn load(path: Option<&Path>, disabled: bool) -> io::Result<Self> {
        if disabled {
            return Ok(Self::default());
        }
        Self::find(path)?
            .map_or_else(|| Ok(Self::default()), |path| Self::read(&path))
    }

    pub fn find(path: Option<&Path>) -> io::Result<Option<PathBuf>> {
        if let Some(path) = path {
            return Ok(Some(path.to_owned()));
        }
        let dirs = xdg::BaseDirectories::new();
        let paths = dirs
            .get_config_home()
            .into_iter()
            .chain(dirs.get_config_dirs())
            .map(|dir| dir.join("rmycli.toml"));
        for path in paths {
            match std::fs::metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => return Ok(Some(path)),
            }
        }
        Ok(None)
    }

    fn read(path: &Path) -> io::Result<Self> {
        let source = std::fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read config {}: {error}", path.display()),
            )
        })?;
        Self::parse(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid config {}: {error}", path.display()),
            )
        })
    }

    pub fn parse(source: &str) -> Result<Self, String> {
        // Do not print the source excerpt: it may contain credentials.
        let settings: Self = toml::from_str(source)
            .map_err(|error: toml::de::Error| error.message().to_owned())?;
        for (name, instance) in &settings.instances {
            if name.trim().is_empty() {
                return Err("instance names must not be empty".into());
            }
            instance
                .validate()
                .map_err(|error| format!("instance {name:?}: {error}"))?;
        }
        Ok(settings)
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Theme {
    pub inline_hint: Color,
    #[serde(deserialize_with = "command_colors")]
    pub commands: BTreeMap<String, Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            inline_hint: Color::DarkGrey,
            commands: BTreeMap::new(),
        }
    }
}

impl Theme {
    /// The caller supplies the canonical command name from server metadata.
    pub fn hint_color(&self, command: &str) -> Color {
        let mut name = command;
        loop {
            if let Some(color) = self.commands.get(name) {
                return *color;
            }
            let Some((parent, _)) = name.rsplit_once(' ') else {
                return self.inline_hint;
            };
            name = parent;
        }
    }
}

fn command_colors<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Color>, D::Error>
where
    D: Deserializer<'de>,
{
    let colors = BTreeMap::<String, Color>::deserialize(deserializer)?;
    let mut normalized = BTreeMap::new();
    for (name, color) in colors {
        let name = normalize_command(&name);
        if name.is_empty() {
            return Err(D::Error::custom("command names must not be empty"));
        }
        if normalized.insert(name.clone(), color).is_some() {
            return Err(D::Error::custom(format!(
                "duplicate command color for {name:?}"
            )));
        }
    }
    Ok(normalized)
}

pub fn normalize_command(name: &str) -> String {
    name.split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Instance {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub db: Option<i64>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub password_env: Option<String>,
    pub tls: bool,
}

impl Instance {
    fn validate(&self) -> Result<(), &'static str> {
        if self
            .host
            .as_ref()
            .is_some_and(|host| host.trim().is_empty())
        {
            return Err("host must not be empty");
        }
        if self.port == Some(0) {
            return Err("port must be between 1 and 65535");
        }
        if self.db.is_some_and(|db| db < 0) {
            return Err("db must be nonnegative");
        }
        if self.password.is_some() && self.password_env.is_some() {
            return Err("use either password or password-env, not both");
        }
        if self
            .password_env
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.contains(['=', '\0']))
        {
            return Err(
                "password-env must be a valid environment variable name",
            );
        }
        Ok(())
    }

    pub fn password(
        &self,
        lookup: impl FnOnce(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        self.password_env.as_ref().map_or_else(
            || Ok(self.password.clone()),
            |name| {
                lookup(name).map(Some).ok_or_else(|| {
                    format!("password environment variable {name:?} is not set or is not Unicode")
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_settings() {
        let settings = Settings::parse("").unwrap();
        assert!(settings.auto_cluster);
        assert!(settings.history_per_server);
        assert!(
            !Settings::parse("history-per-server = false")
                .unwrap()
                .history_per_server
        );
        assert!(
            !Settings::parse("auto-cluster = false")
                .unwrap()
                .auto_cluster
        );
        assert_eq!(settings.theme.hint_color("GET"), Color::DarkGrey);
        assert!(settings.instances.is_empty());
        let settings =
            Settings::parse("[theme]\ninline-hint = '#12abEF'").unwrap();
        assert_eq!(
            settings.theme.hint_color("GET"),
            Color::Rgb {
                r: 18,
                g: 171,
                b: 239
            }
        );
    }

    #[test]
    fn command_colors_are_normalized_and_inherit_from_parent() {
        let settings = Settings::parse(
            "[theme.commands]\nget = 'cyan'\nxgroup = 'green'\n\
             ' xgroup   create ' = 'red'",
        )
        .unwrap();
        assert_eq!(settings.theme.hint_color("GET"), Color::Cyan);
        assert_eq!(settings.theme.hint_color("XGROUP CREATE"), Color::Red);
        assert_eq!(settings.theme.hint_color("XGROUP DESTROY"), Color::Green);
        assert_eq!(settings.theme.hint_color("GETEX"), Color::DarkGrey);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        for source in [
            "unknown = true",
            "auto-cluster = 'false'",
            "auto-cluster = 1",
            "history-per-server = 'false'",
            "history-per-server = 1",
            "[theme]\ninline-hnit = 'red'",
            "[theme]\ninline-hint = 'invisible'",
            "[theme]\ninline-hint = '#12345g'",
            "[theme]\ninline-hint = 12",
            "[theme.commands]\nget = 'red'\nGET = 'blue'",
            "[theme.commands]\n' ' = 'red'",
            "[instances.qa]\nport = 0",
            "[instances.qa]\nport = 65536",
            "[instances.qa]\ndb = -1",
            "[instances.qa]\nhost = ''",
            "[instances.qa]\npassword = 'secret'\npassword-env = 'QA_AUTH'",
            "[instances.qa]\npassword-env = ''",
            "[instances.qa]\npassword-env = 'A=B'",
            "[instances.qa]\nusername = 'alice'",
            "[instances.'']",
            "[broken",
        ] {
            assert!(Settings::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn passwords_are_resolved_only_when_requested() {
        let settings = Settings::parse(
            "[instances.qa]\nhost = 'qa.example'\nport = 6380\n\
             user = 'alice'\npassword-env = 'QA_AUTH'\ntls = true\n\
             [instances.local]\npassword = 'local-secret'",
        )
        .unwrap();
        let qa = &settings.instances["qa"];
        assert!(qa.password(|_| None).is_err());
        assert_eq!(
            qa.password(|name| {
                assert_eq!(name, "QA_AUTH");
                Some("env-secret".into())
            })
            .unwrap()
            .as_deref(),
            Some("env-secret")
        );
        assert_eq!(
            settings.instances["local"]
                .password(|_| None)
                .unwrap()
                .as_deref(),
            Some("local-secret")
        );
    }
}
