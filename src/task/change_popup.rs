use super::*;
use crate::edit::ChangeRun;

/// Lines of a change shown at most: the popup is a glance, the Changes tab
/// has the rest.
const SHOWN_LINES: usize = 200;

/// A change of the open file, opened from its mark in the gutter, as
/// JetBrains IDEs do: what the lines were, and what can be done with it.
pub(super) struct ChangePopup {
    /// Which of the file's changes, in `FileView::change_runs`.
    ix: usize,
    run: ChangeRun,
    /// Its lines as the commit has them; empty when it only adds.
    old: SharedString,
    /// The same change in the task's diff, and whether it is staged: what
    /// the Changes tab's Revert and Stage take. None while the file has
    /// unsaved edits, or when the marks are against the base branch and the
    /// change is committed.
    hunk: Option<(git::Hunk, bool)>,
}

impl FileView {
    /// A click on the mark of a change at `row`.
    pub(super) fn show_change(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(ix) = self.change_runs.iter().position(|run| run.is_at(row)) {
            self.open_change(ix, cx);
        }
    }

    fn open_change(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(run) = self.change_runs.get(ix).cloned() else { return };
        let old: String = self
            .committed
            .as_deref()
            .map(|text| text.split_inclusive('\n').skip(run.old.start).take(run.old.len()).collect())
            .unwrap_or_default();
        let hunk = self.hunk_of(&run, &old, cx);
        self.change_popup = Some(ChangePopup { ix, run, old: old.into(), hunk });
        cx.notify();
    }

    /// The task's hunk that is this change: in the same file, taking away
    /// the same lines and adding as many, at the same place.
    fn hunk_of(&self, run: &ChangeRun, old: &str, cx: &App) -> Option<(git::Hunk, bool)> {
        if self.dirty {
            return None;
        }
        let task = self.task.upgrade()?;
        let task = task.read(cx);
        let path = self.path.strip_prefix(&self.root).ok()?.to_string_lossy().replace('\\', "/");
        let hunk = task.uncommitted_hunks.iter().find(|h| {
            let (first, _) = super::changes::change_lines(h);
            let at = if h.modified { run.new.start } else { run.old.start };
            h.path == path
                && first as usize == at + 1
                && h.changes.lines().filter(|l| l.starts_with('+')).count() == run.new.len()
                && h.changes.lines().filter_map(|l| l.strip_prefix('-')).eq(old.lines())
        })?;
        let staged = task.changes.read(cx).staged.contains(&super::changes::staged_key(&hunk.path, &hunk.changes));
        Some((hunk.clone(), staged))
    }

    /// The previous or next change of the file, the cursor taken there.
    fn step_change(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = &self.change_popup else { return };
        let ix = if forward { popup.ix + 1 } else { popup.ix.wrapping_sub(1) };
        let Some(run) = self.change_runs.get(ix) else { return };
        let row = run.new.start.min(self.editor.read(cx).text().lines_len().saturating_sub(1));
        self.jump(row as u32, 0, window, cx);
        self.open_change(ix, cx);
    }

    /// Takes the change back: as the Changes tab does when the task's diff
    /// has it, else in the editor, where Undo brings it back.
    fn revert_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = self.change_popup.take() else { return };
        cx.notify();
        if let Some((hunk, _)) = popup.hunk {
            let task = self.task.clone();
            window.defer(cx, move |window, cx| {
                let _ = task.update(cx, |t, cx| t.revert_hunk(hunk, window, cx));
            });
            return;
        }
        let range = {
            let text = self.editor.read(cx).text();
            text.line_start_offset(popup.run.new.start)..text.line_start_offset(popup.run.new.end)
        };
        self.editor.update(cx, |state, cx| {
            state.set_selected_range(range.clone(), cx);
            state.replace(popup.old.to_string(), window, cx);
            state.set_selected_range(range.start..range.start, cx);
            state.focus(window, cx);
        });
    }

    fn stage_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((hunk, staged)) = self.change_popup.take().and_then(|p| p.hunk) else { return };
        cx.notify();
        let task = self.task.clone();
        window.defer(cx, move |window, cx| {
            let _ = task.update(cx, |t, cx| t.stage_hunk(hunk, !staged, window, cx));
        });
    }

    pub(super) fn close_change(&mut self, cx: &mut Context<Self>) {
        if self.change_popup.take().is_some() {
            cx.notify();
        }
    }

    /// The popup under the change, or over the row before which lines are
    /// gone; nothing while that row is out of view.
    pub(super) fn render_change_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popup = self.change_popup.as_ref()?;
        let editor = self.editor.read(cx);
        let text = editor.text();
        let gone = popup.run.new.is_empty();
        let row = if gone { popup.run.new.start } else { popup.run.new.end - 1 };
        let row = row.min(text.lines_len().saturating_sub(1));
        let offset = text.line_start_offset(row);
        let line = editor.range_to_bounds(&(offset..offset))?;
        let at = point(editor.input_bounds().origin.x, if gone && popup.run.new.start < text.lines_len() { line.top() } else { line.bottom() });

        let (count, last) = (self.change_runs.len(), popup.ix + 1 >= self.change_runs.len());
        let lines = |n: usize| if n == 1 { "1 line".to_string() } else { format!("{n} lines") };
        let title = match (popup.run.old.len(), popup.run.new.len()) {
            (0, n) => format!("Added {}", lines(n)),
            (n, 0) => format!("Removed {}", lines(n)),
            (_, n) => format!("Changed {}", lines(n)),
        };
        let staged = popup.hunk.as_ref().map(|(_, staged)| *staged);
        let why_not = if self.dirty { "Save the file first: staging takes what is on disk" } else { "Not a change of the worktree (the marks are against the base branch)" };
        let mono = cx.theme().mono_font_family.clone();
        let removed = hsla(0.0, 0.7, 0.55, 1.);
        let body = if popup.old.is_empty() {
            div().px_3().py_2().text_xs().text_color(cx.theme().muted_foreground).child("Added lines: nothing was here before.").into_any_element()
        } else {
            let mut shown: Vec<AnyElement> = popup
                .old
                .lines()
                .take(SHOWN_LINES)
                .map(|l| div().px_3().min_h(px(16.)).whitespace_nowrap().child(l.replace('\t', "    ")).into_any_element())
                .collect();
            if popup.run.old.len() > SHOWN_LINES {
                shown.push(div().px_3().text_color(cx.theme().muted_foreground).child(format!("… {} more", popup.run.old.len() - SHOWN_LINES)).into_any_element());
            }
            div()
                .id("change-popup-old")
                .max_h(px(260.))
                .overflow_y_scroll()
                .overflow_x_hidden()
                .py_1()
                .bg(removed.opacity(0.12))
                .font_family(mono)
                .text_xs()
                .children(shown)
                .into_any_element()
        };
        let button = |id: &'static str, label: &'static str| Button::new(id).xsmall().ghost().label(label);
        let card = v_flex()
            .id("change-popup")
            .occlude()
            .w(px(560.))
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_md()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_change(cx)))
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().text_xs().text_color(cx.theme().muted_foreground).child(format!("{title} · change {} of {count}", popup.ix + 1)))
                    .child(button("change-prev", "↑").disabled(popup.ix == 0).tooltip("The previous change").on_click(cx.listener(|this, _, window, cx| this.step_change(false, window, cx))))
                    .child(button("change-next", "↓").disabled(last).tooltip("The next change").on_click(cx.listener(|this, _, window, cx| this.step_change(true, window, cx))))
                    .child(
                        button("change-revert", "↺ Revert")
                            .tooltip("Take this change back; the rest of the file stays (Undo follows)")
                            .on_click(cx.listener(|this, _, window, cx| this.revert_change(window, cx))),
                    )
                    .child(
                        button("change-stage", if staged == Some(true) { "− Unstage" } else { "+ Stage" })
                            .disabled(staged.is_none())
                            .tooltip(match staged {
                                Some(true) => "Take this change out of what is staged",
                                Some(false) => "Stage this change alone",
                                None => why_not,
                            })
                            .on_click(cx.listener(|this, _, window, cx| this.stage_change(window, cx))),
                    )
                    .child(button("change-close", "×").tooltip("Close (Esc)").on_click(cx.listener(|this, _, _, cx| this.close_change(cx)))),
            )
            .child(body);
        Some(deferred(anchored().position(at).snap_to_window().child(card)).with_priority(1).into_any_element())
    }
}
