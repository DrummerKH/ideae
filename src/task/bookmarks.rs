use super::*;

// ---------------------------------------------------------------- bookmarks

actions!(ideae_file, [ToggleBookmark, NextBookmark, PrevBookmark]);
actions!(ideae_task, [ShowBookmarks]);

pub(super) fn bookmark_color() -> Hsla {
    hsla(0.58, 0.75, 0.55, 1.)
}

/// The bookmark after the line, or before it when `back`: round the end
/// of the file to the other end.
pub(super) fn next_bookmark(rows: &[usize], line: usize, back: bool) -> Option<usize> {
    if back {
        rows.iter().rev().find(|&&r| r < line).or(rows.last()).copied()
    } else {
        rows.iter().find(|&&r| r > line).or(rows.first()).copied()
    }
}

impl FileView {
    /// F11: a bookmark on the cursor's line, or none any more.
    pub(super) fn toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        let row = self.editor.read(cx).cursor_position().line as usize;
        self.editor.update(cx, |state, cx| state.toggle_bookmark(row, cx));
    }

    pub(super) fn go_to_bookmark(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let line = editor.cursor_position().line as usize;
        if let Some(row) = next_bookmark(&editor.bookmark_rows(), line, back) {
            self.go_to_line(row as u32, 0, window, cx);
        }
    }

    /// The bookmarked lines, 0-based.
    pub(super) fn bookmarks(&self, cx: &App) -> Vec<u32> {
        self.editor.read(cx).bookmark_rows().into_iter().map(|r| r as u32).collect()
    }
}

impl TaskView {
    fn rel_path(&self, path: &std::path::Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
    }

    /// Every bookmark of the task: of the open files as they are now, of
    /// the others as they were when closed.
    pub(super) fn all_bookmarks(&self, cx: &App) -> BTreeMap<String, Vec<u32>> {
        let mut all = self.bookmarks.clone();
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                let f = file.read(cx);
                let rows = f.bookmarks(cx);
                let path = self.rel_path(&f.path);
                if rows.is_empty() {
                    all.remove(&path);
                } else {
                    all.insert(path, rows);
                }
            }
        }
        all
    }

    /// A file opens with the bookmarks it had.
    pub(super) fn give_bookmarks(&self, path: &std::path::Path, editor: &Entity<EditorState>, cx: &mut App) {
        let rows: Vec<usize> = self.bookmarks.get(&self.rel_path(path)).map(|rows| rows.iter().map(|&r| r as usize).collect()).unwrap_or_default();
        editor.update(cx, |state, cx| state.set_bookmarks(&rows, bookmark_color(), cx));
    }

    /// A file closes: its bookmarks are kept for when it opens again.
    pub(super) fn keep_bookmarks(&mut self, file: &Entity<FileView>, cx: &App) {
        let f = file.read(cx);
        let (path, rows) = (self.rel_path(&f.path), f.bookmarks(cx));
        if rows.is_empty() {
            self.bookmarks.remove(&path);
        } else {
            self.bookmarks.insert(path, rows);
        }
    }

    /// Shift+F11: the task's bookmarks, to go to one.
    pub(super) fn show_bookmarks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let all = self.all_bookmarks(cx);
        // The lines of the open files from their editors, of the others from disk.
        let mut open: HashMap<String, Vec<String>> = HashMap::new();
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                let f = file.read(cx);
                let path = self.rel_path(&f.path);
                let Some(rows) = all.get(&path) else { continue };
                let text = f.editor.read(cx).text();
                let lines = rows.iter().map(|&r| text.slice_line(r as usize).to_string()).collect();
                open.insert(path, lines);
            }
        }
        let root = self.root.clone();
        let task = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let hits = cx
                .background_spawn(async move {
                    let mut hits = Vec::new();
                    for (path, rows) in all {
                        let lines = match open.remove(&path) {
                            Some(lines) => lines,
                            // Gone since, or moved: its bookmarks are not listed.
                            None => {
                                let Ok(content) = std::fs::read_to_string(root.join(&path)) else { continue };
                                let lines: Vec<&str> = content.lines().collect();
                                rows.iter().map(|&r| lines.get(r as usize).copied().unwrap_or("").to_string()).collect()
                            }
                        };
                        for (&row, text) in rows.iter().zip(lines) {
                            hits.push(crate::finder::Hit { path: path.clone(), line: row + 1, col: 0, text: text.trim().to_string(), ranges: Vec::new() });
                        }
                    }
                    (root, hits)
                })
                .await;
            let (root, hits) = hits;
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_places(root, "Bookmarks".into(), vec![("Bookmarks".into(), hits)], task, window, cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::next_bookmark;

    #[test]
    fn the_next_bookmark_goes_round() {
        let rows = [3, 10, 20];
        assert_eq!(next_bookmark(&rows, 0, false), Some(3));
        assert_eq!(next_bookmark(&rows, 10, false), Some(20));
        assert_eq!(next_bookmark(&rows, 20, false), Some(3));
        assert_eq!(next_bookmark(&rows, 10, true), Some(3));
        assert_eq!(next_bookmark(&rows, 2, true), Some(20));
        assert_eq!(next_bookmark(&[], 2, true), None);
    }

    /// On a real editor: bookmarks go with their lines as lines are put in
    /// and taken out above them, and the gutter draws them beside notes.
    #[gpui_kit::test]
    fn bookmarks_follow_their_lines(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::base::input::{GutterNote, GutterNotes};
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::component::input::{Editor, EditorState};
        use gpui_kit::{AppContext as _, Bounds, IntoElement, ParentElement as _, Point, Render, Styled as _, Window, WindowBounds, WindowOptions, div, px, size};

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
                let state = cx.new(|cx| EditorState::new(window, cx).line_number(true));
                editor = Some(state.clone());
                cx.new(|_| Host(state))
            })
            .expect("a test window")
            .0
        });
        let editor = editor.unwrap();
        cx.update_window(handle, |_, window, cx| {
            let edit = |range: std::ops::Range<usize>, text: &str, window: &mut Window, cx: &mut gpui_kit::App| {
                editor.update(cx, |state, cx| {
                    state.set_selected_range(range, cx);
                    state.replace(text.to_string(), window, cx);
                });
                editor.read(cx).bookmark_rows()
            };
            editor.update(cx, |state, cx| {
                state.set_value("zero\none\ntwo\nthree\n", window, cx);
                state.set_bookmarks(&[1, 3], super::bookmark_color(), cx);
            });
            assert_eq!(editor.read(cx).bookmark_rows(), [1, 3]);
            assert_eq!(edit(0..0, "new\n", window, cx), [2, 4], "a line put in above both");
            assert_eq!(edit(0..4, "", window, cx), [1, 3], "and taken out");
            assert_eq!(edit(9..9, "\n", window, cx), [1, 4], "a line opened at the start of one goes before it");
            assert_eq!(edit(5..6, "O", window, cx), [1, 4], "an edit in the line leaves it");
            editor.update(cx, |state, cx| state.toggle_bookmark(1, cx));
            assert_eq!(editor.read(cx).bookmark_rows(), [4]);
            editor.update(cx, |state, cx| state.toggle_bookmark(0, cx));
            assert_eq!(editor.read(cx).bookmark_rows(), [0, 4]);

            let note = |text: &str| Some(GutterNote { text: text.to_string().into(), color: gpui_kit::black(), tint: Some(gpui_kit::blue()) });
            let notes = GutterNotes { chars: 20, rows: vec![note("2026-11-03 Ann"), note(""), None, note("2026-11-04 Bob")] };
            editor.update(cx, |state, cx| state.set_gutter_notes(Some(std::rc::Rc::new(notes)), cx));
            window.render_frame(cx);
            editor.update(cx, |state, cx| state.set_gutter_notes(None, cx));
            window.render_frame(cx);
        })
        .unwrap();
    }
}
