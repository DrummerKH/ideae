//! Whatever cuts or measures text, run at every position of a text in many
//! scripts: a byte offset inside a character ends the app.

use std::path::Path;

use gpui_kit::component::Rope;

use super::helpers::{markdown_link_at, plain_parent, text_before, toggle_comment_lines};
use super::outline::outline;
use crate::{db, edit, naming, said};

const TEXT: &str = "Привет, мир — «слово» 你好世界 こんにちは 🎉👍🏽 é e\u{301} مرحبا\n\
    \tfn main() { let ñ = \"строка 🎉\"; } // комментарий — ещё\n\
    # заголовок [ссылка](файл.md) и [🎉](путь/図.md#якорь) текст\n\
    // region Область «один»\n\
    select 'стр\\'ока', `поле` from таблица where имя = :параметр; -- конец\n\
    -- endregion\n\
    \n\
    \u{a0}конец 🎉";

fn places(text: &str) -> Vec<usize> {
    text.char_indices().map(|(at, _)| at).chain([text.len()]).collect()
}

fn whole(text: &str, range: &std::ops::Range<usize>) {
    assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end), "{range:?}");
}

#[test]
fn selections_and_line_edits() {
    let all = places(TEXT);
    for &at in &all {
        let mut range = at..at;
        for _ in 0..4 {
            range = plain_parent(TEXT, range);
            whole(TEXT, &range);
        }
        if let Some(word) = edit::word_at(TEXT, at) {
            whole(TEXT, &word);
        }
        for path in ["a.rs", "a.sql", "a.py", "a.html"] {
            toggle_comment_lines(Path::new(path), TEXT, at..at);
        }
        let _ = edit::next_occurrence(TEXT, "🎉", at, 0);
        let _ = edit::rewrite(TEXT, &TEXT.replace("слово", "дело 🎉"), at);
    }
    for (i, &from) in all.iter().enumerate().step_by(3) {
        for &to in all[i..].iter().step_by(7) {
            let selection = from..to;
            edit::delete_lines(TEXT, selection.clone());
            edit::duplicate(TEXT, selection.clone());
            let _ = edit::move_lines(TEXT, selection.clone(), true);
            let _ = edit::move_lines(TEXT, selection.clone(), false);
            let _ = edit::join_lines(TEXT, selection.clone());
            let _ = edit::toggle_case(TEXT, selection.clone());
            toggle_comment_lines(Path::new("a.rs"), TEXT, selection);
        }
    }
}

#[test]
fn what_is_read_before_a_word_starts_at_a_letter() {
    let rope = Rope::from(TEXT);
    for at in places(TEXT) {
        for bytes in [1, 2, 3, 5, 64] {
            let before = text_before(&rope, at, bytes);
            assert!(TEXT[..at].ends_with(&before));
        }
    }
}

#[test]
fn links_and_structure() {
    for line in TEXT.lines() {
        for at in 0..=line.len() {
            if let Some((bytes, _)) = markdown_link_at(line, at) {
                whole(line, &bytes);
            }
        }
    }
    crate::grammars::register();
    for end in places(TEXT).into_iter().step_by(4) {
        for path in ["a.md", "a.sql", "a.rs", "a.py", "a.ts", "a.yaml", "a.html", "a.txt"] {
            outline(Path::new(path), &TEXT[..end]);
        }
    }
}

#[test]
fn statements_and_names() {
    let schema = db::schema::Schema::default();
    let all = places(TEXT);
    for &end in &all {
        let head = &TEXT[..end];
        for range in db::statement::split(head) {
            whole(head, &range);
            db::statement::top_words(&head[range]);
        }
        db::statement::parameters(head);
        db::statement::with_parameters(head, db::config::Driver::MySql, |_| Some("значение 🎉".into()));
        db::schema::suggest(&schema, head, TEXT);
        let _ = db::schema::name_at(Some(&schema), TEXT, end);
    }
    let statements = db::statement::split(TEXT);
    for at in all {
        if let Some(range) = db::statement::at(TEXT, &statements, at) {
            whole(TEXT, &range);
        }
    }
}

#[test]
fn names_and_what_is_shown_short() {
    for end in places(TEXT) {
        let head = &TEXT[..end];
        naming::branch_name(head);
        naming::preview(&format!("DEV-1 {head}"));
        for max in [1, 2, 5, 40] {
            said::cap(head, max);
            said::lead(head, max);
        }
    }
    let long = "ё".repeat(100);
    assert!(naming::branch_name(&long).is_ascii());
}
