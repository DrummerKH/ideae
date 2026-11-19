use anyhow::Result;
use gpui::{App, Context, MouseMoveEvent, Task, Window};
use instant::Duration;
use ropey::Rope;

use crate::input::{EditorMode, HoverPopoverState, InputBaseState, RopeExt};

/// Hover provider
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
pub trait HoverProvider {
    /// textDocument/hover
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
    fn hover(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>>;
}

impl InputBaseState<EditorMode> {
    /// Handle hover trigger LSP request.
    pub(super) fn handle_hover_popover(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<InputBaseState<EditorMode>>,
    ) {
        if self.selecting {
            return;
        }

        let Some(provider) = self.extras.lsp.hover_provider.clone() else {
            return;
        };

        if let Some(hover_popover) = self.extras.hover_popover.as_ref() {
            if hover_popover.symbol_range.contains(&offset) {
                return;
            }
        }

        let text = self.text.clone();
        let mut symbol_range = self.text.word_range(offset).unwrap_or(offset..offset);
        let editor = cx.entity();
        // ideae patch: the pointer rests a while before a hover shows, and
        // again before one shown gives way to another (JetBrains waits half a
        // second): passed over on the way somewhere, words show nothing.
        let delay = if self.extras.hover_popover.is_none() { 500 } else { 300 };
        // Ask the provider only after the delay: every mouse move replaces this
        // task, so positions the pointer merely passes over never reach it.
        self.extras.lsp._hover_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;

            let task = this.update_in(cx, |_, window, cx| {
                provider.hover(&text, offset, window, cx)
            })?;
            let result = task.await?;

            _ = editor.update(cx, |editor, cx| {
                match result {
                    Some(hover) => {
                        if let Some(range) = hover.range {
                            let start = editor.text.position_to_offset(&range.start);
                            let end = editor.text.position_to_offset(&range.end);
                            symbol_range = start..end;
                        }
                        editor.extras.hover_popover = Some(HoverPopoverState {
                            symbol_range,
                            hover,
                        });
                    }
                    None => {
                        editor.extras.hover_popover = None;
                    }
                }
                cx.notify();
            });

            Ok(())
        });
    }

    pub(crate) fn handle_mouse_move(
        &mut self,
        offset: usize,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.alt {
            self.clear_hover_state(cx);
            return;
        }
        if event.modifiers.secondary(cx) {
            self.handle_hover_definition(offset, window, cx);
        } else {
            // Repaint only when the definition underline goes away; the hover
            // and definition results notify when they arrive.
            if !self.extras.hover_definition.is_empty() {
                self.extras.hover_definition.clear();
                cx.notify();
            }
            self.handle_hover_popover(offset, window, cx);
        }
    }

    pub fn clear_hover_state(&mut self, cx: &mut Context<Self>) {
        let changed =
            !self.extras.hover_definition.is_empty() || self.extras.hover_popover.is_some();
        self.extras.hover_definition.clear();
        self.extras.hover_popover = None;
        self.extras.lsp._hover_task = Task::ready(Ok(()));
        if changed {
            cx.notify();
        }
    }
}
