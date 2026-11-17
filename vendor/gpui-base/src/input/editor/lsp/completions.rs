use crate::input::EditorMode;
use anyhow::Result;
use gpui::{App, Context, EntityInputHandler, Pixels, Task, Window, px};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext,
    InlineCompletionItem, InlineCompletionResponse, InlineCompletionTriggerKind,
    request::Completion,
};
use ropey::Rope;
use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use crate::input::{InputBaseState, RopeExt as _, ShowCompletions};

/// Default debounce duration for inline completions.
const DEFAULT_INLINE_COMPLETION_DEBOUNCE: Duration = Duration::from_millis(300);

/// Decides which bytes of a completion label the menu paints as the match.
///
/// `query` is the word typed since the menu opened. Returned ranges are byte
/// offsets into [`CompletionItem::label`].
pub type CompletionHighlighter = dyn Fn(&str, &CompletionItem) -> Vec<Range<usize>>;

/// Highlight a prefix of the completion label, matching today's menu.
///
/// The painted length is [`CompletionItem::filter_text`] when the item sets
/// one, otherwise the query. It is shortened so the range never splits a
/// character. An empty length paints nothing.
pub fn highlight_prefix(query: &str, item: &CompletionItem) -> Vec<Range<usize>> {
    let matched_len = item
        .filter_text
        .as_ref()
        .map(|text| text.len())
        .unwrap_or(query.len())
        .min(item.label.len());
    let end = item.label.floor_char_boundary(matched_len);
    if end == 0 { Vec::new() } else { vec![0..end] }
}

/// Highlight the first case-insensitive occurrence of `query` in the label.
///
/// An empty query, or a query that does not occur as a contiguous span,
/// paints nothing.
pub fn highlight_contains(query: &str, item: &CompletionItem) -> Vec<Range<usize>> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Vec::new();
    }

    let label: Vec<(usize, char)> = item.label.char_indices().collect();
    let Some(last_start) = label.len().checked_sub(query.len()) else {
        return Vec::new();
    };

    for start in 0..=last_start {
        let matched = query
            .iter()
            .zip(label[start..].iter())
            .all(|(query_char, (_, label_char))| chars_eq_ignore_case(*query_char, *label_char));
        if !matched {
            continue;
        }

        let byte_start = label[start].0;
        let (last_index, last_char) = label[start + query.len() - 1];
        return vec![byte_start..last_index + last_char.len_utf8()];
    }
    Vec::new()
}

/// Highlight each character of `query` in order, skipping characters in between.
///
/// Matching is case-insensitive. Adjacent hits merge into one range, so a
/// contiguous match still paints as a single span. A query that cannot be
/// matched in full paints nothing.
pub fn highlight_characters(query: &str, item: &CompletionItem) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut rest = item.label.as_str();
    let mut base = 0usize;
    for query_char in query.chars() {
        let Some((index, matched)) = rest
            .char_indices()
            .find(|(_, label_char)| chars_eq_ignore_case(query_char, *label_char))
        else {
            return Vec::new();
        };

        let start = base + index;
        let end = start + matched.len_utf8();
        let next = index + matched.len_utf8();
        base += next;
        rest = &rest[next..];
        if let Some(last) = ranges.last_mut() {
            if last.end == start {
                last.end = end;
                continue;
            }
        }
        ranges.push(start..end);
    }
    ranges
}

/// ideae patch: an LSP snippet as plain text, and where the caret goes.
///
/// Tabstops (`$1`, `${1}`), placeholders (`${1:name}`, nested), choices
/// (`${1|a,b|}`, the first) and variables (`$TM_FILENAME`, `${VAR:default}`,
/// their default) become their text. The returned range is the first
/// tabstop's text (`$1` before `$2`, `$0` last), selected when it has a
/// placeholder; with no tabstop, the end.
pub fn expand_snippet(snippet: &str) -> (String, Range<usize>) {
    let mut out = String::with_capacity(snippet.len());
    let mut stops: Vec<(u32, Range<usize>)> = Vec::new();
    expand_snippet_into(&mut snippet.chars().peekable(), &mut out, &mut stops, false);
    let first = stops
        .iter()
        .filter(|(n, _)| *n > 0)
        .min_by_key(|(n, _)| *n)
        .or_else(|| stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, range)| range.clone());
    let end = out.len();
    (out, first.unwrap_or(end..end))
}

fn expand_snippet_into(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
    stops: &mut Vec<(u32, Range<usize>)>,
    nested: bool,
) {
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next @ ('$' | '}' | '\\')) => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            '}' if nested => return,
            '$' => match chars.peek().copied() {
                Some(d) if d.is_ascii_digit() => {
                    let n = snippet_number(chars);
                    stops.push((n, out.len()..out.len()));
                }
                Some('{') => {
                    chars.next();
                    if chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                        let n = snippet_number(chars);
                        let start = out.len();
                        match chars.next() {
                            Some(':') => expand_snippet_into(chars, out, stops, true),
                            Some('|') => {
                                let mut first = true;
                                while let Some(c) = chars.next() {
                                    match c {
                                        '|' => {
                                            if chars.peek() == Some(&'}') {
                                                chars.next();
                                            }
                                            break;
                                        }
                                        ',' => first = false,
                                        '\\' => {
                                            if let (true, Some(next)) = (first, chars.next()) {
                                                out.push(next);
                                            }
                                        }
                                        c if first => out.push(c),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                        stops.push((n, start..out.len()));
                    } else {
                        while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                            chars.next();
                        }
                        match chars.next() {
                            Some(':') => expand_snippet_into(chars, out, stops, true),
                            Some('}') | None => {}
                            Some(_) => {
                                let mut depth = 1;
                                for c in chars.by_ref() {
                                    match c {
                                        '{' => depth += 1,
                                        '}' => {
                                            depth -= 1;
                                            if depth == 0 {
                                                break;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
                Some(v) if v.is_ascii_alphabetic() || v == '_' => {
                    while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                        chars.next();
                    }
                }
                _ => out.push('$'),
            },
            c => out.push(c),
        }
    }
}

fn snippet_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> u32 {
    let mut n = 0u32;
    while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
        n = n.saturating_mul(10).saturating_add(d);
        chars.next();
    }
    n
}

fn chars_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.eq_ignore_ascii_case(&right)
        || left.to_lowercase().eq(right.to_lowercase())
}

/// Display options for the LSP completion popover.
///
/// Accessed through [`super::Lsp::completion_menu`] so embedders can tweak the
/// popover without growing the [`InputBaseState`] API. Label highlighting is
/// configured separately, on [`super::Lsp::completion_highlighter`].
///
/// ```
/// use gpui_base::input::CompletionMenuOptions;
/// let options = CompletionMenuOptions {
///     max_width: gpui::px(480.),
/// };
/// let _copied = options;
/// let _again = options;
/// ```
#[derive(Debug, Clone, Copy)]
pub struct CompletionMenuOptions {
    /// Maximum width of the popover.
    ///
    /// Defaults to 320 px, which is fine for most identifiers but can
    /// truncate longer labels. Widen this when hosting an editor that
    /// surfaces long completion labels.
    pub max_width: Pixels,
}

impl Default for CompletionMenuOptions {
    fn default() -> Self {
        Self {
            max_width: px(320.),
        }
    }
}

/// A trait for providing code completions based on the current input state and context.
pub trait CompletionProvider {
    /// Fetches completions based on the given byte offset.
    ///
    /// - The `offset` is in bytes of current cursor.
    ///
    /// textDocument/completion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_completion
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>>;

    /// Fetches an inline completion suggestion for the given position.
    ///
    /// This is called after a debounce period when the user stops typing.
    /// The provider can analyze the text and cursor position to determine
    /// what inline completion suggestion to show.
    ///
    ///
    /// # Arguments
    /// * `rope` - The current text content
    /// * `offset` - The cursor position in bytes
    ///
    /// textDocument/inlineCompletion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlineCompletion
    fn inline_completion(
        &self,
        _rope: &Rope,
        _offset: usize,
        _trigger: InlineCompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<InlineCompletionResponse>> {
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    /// Returns the debounce duration for inline completions.
    ///
    /// Default: 300ms
    #[inline]
    fn inline_completion_debounce(&self) -> Duration {
        DEFAULT_INLINE_COMPLETION_DEBOUNCE
    }

    fn resolve_completions(
        &self,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _: &mut App,
    ) -> Task<Result<bool>> {
        Task::ready(Ok(false))
    }

    /// ideae patch: `item` went in over `range` of `before`, which is now
    /// `after`; what the server adds to it (imports, a command) follows here.
    fn completion_inserted(
        &self,
        _item: &CompletionItem,
        _before: &Rope,
        _range: Range<usize>,
        _after: &Rope,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    /// Determines if the completion should be triggered based on the given byte offset.
    ///
    /// This is called on the main thread.
    fn is_completion_trigger(&self, offset: usize, new_text: &str, cx: &mut App) -> bool;
}

pub(crate) struct InlineCompletion {
    /// Completion item to display as an inline completion suggestion
    pub(crate) item: Option<InlineCompletionItem>,
    /// Task for debouncing inline completion requests
    pub(crate) task: Task<Result<InlineCompletionResponse>>,
    /// ideae patch: the suggestion the last clear took away, for an edit that types along it.
    pub(crate) cleared: Option<ClearedInlineCompletion>,
}

pub(crate) struct ClearedInlineCompletion {
    item: InlineCompletionItem,
    cursor: usize,
    len: usize,
}

impl Default for InlineCompletion {
    fn default() -> Self {
        Self {
            item: None,
            task: Task::ready(Ok(InlineCompletionResponse::Array(vec![]))),
            cleared: None,
        }
    }
}

impl InputBaseState<EditorMode> {
    pub(crate) fn handle_completion_trigger(
        &mut self,
        range: &Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_inserting {
            return;
        }

        let Some(provider) = self.extras.lsp.completion_provider.clone() else {
            return;
        };

        // ideae patch: typing what the suggestion shows keeps the rest of it, and no menu comes over it.
        if self.follow_inline_completion(range, new_text, cx) {
            return;
        }

        // Always schedule inline completion (debounced).
        // It will check if menu is open before showing the suggestion.
        self.schedule_inline_completion(window, cx);

        let new_offset = self.cursor();
        // Measure the inserted text in the current document. The replaced range
        // uses pre-edit coordinates, which preceding multi-cursor edits can
        // shift. The active caret ends immediately after the normalized input,
        // including selection replacements and IME commits.
        let Some(start) = new_offset.checked_sub(new_text.len()) else {
            return;
        };

        if !provider.is_completion_trigger(start, new_text, cx) {
            return;
        }

        // `trigger_start_offset` latches where the word a menu was opened for
        // begins, so later keystrokes refine the same query instead of starting
        // over at each character. It only describes this edit while the edit
        // continues that word: the document between the latch and the edit
        // must still read as a prefix of the last query. Deleting back into
        // the word keeps it; typing somewhere else, or into a document that
        // has since been replaced, starts a new query at this edit instead of
        // handing the provider text the user never typed as a prefix.
        let completion = &self.extras.context_menu_content.completion;
        let latched = completion.trigger_start_offset.filter(|&latched| {
            latched <= start
                && start <= latched + completion.query.len()
                && self.text.is_char_boundary(latched)
                && self.text.is_char_boundary(start)
                && completion
                    .query
                    .starts_with(self.text.slice(latched..start).to_string().as_str())
        });
        let start_offset = latched.unwrap_or(start);
        if new_offset < start_offset {
            return;
        }

        let query = self
            .text_for_range(
                self.range_to_utf16(&(start_offset..new_offset)),
                &mut None,
                window,
                cx,
            )
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        self.request_completions(
            provider,
            start_offset,
            query.clone(),
            CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(query),
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_action_show_completions(
        &mut self,
        _: &ShowCompletions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_completions(window, cx);
    }

    /// Request completions at the caret and focus the editor, without editing text.
    ///
    /// The current identifier prefix is used to refine the menu. An empty
    /// prefix still asks the provider for suggestions. There is no default
    /// keybinding; applications can dispatch [`ShowCompletions`] in the
    /// `Input && mode == editor` context or call this method from a command.
    ///
    /// Disabled/readonly editors, IME preedit, and an already-open completion
    /// menu are left alone. A new request cancels the previous overlay request
    /// and inline suggestion, and closes the code-action menu.
    pub fn show_completions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled
            || self.readonly
            || self.completion_inserting
            || self.ime_marked_range.is_some()
            || self.extras.context_menu_content.completion.open
        {
            return;
        }
        let Some(provider) = self.extras.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let word_len: usize = self
            .text
            .chars_at(offset)
            .reversed()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .map(char::len_utf8)
            .sum();
        let start_offset = offset - word_len;
        let query = self.text.slice(start_offset..offset).to_string();

        self.hide_context_menu(cx);
        self.clear_inline_completion(cx);
        self.focus(window, cx);

        self.request_completions(
            provider,
            start_offset,
            query,
            CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            window,
            cx,
        );
    }

    /// Record the word a menu is for and ask `provider` about the caret.
    fn request_completions(
        &mut self,
        provider: Rc<dyn CompletionProvider>,
        start_offset: usize,
        query: String,
        completion_context: CompletionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_offset = self.cursor();
        let revision = self.document_revision;
        let invoked = completion_context.trigger_kind == lsp_types::CompletionTriggerKind::INVOKED;
        self.extras.context_menu_task = Task::ready(Ok(()));
        self.extras.completion_pending = true;
        self.extras.context_menu_content.code_action.open = false;
        self.extras
            .context_menu_content
            .completion
            .trigger_start_offset = Some(start_offset);
        self.extras
            .context_menu_content
            .completion
            .query
            .clone_from(&query);

        let provider_responses =
            provider.completions(&self.text, new_offset, completion_context, window, cx);
        self.extras.context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let mut completions: Vec<CompletionItem> = vec![];
            if let Some(provider_responses) = provider_responses.await.ok() {
                match provider_responses {
                    CompletionResponse::Array(items) => completions.extend(items),
                    CompletionResponse::List(list) => completions.extend(list.items),
                }
            }

            editor
                .update_in(cx, |editor, window, cx| {
                    editor.extras.completion_pending = false;
                    if !editor.focus_handle.is_focused(window)
                        || editor.document_revision != revision
                        || editor.cursor() != new_offset
                        || editor.ime_marked_range.is_some()
                        || editor.disabled
                        || editor.readonly
                    {
                        return;
                    }

                    // ideae patch: the menu wins over a suggestion; with nothing to show, a typed word may get one.
                    if completions.is_empty() {
                        if !invoked && !editor.has_inline_completion() {
                            editor.schedule_inline_completion(window, cx);
                        }
                    } else {
                        editor.clear_inline_completion(cx);
                    }
                    editor.extras.context_menu_content.completion.open = !completions.is_empty();
                    editor.extras.context_menu_content.completion.items = completions;
                    editor.extras.context_menu_content.completion.bump();
                    cx.notify();
                })
                .ok();

            Ok(())
        });
    }

    pub(crate) fn hide_context_menu(&mut self, cx: &mut Context<Self>) {
        self.extras.context_menu_content.completion.open = false;
        self.extras.context_menu_content.code_action.open = false;
        self.extras.context_menu_task = Task::ready(Ok(()));
        self.extras.completion_pending = false;
        cx.notify();
    }

    /// ideae patch: Tab on an open completion menu takes its selected item.
    pub(crate) fn accept_completion_menu_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.extras.context_menu_content.completion.open {
            return false;
        }
        let enter = crate::input::Enter {
            secondary: false,
            shift: false,
        };
        self.handle_action_for_context_menu(Box::new(enter), window, cx)
    }

    pub(crate) fn is_context_menu_open(&self, _cx: &gpui::App) -> bool {
        self.extras.context_menu_content.completion.open
            || self.extras.context_menu_content.code_action.open
    }

    pub(crate) fn handle_action_for_context_menu(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let closes_overlay =
            crate::input::Enter::is_primary(&*action) || action.partial_eq(&crate::input::Escape);
        let kind = if self.extras.context_menu_content.completion.open {
            Some(super::InputOverlayKind::Completion)
        } else if self.extras.context_menu_content.code_action.open {
            Some(super::InputOverlayKind::CodeAction)
        } else {
            None
        };
        let Some((kind, handler)) = kind.zip(self.overlay_action_handler.clone()) else {
            return false;
        };
        let handled = handler(kind, action, window, cx);
        if handled && closes_overlay {
            self.hide_context_menu(cx);
        }
        handled
    }

    /// Schedule an inline completion request after debouncing.
    pub(crate) fn schedule_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clear any existing inline completion on text change
        self.clear_inline_completion(cx);

        let Some(provider) = self.extras.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let text = self.text.clone();
        // ideae patch: an answer about a text that changed since is dropped, not only one for another cursor.
        let revision = self.document_revision;
        let debounce = provider.inline_completion_debounce();
        let background_executor = cx.background_executor().clone();

        self.extras.inline_completion.task = cx.spawn_in(window, async move |editor, cx| {
            // Debounce: wait before fetching to avoid unnecessary requests while typing
            background_executor.timer(debounce).await;

            // Now fetch the inline completion after the debounce period
            let task = editor.update_in(cx, |editor, window, cx| {
                // Check if cursor has moved during debounce
                // ideae patch: and only for the editor typed in.
                if editor.cursor() != offset
                    || editor.document_revision != revision
                    || !editor.focus_handle.is_focused(window)
                {
                    return None;
                }

                // Don't fetch if completion menu is open
                // ideae patch: or is still being asked for.
                if editor.is_context_menu_open(cx) || editor.extras.completion_pending {
                    return None;
                }

                let trigger = InlineCompletionContext {
                    trigger_kind: InlineCompletionTriggerKind::Automatic,
                    selected_completion_info: None,
                };

                Some(provider.inline_completion(&text, offset, trigger, window, cx))
            })?;

            let Some(task) = task else {
                return Ok(InlineCompletionResponse::Array(vec![]));
            };

            let response = task.await?;

            editor.update_in(cx, |editor, _window, cx| {
                // Only apply if cursor still hasn't moved
                if editor.cursor() != offset || editor.document_revision != revision {
                    return;
                }

                // Don't show if completion menu opened while we were fetching
                if editor.is_context_menu_open(cx) || editor.extras.completion_pending {
                    return;
                }

                if let Some(item) = match response.clone() {
                    InlineCompletionResponse::Array(items) => items.into_iter().next(),
                    InlineCompletionResponse::List(comp_list) => comp_list.items.into_iter().next(),
                } {
                    editor.extras.inline_completion.item = Some(item);
                    cx.notify();
                }
            })?;

            Ok(response)
        });
    }

    /// Check if an inline completion suggestion is currently displayed.
    #[inline]
    pub(crate) fn has_inline_completion(&self) -> bool {
        self.extras.inline_completion.item.is_some()
    }

    /// Clear the inline completion suggestion.
    pub(crate) fn clear_inline_completion(&mut self, cx: &mut Context<Self>) {
        let cleared = self.extras.inline_completion.item.take().map(|item| ClearedInlineCompletion {
            item,
            cursor: self.cursor(),
            len: self.text.len(),
        });
        self.extras.inline_completion = InlineCompletion {
            cleared,
            ..InlineCompletion::default()
        };
        cx.notify();
    }

    /// ideae patch: an edit that typed the start of the suggestion just
    /// cleared, at its cursor, brings back the rest of it.
    fn follow_inline_completion(
        &mut self,
        range: &Range<usize>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(cleared) = self.extras.inline_completion.cleared.take() else {
            return false;
        };
        if !range.is_empty()
            || range.start != cleared.cursor
            || new_text.is_empty()
            || new_text.contains('\n')
            || !self.selections.is_single()
            || self.ime_marked_range.is_some()
            || self.cursor() != range.start + new_text.len()
            || self.text.len() != cleared.len + new_text.len()
        {
            return false;
        }
        let mut item = cleared.item;
        match item.insert_text.strip_prefix(new_text) {
            Some(rest) if !rest.is_empty() => item.insert_text = rest.to_string(),
            _ => return false,
        }
        if let Some(replaced) = item.range.as_mut() {
            if replaced.end.line == replaced.start.line {
                replaced.end.character += new_text.encode_utf16().count() as u32;
            }
            replaced.start = self.text.offset_to_position(self.cursor());
        }
        self.extras.inline_completion.item = Some(item);
        cx.notify();
        true
    }

    /// Accept the inline completion, inserting it at the cursor position.
    /// Returns true if a completion was accepted, false if there was none.
    pub(crate) fn accept_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion_item) = self.extras.inline_completion.item.take() else {
            return false;
        };

        let cursor = self.cursor();
        // ideae patch: a suggestion with a range replaces the text from the cursor to its end.
        let end = completion_item
            .range
            .map(|range| self.text.position_to_offset(&range.end).clamp(cursor, self.text.len()))
            .unwrap_or(cursor);
        let range_utf16 = self.range_to_utf16(&(cursor..end));
        let completion_text = completion_item.insert_text;
        self.replace_text_in_range_silent(Some(range_utf16), &completion_text, window, cx);
        // ideae patch: what comes after it is asked for at once.
        self.schedule_inline_completion(window, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, filter_text: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            filter_text: filter_text.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_uses_the_query_length_unless_filter_text_is_set() {
        assert_eq!(highlight_prefix("fl", &item("flag", None)), vec![0..2]);
        assert_eq!(highlight_prefix("fl", &item("flag", Some("f"))), vec![0..1]);
        assert_eq!(
            highlight_prefix("flagpole", &item("flag", None)),
            vec![0..4]
        );
        assert!(highlight_prefix("", &item("flag", None)).is_empty());
    }

    #[test]
    fn prefix_does_not_split_a_character() {
        assert_eq!(highlight_prefix("你", &item("你好", None)), vec![0..3]);
        assert!(highlight_prefix("a", &item("你", None)).is_empty());
    }

    #[test]
    fn contains_highlights_the_first_contiguous_match() {
        // "dialog" itself ends in "log", so that hit comes before the suffix.
        assert_eq!(
            highlight_contains("log", &item("dialog_log", None)),
            vec![3..6]
        );
        assert_eq!(
            highlight_contains("LOG", &item("dialog_log", None)),
            vec![3..6]
        );
        assert_eq!(
            highlight_contains("log", &item("catalog", None)),
            vec![4..7]
        );
        assert_eq!(
            highlight_contains("好世", &item("你好世界", None)),
            vec![3..9]
        );
        assert!(highlight_contains("log", &item("flag", None)).is_empty());
        assert!(highlight_contains("", &item("flag", None)).is_empty());
    }

    #[test]
    fn characters_highlight_gaps_and_merge_adjacent_hits() {
        assert_eq!(highlight_characters("fl", &item("flag", None)), vec![0..2]);
        assert_eq!(
            highlight_characters("fl", &item("file_loader", None)),
            vec![0..1, 2..3]
        );
        assert_eq!(
            highlight_characters("你世", &item("你好世界", None)),
            vec![0..3, 6..9]
        );
        assert!(highlight_characters("fx", &item("flag", None)).is_empty());
        assert!(highlight_characters("", &item("flag", None)).is_empty());
    }
}
