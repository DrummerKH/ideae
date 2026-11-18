use super::*;

use crate::acp::link::Link;
use crate::acp::thread::{ConfigOption, Entry, Op, Status, Thread};
use crate::acp::wire::{HostSpec, ToHost, ToWindow};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{ClipboardEntry, ImageFormat};

// ---------------------------------------------------------------- chat

/// An agent that speaks ACP, in its tab: the conversation its host keeps,
/// and a prompt.
pub struct ChatView {
    /// The daemon's name for its host: the task's, so a window that comes
    /// back attaches to it.
    pub(super) id: String,
    spec: HostSpec,
    link: Option<Link>,
    pub thread: Thread,
    list: ListState,
    input: Entity<gpui_kit::component::input::TextareaState>,
    pub(super) task: WeakEntity<TaskView>,
    /// Thoughts and tool calls opened to read.
    expanded: HashSet<usize>,
    /// What rendering needs of an entry, made when it changes, not each
    /// frame: its text to share, a tool call's diffs as lines.
    texts: HashMap<usize, SharedString>,
    diffs: HashMap<usize, Rc<Vec<DiffLines>>>,
    /// Files the agent reads or changes open beside the chat as it goes.
    follow: bool,
    followed: Option<(String, String)>,
    /// Pasted images for the next prompt: what they are, and their size.
    images: Vec<(crate::acp::wire::Image, usize)>,
    /// The current plan opened in full.
    plan_open: bool,
    /// Stopped while out of view: started again when shown.
    asleep: bool,
    /// The list's rows: an entry each, a long message in blocks, so only the
    /// blocks in view are laid out, and only the last one changes as it streams.
    rows: Vec<Row>,
    /// Diffs being made in the background, by entry: the one made last
    /// for an entry is the one kept.
    diff_jobs: HashMap<usize, u64>,
    diff_job: u64,
    /// Looking for words in the conversation (Ctrl+F).
    search: Option<Search>,
    /// Files about to be put back as at an entry's checkpoint: the entry,
    /// and what would change once the host has looked (None inside: gone).
    restoring: Option<(usize, Option<Option<Vec<(char, String)>>>)>,
    /// The search waiting for a pause in typing.
    finding: Task<()>,
    /// Its host is of another build (left running by the daemon through an
    /// update): what came since may not answer until it is restarted.
    stale_host: bool,
    _restore_wait: Task<()>,
    /// What it sent, for the tests.
    #[cfg(test)]
    sent: Rc<std::cell::RefCell<Vec<ToHost>>>,
    /// Each choice's values, shared with its menu rather than copied each frame.
    option_values: HashMap<String, Rc<Vec<crate::acp::thread::OptionValue>>>,
    _reading: Task<()>,
}

struct Search {
    input: Entity<InputState>,
    /// The entries that have the words, and the one shown.
    hits: Vec<usize>,
    at: usize,
}

/// A row of the chat's list: an entry, or `start..end` of an agent's
/// message, its text shared with the view that shows it.
#[derive(Clone)]
struct Row {
    entry: usize,
    start: usize,
    end: usize,
    text: Option<SharedString>,
}

/// Where a message is cut into blocks: after a blank line outside code, once
/// a block has some length. Each block is markdown of its own.
#[cfg(test)]
fn blocks(text: &str) -> Vec<(usize, usize)> {
    blocks_with_fences(text).into_iter().map(|b| (b.start, b.end)).collect()
}

/// A block of a message, and the fence it is inside of at its start (a
/// long piece of code is cut too, each part shown as code of its own).
#[derive(Debug, PartialEq)]
struct Block {
    start: usize,
    end: usize,
    /// The fence line the code it starts in opened with.
    opens: Option<String>,
    /// It ends inside that code.
    closes: bool,
}

fn blocks_with_fences(text: &str) -> Vec<Block> {
    const SOME: usize = 1200;
    /// Code is cut only past this: each part laid out again as it grows.
    const CODE_MOST: usize = 16 << 10;
    let (mut out, mut start, mut at) = (Vec::new(), 0usize, 0usize);
    let (mut fence, mut opens): (Option<String>, Option<String>) = (None, None);
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = match fence {
                Some(_) => None,
                None => Some(trimmed.trim_end().to_string()),
            };
        }
        at += line.len();
        let cut = match &fence {
            None => line.trim().is_empty() && at - start >= SOME,
            Some(_) => at - start >= CODE_MOST,
        };
        if cut && at < text.len() {
            out.push(Block { start, end: at, opens: opens.take(), closes: fence.is_some() });
            start = at;
            opens = fence.clone();
        }
    }
    out.push(Block { start, end: text.len(), opens, closes: false });
    out
}

/// The conversation's column at most this wide, and its text: lines a
/// wide window's width long, in a small font, read worse than a terminal.
const COLUMN: f32 = 860.;
const TEXT: f32 = 15.;
const TEXT_LINE: f32 = 23.;

/// A file's change as lines to show: what was there and what is now, the
/// lines both have left out.
struct DiffLines {
    name: SharedString,
    /// The file, as the agent named it, and its first changed line (from 1).
    path: String,
    first: u32,
    new_file: bool,
    /// '-', '+', ' ', or '⋯' where lines were left out.
    lines: Vec<(char, SharedString)>,
}

pub enum ChatEvent {
    /// Its status, session or usage changed: the task tells the workspace.
    Changed,
    /// You chose a model, a mode…: the agent's new sessions start with it.
    Chose { id: String, value: String },
    /// What it offers to choose from changed.
    Offered,
    /// A file it reads or changes, to show beside the chat.
    Follow { path: PathBuf, line: u32 },
}

impl EventEmitter<ChatEvent> for ChatView {}

impl ChatView {
    pub(super) fn new(id: String, spec: HostSpec, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::unlinked(id, spec, task, window, cx);
        this.connect(cx);
        this
    }

    /// The view without a host yet (a test's, which reads what it sends).
    fn unlinked(id: String, spec: HostSpec, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .submit_on_enter(true)
                .placeholder("Ask the agent; Shift+Enter for a new line")
        });
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                // An agent that ended takes nothing: the text stays, for after Restart.
                if !this.thread.status.live() {
                    return;
                }
                let text = input.read(cx).value().trim_end().to_string();
                if !text.trim().is_empty() || !this.images.is_empty() {
                    input.update(cx, |i, cx| i.set_value("", window, cx));
                    let files = this.mentioned(&text, cx);
                    let images = std::mem::take(&mut this.images).into_iter().map(|(i, _)| i).collect();
                    this.send(ToHost::Prompt { text, files, images });
                    cx.notify();
                }
            }
        })
        .detach();
        let list = ListState::new(0, ListAlignment::Top, px(600.));
        list.set_follow_mode(FollowMode::Tail);
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        let this = Self {
            id,
            spec,
            link: None,
            thread: Thread::default(),
            list,
            input,
            task,
            expanded: HashSet::new(),
            texts: HashMap::new(),
            diffs: HashMap::new(),
            follow: false,
            followed: None,
            images: Vec::new(),
            plan_open: false,
            asleep: false,
            rows: Vec::new(),
            diff_jobs: HashMap::new(),
            diff_job: 0,
            search: None,
            restoring: None,
            finding: Task::ready(()),
            stale_host: false,
            _restore_wait: Task::ready(()),
            option_values: HashMap::new(),
            _reading: Task::ready(()),
            #[cfg(test)]
            sent: Rc::default(),
        };
        this
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let link = Link::open(self.id.clone(), self.spec.clone());
        let events = link.events.clone();
        self.link = Some(link);
        self._reading = cx.spawn(async move |this, cx| {
            while let Ok(first) = events.recv().await {
                // All that came meanwhile, then one frame.
                let mut batch = vec![first];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                if this.update(cx, |this, cx| this.hear(batch, cx)).is_err() {
                    break;
                }
            }
        });
    }

    fn hear(&mut self, batch: Vec<ToWindow>, cx: &mut Context<Self>) {
        let mut changed = false;
        for msg in batch {
            match msg {
                ToWindow::Hello { thread, version } => {
                    self.stale_host = version != crate::acp::wire::VERSION;
                    self.forget(None);
                    self.option_values.clear();
                    self.thread = *thread;
                    self.rebuild_rows();
                    changed = true;
                }
                ToWindow::Op { op } => {
                    match &op {
                        Op::Set { ix, .. } | Op::Append { ix, .. } => self.forget(Some(*ix)),
                        Op::Reset { .. } => self.forget(None),
                        Op::Status { .. } | Op::Session { .. } | Op::Usage { .. } => changed = true,
                        _ => {}
                    }
                    let offered = matches!(op, Op::Options { .. });
                    if offered || matches!(op, Op::Reset { .. }) {
                        self.option_values.clear();
                    }
                    let touched = match &op {
                        Op::Push { .. } => Some(self.thread.entries.len()),
                        Op::Set { ix, .. } | Op::Append { ix, .. } => Some(*ix),
                        _ => None,
                    };
                    let reset = matches!(op, Op::Reset { .. });
                    self.thread.apply(op);
                    if reset {
                        self.rebuild_rows();
                    } else if let Some(ix) = touched.filter(|&ix| ix < self.thread.entries.len()) {
                        self.sync_rows(ix);
                    }
                    if offered {
                        cx.emit(ChatEvent::Offered);
                    }
                    if let Some(Entry::Tool(tool)) = touched.and_then(|ix| self.thread.entries.get(ix)) {
                        let place = tool.locations.first().cloned();
                        // Once it is done: a file read or changed, not one it only meant to.
                        if let (true, Some(place)) = (self.follow && tool.status == "completed", place) {
                            let key = (tool.id.clone(), place.path.clone());
                            if self.followed.as_ref() != Some(&key) {
                                self.followed = Some(key);
                                cx.emit(ChatEvent::Follow { path: PathBuf::from(&place.path), line: place.line.unwrap_or(0) });
                            }
                        }
                    }
                }
                ToWindow::Read { request, path } => {
                    let text = self.task.upgrade().and_then(|task| task.read(cx).unsaved_text(&path, cx));
                    self.send(ToHost::Buffer { request, text });
                }
                ToWindow::Restorable { ix, files } => {
                    if let Some((at, shown)) = &mut self.restoring {
                        if *at == ix {
                            *shown = Some(files);
                        }
                    }
                }
                ToWindow::Wrote { .. } => {
                    if let Some(task) = self.task.upgrade() {
                        task.read(cx).wake_refresh();
                    }
                }
            }
        }
        if self.option_values.is_empty() {
            self.option_values = self.thread.options.iter().map(|o| (o.id.clone(), Rc::new(o.values.clone()))).collect();
        }
        if changed {
            cx.emit(ChatEvent::Changed);
        }
        cx.notify();
    }

    pub(super) fn send(&self, msg: ToHost) {
        #[cfg(test)]
        self.sent.borrow_mut().push(msg.clone());
        if let Some(link) = &self.link {
            link.send(msg);
        }
    }

    /// Ends the agent and its host.
    pub fn end(&mut self) {
        if let Some(link) = self.link.take() {
            link.stop();
        }
    }

    /// Stops the agent when it has nothing to do: not working, not asking,
    /// nothing queued. What it said stays on screen.
    pub fn sleep(&mut self, cx: &mut Context<Self>) {
        let idle = matches!(self.thread.status, Status::Idle | Status::Done) && self.thread.queued.is_empty();
        if self.asleep || !idle || self.link.is_none() {
            return;
        }
        self.asleep = true;
        self.end();
        // What was made for showing it is made again when it wakes.
        self.forget(None);
        cx.notify();
    }

    pub fn wake(&mut self, cx: &mut Context<Self>) {
        if self.asleep {
            self.asleep = false;
            self.restart(cx);
        }
    }

    /// A new host and agent, going on with the session.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.end();
        // A session never prompted is kept by no agent: nothing to go on with.
        if let Some(session) = self.thread.session.clone().filter(|_| self.thread.entries.iter().any(|e| matches!(e, Entry::User { .. }))) {
            self.spec.resume = Some(session);
        }
        // What it said stays in view until the host tells the thread again.
        self.connect(cx);
        cx.notify();
    }

    pub fn session(&self) -> Option<&str> {
        self.link.as_ref().and_then(|l| l.session())
    }

    /// Text into the prompt, after what is there; `send` sends it all.
    pub fn put(&mut self, text: String, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        if send {
            return self.send(ToHost::prompt(text));
        }
        let before = self.input.read(cx).value().to_string();
        let joined = if before.trim().is_empty() { text } else { format!("{}\n{text}", before.trim_end()) };
        self.input.update(cx, |i, cx| i.set_value(joined, window, cx));
        let handle = self.input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    // ------------------------------------------------------------- the keys

    /// A question answered by its letter while the prompt is empty: Y, A
    /// (always), N. Letters bound to nothing come as keys, before the prompt
    /// takes them as text.
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() || self.thread.status != Status::Asking || !self.focus_handle(cx).is_focused(window) || self.input.read(cx).text().len() > 0 {
            return;
        }
        let Some(permission) = self.thread.asking().and_then(|t| t.permission.as_ref()) else { return };
        let key = keystroke.key.as_str();
        if let Some(option) = permission.options.iter().find(|o| answer_key(&o.kind) == Some(key)) {
            let (request, option) = (permission.request, option.id.clone());
            self.send(ToHost::Answer { request, option: Some(option) });
            cx.stop_propagation();
        }
    }

    /// Esc closes the search, else stops the agent's turn.
    fn escape(&mut self, _: &gpui_kit::component::input::Escape, window: &mut Window, cx: &mut Context<Self>) {
        // Captured: the prompt's own handling is passed over only when done here.
        if self.search.take().is_some() {
            self.focus_handle(cx).focus(window, cx);
            cx.notify();
            cx.stop_propagation();
        } else if matches!(self.thread.status, Status::Working | Status::Asking) {
            self.send(ToHost::Cancel);
            cx.stop_propagation();
        }
    }

    /// ↑ in an empty prompt brings back the last one.
    fn up(&mut self, _: &gpui_kit::component::input::MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        let empty = self.focus_handle(cx).is_focused(window) && self.input.read(cx).text().len() == 0;
        let last = self.thread.entries.iter().rev().find_map(|e| match e {
            Entry::User { text } => Some(text.clone()),
            _ => None,
        });
        let (true, Some(last)) = (empty, last) else { return };
        // Without the note of the images it was sent with.
        let text = match last.rfind("\n\n[") {
            Some(at) if last.ends_with(']') && last[at..].contains(" image") => last[..at].to_string(),
            _ => last,
        };
        self.input.update(cx, |i, cx| i.set_value(text, window, cx));
        cx.stop_propagation();
    }

    /// Tab completes the agent's command being typed.
    fn tab(&mut self, _: &gpui_kit::component::input::IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.typed_command(cx).unwrap_or_default();
        let name = self.commands_for(&value).first().map(|c| c.name.clone());
        let (true, Some(name)) = (self.focus_handle(cx).is_focused(window), name) else { return };
        self.input.update(cx, |i, cx| i.set_value(format!("/{name} "), window, cx));
        cx.stop_propagation();
    }

    fn search_action(&mut self, _: &gpui_kit::component::input::Search, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search(window, cx);
        cx.stop_propagation();
    }

    /// What is typed, while it may be a command: short, after a `/`. Read
    /// at every key, so a long prompt is not copied for it.
    fn typed_command(&self, cx: &App) -> Option<String> {
        let text = self.input.read(cx).text();
        (text.len() > 0 && text.len() <= 64).then(|| text.to_string()).filter(|t| t.starts_with('/'))
    }

    /// The agent's slash commands that what is typed starts, while it is
    /// one word after a `/`.
    fn commands_for(&self, typed: &str) -> Vec<&crate::acp::thread::SlashCommand> {
        let Some(word) = typed.strip_prefix('/').filter(|w| !w.contains(char::is_whitespace)) else { return Vec::new() };
        let word = word.to_lowercase();
        self.thread.commands.iter().filter(|c| c.name.to_lowercase().starts_with(&word)).take(8).collect()
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            let input = search.input.clone();
            input.update(cx, |i, cx| i.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in the conversation"));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| match event {
            // Looked for once typing pauses: a long conversation is read once.
            InputEvent::Change => {
                this.finding = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                    let _ = this.update(cx, |this, cx| this.find(cx));
                });
            }
            InputEvent::PressEnter { shift, .. } => this.next_hit(if *shift { -1 } else { 1 }, cx),
            _ => {}
        })
        .detach();
        input.update(cx, |i, cx| i.focus(window, cx));
        self.search = Some(Search { input, hits: Vec::new(), at: 0 });
        cx.notify();
    }

    /// The entries with the words looked for; the first from the end shown.
    fn find(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.search else { return };
        let query = search.input.read(cx).value().trim().to_lowercase();
        let hits: Vec<usize> = if query.is_empty() {
            Vec::new()
        } else {
            self.thread
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| match e {
                    Entry::User { text } | Entry::Agent { text } | Entry::Thought { text } | Entry::Notice { text } => contains_folded(text, &query),
                    Entry::Tool(tool) => contains_folded(&tool.title, &query) || contains_folded(&tool.output, &query),
                    Entry::Plan { items } => items.iter().any(|i| contains_folded(&i.text, &query)),
                })
                .map(|(ix, _)| ix)
                .collect()
        };
        let at = hits.len().saturating_sub(1);
        if let Some(search) = &mut self.search {
            search.hits = hits;
            search.at = at;
        }
        self.show_hit();
        cx.notify();
    }

    fn next_hit(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        if search.hits.is_empty() {
            return;
        }
        let n = search.hits.len() as isize;
        search.at = ((search.at as isize + by).rem_euclid(n)) as usize;
        self.show_hit();
        cx.notify();
    }

    fn show_hit(&mut self) {
        let Some(&ix) = self.search.as_ref().and_then(|s| s.hits.get(s.at)) else { return };
        let row = self.rows_of(ix).start;
        self.list.scroll_to(gpui_kit::ListOffset { item_ix: row, offset_in_item: px(0.) });
    }

    /// The entry the search shows now.
    fn found(&self, ix: usize) -> bool {
        self.search.as_ref().is_some_and(|s| s.hits.get(s.at) == Some(&ix))
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let search = self.search.as_ref()?;
        let theme = cx.theme();
        let count = match (search.hits.len(), search.input.read(cx).value().trim().is_empty()) {
            (_, true) => String::new(),
            (0, false) => "None".to_string(),
            (n, false) => format!("{}/{n}", search.at + 1),
        };
        Some(
            h_flex()
                .flex_none()
                .gap_1()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(div().flex_1().child(Input::new(&search.input).xsmall()))
                .child(div().text_xs().text_color(theme.muted_foreground).min_w(px(40.)).child(count))
                .child(Button::new("chat-find-up").xsmall().ghost().label("↑").tooltip("Earlier (Shift+Enter)").on_click(cx.listener(|this, _, _, cx| this.next_hit(-1, cx))))
                .child(Button::new("chat-find-down").xsmall().ghost().label("↓").tooltip("Later (Enter)").on_click(cx.listener(|this, _, _, cx| this.next_hit(1, cx))))
                .child(
                    Button::new("chat-find-close").xsmall().ghost().label("✕").tooltip("Close (Esc)").on_click(cx.listener(|this, _, window, cx| {
                        this.search = None;
                        this.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    })),
                )
                .into_any_element(),
        )
    }

    /// The agent's commands that match what is typed, above the prompt.
    fn render_commands(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let value = self.typed_command(cx).unwrap_or_default();
        let commands = self.commands_for(&value);
        if commands.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            v_flex()
                .flex_none()
                .py_1()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .children(commands.into_iter().enumerate().map(|(n, command)| {
                    let name = command.name.clone();
                    h_flex()
                        .id(("chat-command", n))
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.muted))
                        .when(n == 0, |d| d.bg(theme.muted))
                        .child(div().flex_none().child(format!("/{}", command.name)))
                        .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(command.description.clone()))
                        .when(n == 0, |d| d.child(div().text_color(theme.muted_foreground).child("Tab")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.input.update(cx, |i, cx| i.set_value(format!("/{name} "), window, cx));
                            this.focus_handle(cx).focus(window, cx);
                        }))
                }))
                .into_any_element(),
        )
    }

    /// The prompts waiting their turn: one taken back to change, or all dropped.
    fn render_queued(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let queued = self.thread.queued.clone();
        if queued.is_empty() {
            return None;
        }
        let view = cx.weak_entity();
        Some(
            Button::new("chat-queued")
                .xsmall()
                .ghost()
                .label(format!("{} queued ▾", queued.len()))
                .tooltip("Sent one by one when the agent is done")
                .dropdown_menu(move |mut menu, _, _| {
                    for (at, prompt) in queued.iter().enumerate() {
                        let (view, prompt) = (view.clone(), prompt.clone());
                        let short: String = prompt.lines().next().unwrap_or_default().chars().take(48).collect();
                        menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(format!("Change: {short}")).on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.send(ToHost::Take { at });
                                this.put(prompt.clone(), false, window, cx);
                            });
                        }));
                    }
                    let view = view.clone();
                    menu.separator().item(gpui_kit::component::menu::PopupMenuItem::new("Drop all").on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, _| this.send(ToHost::Unqueue));
                    }))
                })
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------ rendering

    /// A row of the conversation, in a column of a width that reads well
    /// (lines as long as a wide window are hard to follow), centered.
    fn render_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let inner = self.render_row_inner(row, window, cx);
        div().w_full().flex().justify_center().child(div().w_full().max_w(px(COLUMN)).child(inner)).into_any_element()
    }

    fn render_row_inner(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(row).cloned() else { return div().into_any_element() };
        match row.text {
            // A block of a message: its own markdown, cached while it does not change.
            Some(text) => {
                let theme = cx.theme();
                let last = matches!(self.thread.entries.get(row.entry), Some(Entry::Agent { text }) if row.end == text.len());
                let group = SharedString::from(format!("chat-block-{}-{}", row.entry, row.start));
                let entry = row.entry;
                div()
                    .group(group.clone())
                    .relative()
                    .px_4()
                    .when(row.start == 0, |d| d.pt_1p5())
                    .w_full()
                    .text_size(px(TEXT))
                    .line_height(px(TEXT_LINE))
                    .text_color(theme.foreground)
                    .when(self.found(entry), |d| d.border_l_2().border_color(theme.primary))
                    .child(gpui_kit::component::text::TextView::markdown(SharedString::from(format!("chat-agent-{}-{}", row.entry, row.start)), text).selectable(true))
                    .when(last, |d| {
                        d.child(
                            div().absolute().right_2().bottom_0().opacity(0.).group_hover(group, |s| s.opacity(1.)).child(
                                Button::new(("chat-copy", entry))
                                    .xsmall()
                                    .ghost()
                                    .label("Copy")
                                    .tooltip("Copy the whole message, as markdown")
                                    .on_click(cx.listener(move |this, _, _, cx| this.copy(entry, cx))),
                            ),
                        )
                    })
                    .into_any_element()
            }
            None => {
                let found = self.found(row.entry);
                let primary = cx.theme().primary;
                div().w_full().when(found, |d| d.border_l_2().border_color(primary)).child(self.render_entry(row.entry, window, cx)).into_any_element()
            }
        }
    }

    /// Whether entry `ix` has files to go back to, and now is a time for it.
    fn can_restore(&self, ix: usize) -> bool {
        self.thread.checkpoints.contains_key(&ix) && matches!(self.thread.status, Status::Idle | Status::Done | Status::Exited(_)) && self.link.is_some()
    }

    /// What going back would change, asked of the host; shown above the
    /// prompt to confirm.
    fn ask_restore(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.restoring = Some((ix, None));
        self.send(ToHost::Preview { ix });
        // A host that never answers is one from before this: said so, not
        // "looking" for ever.
        self._restore_wait = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(15)).await;
            let _ = this.update(cx, |this, cx| {
                if matches!(this.restoring, Some((at, None)) if at == ix) {
                    this.restoring = None;
                    this.stale_host = true;
                    cx.notify();
                }
            });
        });
        cx.notify();
    }

    fn render_restore(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (ix, files) = self.restoring.clone()?;
        let theme = cx.theme();
        let what = match self.thread.entries.get(ix) {
            Some(Entry::User { text }) => format!("Put the files back as they were before “{}”?", text.lines().next().unwrap_or_default().chars().take(60).collect::<String>()),
            _ => "Undo: put the files back as they were before that?".to_string(),
        };
        let cancel = Button::new("chat-restore-cancel").xsmall().ghost().label("Cancel").on_click(cx.listener(|this, _, _, cx| {
            this.restoring = None;
            cx.notify();
        }));
        let mut panel = v_flex().flex_none().gap_1().px_3().py_2().text_xs().border_t_1().border_color(theme.border).bg(theme.muted).child(div().text_sm().child(what));
        let (body, ready) = match &files {
            None => (vec![div().text_color(theme.muted_foreground).child("Looking at what changed since…").into_any_element()], false),
            Some(None) => (vec![div().text_color(theme.muted_foreground).child("That checkpoint is gone (its worktree was archived or cleaned up).").into_any_element()], false),
            Some(Some(list)) if list.is_empty() => (vec![div().text_color(theme.muted_foreground).child("Nothing changed since.").into_any_element()], false),
            Some(Some(list)) => {
                let mut rows: Vec<AnyElement> = list
                    .iter()
                    .take(12)
                    .map(|(status, path)| {
                        let (mark, say, color) = match status {
                            'A' => ("−", "made since: removed", theme.danger),
                            'D' => ("+", "deleted since: back", theme.success),
                            _ => ("~", "changed since: as it was", theme.foreground),
                        };
                        h_flex().gap_2().child(div().w(px(10.)).text_color(color).child(mark)).child(div().flex_1().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(path.clone())).child(div().text_color(theme.muted_foreground).child(say)).into_any_element()
                    })
                    .collect();
                if list.len() > 12 {
                    rows.push(div().text_color(theme.muted_foreground).child(format!("and {} more", list.len() - 12)).into_any_element());
                }
                rows.push(div().text_color(theme.muted_foreground).child("Only the files change: the conversation and what is staged stay, and the agent is not told. It can be undone.").into_any_element());
                (rows, true)
            }
        };
        panel = panel.children(body);
        let mut buttons = h_flex().gap_2().pt_1().justify_end().child(cancel);
        if ready {
            buttons = buttons.child(Button::new("chat-restore-go").xsmall().primary().label(format!("Put back {} file{}", files.as_ref().and_then(|f| f.as_ref()).map_or(0, Vec::len), if files.as_ref().and_then(|f| f.as_ref()).map_or(0, Vec::len) == 1 { "" } else { "s" })).on_click(cx.listener(move |this, _, _, cx| {
                this.send(ToHost::Restore { ix });
                this.restoring = None;
                cx.notify();
            })));
        }
        Some(panel.child(buttons).into_any_element())
    }

    /// The agent's log, in the editor beside the chat.
    fn open_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.spec.journal.as_deref().map(crate::acp::host::log_path) else { return };
        let task = self.task.clone();
        cx.spawn_in(window, async move |_, cx| {
            let there = cx.background_spawn({
                let path = path.clone();
                async move { path.is_file() }
            })
            .await;
            let _ = cx.update(|window, cx| {
                if !there {
                    window.push_notification(Notification::info("The agent has written nothing to its log yet."), cx);
                    return;
                }
                let _ = task.update(cx, |t, cx| t.open_file_as(path, 0, 0, OpenAs::Preview, window, cx));
            });
        })
        .detach();
    }

    /// A message to the clipboard, whole.
    fn copy(&self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Agent { text } | Entry::User { text }) = self.thread.entries.get(ix) {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.clone()));
        }
    }

    fn render_entry(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let text = self.text_of(ix);
        let Some(entry) = self.thread.entries.get(ix) else { return div().into_any_element() };
        let theme = cx.theme();
        let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
        let row = div().px_4().py_1p5().w_full();
        match entry {
            Entry::User { .. } => {
                let group = SharedString::from(format!("chat-user-row-{ix}"));
                // Beside the prompt, in the room its bubble leaves, on hover.
                let back = self.can_restore(ix).then(|| {
                    div().flex_none().opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)).child(
                        Button::new(("chat-restore", ix))
                            .xsmall()
                            .ghost()
                            .label("↺ Files before this")
                            .tooltip("Put the task's files back as they were before this prompt: shows what would change first")
                            .on_click(cx.listener(move |this, _, _, cx| this.ask_restore(ix, cx))),
                    )
                });
                // Yours on the right, as wide as it needs up to most of the
                // column; the agent's on the left, the whole width.
                row.group(group)
                    .child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .items_start()
                            .gap_2()
                            .children(back)
                            .child(
                                div()
                                    .max_w(relative(0.8))
                                    .px_3()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(theme.muted)
                                    .text_size(px(TEXT))
                                    .line_height(px(TEXT_LINE))
                                    .child(gpui_kit::component::text::TextView::markdown(SharedString::from(format!("chat-user-{ix}")), text.clone()).selectable(true)),
                            ),
                    )
                    .into_any_element()
            }
            Entry::Agent { .. } => row
                .text_size(px(TEXT))
                .line_height(px(TEXT_LINE))
                .child(gpui_kit::component::text::TextView::markdown(SharedString::from(format!("chat-agent-{ix}")), text.clone()).selectable(true))
                .into_any_element(),
            Entry::Thought { .. } => {
                let open = self.expanded.contains(&ix);
                row.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .id(("chat-thought", ix))
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .child(if open { "▾ Thinking" } else { "▸ Thinking" })
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx))),
                        )
                        .when(open, |this| this.child(div().pl_3().text_xs().text_color(muted).child(text.clone()))),
                )
                .into_any_element()
            }
            Entry::Tool(_) => self.render_tool(ix, cx),
            Entry::Plan { items } => row
                .child(
                    v_flex()
                        .gap_0p5()
                        .px_3()
                        .py_2()
                        .border_1()
                        .border_color(border)
                        .rounded_md()
                        .text_xs()
                        .child(div().text_color(muted).child("Plan"))
                        .children(items.iter().map(|item| {
                            let mark = match item.status.as_str() {
                                "completed" => "✓",
                                "in_progress" => "▸",
                                _ => "○",
                            };
                            h_flex().gap_2().child(div().w(px(12.)).child(mark)).child(div().text_color(if item.status == "completed" { muted } else { fg }).child(item.text.clone()))
                        })),
                )
                .into_any_element(),
            Entry::Notice { .. } => {
                let undo = self.can_restore(ix).then(|| {
                    Button::new(("chat-undo-restore", ix)).xsmall().ghost().label("Undo").tooltip("Put the files back as they were before this").on_click(cx.listener(move |this, _, _, cx| this.ask_restore(ix, cx)))
                });
                row.child(h_flex().gap_2().items_start().child(div().flex_1().min_w_0().text_xs().text_color(theme.warning).child(text)).children(undo)).into_any_element()
            }
        }
    }

    /// A tool call's card. Its diffs and output are not copied for it: they
    /// are shown from what was made of them once.
    fn render_tool(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Entry::Tool(tool)) = self.thread.entries.get(ix) else { return div().into_any_element() };
        let status = tool.status.clone();
        let whole: SharedString = if tool.title.is_empty() { tool.kind.clone().into() } else { tool.title.clone().into() };
        // A command of many lines (a script in a heredoc) is its first line
        // here, and all of it when the card is opened.
        let many = whole.trim_end().contains('\n');
        let title: SharedString = if many { format!("{} …", whole.lines().next().unwrap_or_default()).into() } else { whole.clone() };
        let decided = tool.decided.clone();
        let permission = tool.permission.clone();
        let has_output = !tool.output.is_empty();
        let has_more = !tool.diffs.is_empty() || has_output || tool.terminal.is_some() || many;
        let running = tool.terminal.is_some() && !matches!(status.as_str(), "completed" | "failed");
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        // A command running through the app shows what it prints as it does.
        let open = self.expanded.contains(&ix) || permission.is_some() || running;
        let mark = match status.as_str() {
            "completed" => "✓",
            "failed" => "✗",
            "in_progress" => "…",
            _ => "○",
        };
        let color = match status.as_str() {
            "failed" => theme.danger,
            "completed" => muted,
            _ => theme.foreground,
        };
        let mut card = v_flex()
            .gap_1()
            .px_3()
            .py_1p5()
            .border_1()
            .border_color(if permission.is_some() { theme.warning } else { border })
            .rounded_md()
            .text_xs()
            .child(
                h_flex()
                    .id(("chat-tool", ix))
                    .gap_2()
                    .when(has_more, |this| this.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx))))
                    .child(div().text_color(color).child(mark))
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .when_some(decided, |this, decided| this.child(div().text_color(muted).child(decided)))
                    .when(has_more && permission.is_none(), |this| this.child(div().text_color(muted).child(if open { "▾" } else { "▸" }))),
            );
        if open && many {
            let theme = cx.theme();
            card = card.child(div().px_2().py_1().rounded_sm().bg(theme.muted).font_family(theme.mono_font_family.clone()).child(whole.clone()));
        }
        if open {
            match self.diffs_of(ix, cx) {
                Some(diffs) => {
                    for (n, diff) in diffs.iter().enumerate() {
                        let (path, line) = (diff.path.clone(), diff.first);
                        let review = Button::new(("chat-review", ix * 16 + n))
                            .xsmall()
                            .ghost()
                            .label("Review in Changes")
                            .tooltip("Open this change in the Changes tab: stage or revert it, or a part of it")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // After this chat's update: the task gives it the keyboard back.
                                let (task, path) = (this.task.clone(), path.clone());
                                window.defer(cx, move |window, cx| {
                                    let _ = task.update(cx, |t, cx| t.review_change(&path, line, window, cx));
                                });
                            }));
                        card = card.child(render_diff(diff, review.into_any_element(), cx));
                    }
                }
                None => card = card.child(div().text_color(muted).child("Comparing…")),
            }
            if has_output && permission.is_none() {
                let shown = self.text_of(ix);
                let theme = cx.theme();
                card = card.child(div().px_2().py_1().rounded_sm().bg(theme.muted).font_family(theme.mono_font_family.clone()).child(shown));
            }
        }
        if let Some(permission) = &permission {
            let request = permission.request;
            card = card.child(h_flex().gap_2().pt_1().flex_wrap().children(permission.options.iter().enumerate().map(|(n, option)| {
                let id = option.id.clone();
                let key = answer_key(&option.kind).map(|k| format!(" ({})", k.to_uppercase())).unwrap_or_default();
                let button = Button::new(("chat-answer", ix * 16 + n)).xsmall().label(format!("{}{key}", option.name));
                let button = if option.allows() { button.primary() } else { button.outline() };
                button.on_click(cx.listener(move |this, _, _, _| this.send(ToHost::Answer { request, option: Some(id.clone()) })))
            })));
        }
        div().px_4().py_1().w_full().child(card).into_any_element()
    }

    /// What was made of an entry (None: of all) is made again when shown.
    fn forget(&mut self, ix: Option<usize>) {
        match ix {
            Some(ix) => {
                self.texts.remove(&ix);
                self.diffs.remove(&ix);
                self.diff_jobs.remove(&ix);
            }
            None => {
                self.texts.clear();
                self.diffs.clear();
                self.diff_jobs.clear();
            }
        }
    }

    fn text_of(&mut self, ix: usize) -> SharedString {
        let entries = &self.thread.entries;
        self.texts
            .entry(ix)
            .or_insert_with(|| match entries.get(ix) {
                Some(Entry::User { text } | Entry::Agent { text } | Entry::Thought { text } | Entry::Notice { text }) => text.clone().into(),
                // A tool call's output: its last lines.
                Some(Entry::Tool(tool)) => {
                    let lines: Vec<&str> = tool.output.lines().collect();
                    lines[lines.len().saturating_sub(30)..].join("\n").into()
                }
                _ => SharedString::default(),
            })
            .clone()
    }

    /// A tool call's diffs as lines: made in the background (a file can be
    /// long), None until they are.
    fn diffs_of(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<Rc<Vec<DiffLines>>> {
        if let Some(diffs) = self.diffs.get(&ix) {
            return Some(diffs.clone());
        }
        // Kept for the cards in view lately, not for every one ever opened.
        if self.diffs.len() >= 64 {
            self.diffs.clear();
        }
        if self.diff_jobs.contains_key(&ix) {
            return None;
        }
        let Some(Entry::Tool(tool)) = self.thread.entries.get(ix) else { return Some(Rc::default()) };
        let diffs = tool.diffs.clone();
        self.diff_job += 1;
        let job = self.diff_job;
        self.diff_jobs.insert(ix, job);
        cx.spawn(async move |this, cx| {
            let lines = cx.background_spawn(async move { diffs.iter().map(diff_lines).collect::<Vec<_>>() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.diff_jobs.get(&ix) == Some(&job) {
                    this.diff_jobs.remove(&ix);
                    this.diffs.insert(ix, Rc::new(lines));
                    let rows = this.rows_of(ix);
                    this.list.splice(rows.clone(), rows.len());
                    cx.notify();
                }
            });
        })
        .detach();
        None
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&ix) {
            self.expanded.insert(ix);
        }
        let rows = self.rows_of(ix);
        self.list.splice(rows.clone(), rows.len());
        cx.notify();
    }

    /// Where the rows of entry `ix` are in the list.
    fn rows_of(&self, ix: usize) -> std::ops::Range<usize> {
        let start = self.rows.partition_point(|r| r.entry < ix);
        let end = self.rows.partition_point(|r| r.entry <= ix);
        start..end
    }

    /// The rows entry `ix` is shown in: its blocks when it is a message,
    /// those that did not change kept with their text.
    fn make_rows(&self, ix: usize, old: &[Row]) -> Vec<Row> {
        match self.thread.entries.get(ix) {
            Some(Entry::Agent { text }) => blocks_with_fences(text)
                .into_iter()
                .map(|b| {
                    let kept = old.binary_search_by_key(&b.start, |r| r.start).ok().map(|i| &old[i]).filter(|r| r.end == b.end).and_then(|r| r.text.clone());
                    let text = kept.unwrap_or_else(|| {
                        let body = &text[b.start..b.end];
                        match (&b.opens, b.closes) {
                            (None, false) => SharedString::from(body.to_string()),
                            (opens, closes) => SharedString::from(format!("{}{body}{}", opens.as_deref().map(|f| format!("{f}\n")).unwrap_or_default(), if closes { "```\n" } else { "" })),
                        }
                    });
                    Row { entry: ix, start: b.start, end: b.end, text: Some(text) }
                })
                .collect(),
            _ => vec![Row { entry: ix, start: 0, end: 0, text: None }],
        }
    }

    /// Entry `ix` changed or came: its rows again, the list told only of
    /// those that differ (a stream changes its last block alone).
    fn sync_rows(&mut self, ix: usize) {
        let range = self.rows_of(ix);
        let new = self.make_rows(ix, &self.rows[range.clone()]);
        let same = self.rows[range.clone()].iter().zip(&new).take_while(|(a, b)| a.start == b.start && a.end == b.end && a.text.is_some() == b.text.is_some()).count();
        // A block that only grew is shown anew: its text is another string.
        let same = if same == new.len() && same == range.len() { same.saturating_sub(1) } else { same };
        self.list.splice(range.start + same..range.end, new.len() - same);
        self.rows.splice(range, new);
    }

    fn rebuild_rows(&mut self) {
        self.rows = (0..self.thread.entries.len()).flat_map(|ix| self.make_rows(ix, &[])).collect();
        self.list.reset(self.rows.len());
    }

    /// The files of the task a prompt mentions as `@path`: sent along as
    /// links, for agents that read them as such.
    fn mentioned(&self, text: &str, cx: &App) -> Vec<PathBuf> {
        let Some(task) = self.task.upgrade() else { return Vec::new() };
        let task = task.read(cx);
        text.split_whitespace()
            .filter_map(|word| word.strip_prefix('@'))
            .map(|path| path.trim_end_matches([',', '.', ';', ':', ')']))
            .filter(|path| task.tree_files.iter().any(|f| f == path))
            .map(|path| task.root.join(path))
            .collect()
    }

    /// A file of the task, looked for by name, into the prompt as `@path`.
    fn mention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task.upgrade() else { return };
        let (root, files) = {
            let t = task.read(cx);
            (t.root.clone(), t.tree_files.clone())
        };
        let items: Vec<crate::finder::Hit> = files
            .iter()
            .map(|rel| {
                let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
                crate::finder::Hit { path: rel.clone(), line: 0, col: 0, text: format!("{name}{}{dir}", crate::finder::DETAIL), ranges: Vec::new() }
            })
            .collect();
        let view = cx.weak_entity();
        let picked: crate::finder::Picked = Rc::new(move |path, window, cx| {
            let _ = view.update(cx, |this, cx| this.put(format!("@{path} "), false, window, cx));
        });
        crate::finder::Finder::open_pick("Mention a file".into(), root, items, picked, self.task.clone(), window, cx);
    }

    /// An image in the clipboard goes with the next prompt; text pastes as text.
    fn paste(&mut self, _: &gpui_kit::component::input::Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return cx.propagate() };
        let image = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        let Some(image) = image.filter(|_| self.thread.images) else { return cx.propagate() };
        use base64::Engine as _;
        let mime = match image.format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Gif => "image/gif",
            _ => return cx.propagate(),
        };
        let size = image.bytes.len();
        self.images.push((crate::acp::wire::Image { mime: mime.into(), data: base64::engine::general_purpose::STANDARD.encode(&image.bytes) }, size));
        cx.notify();
    }

    /// A choice made here: sent, and kept for the agent's new sessions.
    fn choose(&mut self, id: String, value: String, cx: &mut Context<Self>) {
        self.send(ToHost::Choose { id: id.clone(), value: value.clone() });
        cx.emit(ChatEvent::Chose { id, value });
    }

    /// A long list (models) in a searchable picker, by provider when the
    /// names tell one ("Ollama (local)/qwen3-coder").
    fn pick(&mut self, option: ConfigOption, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
        let mut groups: Vec<(String, Vec<crate::acp::thread::OptionValue>)> = Vec::new();
        for value in option.values.iter().cloned() {
            let head = value.name.split_once('/').map(|(h, _)| h.to_string()).unwrap_or_default();
            match groups.iter_mut().find(|(h, _)| *h == head) {
                Some((_, values)) => values.push(value),
                None => groups.push((head, vec![value])),
            }
        }
        let groups = Rc::new(groups);
        let command = cx.new(|cx| CommandState::new(window, cx));
        let view = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        let (id, current, title) = (option.id.clone(), option.current.clone(), option.name.clone());
        window.open_dialog(cx, move |dialog, _, _| {
            let (command, view, focused, groups, id, current, title) = (command.clone(), view.clone(), focused.clone(), groups.clone(), id.clone(), current.clone(), title.clone());
            dialog.close_button(false).p_0().w(px(420.)).content(move |content, window, cx| {
                if !focused.replace(true) {
                    let command = command.clone();
                    window.defer(cx, move |window, cx| command.read(cx).focus_handle(cx).focus(window, cx));
                }
                let mut list = Command::new(&command).bordered(false).placeholder(format!("{title}…"));
                for (head, values) in groups.iter() {
                    let items = values.iter().map(|v| {
                        let label = v.name.split_once('/').map_or(v.name.clone(), |(_, rest)| rest.to_string());
                        let mut words = vec![v.value.clone(), v.name.clone()];
                        words.extend(v.description.clone());
                        CommandItem::new().label(label).checked(v.value == current).keywords(words)
                    });
                    let group = if head.is_empty() { CommandGroup::new() } else { CommandGroup::new().label(head.clone()) };
                    list = list.group(group.items(items));
                }
                let (view, groups, id) = (view.clone(), groups.clone(), id.clone());
                content.child(list.on_confirm(move |index, window, cx| {
                    if let Some(value) = groups.get(index.section).and_then(|(_, values)| values.get(index.row)) {
                        let (id, value) = (id.clone(), value.value.clone());
                        let _ = view.update(cx, |this, cx| this.choose(id, value, cx));
                    }
                    window.close_dialog(cx);
                }).on_cancel(|window, cx| window.close_dialog(cx)))
            })
        });
    }

    /// Whether the agent decides alone what is safe, in a mode of its own
    /// (Claude Code's Auto, Codex's Auto review, Qwen's and Kimi's auto).
    fn own_auto(&self) -> bool {
        self.thread.option("mode").is_some_and(|mode| {
            mode.values.iter().any(|v| {
                let (value, name) = (v.value.to_lowercase(), v.name.to_lowercase());
                (value.contains("auto") || name.contains("auto") || value == "agent") && !value.contains("edit") && !name.contains("edit") && !name.contains("accept")
            })
        })
    }

    /// Who answers the agent's questions, for one with no auto of its own.
    fn render_policy(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::acp::policy::{CHOICE, Policy};
        if self.own_auto() || self.thread.options.is_empty() && self.thread.status == Status::Starting {
            return None;
        }
        let current = Policy::parse(&self.thread.policy);
        let view = cx.weak_entity();
        Some(
            Button::new("chat-policy")
                .xsmall()
                .ghost()
                .label(format!("Permissions: {} ▾", current.label()))
                .tooltip(current.description())
                .dropdown_menu(move |mut menu, _, _| {
                    for policy in Policy::ALL {
                        let view = view.clone();
                        let label = match policy {
                            Policy::Rules => "ideae decides: safe things allowed, the rest asked",
                            other => other.label(),
                        };
                        menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(label).checked(policy == current).on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.send(ToHost::Policy { policy: policy.id().into() });
                                cx.emit(ChatEvent::Chose { id: CHOICE.into(), value: policy.id().into() });
                            });
                        }));
                    }
                    menu
                })
                .into_any_element(),
        )
    }

    fn render_options(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut options: Vec<&ConfigOption> = self.thread.options.iter().collect();
        let rank = |o: &ConfigOption| match o.category.as_str() {
            "model" => 0,
            "thought_level" => 1,
            "mode" => 2,
            _ => 3,
        };
        options.sort_by_key(|o| rank(o));
        options
            .into_iter()
            .enumerate()
            .map(|(n, option)| {
                let view = cx.weak_entity();
                let (id, current) = (option.id.clone(), option.current.clone());
                let values = self.option_values.get(&option.id).cloned().unwrap_or_default();
                // A model's provider ("Ollama (local)/qwen3-coder") is in the tooltip.
                let (provider, short) = match option.current_name().split_once('/') {
                    Some((provider, name)) => (Some(provider.to_string()), name.to_string()),
                    None => (None, option.current_name().to_string()),
                };
                let label = match option.category.as_str() {
                    "model" | "mode" => short,
                    _ => format!("{}: {short}", option.name),
                };
                let tip = match provider {
                    Some(provider) => format!("{} ({provider}): new tasks of this agent start with what you choose", option.name),
                    None => format!("{}: new tasks of this agent start with what you choose", option.name),
                };
                let button = Button::new(("chat-option", n)).xsmall().ghost().label(format!("{label} ▾")).tooltip(tip);
                if values.len() > 10 {
                    let option = option.clone();
                    return button.on_click(cx.listener(move |this, _, window, cx| this.pick(option.clone(), window, cx))).into_any_element();
                }
                button
                    .dropdown_menu(move |mut menu, _, _| {
                        menu = menu.scrollable(true).max_h(px(420.));
                        for value in values.iter() {
                            let (view, id, chosen) = (view.clone(), id.clone(), value.value.clone());
                            let item = gpui_kit::component::menu::PopupMenuItem::new(value.name.clone()).checked(value.value == current).on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.choose(id.clone(), chosen.clone(), cx));
                            });
                            menu = menu.item(item);
                        }
                        menu
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The agent's plan of this turn, while it is not done: kept in sight
    /// above the prompt, its step in hand first, all of it on a click.
    fn render_plan(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let last_prompt = self.thread.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(0);
        let items = self.thread.entries[last_prompt..].iter().rev().find_map(|e| match e {
            Entry::Plan { items } => Some(items),
            _ => None,
        })?;
        let done = items.iter().filter(|i| i.status == "completed").count();
        if done == items.len() {
            return None;
        }
        let theme = cx.theme();
        let current = items.iter().find(|i| i.status == "in_progress").or_else(|| items.iter().find(|i| i.status != "completed"))?;
        let open = self.plan_open;
        Some(
            v_flex()
                .id("chat-plan")
                .flex_none()
                .px_3()
                .py_1()
                .gap_0p5()
                .text_xs()
                .border_t_1()
                .border_color(theme.border)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.plan_open = !this.plan_open;
                    cx.notify();
                }))
                .child(h_flex().gap_2().child(div().text_color(theme.muted_foreground).child(format!("{} Plan {done}/{}", if open { "▾" } else { "▸" }, items.len()))).child(div().truncate().child(current.text.clone())))
                .when(open, |this| {
                    this.children(items.iter().map(|item| {
                        let mark = match item.status.as_str() {
                            "completed" => "✓",
                            "in_progress" => "▸",
                            _ => "○",
                        };
                        h_flex().gap_2().pl_3().child(div().w(px(12.)).child(mark)).child(div().text_color(if item.status == "completed" { theme.muted_foreground } else { theme.foreground }).child(item.text.clone()))
                    }))
                })
                .into_any_element(),
        )
    }

    /// A chat with nothing in it yet: whom it is with, and what it can do.
    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let who = match (&self.thread.agent, &self.thread.status) {
            (_, Status::Starting) => "Starting the agent…".to_string(),
            (Some(agent), _) => agent.clone(),
            (None, _) => "The agent".to_string(),
        };
        let downloads = matches!(self.spec.command.split_whitespace().find(|w| !w.contains('=')), Some("npx" | "uvx" | "bunx" | "pnpx"));
        let first = (self.thread.status == Status::Starting && downloads).then(|| "Its first start downloads it, which can take a minute.".to_string());
        // A mode of its own that asks nothing, kept from last time: said
        // before the first prompt, not found out from what it did.
        let auto = self.thread.option("mode").filter(|m| {
            let (value, name) = (m.current.to_lowercase(), m.current_name().to_lowercase());
            (value.contains("auto") || name.contains("auto") || value.contains("bypass") || value == "yolo") && !value.contains("edit")
        });
        let auto = auto.map(|m| format!("It starts in {}: it runs its commands and changes files without asking. The {} menu below changes that.", m.current_name(), m.name));
        let hints = first.into_iter().chain([
            "Enter sends; Shift+Enter starts a new line. What you send while it works waits its turn; ↑ brings back your last prompt.".to_string(),
            format!("@ mentions a file of the task; / lists the agent's commands; {} pastes an image, for agents that take them.", crate::keys::label("Ctrl+V")),
            format!("When it asks, Y allows, A allows always, N declines; Esc stops its turn; {} looks through the conversation.", crate::keys::label("Ctrl+F")),
            "Follow opens the files it reads or changes beside the chat. It works in this task's worktree, and goes on when the window closes.".to_string(),
        ]);
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .px_6()
            .child(div().text_sm().child(who))
            .children(auto.map(|a| div().max_w(px(460.)).text_center().text_xs().text_color(theme.warning).child(a)))
            .children(hints.map(|h| div().max_w(px(460.)).text_center().text_xs().text_color(theme.muted_foreground).child(h)))
            .into_any_element()
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let bar = || h_flex().flex_none().gap_2().px_3().py_1().text_xs().bg(theme.muted).border_t_1().border_color(theme.border);
        match &self.thread.status {
            Status::Exited(why) => Some(
                bar()
                    .child({
                        let said = match why {
                            Some(why) => format!("The agent has ended: {}", why.lines().last().unwrap_or(why)),
                            None => "The agent has ended".to_string(),
                        };
                        // Long, it keeps the button in view; all of it on hover.
                        div().id("chat-ended").flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(said).when_some(why.clone(), |d, why| d.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(why.clone()).build(window, cx)))
                    })
                    .child(Button::new("chat-restart").xsmall().outline().label("Restart").on_click(cx.listener(|this, _, _, cx| this.restart(cx))))
                    .into_any_element(),
            ),
            _ if self.stale_host && self.thread.status.live() => Some(
                bar()
                    .child(div().flex_1().min_w_0().text_color(theme.muted_foreground).child("This agent runs under ideae's host of before the last update: what is new may not answer. Restart it (its session goes on) when it is not working."))
                    .child(Button::new("chat-restart-stale").xsmall().outline().label("Restart").on_click(cx.listener(|this, _, _, cx| {
                        this.stale_host = false;
                        this.restart(cx);
                    })))
                    .into_any_element(),
            ),
            Status::SignIn => {
                let methods = self.thread.auth.clone();
                Some(
                    bar()
                        .child(div().flex_1().child("The agent wants you to sign in"))
                        .children(methods.into_iter().enumerate().map(|(n, method)| {
                            let label = if method.name.is_empty() { method.id.clone() } else { method.name.clone() };
                            Button::new(("chat-sign-in", n))
                                .xsmall()
                                .outline()
                                .label(label)
                                .when_some(method.description.clone(), |b, d| b.tooltip(d))
                                .on_click(cx.listener(move |this, _, window, cx| match &method.terminal {
                                    Some(args) => this.sign_in_in_terminal(args.clone(), window, cx),
                                    None => this.send(ToHost::Authenticate { method: method.id.clone() }),
                                }))
                        }))
                        .child(Button::new("chat-signed-in").xsmall().ghost().label("Signed in: start again").on_click(cx.listener(|this, _, _, cx| this.restart(cx))))
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    /// The agent's own command with its sign-in arguments, in a terminal of the task.
    fn sign_in_in_terminal(&mut self, args: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let line = std::iter::once(self.spec.command.clone()).chain(args.iter().map(|a| crate::agents::quote(a))).collect::<Vec<_>>().join(" ");
        let env = self.spec.env.clone();
        if let Some(task) = self.task.upgrade() {
            task.update(cx, |t, cx| t.run_in_terminal(line, env, window, cx));
        }
    }
}

/// The key that answers a question with an option of this kind.
fn answer_key(kind: &str) -> Option<&'static str> {
    match kind {
        "allow_once" => Some("y"),
        "allow_always" => Some("a"),
        "reject_once" => Some("n"),
        _ => None,
    }
}

/// `query` (lowercase) in `text`, case aside: ASCII folded in place, other
/// text lowercased only when the query needs it.
fn contains_folded(text: &str, query: &str) -> bool {
    if query.is_ascii() {
        let (hay, needle) = (text.as_bytes(), query.as_bytes());
        return needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle));
    }
    // Character by character, nothing copied.
    let needle: Vec<char> = query.chars().collect();
    let folded = |c: char| c.to_lowercase().next().unwrap_or(c);
    text.char_indices().any(|(at, _)| {
        let mut rest = text[at..].chars();
        needle.iter().all(|n| rest.next().is_some_and(|c| folded(c) == *n))
    })
}

fn diff_lines(diff: &crate::acp::thread::Diff) -> DiffLines {
    let old = diff.old.clone().unwrap_or_default();
    // A diff of two long, very different files gives up on the finest one.
    let changes = similar::TextDiff::configure().timeout(std::time::Duration::from_millis(300)).diff_lines(&old, &diff.new);
    let mut lines: Vec<(char, SharedString)> = Vec::new();
    let first = changes
        .iter_all_changes()
        .find(|c| c.tag() != similar::ChangeTag::Equal)
        .and_then(|c| c.new_index().or(c.old_index()))
        .map_or(1, |i| i as u32 + 1);
    'groups: for group in changes.grouped_ops(2) {
        if !lines.is_empty() {
            lines.push(('⋯', SharedString::default()));
        }
        for op in group {
            for change in changes.iter_changes(&op) {
                if lines.len() > 200 {
                    lines.push(('⋯', SharedString::default()));
                    break 'groups;
                }
                let sign = match change.tag() {
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                    similar::ChangeTag::Equal => ' ',
                };
                lines.push((sign, change.value().trim_end_matches('\n').to_string().into()));
            }
        }
    }
    let name = std::path::Path::new(&diff.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| diff.path.clone());
    DiffLines { name: name.into(), path: diff.path.clone(), first, new_file: diff.old.is_none(), lines }
}

fn render_diff(diff: &DiffLines, review: AnyElement, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let lines = diff.lines.iter().map(|(sign, text)| {
        let bg = match sign {
            '-' => Some(theme.danger.opacity(0.15)),
            '+' => Some(theme.success.opacity(0.15)),
            _ => None,
        };
        match sign {
            '⋯' => div().text_color(theme.muted_foreground).child("⋯"),
            _ => div().when_some(bg, |d, bg| d.bg(bg)).child(SharedString::from(format!("{sign} {text}"))),
        }
    });
    v_flex()
        .child(h_flex().gap_2().child(div().flex_1().text_color(theme.muted_foreground).child(if diff.new_file { SharedString::from(format!("{} (new)", diff.name)) } else { diff.name.clone() })).child(review))
        .child(v_flex().px_2().py_1().rounded_sm().border_1().border_color(theme.border).font_family(theme.mono_font_family.clone()).children(lines))
        .into_any_element()
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, bg) = (theme.muted_foreground, theme.border, theme.background);
        let working = matches!(self.thread.status, Status::Working | Status::Asking);
        let usage = self.thread.usage.as_ref().map(|u| {
            let fill = if u.size > 0 { format!("{}%", (u.used * 100 / u.size).min(100)) } else { format!("{}k", u.used / 1000) };
            match &u.cost {
                Some(cost) => format!("{fill} · {cost}"),
                None => fill,
            }
        });
        let status = match &self.thread.status {
            Status::Starting => Some("Starting…"),
            Status::Working => Some("Working…"),
            Status::Asking => Some("Waiting for your answer"),
            _ => None,
        };
        let queued = self.render_queued(cx);
        let search = self.render_search(cx);
        let commands = self.render_commands(cx);
        let mut options = self.render_options(cx);
        options.extend(self.render_policy(cx));
        let plan = self.render_plan(cx);
        let images = self.images.iter().enumerate().map(|(n, (image, size))| {
            Button::new(("chat-image", n))
                .xsmall()
                .outline()
                .label(format!("{} · {} KB ✕", image.mime.trim_start_matches("image/"), size.div_ceil(1024)))
                .tooltip("Sent with the next prompt; click to drop it")
                .on_click(cx.listener(move |this, _, _, cx| {
                    if n < this.images.len() {
                        this.images.remove(n);
                    }
                    cx.notify();
                }))
        });
        let images: Vec<_> = images.collect();
        let entries = cx.processor(|this, row: usize, window, cx| this.render_row(row, window, cx));
        v_flex()
            .size_full()
            .bg(bg)
            .capture_action(cx.listener(Self::paste))
            .capture_key_down(cx.listener(Self::key))
            .capture_action(cx.listener(Self::escape))
            .capture_action(cx.listener(Self::up))
            .capture_action(cx.listener(Self::tab))
            .capture_action(cx.listener(Self::search_action))
            .children(search)
            .when(self.thread.entries.is_empty(), |this| this.child(self.render_empty(cx)))
            .when(!self.thread.entries.is_empty(), |this| this.child(list(self.list.clone(), entries).flex_1().min_h_0()))
            .children(plan)
            .children(self.render_banner(cx))
            .children(self.render_restore(cx))
            .children(commands)
            .child(
                // The prompt in the conversation's column.
                v_flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .p_2()
                    .border_t_1()
                    .border_color(border)
                    .when(!images.is_empty(), |this| this.child(h_flex().w_full().max_w(px(COLUMN)).gap_1().children(images)))
                    .child(div().w_full().max_w(px(COLUMN)).child(gpui_kit::component::input::Textarea::new(&self.input)))
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(COLUMN))
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("chat-mention")
                                    .xsmall()
                                    .ghost()
                                    .label("@")
                                    .tooltip("Mention a file of the task: it goes to the agent as a link to it")
                                    .on_click(cx.listener(|this, _, window, cx| this.mention(window, cx))),
                            )
                            .child(
                                Button::new("chat-follow")
                                    .xsmall()
                                    .ghost()
                                    .label("Follow")
                                    .selected(self.follow)
                                    .tooltip("Open the files the agent reads or changes beside the chat, as it goes")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.follow = !this.follow;
                                        this.followed = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("chat-log")
                                    .xsmall()
                                    .ghost()
                                    .label("Log")
                                    .tooltip("What the agent wrote on its error stream: for when it fails or acts up")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_log(window, cx))),
                            )
                            .children(options)
                            .child(div().flex_1())
                            .children(queued)
                            .when_some(status, |this, status| this.child(div().text_xs().text_color(muted).child(status)))
                            .when_some(usage, |this, usage| {
                                // Nearly full: said in the warning's color, and what to do.
                                let full = self.thread.usage.as_ref().is_some_and(|u| u.size > 0 && u.used * 10 >= u.size * 9);
                                let text = div().id("chat-usage").text_xs().text_color(if full { cx.theme().warning } else { muted }).child(usage);
                                this.child(if full {
                                    text.tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Its context is nearly full: each turn costs more, and the agent may forget the start. /compact, if it has it, sums it up; or carry the task on in a new session.").build(window, cx)).into_any_element()
                                } else {
                                    text.into_any_element()
                                })
                            })
                            .when(working, |this| {
                                this.child(Button::new("chat-cancel").xsmall().outline().label("Stop").tooltip("Cancel the agent's turn (Esc)").on_click(cx.listener(|this, _, _, _| this.send(ToHost::Cancel))))
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod ui_tests {
    use super::ChatView;
    use crate::acp::thread::{Entry, Permission, PermissionOption, SlashCommand, Status, Thread, Tool};
    use crate::acp::wire::{HostSpec, ToHost, ToWindow};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Bounds, Entity, Keystroke, Point, TestAppContext, WeakEntity, WindowBounds, WindowOptions, px, size};

    /// A chat with no host, in a window: what it is told, and what it sends.
    fn chat(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<ChatView>) {
        cx.update(gpui_kit::init);
        // In the app's root, its key bindings and all (Tab moves the focus).
        cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(900.), px(700.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| ChatView::unlinked("t".into(), HostSpec::default(), WeakEntity::new_invalid(), window, cx))).expect("a test window")
        })
    }

    fn thread(status: Status, entries: Vec<Entry>) -> Thread {
        Thread { session: Some("s".into()), status, entries, commands: vec![SlashCommand { name: "review".into(), description: "Review".into() }], ..Default::default() }
    }

    fn press(cx: &mut TestAppContext, handle: gpui_kit::AnyWindowHandle, key: &'static str) {
        cx.update_window(handle, move |_, window, cx| {
            let mut stroke = Keystroke::parse(key).unwrap();
            if key.chars().count() == 1 {
                stroke.key_char = Some(key.to_string());
            }
            window.dispatch_keystroke(stroke, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn focus_prompt(cx: &mut TestAppContext, handle: gpui_kit::AnyWindowHandle, view: &Entity<ChatView>) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.read(cx).focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    /// A question answered by its letter, the letter not typed; Esc stops
    /// the turn.
    #[gpui_kit::test]
    fn a_question_is_answered_by_its_key(cx: &mut TestAppContext) {
        let (handle, view) = chat(cx);
        let tool = Tool {
            id: "t1".into(),
            title: "Edit a.rs".into(),
            status: "pending".into(),
            permission: Some(Permission {
                request: 7,
                options: vec![PermissionOption { id: "yes".into(), name: "Allow".into(), kind: "allow_once".into() }, PermissionOption { id: "no".into(), name: "Reject".into(), kind: "reject_once".into() }],
            }),
            ..Default::default()
        };
        view.update(cx, |v, cx| v.hear(vec![ToWindow::Hello { version: crate::acp::wire::VERSION, thread: Box::new(thread(Status::Asking, vec![Entry::User { text: "fix it".into() }, Entry::Tool(tool)])) }], cx));
        focus_prompt(cx, handle, &view);
        press(cx, handle, "y");
        let (sent, typed) = view.read_with(cx, |v, cx| (v.sent.borrow().clone(), v.input.read(cx).text().to_string()));
        assert!(sent.iter().any(|m| matches!(m, ToHost::Answer { request: 7, option: Some(o) } if o == "yes")), "{sent:?}");
        assert_eq!(typed, "", "the letter answered, not typed");
        press(cx, handle, "escape");
        assert!(view.read_with(cx, |v, _| v.sent.borrow().iter().any(|m| matches!(m, ToHost::Cancel))), "Esc stops the turn");
    }

    /// Tab completes a command and the prompt keeps the keyboard; ↑ brings
    /// the last prompt back; Ctrl+F opens the search, Esc closes it.
    #[gpui_kit::test]
    fn the_prompt_s_keys(cx: &mut TestAppContext) {
        let (handle, view) = chat(cx);
        view.update(cx, |v, cx| v.hear(vec![ToWindow::Hello { version: crate::acp::wire::VERSION, thread: Box::new(thread(Status::Done, vec![Entry::User { text: "fix it".into() }, Entry::Agent { text: "Fixed.".into() }])) }], cx));
        focus_prompt(cx, handle, &view);
        cx.update_window(handle, |_, window, cx| view.update(cx, |v, cx| v.input.update(cx, |i, cx| i.set_value("/rev", window, cx)))).unwrap();
        press(cx, handle, "tab");
        let (value, focused) = cx.update_window(handle, |_, window, cx| (view.read(cx).input.read(cx).text().to_string(), view.read(cx).focus_handle(cx).is_focused(window))).unwrap();
        assert_eq!((value.as_str(), focused), ("/review ", true));
        cx.update_window(handle, |_, window, cx| view.update(cx, |v, cx| v.input.update(cx, |i, cx| i.set_value("", window, cx)))).unwrap();
        press(cx, handle, "up");
        assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text().to_string()), "fix it");
        press(cx, handle, if cfg!(target_os = "macos") { "cmd-f" } else { "ctrl-f" });
        assert!(view.read_with(cx, |v, _| v.search.is_some()), "the search opens");
        press(cx, handle, "escape");
        assert!(view.read_with(cx, |v, _| v.search.is_none()), "and closes");
    }

    /// Going back to a prompt's files: asked of the host, shown, confirmed.
    #[gpui_kit::test]
    fn files_go_back_once_confirmed(cx: &mut TestAppContext) {
        let (handle, view) = chat(cx);
        let mut t = thread(Status::Done, vec![Entry::User { text: "fix it".into() }, Entry::Agent { text: "Fixed.".into() }]);
        t.checkpoints.insert(0, "abc".into());
        view.update(cx, |v, cx| {
            v.hear(vec![ToWindow::Hello { version: crate::acp::wire::VERSION, thread: Box::new(t) }], cx);
            v.ask_restore(0, cx);
            v.hear(vec![ToWindow::Restorable { ix: 0, files: Some(vec![('M', "a.rs".into())]) }], cx);
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("chat-restore-go", cx);
        })
        .unwrap();
        let sent = view.read_with(cx, |v, _| v.sent.borrow().clone());
        assert!(matches!(sent.as_slice(), [ToHost::Preview { ix: 0 }, ToHost::Restore { ix: 0 }]), "{sent:?}");
        assert!(view.read_with(cx, |v, _| v.restoring.is_none()));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_message_is_cut_between_blocks_never_in_code() {
        let para = "word ".repeat(300);
        let code = format!("```\n{}\n\n{}\n```\n", "let a = 1;".repeat(80), "let b = 2;".repeat(80));
        let text = format!("{para}\n\n{code}\n{para}\n\n{para}");
        let blocks = super::blocks(&text);
        assert!(blocks.len() >= 3, "{blocks:?}");
        assert_eq!(blocks.first().unwrap().0, 0);
        assert_eq!(blocks.last().unwrap().1, text.len());
        for pair in blocks.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "no gap, no overlap");
        }
        for (start, end) in &blocks {
            let fences = text[*start..*end].matches("```").count();
            assert_eq!(fences % 2, 0, "a block keeps its code whole: {}", &text[*start..*end]);
        }
        assert_eq!(super::blocks("short"), vec![(0, 5)]);

        // A very long piece of code: cut too, each part shown as code.
        let code = format!("```rust\n{}```\n", "let x = 1;\n".repeat(5000));
        let parts = super::blocks_with_fences(&code);
        assert!(parts.len() >= 3, "{}", parts.len());
        assert_eq!(parts[0].opens, None);
        assert!(parts[0].closes && parts[1].opens.as_deref() == Some("```rust") && parts[1].closes);
        assert!(parts.last().is_some_and(|p| p.opens.is_some() && !p.closes));
    }
}
