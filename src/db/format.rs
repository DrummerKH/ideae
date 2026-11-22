//! SQL laid out for reading. Statement by statement, so that comments between
//! them and `DELIMITER` lines stay, and a statement whose layout would change
//! anything but white space is left as it was written.

use std::ops::Range;

use sqlformat::{FormatOptions, Indent, QueryParams};

use super::statement;

/// A statement's text outside quotes without white space; quoted text whole.
fn essence(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quote: Option<char> = None;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == '\\' && q != '`' {
                    out.extend(chars.next());
                } else if c == q {
                    quote = None;
                }
            }
            None if matches!(c, '\'' | '"' | '`') => {
                quote = Some(c);
                out.push(c);
            }
            None if c.is_whitespace() => {}
            None => out.push(c),
        }
    }
    out
}

/// A statement laid out, or None when that would change more than its spacing.
pub fn statement(sql: &str) -> Option<String> {
    // Keywords keep their case: a table's name can be one, and is case-sensitive on Linux.
    let options = FormatOptions { indent: Indent::Spaces(2), uppercase: None, lines_between_queries: 1, ..FormatOptions::default() };
    let laid_out = sqlformat::format(sql, &QueryParams::None, &options);
    let laid_out = laid_out.trim();
    (essence(laid_out) == essence(sql)).then(|| laid_out.to_string())
}

/// The part of `text` to replace, and what with: the statements in `within`,
/// each laid out, what is between them as it was.
pub fn replace(text: &str, within: Range<usize>) -> Option<(Range<usize>, String)> {
    let statements: Vec<Range<usize>> = statement::split(text).into_iter().filter(|s| s.start < within.end && s.end > within.start).collect();
    let (first, last) = (statements.first()?.start, statements.last()?.end);
    let mut out = String::with_capacity(last - first);
    let mut from = first;
    for place in &statements {
        out.push_str(&text[from..place.start]);
        match self::statement(&text[place.clone()]) {
            Some(laid_out) => out.push_str(&laid_out),
            None => out.push_str(&text[place.clone()]),
        }
        from = place.end;
    }
    (out != text[first..last]).then_some((first..last, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_is_laid_out_and_keeps_its_case_and_strings() {
        let out = statement("select id,  Name from Users where note = 'a   b' and x=1").unwrap();
        assert!(out.contains('\n'), "{out}");
        assert!(out.contains("Users") && out.contains("Name") && out.contains("'a   b'"), "{out}");
        assert!(out.starts_with("select"), "{out}");
    }

    #[test]
    fn only_spacing_counts_as_the_same() {
        assert_eq!(essence("select  a\n from t"), essence("select a from t"));
        assert_ne!(essence("select 'a b'"), essence("select 'ab'"));
    }

    #[test]
    fn what_is_between_statements_stays() {
        let text = "-- first\nselect 1,2 from a;\n# keep me\nselect 3 from b;\n";
        let (range, out) = replace(text, 0..text.len()).unwrap();
        let mut whole = text.to_string();
        whole.replace_range(range, &out);
        assert!(whole.starts_with("-- first\n") && whole.contains("# keep me"), "{whole}");
        assert_eq!(statement::split(&whole).len(), 2);
    }

    #[test]
    fn only_the_statements_asked_for_change() {
        let text = "select 1,2 from a;select 3,4 from b;";
        let (range, _) = replace(text, 20..22).unwrap();
        assert!(range.start >= 18, "{range:?}");
    }
}
