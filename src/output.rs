//! redis-cli output modes. The terminal keeps control bytes escaped.
use std::io::{self, IsTerminal, Write};

use redis::Value;

#[derive(Clone, Debug, Default, clap::Args)]
#[group(id = "OutputOptions", multiple = true)]
// Clap models independent command-line switches as booleans.
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    /// Write reply bytes verbatim (default when stdout is not a terminal)
    #[arg(long, overrides_with_all = ["no_raw", "csv", "json", "quoted_json"])]
    pub raw: bool,
    /// Force human-readable replies, including when output is redirected
    #[arg(long, overrides_with_all = ["raw", "csv", "json", "quoted_json"])]
    pub no_raw: bool,
    /// Print comma-separated replies with redis-cli string escaping
    #[arg(long, overrides_with_all = ["raw", "no_raw", "json", "quoted_json"])]
    pub csv: bool,
    /// Print JSON replies (selects RESP3 unless -2 is given)
    #[arg(long, overrides_with_all = ["raw", "no_raw", "csv", "quoted_json"])]
    pub json: bool,
    /// Print JSON with ASCII-safe, byte-preserving string escapes
    #[arg(long, overrides_with_all = ["raw", "no_raw", "csv", "json"])]
    pub quoted_json: bool,
    /// Separator between raw array elements (default: newline)
    #[arg(short = 'd', value_name = "DELIMITER")]
    pub delimiter: Option<String>,
    /// Separator between raw replies (default: newline; use -D '' for none)
    #[arg(short = 'D', value_name = "DELIMITER")]
    pub response_delimiter: Option<String>,
}

impl Options {
    pub fn print(&self, reply: &Value) -> io::Result<()> {
        let mut out = io::stdout().lock();
        self.write(&mut out, reply, io::stdout().is_terminal())?;
        out.flush()
    }

    pub fn lines(&self, reply: &Value) -> Vec<String> {
        if let Value::Push { kind, data } = reply {
            return self.lines(&push_array(kind, data));
        }
        if self.csv || self.json || self.quoted_json {
            let mut bytes = Vec::new();
            self.write(&mut bytes, reply, true)
                .expect("writing to a Vec cannot fail");
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(str::to_owned)
                .collect()
        } else {
            // Raw control bytes would corrupt the live editor's cursor state.
            crate::session::format_reply(reply)
        }
    }

    pub fn write(
        &self,
        out: &mut impl Write,
        reply: &Value,
        terminal: bool,
    ) -> io::Result<()> {
        if let Value::Push { kind, data } = reply {
            return self.write(out, &push_array(kind, data), terminal);
        }
        if self.json || self.quoted_json {
            json_reply(out, reply, self.quoted_json)?;
        } else if self.csv {
            csv_reply(out, reply)?;
        } else if self.raw || (!self.no_raw && !terminal) {
            raw_reply(out, reply, self.delimiter.as_deref().unwrap_or("\n"))?;
            return out.write_all(
                self.response_delimiter
                    .as_deref()
                    .unwrap_or("\n")
                    .as_bytes(),
            );
        } else {
            for line in crate::session::format_reply(reply) {
                writeln!(out, "{line}")?;
            }
            return Ok(());
        }
        writeln!(out)
    }
}

fn push_array(kind: &redis::PushKind, data: &[Value]) -> Value {
    let mut values = Vec::with_capacity(data.len() + 1);
    values.push(Value::SimpleString(kind.to_string()));
    values.extend_from_slice(data);
    Value::Array(values)
}

fn raw_reply(
    out: &mut impl Write,
    value: &Value,
    delimiter: &str,
) -> io::Result<()> {
    match value {
        Value::Nil => Ok(()),
        Value::Okay => out.write_all(b"OK"),
        Value::BulkString(bytes) => out.write_all(bytes),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            out.write_all(s.as_bytes())
        }
        Value::Int(n) => write!(out, "{n}"),
        Value::Double(n) => write!(out, "{n}"),
        Value::BigNumber(n) => write!(out, "{n}"),
        Value::Boolean(b) => {
            out.write_all(if *b { b"(true)" } else { b"(false)" })
        }
        Value::Array(values)
        | Value::Set(values)
        | Value::Push { data: values, .. } => {
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.write_all(delimiter.as_bytes())?;
                }
                raw_reply(out, value, delimiter)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.write_all(delimiter.as_bytes())?;
                }
                raw_reply(out, key, delimiter)?;
                out.write_all(b" ")?;
                raw_reply(out, value, delimiter)?;
            }
            Ok(())
        }
        Value::Attribute { data, .. } => raw_reply(out, data, delimiter),
        Value::ServerError(error) => {
            write!(out, "{}", redis::RedisError::from(error.clone()))
        }
    }
}

fn quoted(bytes: &[u8]) -> String {
    let quoted = crate::input::quote(bytes);
    if quoted.starts_with('"') {
        quoted
    } else {
        format!("\"{quoted}\"")
    }
}

fn csv_reply(out: &mut impl Write, value: &Value) -> io::Result<()> {
    match value {
        Value::Nil => out.write_all(b"NULL"),
        Value::Okay => out.write_all(b"\"OK\""),
        Value::BulkString(bytes) => out.write_all(quoted(bytes).as_bytes()),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            out.write_all(quoted(s.as_bytes()).as_bytes())
        }
        Value::Boolean(b) => write!(out, "{b}"),
        Value::Array(values)
        | Value::Set(values)
        | Value::Push { data: values, .. } => {
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                csv_reply(out, value)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                csv_reply(out, key)?;
                out.write_all(b",")?;
                csv_reply(out, value)?;
            }
            Ok(())
        }
        Value::Attribute { data, .. } => csv_reply(out, data),
        Value::ServerError(error) => write!(
            out,
            "ERROR,{}",
            quoted(
                redis::RedisError::from(error.clone())
                    .to_string()
                    .as_bytes()
            )
        ),
        _ => raw_reply(out, value, ","),
    }
}

fn json_string(
    out: &mut impl Write,
    bytes: &[u8],
    ascii: bool,
) -> io::Result<()> {
    if ascii {
        let escaped = quoted(bytes);
        // Quoted JSON represents Redis's byte escapes as literal JSON strings.
        serde_json::to_writer(out, &escaped[1..escaped.len() - 1])?;
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        serde_json::to_writer(out, text)?;
    } else {
        // Invalid UTF-8 must not silently become replacement characters.
        return json_string(out, bytes, true);
    }
    Ok(())
}

fn json_reply(
    out: &mut impl Write,
    value: &Value,
    ascii: bool,
) -> io::Result<()> {
    match value {
        Value::Nil => out.write_all(b"null"),
        Value::Okay => json_string(out, b"OK", ascii),
        Value::BulkString(bytes) => json_string(out, bytes, ascii),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            json_string(out, s.as_bytes(), ascii)
        }
        Value::Boolean(b) => write!(out, "{b}"),
        Value::Double(n) if !n.is_finite() => {
            json_string(out, n.to_string().as_bytes(), ascii)
        }
        Value::Array(values)
        | Value::Set(values)
        | Value::Push { data: values, .. } => {
            out.write_all(b"[")?;
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                json_reply(out, value, ascii)?;
            }
            out.write_all(b"]")
        }
        Value::Map(entries) => {
            out.write_all(b"{")?;
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                match key {
                    Value::BulkString(_)
                    | Value::SimpleString(_)
                    | Value::VerbatimString { .. }
                    | Value::Okay => json_reply(out, key, ascii)?,
                    _ => {
                        let mut bytes = Vec::new();
                        json_reply(&mut bytes, key, ascii)?;
                        json_string(out, &bytes, false)?;
                    }
                }
                out.write_all(b":")?;
                json_reply(out, value, ascii)?;
            }
            out.write_all(b"}")
        }
        Value::Attribute { data, .. } => json_reply(out, data, ascii),
        Value::ServerError(error) => {
            out.write_all(b"{\"error\":")?;
            json_string(
                out,
                redis::RedisError::from(error.clone())
                    .to_string()
                    .as_bytes(),
                ascii,
            )?;
            out.write_all(b"}")
        }
        _ => raw_reply(out, value, ","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_delimiters_preserve_bytes_and_empty_values() {
        let options = Options {
            delimiter: Some("|".into()),
            response_delimiter: Some(String::new()),
            ..Options::default()
        };
        let mut bytes = Vec::new();
        options
            .write(
                &mut bytes,
                &Value::Array(vec![
                    Value::BulkString(vec![0, 255]),
                    Value::Nil,
                    Value::Int(42),
                ]),
                false,
            )
            .unwrap();
        assert_eq!(bytes, b"\0\xff||42");
        bytes.clear();
        options
            .write(
                &mut bytes,
                &Value::Map(vec![(Value::Int(1), Value::Int(2))]),
                false,
            )
            .unwrap();
        assert_eq!(bytes, b"1 2");
    }

    #[test]
    fn structured_formats_handle_resp3_and_binary_strings() {
        let value = Value::Map(vec![(
            Value::Int(1),
            Value::Set(vec![
                Value::Boolean(true),
                Value::Nil,
                Value::BulkString(vec![255]),
            ]),
        )]);
        let mut bytes = Vec::new();
        Options {
            json: true,
            ..Options::default()
        }
        .write(&mut bytes, &value, false)
        .unwrap();
        let decoded: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["1"], serde_json::json!([true, null, "\\xff"]));
        bytes.clear();
        Options {
            csv: true,
            ..Options::default()
        }
        .write(&mut bytes, &value, false)
        .unwrap();
        assert_eq!(bytes, b"1,true,NULL,\"\\xff\"\n");
    }
}
