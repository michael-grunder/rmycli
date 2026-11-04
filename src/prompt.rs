//! Named prompt interpolation; styles stay separate from printable text.
use std::ops::Range;

use crossterm::style::Color;
use redis::{ConnectionAddr, ConnectionInfo};
use serde::Deserialize;

pub const DEFAULT: &str = "{color:cyan}{user}@{address}{database}{reset} {color:green}{ctrl}❯{reset} ";

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct Prompt {
    source: String,
    parts: Vec<Part>,
}

enum Part {
    Text(String),
    Field(String),
    Color(Color),
}

#[derive(Default)]
pub struct Rendered {
    pub text: String,
    pub colors: Vec<(Range<usize>, Color)>,
}

impl Default for Prompt {
    fn default() -> Self {
        Self::try_from(DEFAULT.to_owned()).expect("valid built-in prompt")
    }
}

impl TryFrom<String> for Prompt {
    type Error = String;

    fn try_from(source: String) -> Result<Self, Self::Error> {
        if source.chars().any(char::is_control) {
            return Err(
                "prompt must be a single line without control characters"
                    .into(),
            );
        }
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            if matches!(c, '{' | '}') && chars.peek() == Some(&c) {
                chars.next();
                literal.push(c);
            } else if c == '{' {
                if !literal.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut literal)));
                }
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => {
                            return Err("unclosed prompt placeholder".into());
                        }
                    }
                }
                parts.push(match name.as_str() {
                    "user" | "address" | "host" | "port" | "db"
                    | "database" | "transport" | "ctrl" => Part::Field(name),
                    "reset" => Part::Color(Color::Reset),
                    _ => {
                        let color = name
                            .strip_prefix("color:")
                            .ok_or("unknown prompt placeholder")?;
                        let color = Color::deserialize(
                            serde::de::value::StrDeserializer::<
                                serde::de::value::Error,
                            >::new(color),
                        )
                        .map_err(|_| "invalid prompt color")?;
                        Part::Color(color)
                    }
                });
            } else if c == '}' {
                return Err(
                    "unmatched prompt brace; use }} for a literal }".into()
                );
            } else {
                literal.push(c);
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Ok(Self { source, parts })
    }
}

impl Prompt {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn render(&self, info: &ConnectionInfo, ctrl: bool) -> Rendered {
        let mut rendered = Rendered::default();
        let mut color = Color::Reset;
        for part in &self.parts {
            let text = match part {
                Part::Color(next) => {
                    color = *next;
                    continue;
                }
                Part::Text(text) => text.clone(),
                Part::Field(name) => field(name, info, ctrl),
            };
            let start = rendered.text.len();
            // Connection values are data, never template syntax or terminal codes.
            for c in text.chars() {
                if c.is_control() {
                    rendered.text.extend(c.escape_default());
                } else {
                    rendered.text.push(c);
                }
            }
            let end = rendered.text.len();
            if start != end {
                if let Some((range, previous)) = rendered.colors.last_mut()
                    && *previous == color
                {
                    range.end = end;
                } else {
                    rendered.colors.push((start..end, color));
                }
            }
        }
        rendered
    }
}

fn field(name: &str, info: &ConnectionInfo, ctrl: bool) -> String {
    let settings = info.redis_settings();
    match name {
        "user" => settings.username().unwrap_or("default").to_owned(),
        "address" => info.addr().to_string(),
        "host" => match info.addr() {
            ConnectionAddr::Tcp(host, _)
            | ConnectionAddr::TcpTls { host, .. } => host.clone(),
            _ => info.addr().to_string(),
        },
        "port" => match info.addr() {
            ConnectionAddr::Tcp(_, port)
            | ConnectionAddr::TcpTls { port, .. } => port.to_string(),
            _ => String::new(),
        },
        "db" => settings.db().to_string(),
        "database" if settings.db() != 0 => format!("[{}]", settings.db()),
        "transport" => match info.addr() {
            ConnectionAddr::Tcp(..) => "redis",
            ConnectionAddr::TcpTls { .. } => "rediss",
            ConnectionAddr::Unix(_) => "unix",
            _ => "unknown",
        }
        .into(),
        "ctrl" if ctrl => "[Ctrl]".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::IntoConnectionInfo;

    #[test]
    fn fields_follow_connection_state_without_credentials_or_recursion() {
        let prompt = Prompt::try_from(
            "{{{user}}} {transport} {address} {host} {port} {db}{database}{ctrl}".to_owned(),
        ).unwrap();
        let info = "rediss://alice:secret@localhost:6380/2"
            .into_connection_info()
            .unwrap();
        assert_eq!(
            prompt.render(&info, true).text,
            "{alice} rediss localhost:6380 localhost 6380 2[2][Ctrl]"
        );
        let info = info.set_redis_settings(
            redis::RedisConnectionInfo::default().set_username("{db}\x1b[2J"),
        );
        assert_eq!(
            prompt.render(&info, false).text,
            "{{db}\\u{1b}[2J} rediss localhost:6380 localhost 6380 0"
        );
        let socket = ConnectionAddr::Unix("/tmp/redis.sock".into())
            .into_connection_info()
            .unwrap();
        assert_eq!(
            prompt.render(&socket, false).text,
            "{default} unix /tmp/redis.sock /tmp/redis.sock  0"
        );
    }

    #[test]
    fn colors_are_separate_from_unicode_text_and_support_theme_formats() {
        let info = "redis://localhost".into_connection_info().unwrap();
        let rendered = Prompt::try_from(
            "{color:#12abef}你{color:ansi_(117)}❯{reset} ".to_owned(),
        )
        .unwrap()
        .render(&info, false);
        assert_eq!(rendered.text, "你❯ ");
        assert_eq!(
            rendered.colors,
            vec![
                (
                    0..3,
                    Color::Rgb {
                        r: 18,
                        g: 171,
                        b: 239
                    }
                ),
                (3..6, Color::AnsiValue(117)),
                (6..7, Color::Reset),
            ]
        );
        assert_eq!(
            Prompt::default().render(&info, false).text,
            "default@localhost:6379 ❯ "
        );
    }

    #[test]
    fn invalid_templates_fail_but_empty_and_literal_braces_work() {
        for source in [
            "{",
            "}",
            "{}",
            "{password}",
            "{user",
            "{color:bogus}",
            "{color:#12345g}",
            "a\nb",
            "\x1b[31m",
        ] {
            assert!(Prompt::try_from(source.to_owned()).is_err(), "{source:?}");
        }
        let info = "redis://localhost".into_connection_info().unwrap();
        for (source, expected) in [("", ""), ("{{user}}", "{user}"), ("❯", "❯")]
        {
            assert_eq!(
                Prompt::try_from(source.to_owned())
                    .unwrap()
                    .render(&info, true)
                    .text,
                expected
            );
        }
    }
}
