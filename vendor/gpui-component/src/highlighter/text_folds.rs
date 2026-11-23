//! ideae patch: folds read from the lines themselves, in any language:
//! `region`/`endregion` markers and runs of line comments. Grammars cannot
//! give them: a marker is a comment to them, and the SQL one does not even
//! take MySQL's `#` for a comment.

use gpui_base::input::{FoldRange, RopeExt};
use ropey::Rope;

/// Enough of a line to tell what it is.
const LINE_PREFIX: usize = 160;

struct Syntax {
    comments: &'static [&'static str],
    /// Open a marker without being comments: `#region` in C#.
    directives: &'static [&'static str],
}

fn syntax(language: &str) -> Option<Syntax> {
    let (comments, directives): (&[&str], &[&str]) = match language {
        "sql" => (&["--", "#", "//"], &[]),
        "lua" | "haskell" | "elm" => (&["--"], &[]),
        "python" | "ruby" | "bash" | "fish" | "yaml" | "toml" | "make" | "cmake" | "elixir"
        | "graphql" | "dockerfile" | "nginx" | "powershell" | "r" | "julia" | "nix" => {
            (&["#"], &[])
        }
        "ini" => (&["#", ";"], &[]),
        "properties" => (&["#", "!"], &[]),
        "php" | "hcl" => (&["//", "#"], &[]),
        "erlang" => (&["%"], &[]),
        "batch" => (&["::", "REM ", "rem "], &[]),
        "c" | "cpp" | "glsl" => (&["//"], &["#pragma"]),
        "csharp" => (&["//"], &["#"]),
        "rust" | "go" | "javascript" | "typescript" | "tsx" | "java" | "kotlin" | "scala"
        | "swift" | "zig" | "proto" | "groovy" | "dart" | "solidity" | "prisma" | "gleam"
        | "less" => (&["//"], &[]),
        "css" => (&[], &["/*"]),
        "html" | "xml" | "markdown" | "astro" | "svelte" => (&[], &["<!--"]),
        "twig" => (&[], &["{#"]),
        "ocaml" => (&[], &["(*"]),
        _ => return None,
    };
    Some(Syntax {
        comments,
        directives,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Line {
    Code,
    Blank,
    Comment,
    Open,
    Close,
}

fn is_word(body: &str, word: &str) -> bool {
    body.get(..word.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(word))
        && body[word.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

/// What a line is, and for a marker the words after it.
fn read_line<'a>(line: &'a str, syntax: &Syntax) -> (Line, &'a str) {
    let line = line.trim();
    if line.is_empty() {
        return (Line::Blank, "");
    }
    let comment = syntax.comments.iter().find(|p| line.starts_with(**p));
    let Some(prefix) = comment.or_else(|| syntax.directives.iter().find(|p| line.starts_with(**p)))
    else {
        return (Line::Code, "");
    };
    let body = line[prefix.len()..].trim_start_matches(['#', '-', '/', '*', ' ', '\t']);
    if body.starts_with("<editor-fold") {
        return (Line::Open, body);
    }
    if body.starts_with("</editor-fold") {
        return (Line::Close, "");
    }
    let body = ["-->", "*/", "#}", "*)"]
        .iter()
        .find_map(|end| body.strip_suffix(end))
        .unwrap_or(body)
        .trim_end();
    if is_word(body, "region") {
        (Line::Open, body["region".len()..].trim())
    } else if is_word(body, "endregion") {
        (Line::Close, "")
    } else if comment.is_some() {
        (Line::Comment, "")
    } else {
        (Line::Code, "")
    }
}

fn classify(line: &str, syntax: &Syntax) -> Line {
    read_line(line, syntax).0
}

/// A region's name as it is written: after its word, or `desc="…"`.
fn region_name(words: &str) -> String {
    let name = match words.strip_prefix("<editor-fold") {
        Some(rest) => rest
            .split_once("desc=\"")
            .and_then(|(_, desc)| desc.split_once('"'))
            .map_or("", |(desc, _)| desc),
        None => words,
    };
    name.trim().to_string()
}

/// A pair of markers in a text, for its structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRegion {
    pub name: String,
    pub line: usize,
    pub end_line: usize,
    /// From the start of the first line to the end of the last.
    pub bytes: std::ops::Range<usize>,
}

/// The closed pairs of region markers in `text`, by where they begin.
pub fn text_regions(language: &str, text: &str) -> Vec<TextRegion> {
    let Some(syntax) = syntax(language) else {
        return Vec::new();
    };
    let mut opens: Vec<(usize, usize, String)> = Vec::new();
    let mut regions = Vec::new();
    let mut at = 0;
    for (row, line) in text.split('\n').enumerate() {
        match read_line(line, &syntax) {
            (Line::Open, words) => opens.push((row, at, region_name(words))),
            (Line::Close, _) => {
                if let Some((start, from, name)) = opens.pop() {
                    regions.push(TextRegion {
                        name,
                        line: start,
                        end_line: row,
                        bytes: from..at + line.len(),
                    });
                }
            }
            _ => {}
        }
        at += line.len() + 1;
    }
    regions.sort_by_key(|region| region.line);
    regions
}

/// Folds by indentation, for a text no grammar reads: a line folds what is
/// indented deeper under it.
pub(super) fn indent_folds(text: &Rope) -> Vec<FoldRange> {
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut folds = Vec::new();
    let mut last_text = 0;
    let mut close = |open: &mut Vec<(usize, usize)>, indent: usize, last_text: usize, limit: usize| {
        while open.last().is_some_and(|(_, at)| *at >= indent) {
            let (start, _) = open.pop().expect("just looked at");
            if last_text.saturating_sub(start) >= 2 {
                folds.push(FoldRange::new(start, (last_text + 1).min(limit)));
            }
        }
    };
    let limit = text.lines_len().saturating_sub(1);
    let mut head = String::with_capacity(LINE_PREFIX);
    for (row, line) in text.iter_lines().enumerate() {
        head.clear();
        for chunk in line.chunks() {
            head.push_str(chunk);
            if head.len() >= LINE_PREFIX {
                break;
            }
        }
        if head.trim().is_empty() {
            continue;
        }
        let indent = head
            .chars()
            .take_while(|c| matches!(c, ' ' | '\t'))
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum();
        close(&mut open, indent, last_text, limit);
        open.push((row, indent));
        last_text = row;
    }
    close(&mut open, 0, last_text, limit);
    folds.sort_by_key(|fold| fold.start_line);
    folds
}

/// The syntax tree's folds with those of the text. A region wins a line over
/// the tree, the tree over a run of comments.
pub(super) fn merge(language: &str, tree: Vec<FoldRange>, text: &Rope) -> Vec<FoldRange> {
    let Some(syntax) = syntax(language) else {
        return tree;
    };

    let mut regions = Vec::new();
    let mut comments = Vec::new();
    let mut opens = Vec::new();
    let mut run: Option<usize> = None;
    let mut not_code = Vec::new();
    let mut head = String::with_capacity(LINE_PREFIX);
    // A fold keeps its last line in sight, as a closing brace is. A region's
    // marker and a comment should go, so theirs ends on the line after.
    let last = text.lines_len().saturating_sub(1);

    for (row, line) in text.iter_lines().enumerate() {
        head.clear();
        for chunk in line.chunks() {
            head.push_str(chunk);
            if head.len() >= LINE_PREFIX {
                break;
            }
        }
        let kind = classify(&head, &syntax);
        if language == "sql" {
            not_code.push(kind != Line::Code);
        }
        if kind == Line::Comment {
            run.get_or_insert(row);
            continue;
        }
        if let Some(start) = run.take()
            && row - start >= 3
        {
            comments.push(FoldRange::new(start, row));
        }
        match kind {
            Line::Open => opens.push(row),
            Line::Close => {
                if let Some(start) = opens.pop()
                    && row - start >= 2
                {
                    regions.push(FoldRange::new(start, (row + 1).min(last)));
                }
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        if last.saturating_sub(start) >= 2 {
            comments.push(FoldRange::new(start, last));
        }
    }

    let tree = tree.into_iter().filter_map(|fold| {
        if not_code.is_empty() {
            return Some(fold);
        }
        // The SQL grammar starts a statement at the comments before it.
        let start = (fold.start_line..=fold.end_line)
            .find(|row| !not_code.get(*row).copied().unwrap_or(false))?;
        (fold.end_line - start >= 2).then(|| FoldRange::new(start, fold.end_line))
    });

    let mut folds = regions;
    folds.extend(tree);
    folds.extend(comments);
    folds.sort_by_key(|fold| fold.start_line);
    folds.dedup_by_key(|fold| fold.start_line);
    folds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folds(language: &str, tree: &[(usize, usize)], text: &str) -> Vec<(usize, usize)> {
        let tree = tree.iter().map(|(a, b)| FoldRange::new(*a, *b)).collect();
        merge(language, tree, &Rope::from(text))
            .into_iter()
            .map(|fold| (fold.start_line, fold.end_line))
            .collect()
    }

    #[test]
    fn regions_nest_whatever_comment_holds_them() {
        let text = "#region Config\nselect 1;\n#--region By path\nselect 2;\n#--endregion\n#endregion\n";
        assert_eq!(folds("sql", &[], text), [(0, 6), (2, 5)]);
        let text = "// <editor-fold desc=\"a\">\nlet a;\nlet b;\n// </editor-fold>\n";
        assert_eq!(folds("rust", &[], text), [(0, 4)]);
        let text = "<!-- region -->\n<a/>\n<b/>\n<!-- endregion -->\n";
        assert_eq!(folds("html", &[], text), [(0, 4)]);
        let text = "#pragma region x\nint a;\nint b;\n#pragma endregion\n";
        assert_eq!(folds("cpp", &[], text), [(0, 4)]);
    }

    #[test]
    fn only_a_closed_pair_of_markers_folds() {
        assert!(folds("python", &[], "# region of interest\nx = 1\ny = 2\n").is_empty());
        assert!(folds("python", &[], "# endregion\nx = 1\ny = 2\n").is_empty());
        assert!(folds("python", &[], "# regional\nx = 1\n# endregional\n").is_empty());
    }

    #[test]
    fn a_flag_is_not_a_comment_of_a_language_without_it() {
        let text = "aws s3 ls \\\n  --region a \\\n  --x \\\n  --endregion b\n";
        assert!(folds("bash", &[], text).is_empty());
    }

    #[test]
    fn a_run_of_line_comments_folds() {
        let text = "// Copyright\n// License\n// Terms\nfn main() {}\n// one\n// two\n";
        assert_eq!(folds("rust", &[], text), [(0, 3)]);
        assert_eq!(folds("python", &[], "x = 1\n# a\n# b\n# c"), [(1, 3)]);
    }

    #[test]
    fn a_statement_folds_from_its_own_first_line() {
        let text = "#--endregion\n\n#--region\nselect a\nfrom b\nwhere c;\n";
        assert_eq!(folds("sql", &[(0, 3)], text), []);
        assert_eq!(folds("sql", &[(0, 5)], text), [(3, 5)]);
    }

    #[test]
    fn a_region_has_its_name_and_place() {
        let text = "#region Config\nselect 1;\n#--region By path\n#--endregion\n#endregion\n";
        let regions = text_regions("sql", text);
        assert_eq!(regions.iter().map(|r| (r.name.as_str(), r.line, r.end_line)).collect::<Vec<_>>(), [("Config", 0, 4), ("By path", 2, 3)]);
        assert_eq!(&text[regions[1].bytes.clone()], "#--region By path\n#--endregion");
        let named = text_regions("java", "// <editor-fold desc=\"Getters\">\nint a;\n// </editor-fold>");
        assert_eq!(named[0].name, "Getters");
    }

    #[test]
    fn what_no_grammar_reads_folds_by_its_indentation() {
        let found = |text: &str| -> Vec<(usize, usize)> {
            indent_folds(&Rope::from(text)).into_iter().map(|f| (f.start_line, f.end_line)).collect()
        };
        assert_eq!(found("a\n  b\n  c\n\n  d\ne\n"), [(0, 5)]);
        assert_eq!(found("a\n  b\n    c\n    d\ne"), [(0, 4), (1, 4)]);
        assert!(found("a\n  b\nc\n").is_empty());
    }

    #[test]
    fn a_language_unknown_keeps_the_tree() {
        assert_eq!(folds("text", &[(0, 2)], "# a\n# b\n# c\n"), [(0, 2)]);
    }
}
