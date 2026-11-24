//! What of a long output an agent is to read to fix it: the errors and
//! failures in it (Rust, Go, Jest, pytest, a compiler's `file:line: error`),
//! each with the lines that go with it, and the end of the output, where
//! the summary is. An output in which none is found is its end, as before.

/// Lines after the start of an error that belong to it, at most.
const BLOCK: usize = 14;
/// The end of the output, always given.
const END: usize = 15;
/// Characters of one line: a minified bundle is one line.
const LINE: usize = 400;

/// At most `most` lines of `output` for the agent.
pub fn for_agent(output: &str, most: usize) -> String {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    // The end, given whole after the errors: a third of the room when it is small.
    let end_lines = END.min(most / 3).max(1);
    let tail_from = lines.len().saturating_sub(end_lines);
    let mut blocks: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(tail_from) {
        if !starts_error(line) || blocks.last().is_some_and(|b| b.contains(&i)) {
            continue;
        }
        // Not into the end, which comes anyway.
        let mut end = i + 1;
        while end < tail_from && end - i < BLOCK && !lines[end].trim().is_empty() && !starts_error(lines[end]) {
            end += 1;
        }
        blocks.push(i..end);
    }
    if blocks.is_empty() {
        return lines[lines.len().saturating_sub(most)..].iter().map(|l| cut(l)).collect::<Vec<_>>().join("\n");
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut left_out = 0;
    // The end, its heading and a line for what was left out.
    let room = most.saturating_sub(end_lines + 2);
    for block in &blocks {
        let text: Vec<String> = lines[block.clone()].iter().map(|l| cut(l)).collect();
        // A compiler says the same of every place it is used.
        if !seen.insert(text.join("\n")) {
            continue;
        }
        if out.len() + text.len() + usize::from(!out.is_empty()) > room {
            left_out += 1;
            continue;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(text);
    }
    if left_out > 0 {
        out.push(format!("[… {left_out} more like these left out]"));
    }
    out.push("[… the end of the output:]".into());
    out.extend(lines[tail_from..].iter().map(|l| cut(l)));
    out.join("\n")
}

/// `text` in at most about `most` bytes: its start, where the errors are,
/// and its end, the middle left out. Cut between characters.
pub fn within(text: String, most: usize) -> String {
    if text.len() <= most {
        return text;
    }
    let mut head = most * 3 / 4;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let end_bytes = most / 4;
    let from = text.char_indices().map(|(at, _)| at).find(|&at| at >= head && text.len() - at <= end_bytes).unwrap_or(text.len());
    let (start, rest) = text.split_at(head);
    let (_, end) = rest.split_at(from - head);
    format!("{start}\n[… the middle is left out]\n{end}")
}

fn cut(line: &str) -> String {
    if line.chars().count() > LINE { format!("{}…", line.chars().take(LINE).collect::<String>()) } else { line.to_string() }
}

/// Where an error or a failure is told.
fn starts_error(line: &str) -> bool {
    let t = line.trim_start();
    let first = t.split_whitespace().next().unwrap_or_default();
    // rustc and cargo; a test's output in cargo test.
    t.starts_with("error:") || t.starts_with("error[")
        || (t.starts_with("---- ") && t.ends_with(" ----"))
        || t.contains("panicked at")
        // go test
        || t.starts_with("--- FAIL") || t.starts_with("FAIL\t")
        // jest, vitest, mocha
        || t.starts_with('●') || t.starts_with('✕') || t.starts_with('×') || t.starts_with("FAIL ")
        // pytest; Python
        || t.starts_with("FAILED ") || t.starts_with("E   ") || t.starts_with("Traceback (most recent call last)")
        || first.ends_with("Error:") || first.ends_with("Exception:")
        // gcc, tsc, eslint, go build…: path:line: error
        || t.contains(": error") || t.contains(" error TS")
        || first == "ERROR" || first == "ERROR:"
}

#[cfg(test)]
mod tests {
    use super::for_agent;

    #[test]
    fn the_errors_and_the_end() {
        let mut out: Vec<String> = (0..300).map(|i| format!("   Compiling crate{i} v1.0")).collect();
        out.push("error[E0308]: mismatched types".into());
        out.push("  --> src/main.rs:4:5".into());
        out.push("   |".into());
        out.push("".into());
        out.extend((0..100).map(|i| format!("note {i}")));
        out.push("error: could not compile `app`".into());
        let digest = for_agent(&out.join("\n"), 80);
        assert!(digest.starts_with("error[E0308]: mismatched types\n  --> src/main.rs:4:5"), "{digest}");
        assert!(digest.ends_with("error: could not compile `app`"), "{digest}");
        assert!(!digest.contains("Compiling crate"), "{digest}");
        assert!(digest.lines().count() <= 80);
    }

    #[test]
    fn the_same_error_once_and_no_more_than_asked() {
        let one = "error: unused variable\n  --> a.rs:1:1\n";
        let many: String = (0..50).map(|i| format!("{one}\nerror: other {i}\n  --> b.rs:{i}:1\n\n")).collect();
        let digest = for_agent(&format!("{many}{}", "x\n".repeat(20)), 40);
        assert_eq!(digest.matches("unused variable").count(), 1, "{digest}");
        assert!(digest.contains("more like these left out"), "{digest}");
        assert!(digest.lines().count() <= 42, "{}", digest.lines().count());
    }

    #[test]
    fn no_more_than_asked_and_nothing_twice() {
        let text: String = (0..100).map(|i| if i % 10 == 0 { format!("error: e{i}\n  at {i}\n") } else { format!("x{i}\n") }).collect();
        for most in [3, 5, 8, 20] {
            let digest = for_agent(&text, most);
            assert!(digest.lines().count() <= most, "{most}: {digest}");
        }
        let near_end = format!("{}error: last\n  at the end\nsummary\n", "noise\n".repeat(50));
        assert_eq!(for_agent(&near_end, 40).matches("at the end").count(), 1, "a block reaching the end is not given twice");
    }

    #[test]
    fn the_start_is_kept_within_the_bytes() {
        let text = format!("error: the cause\n{}end of log", "ü".repeat(10_000));
        let kept = super::within(text, 1000);
        assert!(kept.len() <= 1100 && kept.starts_with("error: the cause") && kept.ends_with("end of log"), "{}", kept.len());
    }

    #[test]
    fn without_errors_its_end() {
        let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let digest = for_agent(&text, 80);
        assert!(digest.starts_with("line 120") && digest.ends_with("line 199"), "{digest}");
    }

    #[test]
    fn tests_of_other_languages() {
        for (output, wanted) in [
            ("ok\n--- FAIL: TestLogin (0.00s)\n    login_test.go:12: got 1 want 2\nFAIL\n", "login_test.go:12"),
            ("PASS a.test.js\nFAIL b.test.js\n  ● login › rejects\n    expect(x).toBe(2)\n", "expect(x).toBe(2)"),
            ("collected 3\nE   assert 1 == 2\nFAILED test_a.py::test_x\n", "assert 1 == 2"),
        ] {
            let long = format!("{}{output}", "noise\n".repeat(200));
            assert!(for_agent(&long, 40).contains(wanted), "{output}");
        }
    }
}
