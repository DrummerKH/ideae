//! ideae patch: a prediction that edits more than the cursor's place. Its
//! edits elsewhere are shown where they are: what goes struck through, what
//! comes in green. One out of view, or of lines, is announced at the end of
//! the cursor's line; the first Tab goes there, the next makes them all.

use std::ops::Range;

use gpui::{Context, HighlightStyle, Hsla, StrikethroughStyle, Window, hsla, px};
use lsp_types::InlineCompletionItem;

use crate::input::{
    EditorMode, Inlay, InlayStyle, InputBaseState, RopeExt as _, TextDecoration,
    decorations::normalize,
};

/// The key of the inlays a prediction shows.
pub const EDIT_PREDICTION_KEY: &str = "edit-prediction";
/// An edit further than this many lines from the cursor is gone to first.
const NEAR: usize = 8;

/// What a provider predicts: text typed at the cursor, and edits of the
/// text anywhere, all taken by Tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditPrediction {
    /// Shown at the cursor as gray text.
    pub ghost: Option<InlineCompletionItem>,
    /// Every edit, the ghost's among them, as bytes of the text asked about
    /// and what replaces them; sorted, apart. Empty: the ghost alone.
    pub edits: Vec<(Range<usize>, String)>,
    /// Where the caret goes, in the text once edited; none: after the last edit.
    pub caret: Option<usize>,
}

impl EditPrediction {
    pub fn ghost(item: InlineCompletionItem) -> Self {
        Self {
            ghost: Some(item),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NextEdit {
    pub(crate) edits: Vec<(Range<usize>, String)>,
    pub(crate) caret: Option<usize>,
    /// Out of view: the first Tab moves the caret here.
    pub(crate) jump: Option<usize>,
    /// The edit the ghost text at the cursor shows.
    pub(crate) ghost: Option<usize>,
}

fn removed_style() -> HighlightStyle {
    let red = hsla(0., 0.75, 0.55, 1.);
    HighlightStyle {
        background_color: Some(red.opacity(0.18)),
        strikethrough: Some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(red),
        }),
        ..Default::default()
    }
}

fn inserted_style(foreground: Hsla) -> InlayStyle {
    InlayStyle {
        color: Some(foreground),
        background: Some(hsla(140. / 360., 0.6, 0.45, 0.28)),
        ..Default::default()
    }
}

/// Inserted lines as one line, `⏎` between them.
fn one_line(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').map(str::trim).collect();
    lines.join(" ⏎ ").trim().to_string()
}

impl InputBaseState<EditorMode> {
    /// A provider's answer, shown.
    pub(crate) fn show_edit_prediction(
        &mut self,
        prediction: EditPrediction,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let ghost = prediction.ghost.as_ref().and_then(|_| {
            prediction
                .edits
                .iter()
                .position(|(range, _)| range.start == cursor)
        });
        if prediction.edits.is_empty() || (prediction.edits.len() == 1 && ghost.is_some()) {
            if prediction.ghost.is_some() {
                self.extras.inline_completion.item = prediction.ghost;
                cx.notify();
            }
            return;
        }
        let mut next = NextEdit {
            edits: prediction.edits,
            caret: prediction.caret,
            jump: None,
            ghost,
        };
        next.jump = self.jump_target(&next);
        self.extras.inline_completion.item = ghost.and(prediction.ghost);
        self.extras.inline_completion.next = Some(next);
        self.mark_next_edit(cx);
    }

    /// Where the first Tab goes: the nearest edit that cannot be read from
    /// the cursor, out of view or on lines of its own.
    fn jump_target(&self, next: &NextEdit) -> Option<usize> {
        let row = |offset: usize| self.text.offset_to_point(offset).row;
        let cursor_row = row(self.cursor());
        let visible = self.last_layout.as_ref().and_then(|layout| {
            Some(*layout.visible_buffer_lines.first()?..*layout.visible_buffer_lines.last()? + 1)
        });
        next.edits
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != next.ghost)
            .filter_map(|(_, (range, new))| {
                let (first, last) = (row(range.start), row(range.end));
                let distance = first.abs_diff(cursor_row).max(last.abs_diff(cursor_row));
                let seen = visible
                    .as_ref()
                    .is_none_or(|rows| rows.contains(&first) && rows.contains(&last));
                (new.contains('\n') || distance > NEAR || !seen).then_some((distance, range.start))
            })
            .min()
            .map(|(_, at)| at)
    }

    /// The edits drawn where they are, the jump announced at the end of the
    /// cursor's line.
    fn mark_next_edit(&mut self, cx: &mut Context<Self>) {
        let Some(next) = &self.extras.inline_completion.next else {
            return;
        };
        let mut decorations = Vec::new();
        let mut inlays = Vec::new();
        let cursor_row = self.text.offset_to_point(self.cursor()).row;
        for (i, (range, new)) in next.edits.iter().enumerate() {
            if Some(i) == next.ghost {
                // The gray text covers the rest of the cursor's line; the lines
                // below that it replaces too are struck through.
                let end_row = self.text.offset_to_point(range.end).row;
                if end_row > cursor_row {
                    let lines = self.text.line_start_offset(cursor_row + 1)
                        ..self.text.line_end_offset(end_row);
                    decorations.push(TextDecoration::new(lines, removed_style()));
                }
                continue;
            }
            if !range.is_empty() {
                decorations.push(TextDecoration::new(range.clone(), removed_style()));
            }
            if !new.is_empty() {
                inlays.push(
                    Inlay::new(range.end, one_line(new))
                        .style(inserted_style(self.editor_style.foreground)),
                );
            }
        }
        // Gray text at the cursor goes in first; the jump is told after it.
        if let Some(target) = next.jump.filter(|_| next.ghost.is_none()) {
            let line = self.text.offset_to_point(target).row + 1;
            let style = InlayStyle {
                color: None,
                background: Some(self.editor_style.muted_foreground.opacity(0.15)),
                margin_left: 2.,
                ..Default::default()
            };
            inlays.push(
                Inlay::new(
                    self.text.line_end_offset(cursor_row),
                    format!("⇥ Tab: jump to line {line}"),
                )
                .style(style),
            );
        }
        let decorations = normalize(&self.text, decorations);
        let extras = &mut self.extras;
        let id = *extras
            .inline_completion
            .marks
            .get_or_insert_with(|| extras.decorations.create(Vec::new()));
        extras.decorations.set(id, decorations);
        self.set_inlays(EDIT_PREDICTION_KEY, None, inlays, cx);
        cx.notify();
    }

    pub(crate) fn unmark_next_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.extras.inline_completion.marks {
            self.extras.decorations.set(id, Vec::new());
        }
        self.clear_inlays(EDIT_PREDICTION_KEY, cx);
    }

    /// Tab on a prediction that edits elsewhere: the gray text at the cursor
    /// in first when there is an edit out of view, then to that edit, then
    /// all of them made, the caret where the provider says.
    pub(crate) fn accept_next_edit(
        &mut self,
        mut next: NextEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if next.jump.is_some()
            && let Some(ghost) = next.ghost.take()
        {
            let (range, new) = next.edits.remove(ghost);
            self.unmark_next_edit(cx);
            self.replace_text_in_ranges(&[(range.clone(), new.clone())], window, cx);
            let end = range.start + new.len();
            self.set_selected_range(end..end, cx);
            let shift = new.len() as isize - range.len() as isize;
            for (edit, _) in &mut next.edits {
                if edit.start >= range.end {
                    *edit = (edit.start as isize + shift) as usize
                        ..(edit.end as isize + shift) as usize;
                }
            }
            next.jump = self.jump_target(&next);
            self.extras.inline_completion.next = Some(next);
            self.mark_next_edit(cx);
            return;
        }
        if let Some(target) = next.jump.take() {
            self.move_to(target, None, cx);
            let cursor = self.cursor();
            next.ghost = next
                .edits
                .iter()
                .position(|(range, new)| range.start == cursor && new.contains('\n'));
            self.extras.inline_completion.item = next.ghost.map(|i| self.ghost_of(&next.edits[i]));
            self.extras.inline_completion.next = Some(next);
            self.mark_next_edit(cx);
            return;
        }
        self.unmark_next_edit(cx);
        let mut shift = 0isize;
        let mut end = 0;
        for (range, new) in &next.edits {
            end = (range.start as isize + shift) as usize + new.len();
            shift += new.len() as isize - range.len() as isize;
        }
        self.replace_text_in_ranges(&next.edits, window, cx);
        let caret = next.caret.unwrap_or(end).min(self.text.len());
        self.set_selected_range(caret..caret, cx);
        self.schedule_inline_completion(window, cx);
    }

    /// An edit starting at the cursor as gray text typed there: with the rest
    /// of its last line, over the rest of the cursor's line.
    fn ghost_of(&self, (range, new): &(Range<usize>, String)) -> InlineCompletionItem {
        let cursor = self.cursor();
        let eol = self
            .text
            .line_end_offset(self.text.offset_to_point(range.end).row);
        let replaces = range.end < eol || !range.is_empty();
        let insert_text = if replaces {
            new.clone() + &self.text.slice(range.end..eol).to_string()
        } else {
            new.clone()
        };
        InlineCompletionItem {
            insert_text,
            filter_text: None,
            range: replaces.then(|| {
                lsp_types::Range::new(
                    self.text.offset_to_position(cursor),
                    self.text.offset_to_position(eol),
                )
            }),
            command: None,
            insert_text_format: None,
        }
    }

    /// After `len` bytes typed at `cursor` that start the ghost's edit.
    pub(crate) fn follow_next_edit(
        &mut self,
        mut next: NextEdit,
        cursor: usize,
        len: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ghost) = next.ghost else {
            return false;
        };
        for (i, (range, new)) in next.edits.iter_mut().enumerate() {
            if i == ghost {
                new.drain(..len);
            } else if range.start < cursor {
                continue;
            }
            range.start += len;
            range.end += len;
        }
        if let Some(jump) = next.jump.as_mut().filter(|jump| **jump >= cursor) {
            *jump += len;
        }
        self.extras.inline_completion.next = Some(next);
        self.mark_next_edit(cx);
        true
    }

    /// ideae patch: what is shown of a prediction's edits, for tests.
    #[doc(hidden)]
    pub fn edit_prediction_shown(&self) -> Option<(Vec<(Range<usize>, String)>, Option<usize>)> {
        let next = self.extras.inline_completion.next.as_ref()?;
        Some((next.edits.clone(), next.jump))
    }
}
