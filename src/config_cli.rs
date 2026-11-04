//! Local settings commands; these never open a Redis connection.
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use toml_edit::{DocumentMut, Item, Key, Table, Value};

use crate::config::{Settings, normalize_command};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    name = "rmycli config",
    about = "Read or edit local TOML settings without connecting to Redis"
)]
struct Options {
    /// File to read or edit instead of the XDG configuration
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Show color samples (auto respects `NO_COLOR` and redirected stdout)
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: ColorMode,
    #[command(subcommand)]
    action: Action,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Action {
    /// Print a setting's effective value, including built-in defaults
    Get { key: String },
    /// Validate and save a setting; strings need only shell quoting
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Remove an explicit setting, restoring its default or inheritance
    Unset { key: String },
}

pub fn run(
    args: &[OsString],
    outer_path: Option<&Path>,
    disabled: bool,
) -> Result<()> {
    let options = Options::parse_from(args);
    if outer_path.is_some() && options.config.is_some() {
        return Err("specify --config only once".into());
    }
    let path = options.config.as_deref().or(outer_path);
    if disabled
        && (path.is_some() || !matches!(options.action, Action::Get { .. }))
    {
        return Err(
            "--no-config can only be used with config get and no file override"
                .into(),
        );
    }
    let key = match &options.action {
        Action::Get { key }
        | Action::Set { key, .. }
        | Action::Unset { key } => key,
    };
    let key = SettingKey::parse(key)?;
    let source_path = if disabled {
        None
    } else {
        Settings::find(path)?
    };
    let mut document = read_document(
        source_path.as_deref(),
        matches!(options.action, Action::Set { .. }),
    )?;
    let settings = Settings::parse(&document.to_string())?;
    let colors = match options.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR")
                    .is_none_or(|value| value.is_empty())
        }
    };
    crossterm::style::force_color_output(colors);
    match options.action {
        Action::Get { .. } => {
            display(&key, &document, &settings, colors, false)?;
        }
        Action::Set { value, .. } => {
            let value = key.value(&value)?;
            edit(&mut document, &key, Some(value))?;
            let settings = Settings::parse(&document.to_string())?;
            save(&write_path(path)?, &document)?;
            display(&key, &document, &settings, colors, true)?;
        }
        Action::Unset { .. } => {
            edit(&mut document, &key, None)?;
            Settings::parse(&document.to_string())?;
            save(&write_path(path)?, &document)?;
        }
    }
    Ok(())
}

fn read_document(
    path: Option<&Path>,
    allow_missing: bool,
) -> Result<DocumentMut> {
    let Some(path) = path else {
        return Ok(DocumentMut::new());
    };
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error)
            if allow_missing && error.kind() == io::ErrorKind::NotFound =>
        {
            String::new()
        }
        Err(error) => {
            return Err(format!(
                "cannot read config {}: {error}",
                path.display()
            )
            .into());
        }
    };
    Settings::parse(&source).map_err(|error| {
        format!("invalid config {}: {error}", path.display())
    })?;
    source
        .parse::<DocumentMut>()
        .map_err(|error| error.message().into())
}

fn write_path(explicit: Option<&Path>) -> Result<PathBuf> {
    explicit.map_or_else(
        || {
            xdg::BaseDirectories::new()
                .get_config_home()
                .map(|dir| dir.join("rmycli.toml"))
                .ok_or_else(|| {
                    "cannot determine user config directory; use --config"
                        .into()
                })
        },
        |path| Ok(path.to_owned()),
    )
}

fn save(path: &Path, document: &DocumentMut) -> Result<()> {
    // Follow existing symlinks so editing a managed dotfile keeps its link.
    let path = if path.try_exists()? {
        path.canonicalize()?
    } else {
        path.to_owned()
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = std::fs::metadata(&path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    temporary.write_all(document.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    Ok(())
}

struct SettingKey {
    parts: Vec<String>,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    SaveAuth,
    Color,
    String,
    Integer,
    Boolean,
    Password,
}

impl SettingKey {
    fn parse(key: &str) -> Result<Self> {
        let mut parts = Key::parse(key)
            .map_err(|_| "expected a dotted setting key; quote names containing dots or spaces")?
            .iter().map(|part| part.get().to_owned()).collect::<Vec<_>>();
        let names = parts.iter().map(String::as_str).collect::<Vec<_>>();
        let kind = match names.as_slice() {
            ["auto-cluster" | "history-per-server"] => Kind::Boolean,
            ["save-auth"] => Kind::SaveAuth,
            ["prompt"] => Kind::String,
            ["theme", "inline-hint"] => Kind::Color,
            ["theme", "commands", name] if !name.trim().is_empty() => {
                Kind::Color
            }
            ["instances", name, field] if !name.trim().is_empty() => {
                match *field {
                    "host" | "user" | "password-env" => Kind::String,
                    "port" | "db" => Kind::Integer,
                    "tls" => Kind::Boolean,
                    "password" => Kind::Password,
                    _ => return Err(format!("unknown setting {key:?}").into()),
                }
            }
            _ => return Err(format!("unknown setting {key:?}").into()),
        };
        if matches!(kind, Kind::Color) && parts.len() == 3 {
            parts[2] = normalize_command(&parts[2]);
        }
        Ok(Self { parts, kind })
    }

    fn value(&self, text: &str) -> Result<Value> {
        Ok(match self.kind {
            Kind::SaveAuth => match text {
                "false" => Value::from(false),
                "local" | "always" => Value::from(text),
                _ => {
                    return Err(
                        "save-auth requires false, local, or always".into()
                    );
                }
            },
            Kind::Integer => Value::from(
                text.parse::<i64>()
                    .map_err(|_| "this setting requires an integer")?,
            ),
            Kind::Boolean => Value::from(
                text.parse::<bool>()
                    .map_err(|_| "this setting requires true or false")?,
            ),
            Kind::Color | Kind::String | Kind::Password => Value::from(text),
        })
    }

    fn stored_parts(&self, document: &DocumentMut) -> Vec<String> {
        let mut parts = self.parts.clone();
        if matches!(self.kind, Kind::Color)
            && parts.len() == 3
            && let Some(table) = document
                .get("theme")
                .and_then(|theme| theme.get("commands"))
                .and_then(Item::as_table_like)
            && let Some((stored, _)) = table
                .iter()
                .find(|(name, _)| normalize_command(name) == parts[2])
        {
            stored.clone_into(&mut parts[2]);
        }
        parts
    }

    fn color(&self, settings: &Settings) -> Option<Color> {
        if !matches!(self.kind, Kind::Color) {
            return None;
        }
        Some(if self.parts.len() == 2 {
            settings.theme.inline_hint
        } else {
            settings.theme.hint_color(&self.parts[2])
        })
    }

    fn effective(
        &self,
        document: &DocumentMut,
        settings: &Settings,
    ) -> Result<String> {
        let parts = self.stored_parts(document);
        let mut item = document.as_item();
        for part in &parts {
            if let Some(child) = item.get(part) {
                item = child;
            } else {
                return self.default_value(settings);
            }
        }
        if let Some(text) = item.as_str() {
            return Ok(text.to_owned());
        }
        if let Some(number) = item.as_integer() {
            return Ok(number.to_string());
        }
        item.as_bool()
            .map(|value| value.to_string())
            .ok_or_else(|| "setting is not a scalar value".into())
    }

    fn default_value(&self, settings: &Settings) -> Result<String> {
        if self.parts == ["prompt"] {
            return Ok(settings.prompt.source().to_owned());
        }
        if self.parts == ["auto-cluster"] {
            return Ok(settings.auto_cluster.to_string());
        }
        if self.parts == ["history-per-server"] {
            return Ok(settings.history_per_server.to_string());
        }
        if matches!(self.kind, Kind::SaveAuth) {
            return Ok("false".into());
        }
        if let Some(color) = self.color(settings) {
            return Ok(serde_json::to_value(color)?
                .as_str()
                .ok_or("color has no string representation")?
                .to_owned());
        }
        if !settings.instances.contains_key(&self.parts[1]) {
            return Err(format!("unknown instance {:?}", self.parts[1]).into());
        }
        match self.parts[2].as_str() {
            "host" => Ok("127.0.0.1".into()),
            "port" => Ok("6379".into()),
            "db" => Ok("0".into()),
            "tls" => Ok("false".into()),
            _ => Err("setting is not set".into()),
        }
    }
}

fn edit(
    document: &mut DocumentMut,
    key: &SettingKey,
    value: Option<Value>,
) -> Result<()> {
    let parts = key.stored_parts(document);
    let mut parent = document.as_item_mut();
    for part in &parts[..parts.len() - 1] {
        let table = parent
            .as_table_like_mut()
            .ok_or("setting parent is not a table")?;
        if !table.contains_key(part) {
            if value.is_none() {
                return Err("setting is not explicitly set".into());
            }
            let mut child = Table::new();
            child.set_implicit(true);
            table.insert(part, Item::Table(child));
        }
        parent = table.get_mut(part).ok_or("missing setting table")?;
    }
    let table = parent
        .as_table_like_mut()
        .ok_or("setting parent is not a table")?;
    let leaf = parts.last().ok_or("missing setting key")?;
    if let Some(mut value) = value {
        if let Some(old) = table.get(leaf).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert(leaf, Item::Value(value));
    } else if table.remove(leaf).is_none() {
        return Err("setting is not explicitly set".into());
    }
    Ok(())
}

fn display(
    key: &SettingKey,
    document: &DocumentMut,
    settings: &Settings,
    colors: bool,
    updated: bool,
) -> Result<()> {
    let text = if updated && matches!(key.kind, Kind::Password) {
        "[redacted]".into()
    } else {
        key.effective(document, settings)?
    };
    let text = if io::stdout().is_terminal() {
        text.chars()
            .flat_map(char::escape_debug)
            .collect::<String>()
    } else {
        text
    };
    let mut out = io::stdout().lock();
    write!(out, "{text}")?;
    if colors && let Some(color) = key.color(settings) {
        queue!(
            out,
            Print("  "),
            SetForegroundColor(color),
            Print("sample text"),
            ResetColor
        )?;
    }
    writeln!(out)?;
    Ok(())
}
