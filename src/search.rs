//! In-process ripgrep: the same walk (.gitignore/.ignore, no hidden files) and
//! the same binary-file rule as `rg` with its defaults.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::Bytes};
use ignore::{WalkBuilder, WalkState};

pub struct Line {
    /// Relative to the searched root.
    pub path: PathBuf,
    /// 1-based.
    pub line: u64,
    pub text: String,
}

/// `rg --fixed-strings --smart-case`.
pub fn literal(query: &str) -> Option<RegexMatcher> {
    builder().fixed_strings(true).case_smart(true).build(query).ok()
}

/// `rg -e <pattern>...`.
pub fn regexes(patterns: &[String]) -> Option<RegexMatcher> {
    builder().build_many(patterns).ok()
}

fn builder() -> RegexMatcherBuilder {
    let mut builder = RegexMatcherBuilder::new();
    // As rg does: a match never spans lines, so `\s` cannot eat a newline.
    builder.line_terminator(Some(b'\n'));
    builder
}

/// Matching lines under `root`, at most `per_file` from each file and `limit`
/// in all, sorted by path and line. Each line is cut to `max_columns` chars.
pub fn search(root: &Path, matcher: &RegexMatcher, per_file: u64, max_columns: usize, limit: usize) -> Vec<Line> {
    let found = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    // .gitignore counts in plain folders too: a project need not be a repository.
    WalkBuilder::new(root).require_git(false).build_parallel().run(|| {
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\0'))
            .max_matches(Some(per_file))
            .build();
        let (found, count) = (&found, &count);
        Box::new(move |entry| {
            if count.load(Ordering::Relaxed) >= limit {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else { return WalkState::Continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_path_buf();
            let mut lines = Vec::new();
            let sink = Bytes(|line, bytes| {
                let text = String::from_utf8_lossy(bytes);
                let mut text = text.trim_end_matches(['\r', '\n']).to_string();
                if let Some((cut, _)) = text.char_indices().nth(max_columns) {
                    text.truncate(cut);
                }
                lines.push(Line { path: path.clone(), line, text });
                Ok(true)
            });
            let _ = searcher.search_path(matcher, entry.path(), sink);
            if !lines.is_empty() {
                count.fetch_add(lines.len(), Ordering::Relaxed);
                found.lock().unwrap().extend(lines);
            }
            WalkState::Continue
        })
    });
    let mut found = found.into_inner().unwrap();
    found.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    found.truncate(limit);
    found
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{literal, regexes, search};

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ideae-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "needle\n").unwrap();
        fs::write(root.join(".hidden"), "needle\n").unwrap();
        fs::write(root.join("binary.bin"), b"needle\n\0\x01\x02").unwrap();
        fs::write(root.join("src/a.rs"), "fn needle() {}\nlet Needle = 1;\n").unwrap();
        let many: String = (0..30).map(|i| format!("needle {i}\n")).collect();
        fs::write(root.join("src/many.txt"), many).unwrap();
        fs::write(root.join("long.txt"), format!("needle{}\n", "é".repeat(400))).unwrap();
        root
    }

    #[test]
    fn search_walk_case_and_caps() {
        let root = tree();
        let hits = search(&root, &literal("needle").unwrap(), 20, 300, 200);
        let paths: Vec<_> = hits.iter().map(|h| h.path.to_string_lossy().into_owned()).collect();
        assert!(!paths.iter().any(|p| p == "ignored.txt" || p == ".hidden" || p == "binary.bin"), "{paths:?}");
        assert_eq!(hits.iter().filter(|h| h.path.ends_with("a.rs")).count(), 2, "lowercase query ignores case");
        assert_eq!(hits.iter().filter(|h| h.path.ends_with("many.txt")).count(), 20, "per-file cap");
        let long = hits.iter().find(|h| h.path.ends_with("long.txt")).unwrap();
        assert_eq!(long.text.chars().count(), 300);
        assert_eq!(search(&root, &literal("needle").unwrap(), 20, 300, 5).len(), 5, "overall cap");

        let upper = search(&root, &literal("Needle").unwrap(), 20, 300, 200);
        assert_eq!(upper.len(), 1, "uppercase query is case-sensitive");
        assert_eq!((upper[0].line, upper[0].text.as_str()), (2, "let Needle = 1;"));

        let fixed = search(&root, &literal("needle()").unwrap(), 20, 300, 200);
        assert_eq!(fixed.len(), 1, "fixed string, not a regex");

        let re = search(&root, &regexes(&[r"\bfn\s+needle\b".into(), r"^needle 2\d$".into()]).unwrap(), 20, 300, 200);
        assert_eq!(re.len(), 11);
        fs::remove_dir_all(&root).unwrap();
    }
}
