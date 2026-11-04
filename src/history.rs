use std::collections::{HashSet, VecDeque};

/// Search recent commands using fuzzy terms or apostrophe-prefixed substrings.
// shortcut: fuzzy/exact terms only; extend when more fzf operators are needed.
pub fn search(history: &VecDeque<String>, query: &str) -> Vec<String> {
    let terms = terms(query);
    let mut seen = HashSet::new();
    history
        .iter()
        .rev()
        .filter(|line| seen.insert(line.as_str()))
        .filter(|line| {
            terms.iter().all(|term| {
                let (exact, term) = term
                    .strip_prefix('\'')
                    .map_or((false, term.as_str()), |term| (true, term));
                let folded;
                let text = if term.chars().any(char::is_uppercase) {
                    line.as_str()
                } else {
                    folded = line.to_lowercase();
                    &folded
                };
                if exact {
                    text.contains(term)
                } else {
                    let mut chars = text.chars();
                    term.chars().all(|c| chars.any(|next| next == c))
                }
            })
        })
        .cloned()
        .collect()
}

fn terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut term = String::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&' ') {
            chars.next();
            term.push(' ');
        } else if c == ' ' {
            if !term.is_empty() {
                terms.push(std::mem::take(&mut term));
            }
        } else {
            term.push(c);
        }
    }
    if !term.is_empty() {
        terms.push(term);
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_terms_and_exact_substrings_follow_fzf_conventions() {
        let history = ["GET user:42", "SET user:42 hello", "GET upper:42"]
            .map(String::from)
            .into();
        assert_eq!(search(&history, "gusr"), ["GET user:42"]);
        assert_eq!(search(&history, "'gusr"), Vec::<String>::new());
        assert_eq!(search(&history, "'user g"), ["GET user:42"]);
        assert_eq!(search(&history, "'GET\\ user"), ["GET user:42"]);
        assert_eq!(search(&history, "42 set"), ["SET user:42 hello"]);
        assert_eq!(search(&history, "get Hello"), Vec::<String>::new());
    }

    #[test]
    fn smart_case_unicode_and_literal_command_bytes() {
        let history = ["GET café", "get CAFÉ", r"GET \x00", "GET a\x1bb"]
            .map(String::from)
            .into();
        assert_eq!(search(&history, "cfé"), ["get CAFÉ", "GET café"]);
        assert_eq!(search(&history, "'CAFÉ"), ["get CAFÉ"]);
        assert_eq!(search(&history, "Get"), Vec::<String>::new());
        assert_eq!(search(&history, r"'\x00"), [r"GET \x00"]);
        assert_eq!(search(&history, "ab"), ["GET a\x1bb"]);
    }

    #[test]
    fn empty_queries_and_duplicates_keep_the_most_recent_entry() {
        let history = ["PING", "GET a", "PING"].map(String::from).into();
        for query in ["", "   ", "'"] {
            assert_eq!(search(&history, query), ["PING", "GET a"]);
        }
        assert_eq!(search(&VecDeque::new(), ""), Vec::<String>::new());
        assert_eq!(search(&history, "missing"), Vec::<String>::new());
    }
}
