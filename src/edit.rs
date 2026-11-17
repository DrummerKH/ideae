//! What the editor does to lines and words on a key: delete, duplicate,
//! move and join lines, change case, find the next occurrence. Each is the
//! text to put in place of a range, and what to select after.

use std::ops::Range;

#[derive(Debug, PartialEq)]
pub struct Edit {
    /// What is replaced.
    pub range: Range<usize>,
    pub text: String,
    /// The selection after, in the new text.
    pub select: Range<usize>,
}

/// How the lines of `now` differ from those of `committed`: by row, the
/// added ones, the changed ones, and the rows before which lines are gone
/// (the row after the last, when the end is gone).
#[derive(Debug, Default, PartialEq)]
pub struct LineChanges {
    pub added: Vec<usize>,
    pub changed: Vec<usize>,
    pub gone_before: Vec<usize>,
}

#[cfg(test)]
pub fn line_changes(committed: &str, now: &str) -> LineChanges {
    LineChanges::of(&change_runs(committed, now))
}

impl LineChanges {
    pub fn of(runs: &[ChangeRun]) -> Self {
        let mut found = LineChanges::default();
        for run in runs {
            match (run.old.is_empty(), run.new.is_empty()) {
                (true, _) => found.added.extend(run.new.clone()),
                (false, false) => found.changed.extend(run.new.clone()),
                (false, true) => found.gone_before.push(run.new.start),
            }
        }
        found
    }
}

/// One change of the text: the rows it had and the rows it has now, either
/// empty when lines were only added or only taken away.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeRun {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

impl ChangeRun {
    /// Whether its mark in the gutter is on `row`: one of its rows, or for
    /// lines only taken away, the row before which they were.
    pub fn is_at(&self, row: usize) -> bool {
        self.new.contains(&row) || (self.new.is_empty() && self.new.start == row)
    }
}

pub fn change_runs(committed: &str, now: &str) -> Vec<ChangeRun> {
    use similar::DiffOp;
    similar::TextDiff::from_lines(committed, now)
        .ops()
        .iter()
        .filter_map(|op| match *op {
            DiffOp::Equal { .. } => None,
            DiffOp::Insert { old_index, new_index, new_len } => Some(ChangeRun { old: old_index..old_index, new: new_index..new_index + new_len }),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => Some(ChangeRun { old: old_index..old_index + old_len, new: new_index..new_index + new_len }),
            DiffOp::Delete { old_index, old_len, new_index } => Some(ChangeRun { old: old_index..old_index + old_len, new: new_index..new_index }),
        })
        .collect()
}

/// The edit that makes `new` of `old`: the part of it that differs, so
/// that what is around stays, the cursor and the view with it. None when
/// they are the same.
pub fn rewrite(old: &str, new: &str, cursor: usize) -> Option<Edit> {
    if old == new {
        return None;
    }
    let same_start = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    let mut start = same_start;
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let most = old.len().min(new.len()) - start;
    let mut end = old.bytes().rev().zip(new.bytes().rev()).take(most).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - end) || !new.is_char_boundary(new.len() - end) {
        end -= 1;
    }
    let text = new[start..new.len() - end].to_string();
    // The cursor stays in what did not change; from what did, to its end.
    let at = if cursor <= start {
        cursor
    } else if cursor >= old.len() - end {
        new.len() - (old.len() - cursor)
    } else {
        start + text.len()
    };
    Some(Edit { range: start..old.len() - end, text, select: at..at })
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, offset: usize) -> usize {
    text[offset..].find('\n').map_or(text.len(), |i| offset + i)
}

/// The lines the selection is on, without the last one's line break. A
/// selection that ends at a line's start does not take that line.
fn lines_of(text: &str, selection: &Range<usize>) -> Range<usize> {
    let last = if selection.end > selection.start && text[..selection.end].ends_with('\n') { selection.end - 1 } else { selection.end };
    line_start(text, selection.start)..line_end(text, last)
}

/// Removes the lines the selection is on; the cursor stays in its column,
/// on the line that takes their place.
pub fn delete_lines(text: &str, selection: Range<usize>) -> Edit {
    let lines = lines_of(text, &selection);
    let column = selection.start - lines.start;
    if lines.end < text.len() {
        let next = lines.end + 1..line_end(text, lines.end + 1);
        let at = lines.start + column.min(next.len());
        return Edit { range: lines.start..lines.end + 1, text: String::new(), select: at..at };
    }
    // The last line: its break goes with it, and the cursor to the line before.
    let start = lines.start.saturating_sub(1);
    let before = line_start(text, start);
    let at = before + column.min(start - before);
    Edit { range: start..lines.end, text: String::new(), select: at..at }
}

/// A copy of the selection after it, selected; with nothing selected, of
/// the line under it, the cursor in the same column of the copy.
pub fn duplicate(text: &str, selection: Range<usize>) -> Edit {
    if selection.is_empty() {
        let line = lines_of(text, &selection);
        let copy = format!("\n{}", &text[line.clone()]);
        let at = selection.start + copy.len();
        return Edit { range: line.end..line.end, text: copy, select: at..at };
    }
    let copy = text[selection.clone()].to_string();
    Edit { range: selection.end..selection.end, select: selection.end..selection.end + copy.len(), text: copy }
}

/// The selection's lines one line up or down, the selection with them.
pub fn move_lines(text: &str, selection: Range<usize>, up: bool) -> Option<Edit> {
    let lines = lines_of(text, &selection);
    let block = &text[lines.clone()];
    if up {
        if lines.start == 0 {
            return None;
        }
        let before = line_start(text, lines.start - 1)..lines.start - 1;
        let by = before.len() + 1;
        Some(Edit {
            range: before.start..lines.end,
            text: format!("{block}\n{}", &text[before]),
            select: selection.start - by..selection.end - by,
        })
    } else {
        if lines.end == text.len() {
            return None;
        }
        let after = lines.end + 1..line_end(text, lines.end + 1);
        let by = after.len() + 1;
        Some(Edit {
            range: lines.start..after.end,
            text: format!("{}\n{block}", &text[after]),
            select: selection.start + by..selection.end + by,
        })
    }
}

/// The line under the cursor and the next as one, a space between what
/// they hold; every line of a selection so. The cursor is where they met.
pub fn join_lines(text: &str, selection: Range<usize>) -> Option<Edit> {
    let lines = lines_of(text, &selection);
    let breaks = text[lines.clone()].matches('\n').count().max(1);
    let mut joined = text[lines.start..line_end(text, lines.start)].to_string();
    let mut rest = line_end(text, lines.start);
    let mut met = lines.start + joined.len();
    for _ in 0..breaks {
        if rest >= text.len() {
            break;
        }
        let next = &text[rest + 1..line_end(text, rest + 1)];
        let (left, right) = (joined.trim_end().len(), next.trim_start());
        joined.truncate(left);
        met = lines.start + joined.len();
        if left > 0 && !right.is_empty() {
            joined.push(' ');
        }
        joined.push_str(right);
        rest = line_end(text, rest + 1);
    }
    (rest > line_end(text, lines.start)).then(|| Edit { range: lines.start..rest, text: joined, select: met..met })
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier the offset is in or next to.
pub fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let start = text[..offset].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(offset, |(i, _)| i);
    let end = offset + text[offset..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len() - offset, |(i, _)| i);
    (end > start).then_some(start..end)
}

/// Upper case if any of it is lower, else lower: the selection, or the
/// word under the cursor.
pub fn toggle_case(text: &str, selection: Range<usize>) -> Option<Edit> {
    let range = if selection.is_empty() { word_at(text, selection.start)? } else { selection };
    let old = &text[range.clone()];
    let new = if old.chars().any(char::is_lowercase) { old.to_uppercase() } else { old.to_lowercase() };
    Some(Edit { select: range.start..range.start + new.len(), range, text: new })
}

/// Where `needle` is next at or after `from`, then from the start; never
/// at `first`, where the search began: that one is selected already.
pub fn next_occurrence(text: &str, needle: &str, from: usize, first: usize) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let from = from.min(text.len());
    let at = text[from..].find(needle).map(|i| from + i).or_else(|| text.find(needle))?;
    (at != first).then_some(at..at + needle.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text after the edit, with `|` at the ends of the selection.
    fn after(text: &str, edit: Edit) -> String {
        let mut out = format!("{}{}{}", &text[..edit.range.start], edit.text, &text[edit.range.end..]);
        out.insert(edit.select.end, '|');
        if !edit.select.is_empty() {
            out.insert(edit.select.start, '|');
        }
        out
    }

    /// The text without its `|` marks, and the selection they mark.
    fn marked(text: &str) -> (String, Range<usize>) {
        let start = text.find('|').unwrap();
        let rest = text[start + 1..].find('|').map(|i| start + i);
        (text.replace('|', ""), start..rest.unwrap_or(start))
    }

    fn run(text: &str, edit: impl Fn(&str, Range<usize>) -> Option<Edit>) -> Option<String> {
        let (text, selection) = marked(text);
        edit(&text, selection).map(|e| after(&text, e))
    }

    #[test]
    fn deletes_lines() {
        let delete = |text: &str| run(text, |t, s| Some(delete_lines(t, s))).unwrap();
        assert_eq!(delete("one\ntw|o\nthree"), "one\nth|ree");
        assert_eq!(delete("one\ntwo long|\n3"), "one\n3|");
        assert_eq!(delete("one\ntw|o"), "on|e", "the last line takes its break with it");
        assert_eq!(delete("on|e"), "|");
        assert_eq!(delete("a\n|b\nc\n|d"), "a\n|d", "a selection ending at a line's start leaves that line");
    }

    #[test]
    fn duplicates() {
        let duplicate = |text: &str| run(text, |t, s| Some(duplicate(t, s))).unwrap();
        assert_eq!(duplicate("one\ntw|o\nthree"), "one\ntwo\ntw|o\nthree");
        assert_eq!(duplicate("a |bc| d"), "a bc|bc| d");
    }

    #[test]
    fn moves_lines() {
        assert_eq!(run("one\ntw|o\nthree", |t, s| move_lines(t, s, true)).as_deref(), Some("tw|o\none\nthree"));
        assert_eq!(run("one\ntw|o\nthree", |t, s| move_lines(t, s, false)).as_deref(), Some("one\nthree\ntw|o"));
        assert_eq!(run("o|ne\ntwo", |t, s| move_lines(t, s, true)), None);
        assert_eq!(run("one\ntw|o", |t, s| move_lines(t, s, false)), None);
        assert_eq!(run("a\n|b\nc|\nd", |t, s| move_lines(t, s, false)).as_deref(), Some("a\nd\n|b\nc|"), "a block moves as one");
    }

    #[test]
    fn joins_lines() {
        assert_eq!(run("foo(|\n    bar,\n    baz)", join_lines).as_deref(), Some("foo(| bar,\n    baz)"));
        assert_eq!(run("on|e  \n\ntwo", join_lines).as_deref(), Some("one|\ntwo"), "nothing to put a space before");
        assert_eq!(run("|a\n  b\n  c|\nd", join_lines).as_deref(), Some("a b| c\nd"), "every line of a selection");
        assert_eq!(run("last|", join_lines), None);
    }

    #[test]
    fn words_and_case() {
        assert_eq!(word_at("let my_var = 1", 6), Some(4..10));
        assert_eq!(word_at("a + b", 2), None);
        assert_eq!(run("let my|Var = 1", toggle_case).as_deref(), Some("let |MYVAR| = 1"));
        assert_eq!(run("let |MYVAR| = 1", toggle_case).as_deref(), Some("let |myvar| = 1"));
    }

    #[test]
    fn rewrites_only_what_differs() {
        let edit = rewrite("let a = foo(a);\nrest", "let total = foo(total);\nrest", 17).unwrap();
        assert_eq!((edit.range.clone(), edit.text.as_str()), (4..13, "total = foo(total"));
        assert_eq!(edit.select, 25..25, "the cursor after the change moves with the text");
        assert_eq!(rewrite("één", "éen", 0).unwrap().range, 2..4, "never inside a character");
        assert_eq!(rewrite("same", "same", 2), None);
        assert_eq!(rewrite("ab", "aXb", 1).unwrap(), Edit { range: 1..1, text: "X".into(), select: 1..1 });
    }

    #[test]
    fn lines_against_the_commit() {
        let committed = "one\ntwo\nthree\nfour\n";
        assert_eq!(line_changes(committed, committed), LineChanges::default());
        assert_eq!(line_changes(committed, "one\nnew\ntwo\nthree\nfour\n"), LineChanges { added: vec![1], ..Default::default() });
        assert_eq!(line_changes(committed, "one\nTWO\nthree\nfour\n"), LineChanges { changed: vec![1], ..Default::default() });
        assert_eq!(line_changes(committed, "one\nfour\n"), LineChanges { gone_before: vec![1], ..Default::default() });
        assert_eq!(line_changes(committed, "one\ntwo\nthree\n"), LineChanges { gone_before: vec![3], ..Default::default() }, "the end is gone");
        assert_eq!(line_changes("", "a\nb\n"), LineChanges { added: vec![0, 1], ..Default::default() });
        // A change knows the rows it had, for the gutter's popup to show them.
        let runs = change_runs(committed, "one\nTWO\nTHREE\nfour\nfive\n");
        assert_eq!(runs, [ChangeRun { old: 1..3, new: 1..3 }, ChangeRun { old: 4..4, new: 4..5 }]);
        let gone = change_runs(committed, "one\nfour\n");
        assert_eq!(gone, [ChangeRun { old: 1..3, new: 1..1 }]);
        assert!(gone[0].is_at(1) && !gone[0].is_at(0));
    }

    #[test]
    fn hump_words() {
        use gpui_kit::base::input::{first_hump_end, last_hump_start};
        let (ends, starts) = (|w: &str| first_hump_end(w), |w: &str| last_hump_start(w));
        assert_eq!((ends("fooBarBaz"), starts("fooBarBaz")), (3, 6));
        assert_eq!((ends("XMLHttpRequest"), starts("XMLHttpRequest")), (3, 7));
        assert_eq!((ends("my_var_name"), starts("my_var_name")), (2, 7), "underscores are between the humps");
        assert_eq!((ends("item2Count"), starts("item2Count")), (5, 5));
        assert_eq!((ends("__init__"), starts("__init__")), (6, 2));
        assert_eq!((ends("plain"), starts("plain")), (5, 0));
        assert_eq!((ends("=>"), starts("=>")), (2, 0), "what is no name is one piece");
    }

    #[test]
    fn next_occurrences() {
        let text = "ab cd ab cd ab";
        assert_eq!(next_occurrence(text, "ab", 2, 0), Some(6..8));
        assert_eq!(next_occurrence(text, "ab", 8, 0), Some(12..14));
        assert_eq!(next_occurrence(text, "ab", 14, 0), None, "back where it began");
        assert_eq!(next_occurrence(text, "ab", 14, 6), Some(0..2), "from the start again");
        assert_eq!(next_occurrence(text, "zz", 0, 0), None);
    }

    /// On a real editor: an edit goes in as one change and leaves the
    /// selection asked for, and with hump words on the word keys stop at
    /// the humps.
    #[gpui_kit::test]
    fn edits_and_hump_words_in_an_editor(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::component::input::{Editor, EditorState};
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, Bounds, Focusable as _, IntoElement, ParentElement as _, Point, Render, Styled as _, Window, WindowBounds, WindowOptions, div, px, size};

        struct Host(gpui_kit::Entity<EditorState>);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
                div().size_full().child(Editor::new(&self.0).h_full())
            }
        }
        cx.update(gpui_kit::init);
        let mut editor = None;
        let handle = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(800.), px(400.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| {
                let state = cx.new(|cx| EditorState::new(window, cx).language("rust").line_number(true));
                editor = Some(state.clone());
                cx.new(|_| Host(state))
            })
            .expect("a test window")
            .0
        });
        let editor = editor.unwrap();
        cx.update_window(handle, |_, window, cx| {
            editor.update(cx, |state, cx| state.set_value("let fooBarBaz = my_var;\nsecond\n", window, cx));
            // Duplicate the first line, as the editor's key does.
            let (text, selection) = editor.update(cx, |state, cx| {
                state.set_selected_range(4..4, cx);
                (state.text().to_string(), state.selected_range())
            });
            let edit = duplicate(&text, selection);
            editor.update(cx, |state, cx| {
                state.set_selected_range(edit.range, cx);
                state.replace(edit.text, window, cx);
                state.set_selected_range(edit.select.clone(), cx);
            });
            let state = editor.read(cx);
            assert_eq!(state.text().to_string(), "let fooBarBaz = my_var;\nlet fooBarBaz = my_var;\nsecond\n");
            assert_eq!(state.selected_range(), 28..28, "the cursor in the same column of the copy");

            // By word from the start of the name: whole words, then humps.
            let stops = |window: &mut Window, cx: &mut gpui_kit::App| {
                editor.update(cx, |state, cx| state.set_selected_range(4..4, cx));
                editor.read(cx).focus_handle(cx).focus(window, cx);
                window.render_frame(cx);
                let mut found = Vec::new();
                for _ in 0..3 {
                    window.press(if cfg!(target_os = "macos") { "alt-right" } else { "ctrl-right" }, cx);
                    found.push(editor.read(cx).selected_range().start);
                }
                found
            };
            gpui_kit::base::input::HUMP_WORDS.store(false, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(stops(window, cx), [13, 15, 22], "fooBarBaz, =, my_var");
            gpui_kit::base::input::HUMP_WORDS.store(true, std::sync::atomic::Ordering::Relaxed);
            let humps = stops(window, cx);
            gpui_kit::base::input::HUMP_WORDS.store(false, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(humps, [7, 10, 13], "foo, Bar, Baz");

            // The gutter draws with marks on rows that are there and on one that is not.
            let mut marks = gpui_kit::base::input::LineMarks::default();
            marks.rows.extend([(0, gpui_kit::green()), (1, gpui_kit::blue())]);
            marks.gone_before.extend([(2, gpui_kit::red()), (99, gpui_kit::red())]);
            editor.update(cx, |state, cx| state.set_line_marks(marks, cx));
            window.render_frame(cx);
        })
        .unwrap();
    }

}
