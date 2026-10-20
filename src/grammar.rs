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
            "double" => std::str::from_utf8(bytes)
                .is_ok_and(|s| s.parse::<f64>().is_ok()),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Node {
    edges: Vec<(Option<Atom>, usize)>,
}

#[derive(Clone, Debug, Default)]
pub struct Grammar {
    nodes: Vec<Node>,
    pub syntax: String,
}

pub fn flag(arg: &Value, name: &str) -> bool {
    arg[name].as_bool().unwrap_or(false)
        || arg[name.replace('-', "_")].as_bool().unwrap_or(false)
        || arg["flags"]
            .as_array()
            .is_some_and(|flags| flags.iter().any(|f| f.as_str() == Some(name)))
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

impl Grammar {
    pub fn from_docs(args: &[Value]) -> Self {
        let mut grammar = Self {
            nodes: vec![Node::default(), Node::default()],
            syntax: args.iter().map(render).collect::<Vec<_>>().join(" "),
        };
        grammar.sequence(args, 0, 1);
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

    fn sequence(&mut self, args: &[Value], from: usize, to: usize) {
        let mut current = from;
        for arg in args {
            let next = self.node();
            self.argument(arg, current, next);
            current = next;
        }
        self.edge(current, to, None);
    }

    fn argument(&mut self, arg: &Value, from: usize, to: usize) {
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
        match kind {
            "oneof" => {
                for child in children(arg) {
                    self.argument(child, body, end);
                }
            }
            "block" => self.sequence(children(arg), body, end),
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
        let block = json!({"name":"fields", "type":"block", "token":"FIELDS", "arguments":[{"name":"count","type":"integer"}]});
        assert_eq!(render(&block), "FIELDS count");
    }
}
