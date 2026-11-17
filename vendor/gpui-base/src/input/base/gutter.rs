//! ideae patch: what ideae puts in the gutter beside the line numbers: a
//! column of notes left of them (who last changed each line) and the
//! bookmarked lines, which follow the edits made around them.

use std::{ops::Range, rc::Rc};

use gpui::{App, Context, Hsla, Pixels, Point, SharedString, Window, px};

use super::{InputBaseState, RopeExt as _, kind::InputModeKind};

/// Room between the notes and the line numbers.
pub(super) const NOTE_GAP: Pixels = px(10.);
/// Room for a bookmark's mark, left of the line numbers.
pub(super) const MARK_WIDTH: Pixels = px(12.);

/// A note by each buffer row, in a column `chars` wide.
pub struct GutterNotes {
    pub chars: usize,
    pub rows: Vec<Option<GutterNote>>,
}

#[derive(Clone)]
pub struct GutterNote {
    /// Empty on a row that goes on with the note above it.
    pub text: SharedString,
    /// The note of the run the row is in: shown on the top row in view when
    /// the run began above it.
    pub run: SharedString,
    pub color: Hsla,
    /// Behind the note, the row's height.
    pub tint: Option<Hsla>,
}

/// The pointer over a note of the gutter, with where it is in the window,
/// or a click on one: by buffer row. `Change` is a click on the mark of a
/// change (see [`super::LineMarks`]) at the gutter's right edge.
pub enum GutterEvent {
    Hover(Option<(usize, Point<Pixels>)>),
    Click(usize),
    Change(usize),
}

/// How far left of the gutter's right edge a click is on a change's mark:
/// the mark and the margin around it, short of the fold icons.
const CHANGE_MARK_REACH: Pixels = px(6.);

pub(super) type GutterListener = Rc<dyn Fn(&GutterEvent, &mut Window, &mut App)>;

#[derive(Default)]
pub(super) struct Gutter {
    pub(super) notes: Option<Rc<GutterNotes>>,
    pub(super) listener: Option<GutterListener>,
    pub(super) hovered: Option<usize>,
    /// The row of the last right press in the gutter, for its menu.
    pub(super) pressed: Option<usize>,
    /// Where the bookmarked lines are, as offsets into the text that edits
    /// move; None leaves no room for them.
    pub(super) bookmarks: Option<Vec<usize>>,
    pub(super) bookmark_color: Hsla,
}

/// Where an offset is after `range` was replaced by `new_len` bytes. Text
/// put in right at it pushes it on: a line opened above a bookmarked one
/// leaves the bookmark with its text.
pub(super) fn moved_offset(offset: usize, range: &Range<usize>, new_len: usize) -> usize {
    if offset < range.start {
        offset
    } else if offset >= range.end {
        offset - (range.end - range.start) + new_len
    } else {
        range.start
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    /// The notes left of the line numbers; None takes the column away.
    pub fn set_gutter_notes(&mut self, notes: Option<Rc<GutterNotes>>, cx: &mut Context<Self>) {
        let same = match (&self.gutter.notes, &notes) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.gutter.notes = notes;
            cx.notify();
        }
    }

    pub fn has_gutter_notes(&self) -> bool {
        self.gutter.notes.is_some()
    }

    /// Told when the pointer goes over a note or a note is clicked.
    pub fn on_gutter(&mut self, listener: impl Fn(&GutterEvent, &mut Window, &mut App) + 'static) {
        self.gutter.listener = Some(Rc::new(listener));
    }

    /// The row the last right press in the gutter was on, if it was there.
    pub fn gutter_press(&self) -> Option<usize> {
        self.gutter.pressed
    }

    /// Bookmarks these rows, and keeps room for marks in the gutter.
    pub fn set_bookmarks(&mut self, rows: &[usize], color: Hsla, cx: &mut Context<Self>) {
        let lines = self.text.lines_len();
        let mut offsets: Vec<usize> =
            rows.iter().filter(|&&row| row < lines).map(|&row| self.text.line_start_offset(row)).collect();
        offsets.sort_unstable();
        offsets.dedup();
        self.gutter.bookmarks = Some(offsets);
        self.gutter.bookmark_color = color;
        cx.notify();
    }

    /// The bookmarked rows, in order.
    pub fn bookmark_rows(&self) -> Vec<usize> {
        let Some(offsets) = &self.gutter.bookmarks else { return Vec::new() };
        let len = self.text.len();
        let mut rows: Vec<usize> = offsets.iter().map(|&o| self.text.offset_to_point(o.min(len)).row).collect();
        rows.dedup();
        rows
    }

    /// Bookmarks the row, or takes its bookmark away.
    pub fn toggle_bookmark(&mut self, row: usize, cx: &mut Context<Self>) {
        let mut rows = self.bookmark_rows();
        match rows.binary_search(&row) {
            Ok(ix) => {
                rows.remove(ix);
            }
            Err(ix) => rows.insert(ix, row),
        }
        let color = self.gutter.bookmark_color;
        self.set_bookmarks(&rows, color, cx);
    }

    pub(super) fn move_bookmarks(&mut self, range: &Range<usize>, new_len: usize) {
        if let Some(offsets) = self.gutter.bookmarks.as_mut() {
            for offset in offsets.iter_mut() {
                *offset = moved_offset(*offset, range, new_len);
            }
        }
    }

    /// The buffer row under a point in the gutter, and whether the point is
    /// on its note.
    pub(super) fn gutter_row_at(&self, position: Point<Pixels>) -> Option<(usize, bool)> {
        let layout = self.last_layout.as_ref()?;
        let x = position.x - self.input_bounds.origin.x;
        if x >= layout.line_number_width || !self.input_bounds.contains(&position) {
            return None;
        }
        let (offset, _, _) = self.resolve_mouse_position(position);
        let row = self.text.offset_to_point(offset).row;
        Some((row, x < layout.notes_width))
    }

    /// The pointer moved: tells when it comes onto a note or leaves one.
    pub(super) fn gutter_hover(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.gutter.notes.is_none() && self.gutter.hovered.is_none() {
            return;
        }
        let row = self.gutter_row_at(position).filter(|(_, on_note)| *on_note).map(|(row, _)| row);
        let row = row.filter(|&row| {
            self.gutter.notes.as_ref().is_some_and(|notes| notes.rows.get(row).is_some_and(Option::is_some))
        });
        if row == self.gutter.hovered {
            return;
        }
        self.gutter.hovered = row;
        if let Some(listener) = self.gutter.listener.clone() {
            let event = GutterEvent::Hover(row.map(|row| (row, position)));
            window.defer(cx, move |window, cx| listener(&event, window, cx));
        }
    }

    /// A press in the gutter: a left one on a change's mark or on a note is
    /// theirs; a right one is remembered for the menu. True when the press
    /// is taken.
    pub(super) fn gutter_press_at(
        &mut self,
        position: Point<Pixels>,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let at = self.gutter_row_at(position);
        if right {
            self.gutter.pressed = at.map(|(row, _)| row);
            return false;
        }
        if let Some((row, _)) = at
            && let Some(layout) = self.last_layout.as_ref()
            && position.x - self.input_bounds.origin.x >= layout.line_number_width - CHANGE_MARK_REACH
            && (self.line_marks.rows.contains_key(&row) || self.line_marks.gone_before.contains_key(&row))
            && let Some(listener) = self.gutter.listener.clone()
        {
            window.defer(cx, move |window, cx| listener(&GutterEvent::Change(row), window, cx));
            return true;
        }
        let Some((row, true)) = at else { return false };
        let has_note = self.gutter.notes.as_ref().is_some_and(|notes| notes.rows.get(row).is_some_and(Option::is_some));
        let Some(listener) = self.gutter.listener.clone().filter(|_| has_note) else { return false };
        window.defer(cx, move |window, cx| listener(&GutterEvent::Click(row), window, cx));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::moved_offset;

    #[test]
    fn an_offset_follows_the_edits_around_it() {
        // Typed before it: pushed on; after it: stays.
        assert_eq!(moved_offset(10, &(3..3), 4), 14);
        assert_eq!(moved_offset(10, &(12..12), 4), 10);
        // A line opened right at it: it goes down with its text.
        assert_eq!(moved_offset(10, &(10..10), 1), 11);
        // Text before it taken away; text around it taken away.
        assert_eq!(moved_offset(10, &(2..6), 0), 6);
        assert_eq!(moved_offset(10, &(8..14), 2), 8);
        // Text after it taken away.
        assert_eq!(moved_offset(10, &(10..14), 0), 10);
    }
}
