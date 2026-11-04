use std::collections::BTreeMap;

use redis::{Connection, Value as Reply};
use serde_json::{Value, json};

use crate::grammar::{Atom, Grammar, flag, string};
use crate::input::Context;

#[derive(Clone, Debug, Default)]
pub struct Command {
    pub name: String,
    pub docs: Value,
    pub grammar: Grammar,
    pub specs: Vec<Value>,
    pub legacy: Option<(i64, i64, i64)>,
    pub movable: bool,
    pub live_docs: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub commands: BTreeMap<String, Command>,
    pub notices: Vec<String>,
}

fn json_reply(value: &Reply) -> Value {
    match value {
        Reply::Int(n) => json!(n),
        Reply::SimpleString(s) => json!(s),
        Reply::BulkString(s) => json!(String::from_utf8_lossy(s)),
        Reply::Array(values) | Reply::Set(values) => {
            Value::Array(values.iter().map(json_reply).collect())
        }
        Reply::Map(entries) => Value::Object(
            entries
                .iter()
                .filter_map(|(k, v)| {
                    json_reply(k)
                        .as_str()
                        .map(|k| (k.to_owned(), json_reply(v)))
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// RESP2 represents maps as alternating field/value arrays. Only normalize
/// documented map fields: arbitrary arrays must remain arrays.
fn object(value: &Value) -> Value {
    if value.is_object() {
        return value.clone();
    }
    value.as_array().map_or(Value::Null, |entries| {
        Value::Object(
            entries
                .as_chunks::<2>()
                .0
                .iter()
                .filter_map(|pair| {
                    pair[0].as_str().map(|k| (k.to_owned(), pair[1].clone()))
                })
                .collect(),
        )
    })
}

fn normalize(value: &Value) -> Value {
    let mut value = object(value);
    for field in ["arguments", "key_specs"] {
        if let Some(items) = value[field].as_array() {
            value[field] = Value::Array(items.iter().map(normalize).collect());
        }
    }
    for field in ["begin_search", "find_keys", "spec"] {
        if !value[field].is_null() {
            value[field] = normalize(&value[field]);
        }
    }
    value
}

impl Catalog {
    pub fn bundled() -> Self {
        let docs: Value =
            serde_json::from_str(include_str!("../data/commands.json"))
                .expect("bundled Redis documentation is valid JSON");
        let mut result = Self::default();
        if let Some(docs) = docs.as_object() {
            for (name, doc) in docs {
                result.add_doc(name, doc.clone(), false);
            }
        }
        result
    }

    fn add_doc(&mut self, name: &str, doc: Value, live: bool) {
        let name = name.replace('|', " ").to_ascii_uppercase();
        let entry = self.commands.entry(name.clone()).or_default();
        entry.name = name;
        entry.grammar = Grammar::from_docs(
            doc["arguments"].as_array().map_or(&[], Vec::as_slice),
        );
        if !live {
            entry.specs =
                doc["key_specs"].as_array().cloned().unwrap_or_default();
        }
        entry.docs = doc;
        entry.live_docs = live;
    }

    pub fn load(connection: &mut Connection) -> Self {
        let mut catalog = Self::bundled();
        match redis::cmd("COMMAND").query::<Reply>(connection) {
            Ok(reply) => {
                let mut live = Self::default();
                if let Some(entries) = json_reply(&reply).as_array() {
                    for entry in entries { live.add_command(entry, &catalog); }
                }
                if live.commands.is_empty() {
                    catalog.notices.push("COMMAND returned no catalog; using bundled help".into());
                } else {
                    catalog = live;
                }
            }
            Err(error) if error.code() == Some("NOAUTH") => {
                catalog.notices.push("Authentication required. Use AUTH [username] password; server completions and help will reload after successful authentication. Using bundled metadata until then.".into());
                return catalog;
            }
            Err(_) => catalog.notices.push("COMMAND unavailable; using bundled metadata (may differ from server)".into()),
        }
        match redis::cmd("COMMAND").arg("DOCS").query::<Reply>(connection) {
            Ok(reply) => catalog.add_live_docs(&json_reply(&reply)),
            Err(_) => catalog.notices.push(
                "COMMAND DOCS unavailable; using bundled help where known"
                    .into(),
            ),
        }
        catalog
    }

    fn add_command(&mut self, value: &Value, fallback: &Self) {
        let Some(parts) = value.as_array() else {
            return;
        };
        let Some(name) = parts.first().and_then(Value::as_str) else {
            return;
        };
        let name = name.replace('|', " ").to_ascii_uppercase();
        let mut command =
            fallback.commands.get(&name).cloned().unwrap_or_default();
        command.name.clone_from(&name);
        command.legacy = parts
            .get(3)
            .and_then(Value::as_i64)
            .zip(parts.get(4).and_then(Value::as_i64))
            .zip(parts.get(5).and_then(Value::as_i64))
            .map(|((first, last), step)| (first, last, step));
        command.movable = parts
            .get(2)
            .and_then(Value::as_array)
            .is_some_and(|flags| flags.iter().any(|f| f == "movablekeys"));
        // The connected server is authoritative for key positions.
        if let Some(specs) = parts.get(8).and_then(Value::as_array) {
            command.specs = specs.iter().map(normalize).collect();
        } else if !command.movable {
            command.specs.clear();
        }
        self.commands.insert(name, command);
        if let Some(subs) = parts.get(9).and_then(Value::as_array) {
            for sub in subs {
                self.add_command(sub, fallback);
            }
        }
    }

    fn add_live_docs(&mut self, value: &Value) {
        if let Some(docs) = object(value).as_object() {
            for (name, doc) in docs {
                let normalized = normalize(doc);
                self.add_doc(name, normalized.clone(), true);
                if !normalized["subcommands"].is_null() {
                    self.add_live_docs(&normalized["subcommands"]);
                }
            }
        }
    }

    pub fn resolve(&self, args: &[Vec<u8>]) -> Option<(&Command, usize)> {
        let mut name = String::new();
        let mut found = None;
        for (index, arg) in args.iter().enumerate() {
            if index != 0 {
                name.push(' ');
            }
            name.push_str(&String::from_utf8_lossy(arg).to_ascii_uppercase());
            if let Some(command) = self.commands.get(&name) {
                found = Some((command, index + 1));
            }
        }
        found
    }

    pub fn literals(&self, ctx: &Context) -> Vec<Vec<u8>> {
        let prefix = String::from_utf8_lossy(&ctx.prefix).to_ascii_uppercase();
        let before = ctx.args[..ctx.index]
            .iter()
            .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(" ");
        let mut results: Vec<_> = self
            .commands
            .keys()
            .filter_map(|name| {
                let words: Vec<_> = name.split(' ').collect();
                if words.len() <= ctx.index {
                    return None;
                }
                (words[..ctx.index].join(" ") == before
                    && words[ctx.index].starts_with(&prefix))
                .then(|| words[ctx.index].as_bytes().to_vec())
            })
            .collect();
        if let Some((command, words)) = self.resolve(&ctx.args[..ctx.index]) {
            results.extend(
                command
                    .expected(ctx, words)
                    .into_iter()
                    .filter_map(|atom| atom.token)
                    .filter(|token| {
                        token.to_ascii_uppercase().starts_with(&prefix)
                    })
                    .map(String::into_bytes),
            );
        }
        results.sort();
        results.dedup();
        results
    }

    pub fn help(&self, ctx: &Context) -> Vec<String> {
        let resolved = self.resolve(&ctx.args).or_else(|| {
            let literals = self.literals(ctx);
            if literals.len() != 1 {
                return None;
            }
            let mut name = ctx.args[..ctx.index]
                .iter()
                .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
                .collect::<Vec<_>>();
            name.push(
                String::from_utf8_lossy(&literals[0]).to_ascii_uppercase(),
            );
            self.commands
                .get(&name.join(" "))
                .map(|command| (command, name.len()))
        });
        let Some((command, words)) = resolved else {
            let names = self
                .literals(ctx)
                .iter()
                .take(12)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect::<Vec<_>>();
            return vec![names.join("  ")];
        };
        let mut help = vec![
            format!("{} {}", command.name, command.grammar.syntax),
            string(&command.docs, "summary").into(),
        ];
        let next = command
            .expected(ctx, words)
            .iter()
            .map(|a| a.label.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        if !next.is_empty() {
            help.push(format!(
                "Argument {}: {next}",
                ctx.index.saturating_sub(words) + 1
            ));
        }
        help.push(format!(
            "Since {} • {} • {}{}",
            string(&command.docs, "since"),
            string(&command.docs, "group"),
            string(&command.docs, "complexity"),
            if command.live_docs {
                ""
            } else {
                " (bundled help)"
            }
        ));
        if let Some(deprecated) = command.docs["deprecated_since"].as_str() {
            help.push(format!(
                "Deprecated since {deprecated}; {}",
                string(&command.docs, "replaced_by")
            ));
        }
        help
    }

    pub fn inline_hint(&self, line: &str, cursor: usize) -> String {
        if cursor != line.len() {
            return String::new();
        }
        let parsed = crate::input::parse(line);
        if parsed.error.is_some() {
            return String::new();
        }
        let args: Vec<_> =
            parsed.words.into_iter().map(|word| word.bytes).collect();
        let Some((command, words)) = self.resolve(&args) else {
            return String::new();
        };
        let remaining = command.grammar.remaining(&args[words..]);
        if remaining.is_empty() {
            return remaining;
        }
        let separator = if line.ends_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        format!("{separator}{remaining}")
    }
}

impl Command {
    pub fn needs_probe(&self, ctx: &Context, words: usize) -> bool {
        ctx.index >= words
            && ((self.movable && self.specs.is_empty())
                || self.specs.iter().any(|spec| {
                    flag(spec, "incomplete")
                        || !matches!(
                            string(&spec["begin_search"], "type"),
                            "index" | "keyword"
                        )
                        || !matches!(
                            string(&spec["find_keys"], "type"),
                            "range" | "keynum"
                        )
                }))
    }

    fn expected(&self, ctx: &Context, words: usize) -> Vec<Atom> {
        if ctx.index < words {
            return Vec::new();
        }
        self.grammar.expected(&ctx.args[words..ctx.index])
    }

    pub fn key_position(&self, ctx: &Context, words: usize) -> bool {
        if ctx.index < words {
            return false;
        }
        let expected = self.expected(ctx, words);
        let key_atoms: Vec<_> =
            expected.iter().filter(|a| a.kind == "key").collect();
        for (index, spec) in self.specs.iter().enumerate() {
            if flag(spec, "not_key") {
                continue;
            }
            match spec_position(spec, &ctx.args, ctx.index) {
                Some(true) => return true,
                Some(false) => {}
                None => {
                    if key_atoms.iter().any(|a| a.key_spec == Some(index)) {
                        return true;
                    }
                }
            }
        }
        if !self.specs.is_empty() {
            return false;
        }
        if !key_atoms.is_empty() {
            return true;
        }
        if self.movable {
            return false;
        }
        self.legacy.is_some_and(|(first, last, step)| {
            if first <= 0 || step <= 0 {
                return false;
            }
            let Ok(position) = i64::try_from(ctx.index) else {
                return false;
            };
            // Negative end positions need the not-yet-typed trailing arguments.
            let end = if last < 0 { i64::MAX } else { last };
            position >= first
                && position <= end
                && (position - first) % step == 0
        })
    }
}

/// Ask Redis's command-specific key extractor when metadata cannot describe a
/// position. The marker distinguishes equal key/value bytes at other positions.
/// This only succeeds once the edited command has enough arguments for Redis.
pub fn probe_key_position(
    connection: &mut Connection,
    args: &[Vec<u8>],
    index: usize,
) -> Option<bool> {
    let mut args = args.to_vec();
    let mut marker = b"\0rmycli-completion-probe".to_vec();
    while args.contains(&marker) {
        marker.push(0);
    }
    args.get_mut(index)?.clone_from(&marker);
    let keys: Vec<Vec<u8>> = redis::cmd("COMMAND")
        .arg("GETKEYS")
        .arg(&args)
        .query(connection)
        .ok()?;
    Some(keys.contains(&marker))
}

fn integer(value: &Value, name: &str) -> Option<usize> {
    value[name].as_u64().and_then(|v| usize::try_from(v).ok())
}

pub fn spec_position(
    spec: &Value,
    args: &[Vec<u8>],
    position: usize,
) -> Option<bool> {
    let begin = &spec["begin_search"];
    let begin_spec = &begin["spec"];
    let start = match string(begin, "type") {
        "index" => integer(begin_spec, "index")?,
        "keyword" => {
            let from = begin_spec["startfrom"].as_i64()?;
            let keyword = string(begin_spec, "keyword").as_bytes();
            let from = if from < 0 {
                args.len()
                    .checked_sub(usize::try_from(from.unsigned_abs()).ok()?)?
            } else {
                usize::try_from(from).ok()?
            };
            let found = if begin_spec["startfrom"].as_i64()? < 0 {
                (0..=from.min(args.len().saturating_sub(1)))
                    .rev()
                    .find(|&i| args[i].eq_ignore_ascii_case(keyword))
            } else {
                (from..args.len())
                    .find(|&i| args[i].eq_ignore_ascii_case(keyword))
            };
            let Some(found) = found else {
                return Some(false);
            };
            found + 1
        }
        _ => return None,
    };
    let find = &spec["find_keys"];
    let find_spec = &find["spec"];
    let step = integer(find_spec, "keystep")?;
    if step == 0 {
        return None;
    }
    match string(find, "type") {
        "keynum" => {
            let count_index =
                start.checked_add(integer(find_spec, "keynumidx")?)?;
            let count = std::str::from_utf8(args.get(count_index)?)
                .ok()?
                .parse::<usize>()
                .ok()?;
            let first = start.checked_add(integer(find_spec, "firstkey")?)?;
            Some(
                position >= first
                    && (position - first).is_multiple_of(step)
                    && (position - first) / step < count,
            )
        }
        "range" => {
            if position < start || !(position - start).is_multiple_of(step) {
                return Some(false);
            }
            let last = find_spec["lastkey"].as_i64()?;
            if last >= 0 {
                return Some(position - start <= usize::try_from(last).ok()?);
            }
            let limit = integer(find_spec, "limit").unwrap_or(0);
            if position + 1 < args.len() && (last < -1 || limit > 1) {
                let end = if limit > 1 {
                    start
                        .checked_add(args.len().checked_sub(start)? / limit)?
                        .checked_sub(1)?
                } else {
                    args.len().checked_sub(
                        usize::try_from(last.unsigned_abs()).ok()?,
                    )?
                };
                return Some(position <= end);
            }
            // An unfinished command lacks its timeout/IDs/options. Let the
            // argument grammar resolve these positions instead of treating the
            // current last word as the final word of the completed command.
            if last < -1 || limit > 1 {
                return None;
            }
            Some(true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::context;

    #[test]
    fn inline_hints_follow_arguments_without_modifying_input() {
        let catalog = Catalog::bundled();
        for (line, expected) in [
            ("lrange", " key start stop"),
            ("lrange ", "key start stop"),
            ("lrange list:a", " start stop"),
            ("lrange \"list a\" ", "start stop"),
            ("lrange list:a 0 ", "stop"),
            ("lrange list:a 0 -1", ""),
            ("lrange list:a invalid ", ""),
            ("lrange \"unfinished", ""),
            ("unknown ", ""),
            ("lra", ""),
        ] {
            assert_eq!(
                catalog.inline_hint(line, line.len()),
                expected,
                "{line}"
            );
        }
        assert_eq!(catalog.inline_hint("lrange key 0 -1", 7), "");
        let hint = catalog.inline_hint("SET k v EX ", 11);
        assert!(hint.starts_with("seconds"), "{hint}");
        let hint = catalog.inline_hint("XGROUP CREATE ", 14);
        assert!(hint.starts_with("key group"), "{hint}");
    }

    #[test]
    fn key_positions_from_bundled_specs_and_argument_trees() {
        let catalog = Catalog::bundled();
        for (line, expected) in [
            ("LRANGE list:", true),
            ("LRANGE k 0 ", false),
            ("SET k ", false),
            ("MSET k v ", true),
            ("MSET k ", false),
            ("EVAL script 2 a ", true),
            ("EVAL script 2 a b ", false),
            ("ZUNIONSTORE out 2 a ", true),
            ("ZUNIONSTORE out 2 a b WEIGHTS ", false),
            ("XREAD STREAMS ", true),
            ("BLPOP ", true),
            ("XGROUP CREATE ", true),
            ("XGROUP CREATE key ", false),
            ("SORT source STORE ", true),
        ] {
            let ctx = context(line, line.len());
            let (command, words) = catalog.resolve(&ctx.args).unwrap();
            assert_eq!(command.key_position(&ctx, words), expected, "{line}");
        }
        assert!(catalog.commands.len() > 300);
    }

    #[test]
    fn live_resp2_docs_and_subcommands_override_bundle() {
        let mut catalog = Catalog::bundled();
        catalog.add_live_docs(&json!([
            "foo",
            [
                "summary",
                "Module",
                "subcommands",
                [
                    "foo|bar",
                    [
                        "summary",
                        "Child",
                        "arguments",
                        [["name", "key", "type", "key"]]
                    ]
                ]
            ]
        ]));
        assert!(catalog.commands["FOO BAR"].live_docs);
        assert_eq!(catalog.commands["FOO BAR"].grammar.syntax, "key");
    }

    #[test]
    fn reversed_keyword_search_and_unknown_specs() {
        let spec = json!({"begin_search":{"type":"keyword","spec":{"keyword":"KEYS","startfrom":-2}},
            "find_keys":{"type":"range","spec":{"lastkey":-1,"keystep":1}}});
        let args = [b"CMD".to_vec(), b"KEYS".to_vec(), b"k".to_vec()];
        assert_eq!(spec_position(&spec, &args, 2), Some(true));
        assert_eq!(spec_position(&json!({}), &args, 2), None);
    }

    #[test]
    fn old_metadata_unknown_commands_and_unique_prefix_help() {
        let fallback = Catalog::bundled();
        let mut live = Catalog::default();
        live.add_command(&json!(["custom", -3, [], 1, -1, 2]), &fallback);
        live.add_command(
            &json!(["eval", -3, ["movablekeys"], 0, 0, 0]),
            &fallback,
        );
        for (line, key) in [
            ("CUSTOM key value ", true),
            ("CUSTOM key ", false),
            ("EVAL script 1 k ", false),
            ("EVAL script 1 ", true),
        ] {
            let ctx = context(line, line.len());
            let (command, words) = live.resolve(&ctx.args).unwrap();
            assert_eq!(command.key_position(&ctx, words), key, "{line}");
        }
        assert!(fallback.help(&context("LRA", 3))[0].starts_with("LRANGE key"));
        let stream_command = "XREAD STREAMS first second 0 0";
        for (cursor, expected) in [(19, true), (28, false)] {
            let ctx = context(stream_command, cursor);
            let (command, words) = fallback.resolve(&ctx.args).unwrap();
            assert_eq!(command.key_position(&ctx, words), expected);
        }
    }
}
