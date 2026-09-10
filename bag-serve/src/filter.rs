use sqlx::{QueryBuilder, Sqlite};

#[derive(Debug, PartialEq, Eq)]
pub enum FilterTerm {
    Literal(String),
    Regex(String),
}

/// Bare words and double-quoted strings are literal substrings; a token starting
/// with `/` is a regex. Only delimiter escapes (and `\\` in quotes) are decoded.
pub fn parse_filter(input: &str) -> Result<Vec<FilterTerm>, &'static str> {
    let mut chars = input.chars().peekable();
    let mut terms = Vec::new();
    while let Some(first) = chars.next() {
        if first.is_whitespace() {
            continue;
        }
        if first == '"' || first == '/' {
            let mut value = String::new();
            loop {
                let ch = chars.next().ok_or("Unterminated filter token")?;
                if ch == first {
                    break;
                }
                if ch == '\\' {
                    let escaped = chars.next().ok_or("Unterminated filter escape")?;
                    if escaped != first && !(first == '"' && escaped == '\\') {
                        value.push('\\');
                    }
                    value.push(escaped);
                } else {
                    value.push(ch);
                }
            }
            if first == '/' {
                regex::Regex::new(&value).map_err(|_| "Invalid regular expression in filter")?;
                terms.push(FilterTerm::Regex(value));
            } else {
                terms.push(FilterTerm::Literal(value));
            }
        } else {
            let mut value = String::from(first);
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() || ch == '"' {
                    break;
                }
                value.push(ch);
                chars.next();
            }
            terms.push(FilterTerm::Literal(value));
        }
    }
    Ok(terms)
}

/// Apply every term to the filename, excluding the decoded parent path and `/`.
pub fn push_filename_filters<'a>(
    query: &mut QueryBuilder<'a, Sqlite>,
    terms: &'a [FilterTerm],
    parent_path: &str,
) {
    // SQLite substr is one-based and counts Unicode characters, not UTF-8 bytes.
    let start = if parent_path.is_empty() {
        1
    } else {
        parent_path.chars().count() as i64 + 2
    };
    for term in terms {
        match term {
            FilterTerm::Literal(value) => {
                query
                    .push(" AND instr(substr(path, ")
                    .push_bind(start)
                    .push("), ")
                    .push_bind(value)
                    .push(") > 0");
            }
            FilterTerm::Regex(pattern) => {
                query
                    .push(" AND substr(path, ")
                    .push_bind(start)
                    .push(") REGEXP ")
                    .push_bind(pattern);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FilterTerm, parse_filter};

    #[test]
    fn parses_mixed_filter_tokens() {
        assert_eq!(
            parse_filter(r#"first_keyword "second \"keyword\"" /^some_complex_regex/"#),
            Ok(vec![
                FilterTerm::Literal("first_keyword".to_owned()),
                FilterTerm::Literal("second \"keyword\"".to_owned()),
                FilterTerm::Regex("^some_complex_regex".to_owned()),
            ])
        );
        assert_eq!(parse_filter(" \t\n\u{2003}"), Ok(vec![]));
        assert_eq!(
            parse_filter("café\u{2003}猫"),
            Ok(vec![
                FilterTerm::Literal("café".to_owned()),
                FilterTerm::Literal("猫".to_owned()),
            ])
        );
    }

    #[test]
    fn escaping_depends_on_token_type() {
        assert_eq!(
            parse_filter(r#""quote\" backslash\\ newline\n slash\/" /a\/b\d+\\$/ bare\n"#),
            Ok(vec![
                FilterTerm::Literal("quote\" backslash\\ newline\\n slash\\/".to_owned()),
                FilterTerm::Regex(r"a/b\d+\\$".to_owned()),
                FilterTerm::Literal(r"bare\n".to_owned()),
            ])
        );
        assert_eq!(
            parse_filter(r#""" // a.b*%_ 'quote'"#),
            Ok(vec![
                FilterTerm::Literal(String::new()),
                FilterTerm::Regex(String::new()),
                FilterTerm::Literal("a.b*%_".to_owned()),
                FilterTerm::Literal("'quote'".to_owned()),
            ])
        );
    }

    #[test]
    fn rejects_unterminated_tokens_and_invalid_regexes() {
        for input in [
            "\"missing",
            "/missing",
            "\"trailing\\",
            "/trailing\\",
            "/[/",
            "/\\q/",
        ] {
            assert!(parse_filter(input).is_err(), "{input}");
        }
    }
}
