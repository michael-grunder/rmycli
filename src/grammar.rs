//! Compile COMMAND DOCS argument trees into an epsilon NFA.
//! Keeping all matching paths handles optional, repeated and alternative blocks.
use std::collections::BTreeSet;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Atom {
    pub label: String,
    pub kind: String,
    pub token: Option<String>,
    pub key_spec: Option<usize>,
}

impl Atom {
    fn accepts(&self, bytes: &[u8]) -> bool {
        if let Some(token) = &self.token {
            return bytes.eq_ignore_ascii_case(token.as_bytes());
        }
        match self.kind.as_str() {
            "integer" | "unix-time" => std::str::from_utf8(bytes)
                .is_ok_and(|s| s.parse::<i64>().is_ok()),
            // Score ranges mark an exclusive bound with a leading parenthesis.
            "double" => std::str::from_utf8(bytes).is_ok_and(|s| {
                s.strip_prefix('(').unwrap_or(s).parse::<f64>().is_ok()
            }),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Node {
    edges: Vec<(Option<Atom>, usize)>,
    remaining: String,
}

#[derive(Clone, Debug, Default)]
pub struct Grammar {
    nodes: Vec<Node>,
    pub syntax: String,
}

/// Bundled docs use booleans; servers list flags, spelling hyphens as `_`.
pub fn flag(arg: &Value, name: &str) -> bool {
    let underscored = name.replace('-', "_");
    arg[name].as_bool().unwrap_or(false)
        || arg[&underscored].as_bool().unwrap_or(false)
        || arg["flags"].as_array().is_some_and(|flags| {
            flags.iter().any(|f| {
                f.as_str().is_some_and(|f| f == name || f == underscored)
            })
        })
}

pub fn string<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().unwrap_or_default()
}

fn children(arg: &Value) -> &[Value] {
    arg["arguments"].as_array().map_or(&[], Vec::as_slice)
}

fn render(arg: &Value) -> String {
    let kind = string(arg, "type");
    let token = string(arg, "token");
    let body = if matches!(kind, "oneof" | "block") {
        children(arg)
            .iter()
            .map(render)
            .collect::<Vec<_>>()
            .join(if kind == "oneof" { "|" } else { " " })
    } else if kind == "pure-token" {
        token.to_owned()
    } else {
        arg["display_text"]
            .as_str()
            .unwrap_or_else(|| string(arg, "name"))
            .to_owned()
    };
    let mut text = if token.is_empty() || kind == "pure-token" {
        body.clone()
    } else {
        format!("{token} {body}")
    };
    if flag(arg, "multiple") {
        let repeated = if flag(arg, "multiple-token") {
            &text
        } else {
            &body
        };
        text = format!("{text} [{repeated} ...]");
    }
    if flag(arg, "optional") {
        text = format!("[{text}]");
    }
    text
}

fn join_hint(head: &str, tail: &str) -> String {
    [head, tail]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Grammar {
    pub fn from_docs(args: &[Value]) -> Self {
        let mut grammar = Self {
            nodes: vec![Node::default(), Node::default()],
            syntax: args.iter().map(render).collect::<Vec<_>>().join(" "),
        };
        grammar.sequence(args, 0, 1, "");
        grammar
    }

    fn node(&mut self) -> usize {
        let index = self.nodes.len();
        self.nodes.push(Node::default());
        index
    }

    fn edge(&mut self, from: usize, to: usize, atom: Option<Atom>) {
        self.nodes[from].edges.push((atom, to));
    }

    fn sequence(&mut self, args: &[Value], from: usize, to: usize, tail: &str) {
        let mut current = from;
        for (index, arg) in args.iter().enumerate() {
            let suffix = join_hint(
                &args[index + 1..]
                    .iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join(" "),
                tail,
            );
            self.nodes[current].remaining = join_hint(&render(arg), &suffix);
            let next = self.node();
            self.argument(arg, current, next, &suffix);
            current = next;
        }
        self.nodes[current].remaining = tail.into();
        self.edge(current, to, None);
    }

    fn argument(&mut self, arg: &Value, from: usize, to: usize, tail: &str) {
        if flag(arg, "optional") {
            self.edge(from, to, None);
        }
        let body = self.node();
        let end = self.node();
        let token = string(arg, "token");
        let kind = string(arg, "type");
        let atom = Atom {
            label: arg["display_text"]
                .as_str()
                .unwrap_or_else(|| string(arg, "name"))
                .into(),
            kind: kind.into(),
            token: None,
            key_spec: arg["key_spec_index"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok()),
        };
        if token.is_empty() {
            self.edge(from, body, None);
        } else {
            self.edge(
                from,
                body,
                Some(Atom {
                    label: token.into(),
                    token: Some(token.into()),
                    kind: "pure-token".into(),
                    key_spec: None,
                }),
            );
        }
        let mut once = arg.clone();
        once["optional"] = Value::Bool(false);
        once["multiple"] = Value::Bool(false);
        once["flags"] = Value::Null;
        if !flag(arg, "multiple-token") && kind != "pure-token" {
            once["token"] = Value::Null;
        }
        let after = if flag(arg, "multiple") {
            join_hint(&format!("[{} ...]", render(&once)), tail)
        } else {
            tail.into()
        };
        self.nodes[end].remaining.clone_from(&after);
        self.nodes[body].remaining = match kind {
            "pure-token" => after.clone(),
            "oneof" => join_hint(
                &children(arg)
                    .iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join("|"),
                &after,
            ),
            _ => join_hint(&atom.label, &after),
        };
        match kind {
            "oneof" => {
                for child in children(arg) {
                    self.argument(child, body, end, &after);
                }
            }
            "block" => self.sequence(children(arg), body, end, &after),
            "pure-token" => self.edge(body, end, None),
            _ => self.edge(body, end, Some(atom)),
        }
        self.edge(end, to, None);
        if flag(arg, "multiple") {
            self.edge(
                end,
                if flag(arg, "multiple-token") {
                    from
                } else {
                    body
                },
                None,
            );
        }
    }

    fn closure(&self, states: &BTreeSet<usize>) -> BTreeSet<usize> {
        let mut result = states.clone();
        let mut pending: Vec<_> = states.iter().copied().collect();
        while let Some(index) = pending.pop() {
            for (atom, next) in &self.nodes[index].edges {
                if atom.is_none() && result.insert(*next) {
                    pending.push(*next);
                }
            }
        }
        result
    }

    /// Remaining syntax for display only; never inserted into command input.
    pub fn remaining(&self, consumed: &[Vec<u8>]) -> String {
        if consumed.is_empty() {
            return self.syntax.clone();
        }
        if self.nodes.is_empty() {
            return String::new();
        }
        let mut states = BTreeSet::from([0]);
        for bytes in consumed {
            let mut next = BTreeSet::new();
            let mut literal = BTreeSet::new();
            for state in self.closure(&states) {
                for (atom, target) in &self.nodes[state].edges {
                    if let Some(atom) =
                        atom.as_ref().filter(|a| a.accepts(bytes))
                    {
                        next.insert(*target);
                        if atom.token.is_some() {
                            literal.insert(*target);
                        }
                    }
                }
            }
            states = if literal.is_empty() { next } else { literal };
        }
        states
            .iter()
            .map(|&state| &self.nodes[state].remaining)
            .next()
            .cloned()
            .unwrap_or_default()
    }

    pub fn expected(&self, consumed: &[Vec<u8>]) -> Vec<Atom> {
        if self.nodes.is_empty() {
            return Vec::new();
        }
        let mut states = self.closure(&BTreeSet::from([0]));
        for bytes in consumed {
            let mut next = BTreeSet::new();
            for state in states {
                for (atom, target) in &self.nodes[state].edges {
                    if atom.as_ref().is_some_and(|a| a.accepts(bytes)) {
                        next.insert(*target);
                    }
                }
            }
            states = self.closure(&next);
        }
        states
            .into_iter()
            .flat_map(|state| {
                self.nodes[state]
                    .edges
                    .iter()
                    .filter_map(|(atom, _)| atom.clone())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn optional_alternatives_and_repeating_blocks() {
        let args = json!([
            {"name":"key", "type":"key", "key_spec_index":0},
            {"name":"expire", "type":"oneof", "flags":["optional"],
             "arguments":[{"name":"seconds", "type":"integer", "token":"EX"},
                          {"name":"millis", "type":"integer", "token":"PX"}]},
            {"name":"pairs", "type":"block", "flags":["multiple"],
             "arguments":[{"name":"field","type":"string"},
                          {"name":"value","type":"string"}]}
        ]);
        let g = Grammar::from_docs(args.as_array().unwrap());
        assert_eq!(g.expected(&[])[0].kind, "key");
        assert!(
            g.expected(&[b"k".to_vec()])
                .iter()
                .any(|a| a.token.as_deref() == Some("EX"))
        );
        let expected = g.expected(&[b"k".to_vec(), b"EX".to_vec()]);
        assert!(expected.iter().any(|a| a.label == "seconds"));
        assert!(g.syntax.contains("[EX seconds|PX millis]"));
        assert!(
            g.expected(&[b"k".to_vec(), b"field".to_vec(), b"value".to_vec()])
                .iter()
                .any(|a| a.label == "field")
        );
    }

    #[test]
    fn token_and_repeated_value_rendering() {
        let weights = json!({"name":"weight", "type":"integer", "token":"WEIGHTS", "multiple":true});
        assert_eq!(render(&weights), "WEIGHTS weight [weight ...]");
        let get = json!({"name":"pattern", "type":"pattern", "token":"GET", "flags":["multiple", "multiple-token"]});
        assert_eq!(render(&get), "GET pattern [GET pattern ...]");
        let live = json!({"name":"pattern", "type":"pattern", "token":"GET", "flags":["multiple", "multiple_token"]});
        assert_eq!(render(&live), "GET pattern [GET pattern ...]");
        let block = json!({"name":"fields", "type":"block", "token":"FIELDS", "arguments":[{"name":"count","type":"integer"}]});
        assert_eq!(render(&block), "FIELDS count");
    }

    #[test]
    fn exclusive_score_bounds_are_doubles() {
        let args = json!([{"name":"min", "type":"double"}, {"name":"max", "type":"double"}]);
        let grammar = Grammar::from_docs(args.as_array().unwrap());
        for word in ["(1.5", "-inf", "+inf", "(-inf", "2"] {
            assert_eq!(grammar.remaining(&[word.into()]), "max", "{word}");
        }
        assert_eq!(grammar.remaining(&[b"(x".to_vec()]), "");
    }

    #[test]
    fn remaining_syntax_tracks_nested_choices_and_repeats() {
        let args = json!([
            {"name":"key", "type":"key"},
            {"name":"condition", "type":"oneof", "optional":true,
             "arguments":[{"name":"nx","type":"pure-token","token":"NX"},
                          {"name":"xx","type":"pure-token","token":"XX"}]},
            {"name":"pairs", "type":"block", "multiple":true,
             "arguments":[{"name":"field","type":"string"}, {"name":"value","type":"string"}]}
        ]);
        let grammar = Grammar::from_docs(args.as_array().unwrap());
        for (words, expected) in [
            (vec!["k"], "[NX|XX] field value [field value ...]"),
            (vec!["k", "NX"], "field value [field value ...]"),
            (vec!["k", "NX", "f"], "value [field value ...]"),
            (vec!["k", "NX", "f", "v"], "[field value ...]"),
        ] {
            let words = words
                .into_iter()
                .map(|s| s.as_bytes().to_vec())
                .collect::<Vec<_>>();
            assert_eq!(grammar.remaining(&words), expected);
        }
    }
}
