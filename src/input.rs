//! Byte-preserving command parsing, shared by execution and completion.
use std::fmt::Write as _;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub bytes: Vec<u8>,
    pub span: Range<usize>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub words: Vec<Word>,
    pub error: Option<String>,
}

pub fn parse(line: &str) -> Parsed {
    let input = line.as_bytes();
    let mut result = Parsed::default();
    let mut i = 0;
    while i < input.len() {
        if input[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut bytes = Vec::new();
        let mut quote = None;
        while i < input.len() {
            let b = input[i];
            if quote.is_none() && b.is_ascii_whitespace() {
                break;
            }
            if b == b'\\' {
                i += 1;
                if i == input.len() {
                    result.error = Some("unfinished escape".into());
                    break;
                }
                let escaped = input[i];
                if quote == Some(b'\'') && escaped != b'\'' && escaped != b'\\'
                {
                    bytes.push(b'\\');
                    bytes.push(escaped);
                } else if escaped == b'x' {
                    // from_str_radix alone would also accept a leading `+`.
                    let hex = input
                        .get(i + 1..i + 3)
                        .filter(|s| s.iter().all(u8::is_ascii_hexdigit))
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| u8::from_str_radix(s, 16).ok());
                    if let Some(value) = hex {
                        bytes.push(value);
                        i += 2;
                    } else {
                        result.error =
                            Some("expected two hex digits after \\x".into());
                        break;
                    }
                } else {
                    bytes.push(match escaped {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 8,
                        b'a' => 7,
                        other => other,
                    });
                }
            } else if quote == Some(b) {
                quote = None;
            } else if quote.is_none() && (b == b'\'' || b == b'"') {
                quote = Some(b);
            } else {
                bytes.push(b);
            }
            i += 1;
        }
        if quote.is_some() {
            result.error = Some("unclosed quote".into());
        }
        result.words.push(Word {
            bytes,
            span: start..i,
        });
        if result.error.is_some() {
            break;
        }
    }
    result
}

#[derive(Debug)]
pub struct Context {
    pub args: Vec<Vec<u8>>,
    pub index: usize,
    pub prefix: Vec<u8>,
    pub replace: Range<usize>,
}

pub fn context(line: &str, cursor: usize) -> Context {
    let parsed = parse(&line[..cursor]);
    let active = parsed.words.last().filter(|w| w.span.end == cursor);
    let index = parsed.words.len() - usize::from(active.is_some());
    let start = active.map_or(cursor, |w| w.span.start);
    let words = parse(line).words;
    let end = words
        .iter()
        .find(|w| w.span.start == start)
        .map_or(cursor, |w| w.span.end);
    let prefix = active.map_or_else(Vec::new, |w| w.bytes.clone());
    let mut args: Vec<_> = parsed.words.into_iter().map(|w| w.bytes).collect();
    if index == args.len() {
        args.push(Vec::new());
    }
    args.extend(
        words
            .into_iter()
            .filter(|word| word.span.start >= end && word.span.start >= cursor)
            .map(|word| word.bytes),
    );
    Context {
        args,
        index,
        prefix,
        replace: start..end,
    }
}

/// ASCII-only output also prevents keys and replies from injecting terminal escapes.
pub fn quote(bytes: &[u8]) -> String {
    if !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'))
    {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut result = String::from("\"");
    for &byte in bytes {
        match byte {
            b'"' => result.push_str("\\\""),
            b'\\' => result.push_str("\\\\"),
            b'\n' => result.push_str("\\n"),
            b'\r' => result.push_str("\\r"),
            b'\t' => result.push_str("\\t"),
            32..=126 => result.push(char::from(byte)),
            _ => {
                let _ = write!(result, "\\x{byte:02x}");
            }
        }
    }
    result.push('"');
    result
}

pub fn glob_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut pattern = Vec::new();
    for &b in prefix {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            pattern.push(b'\\');
        }
        pattern.push(b);
    }
    pattern.push(b'*');
    pattern
}

pub fn quote_like(bytes: &[u8], original: &str) -> String {
    let quoted = quote(bytes);
    match original.as_bytes().first() {
        Some(b'"') if !quoted.starts_with('"') => format!("\"{quoted}\""),
        Some(b'\'') if bytes.iter().all(|b| (32..=126).contains(b)) => {
            let escaped = String::from_utf8_lossy(bytes)
                .replace('\\', "\\\\")
                .replace('\'', "\\'");
            format!("'{escaped}'")
        }
        _ => quoted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        for value in [all, vec![], b"hello world".to_vec(), b"a'\\\"b".to_vec()]
        {
            let p = parse(&quote(&value));
            assert!(p.error.is_none());
            assert_eq!(p.words[0].bytes, value);
        }
    }

    #[test]
    fn incomplete_quotes_and_middle_of_word() {
        let c = context("LRANGE \"list:a b\" 0 -1", 14);
        assert_eq!(c.prefix, b"list:a");
        assert_eq!(c.replace, 7..17);
        assert_eq!(c.index, 1);
        assert_eq!(&c.args[2..], &[b"0".to_vec(), b"-1".to_vec()]);
        let c = context("GET ", 4);
        assert_eq!(c.index, 1);
        assert_eq!(c.args.len(), 2);
        assert!(parse("GET \"broken").error.is_some());
        assert!(parse("GET \\xZZ").error.is_some());
        assert!(parse("GET \\x+1").error.is_some());
        assert_eq!(parse("\\x7F\\xaB").words[0].bytes, b"\x7f\xab");
        assert_eq!(parse("SET '' 'a b'").words[1].bytes, b"");
    }

    #[test]
    fn glob_metacharacters_are_literal() {
        assert_eq!(glob_prefix(b"a*?[b]\\"), b"a\\*\\?\\[b\\]\\\\*");
    }

    #[test]
    fn completion_preserves_quote_style_when_representable() {
        for (original, expected) in [
            ("\"li", "\"list:a\""),
            ("'li", "'list:a'"),
            ("li", "list:a"),
        ] {
            assert_eq!(quote_like(b"list:a", original), expected);
        }
        let value = b"a'\\ b\xff";
        assert_eq!(parse(&quote_like(value, "'a")).words[0].bytes, value);
    }
}
