//! The host of an ACP agent: starts it, speaks ACP with it (JSON-RPC on its
//! standard streams), keeps the conversation as a [`Thread`] and its
//! journal, and tells a window what changed. It answers the agent's own
//! requests: file reads (from the window's editor when it has the file
//! open with edits), file writes, and permission questions, which wait for
//! a window, or are answered yes for a variant left to work.
//!
//! In the daemon it is a process of its own (`ideae --acp-host`), so the
//! agent outlives the window and the daemon's other sessions a bad agent;
//! without a daemon it is a thread of the window.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::thread::{AuthMethod, ConfigOption, Diff, Entry, Location, Op, OptionValue, Permission, PermissionOption, PlanItem, SlashCommand, Status, Thread, Tool, Usage};
use super::wire::{self, HostSpec, ToHost, ToWindow};

pub const SPEC_ENV: &str = "IDEAE_ACP_SPEC";

/// Text a tool gave back, kept from its end.
const OUTPUT_KEPT: usize = 16 << 10;
/// What the agent printed on its error stream, for when it ends.
const STDERR_KEPT: usize = 4 << 10;
/// Its log grows to this, then starts anew (the part before kept as .old).
const LOG_MOST: u64 = 1 << 20;
/// How long a file read waits for the window's buffer before the disk.
const BUFFER_WAIT: Duration = Duration::from_millis(1500);
/// Streamed text is sent at most this often, in one piece.
const FLUSH_EVERY: Duration = Duration::from_millis(50);
/// JSON-RPC's code for an agent that wants to be signed in.
const AUTH_REQUIRED: i64 = -32000;
/// How long an agent has to answer `initialize`: its first start through
/// npx or uvx downloads it first.
const START_WAIT: Duration = Duration::from_secs(180);
/// A journal grown past this, and well past its snapshot, is written anew.
pub const JOURNAL_MOST: u64 = 8 << 20;
/// An agent that ends mid-turn is started again once in this long.
const RESTART_EVERY: Duration = Duration::from_secs(60);

pub enum Event {
    /// Made on another thread for the window: sent as it is.
    Tell(ToWindow),
    Agent(Value),
    AgentGone,
    Window(ToHost),
    WindowGone,
    TermExited { id: String, code: Option<i32>, signal: Option<String> },
}

enum Call {
    Initialize,
    NewSession,
    LoadSession,
    Prompt,
    Choose { id: String, value: String },
    Authenticate,
}

struct PendingRead {
    rpc: Value,
    path: PathBuf,
    line: Option<usize>,
    limit: Option<usize>,
    until: Instant,
}

struct Agent {
    child: Child,
    stdin: ChildStdin,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

pub struct Host {
    spec: HostSpec,
    thread: Thread,
    agent: Option<Agent>,
    out: Box<dyn FnMut(ToWindow) + Send>,
    journal: Option<std::fs::File>,
    /// Bytes in the journal, and in its snapshot when it was last written.
    journal_bytes: u64,
    snapshot_bytes: u64,
    next_id: u64,
    calls: HashMap<u64, Call>,
    /// Tool call ids to their entries.
    tools: HashMap<String, usize>,
    /// Permission questions waiting: our number to the agent's request id.
    asks: HashMap<u64, Value>,
    reads: HashMap<u64, PendingRead>,
    next_request: u64,
    /// Streamed text not sent yet: the entry and what came for it.
    streaming: Option<(usize, String)>,
    load_session: bool,
    auth_methods: Vec<AuthMethod>,
    /// A session being loaded replays its conversation: ignored when the
    /// journal has it already.
    replaying: bool,
    /// The agent took the prompt and has not answered yet.
    prompting: bool,
    /// Commands it runs through the app, by terminal id.
    terminals: HashMap<String, super::terminal::Term>,
    /// The tool call that shows each terminal, by its id.
    shown_at: HashMap<String, usize>,
    next_terminal: u64,
    /// The files and images of the prompts queued, in their order.
    queued_with: VecDeque<(Vec<PathBuf>, Vec<wire::Image>)>,
    tx: Option<mpsc::Sender<Event>>,
    /// The first prompt of a new session, if the one asked for is not loaded.
    otherwise: Option<String>,
    /// A window is there to ask in (the daemon says when the last goes).
    window_here: bool,
    /// When each question waiting was asked.
    asked_at: HashMap<u64, Instant>,
    /// npx's broken cache was cleared once this run: not again.
    npx_mended: bool,
    /// The session's cost when this run began (USD), and whether its
    /// budget is spent: then no prompt goes.
    cost_base: f64,
    over_budget: bool,
    /// When `initialize` was sent and is not answered yet.
    starting: Option<Instant>,
    /// When the agent was last started again after it ended mid-turn.
    restarted: Option<Instant>,
}

impl Host {
    pub fn new(spec: HostSpec, out: Box<dyn FnMut(ToWindow) + Send>) -> Self {
        let (thread, journal, snapshot_bytes) = open_journal(spec.journal.as_deref());
        let mut host = Self {
            spec,
            thread,
            agent: None,
            out,
            journal,
            journal_bytes: snapshot_bytes,
            snapshot_bytes,
            next_id: 0,
            calls: HashMap::new(),
            tools: HashMap::new(),
            asks: HashMap::new(),
            reads: HashMap::new(),
            next_request: 0,
            streaming: None,
            load_session: false,
            auth_methods: Vec::new(),
            replaying: false,
            prompting: false,
            terminals: HashMap::new(),
            shown_at: HashMap::new(),
            next_terminal: 0,
            queued_with: VecDeque::new(),
            tx: None,
            starting: None,
            restarted: None,
            otherwise: None,
            window_here: true,
            asked_at: HashMap::new(),
            cost_base: 0.0,
            npx_mended: false,
            over_budget: false,
        };
        host.cost_base = host.thread.usage.as_ref().and_then(|u| u.cost.as_deref()).and_then(super::spend::usd).unwrap_or(0.0);
        // Spent before it starts: nothing goes.
        host.over_budget = host.spec.budget.is_some_and(|b| b <= 0.0);
        host.reindex_tools();
        if host.thread.policy != host.spec.policy {
            let policy = host.spec.policy.clone();
            host.record(Op::Policy { policy });
        }
        host
    }

    /// Runs until the window says stop or goes away for good.
    pub fn serve(mut self, tx: mpsc::Sender<Event>, rx: mpsc::Receiver<Event>) {
        self.tx = Some(tx.clone());
        self.start(tx);
        let mut flushed = Instant::now();
        loop {
            let event = match rx.recv_timeout(FLUSH_EVERY) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match event {
                Some(Event::Agent(msg)) => self.from_agent(msg),
                Some(Event::AgentGone) => self.agent_gone(),
                Some(Event::Window(ToHost::Stop)) | Some(Event::WindowGone) => break,
                Some(Event::Window(msg)) => self.from_window(msg),
                Some(Event::TermExited { id, code, signal }) => self.term_exited(id, code, signal),
                Some(Event::Tell(msg)) => (self.out)(msg),
                None => {}
            }
            // Streamed text goes out at most every FLUSH_EVERY, in one piece,
            // even while the agent never pauses.
            if flushed.elapsed() >= FLUSH_EVERY {
                self.show_terminals();
                self.flush();
                self.expire_reads();
                self.compact_journal();
                self.decline_unanswered();
                if self.starting.is_some_and(|at| at.elapsed() > START_WAIT) {
                    self.starting = None;
                    self.stop_agent();
                    let why = format!("It did not answer in {} minutes", START_WAIT.as_secs() / 60);
                    self.notice(format!("The agent did not start: {}.", why.to_lowercase()));
                    self.emit(Op::Status { status: Status::Exited(Some(why)) });
                }
                flushed = Instant::now();
            }
        }
        self.flush();
        self.stop_agent();
    }

    fn start(&mut self, tx: mpsc::Sender<Event>) {
        match spawn_agent(&self.spec, tx) {
            Ok(agent) => {
                self.agent = Some(agent);
                self.starting = Some(Instant::now());
                self.call(Call::Initialize, "initialize", json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": true, "writeTextFile": true },
                        "terminal": true,
                        "auth": { "terminal": true },
                        "_meta": { "terminal-auth": true },
                    },
                    "clientInfo": { "name": "ideae", "version": env!("CARGO_PKG_VERSION") },
                }));
            }
            Err(e) => {
                self.notice(format!("Could not start `{}`: {e}", self.spec.command));
                self.emit(Op::Status { status: Status::Exited(Some(e.to_string())) });
            }
        }
    }

    // ------------------------------------------------------------ the window

    fn from_window(&mut self, msg: ToHost) {
        match msg {
            ToHost::Attach { .. } => {
                self.window_here = true;
                self.flush();
                let thread = Box::new(self.thread.clone());
                (self.out)(ToWindow::Hello { version: wire::VERSION, thread });
            }
            ToHost::Prompt { text, files, images } => self.prompt(text, files, images),
            ToHost::Cancel => self.cancel(),
            ToHost::Answer { request, option } => self.answer(request, option, None),
            ToHost::Choose { id, value } => self.choose(id, value),
            ToHost::Authenticate { method } => {
                self.call(Call::Authenticate, "authenticate", json!({ "methodId": method }));
            }
            ToHost::Buffer { request, text } => {
                if let Some(read) = self.reads.remove(&request) {
                    self.finish_read(read, text);
                }
            }
            ToHost::Unqueue => {
                self.queued_with.clear();
                self.emit(Op::Queued { prompts: Vec::new() })
            }
            ToHost::Take { at } => {
                if at < self.thread.queued.len() {
                    let mut queued = self.thread.queued.clone();
                    queued.remove(at);
                    self.queued_with.remove(at);
                    self.emit(Op::Queued { prompts: queued });
                }
            }
            ToHost::Policy { policy } => self.emit(Op::Policy { policy }),
            ToHost::Otherwise { prompt } => self.otherwise = Some(prompt),
            ToHost::Detached => {
                self.window_here = false;
                // A question asked a moment ago, to a window now gone.
                if let Some(tool) = self.thread.asking() {
                    let what = if tool.title.is_empty() { tool.kind.clone() } else { tool.title.clone() };
                    self.notify_desktop(&format!("Allow: {what}?"));
                }
            }
            // Several git runs: off the loop, which goes on streaming.
            ToHost::Preview { ix } => {
                let (cwd, name, tree, tx) = (self.spec.cwd.clone(), self.checkpoint_name(), self.thread.checkpoints.get(&ix).cloned(), self.tx.clone());
                std::thread::spawn(move || {
                    let files = tree.and_then(|tree| restorable(&cwd, &name, &tree)).map(|(_, files)| files);
                    if let Some(tx) = tx {
                        let _ = tx.send(Event::Tell(ToWindow::Restorable { ix, files }));
                    }
                });
            }
            ToHost::Restore { ix } => self.restore(ix),
            ToHost::Stop => {}
        }
    }

    fn prompt(&mut self, text: String, files: Vec<PathBuf>, images: Vec<wire::Image>) {
        let text = text.trim_end().to_string();
        if text.is_empty() && images.is_empty() {
            return;
        }
        if self.over_budget {
            return self.notice("This agent's daily limit is spent: your prompt was not sent. Raise the limit in its profile (Settings → Agents), or go on tomorrow.".into());
        }
        // Ended: nothing would take it, and the chat would wait for ever.
        if !self.thread.status.live() || (self.agent.is_none() && self.thread.status != Status::Starting) {
            return self.notice("The agent has ended: your prompt was not sent. Restart it to go on.".into());
        }
        let busy = self.prompting || matches!(self.thread.status, Status::Starting | Status::Working | Status::Asking | Status::SignIn);
        if busy && self.thread.status.live() {
            let mut queued = self.thread.queued.clone();
            queued.push(text);
            self.queued_with.push_back((files, images));
            return self.emit(Op::Queued { prompts: queued });
        }
        let Some(session) = self.thread.session.clone() else { return };
        let shown = match images.len() {
            0 => text.clone(),
            n => format!("{text}\n\n[{n} image{}]", if n == 1 { "" } else { "s" }).trim().to_string(),
        };
        let ix = self.thread.entries.len();
        self.emit(Op::Push { entry: Entry::User { text: shown } });
        self.emit(Op::Status { status: Status::Working });
        // The files as they are before it, to come back to: taken before the
        // agent has it (it may change them at once), after the window shows it.
        if let Some(tree) = super::checkpoint::take(&self.spec.cwd, &self.checkpoint_name()) {
            self.emit(Op::Checkpoint { ix, tree });
            self.prune_checkpoints();
        }
        self.prompting = true;
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        for file in &files {
            let name = file.strip_prefix(&self.spec.cwd).unwrap_or(file).to_string_lossy().into_owned();
            blocks.push(json!({ "type": "resource_link", "uri": format!("file://{}", file.display()), "name": name }));
        }
        if self.thread.images {
            blocks.extend(images.iter().map(|i| json!({ "type": "image", "mimeType": i.mime, "data": i.data })));
        }
        self.call(Call::Prompt, "session/prompt", json!({ "sessionId": session, "prompt": blocks }));
    }

    /// The oldest checkpoints' refs dropped past the last KEPT (a tree two
    /// prompts share stays while a newer one has it); their buttons then
    /// say they are gone.
    fn prune_checkpoints(&mut self) {
        const KEPT: usize = 50;
        let count = self.thread.checkpoints.len();
        if count <= KEPT {
            return;
        }
        let newer: std::collections::HashSet<&String> = self.thread.checkpoints.values().skip(count - KEPT).collect();
        // The one that just fell out: those before it went earlier.
        let Some(tree) = self.thread.checkpoints.values().nth(count - KEPT - 1).filter(|t| !newer.contains(t)).cloned() else { return };
        super::checkpoint::drop_one(&self.spec.cwd, &self.checkpoint_name(), &tree);
    }

    /// A desktop notification, for when no window shows the chat.
    fn notify_desktop(&self, body: &str) {
        let Some(name) = &self.spec.notify_as else { return };
        if cfg!(test) {
            return;
        }
        crate::desktop::notify(&format!("{name} needs your answer"), body);
    }

    /// Questions nobody answered in the time set: declined with the
    /// agent's own "reject", so it goes on with something else.
    fn decline_unanswered(&mut self) {
        let secs = self.spec.unanswered_secs;
        if secs == 0 || self.asked_at.is_empty() {
            return;
        }
        let limit = Duration::from_secs(secs);
        let waited = if secs % 60 == 0 { format!("{} minutes", secs / 60) } else { format!("{secs} s") };
        let late: Vec<u64> = self.asked_at.iter().filter(|(_, at)| at.elapsed() >= limit).map(|(r, _)| *r).collect();
        for request in late {
            self.asked_at.remove(&request);
            let reject = self.thread.entries.iter().find_map(|e| match e {
                Entry::Tool(t) => t.permission.as_ref().filter(|p| p.request == request).and_then(|p| p.options.iter().find(|o| o.kind == "reject_once").or_else(|| p.options.iter().find(|o| !o.allows())).map(|o| o.id.clone())),
                _ => None,
            });
            self.answer(request, reject, Some(format!("Declined: no answer in {waited}")));
        }
    }

    fn checkpoint_name(&self) -> String {
        super::checkpoint::thread_name(self.spec.journal.as_deref())
    }

    /// Entry `ix`'s checkpoint, the files now as a tree, and what differs.
    fn restorable(&self, ix: usize) -> Option<(String, String, Vec<(char, String)>)> {
        let tree = self.thread.checkpoints.get(&ix)?.clone();
        let (now, changed) = restorable(&self.spec.cwd, &self.checkpoint_name(), &tree)?;
        Some((tree, now, changed))
    }

    /// The files back as at entry `ix`, told in the chat with a way back.
    fn restore(&mut self, ix: usize) {
        if self.prompting || matches!(self.thread.status, Status::Working | Status::Asking) {
            return self.notice("The agent is working: stop its turn before putting files back.".into());
        }
        let Some((tree, now, changed)) = self.restorable(ix) else {
            return self.notice("That checkpoint is gone: the files stay as they are.".into());
        };
        if changed.is_empty() {
            return self.notice("Nothing changed since: the files are as they were.".into());
        }
        if let Err(e) = super::checkpoint::restore(&self.spec.cwd, &tree, &changed) {
            // Some may have gone back: the way to where they were stays.
            let at = self.thread.entries.len();
            self.notice(format!("The files could not all be put back: {e}"));
            self.emit(Op::Checkpoint { ix: at, tree: now });
            return;
        }
        let what = match self.thread.entries.get(ix) {
            Some(Entry::User { text }) => format!("before “{}”", text.lines().next().unwrap_or_default().chars().take(60).collect::<String>()),
            _ => "before the files were put back".to_string(),
        };
        let names: Vec<&str> = changed.iter().take(4).map(|(_, p)| p.as_str()).collect();
        let more = changed.len().saturating_sub(names.len());
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        let at = self.thread.entries.len();
        self.notice(format!("Files put back as they were {what}: {}{more}. The agent is not told; say so in your next prompt if it should know.", names.join(", ")));
        self.emit(Op::Checkpoint { ix: at, tree: now });
        for (_, path) in &changed {
            (self.out)(ToWindow::Wrote { path: self.spec.cwd.join(path) });
        }
    }

    fn next_queued(&mut self) {
        // Still starting or asking: the queue waits as it is, in its order.
        let busy = matches!(self.thread.status, Status::Starting | Status::Working | Status::Asking | Status::SignIn);
        if self.thread.queued.is_empty() || self.prompting || busy || self.thread.session.is_none() {
            return;
        }
        let mut queued = self.thread.queued.clone();
        let next = queued.remove(0);
        let (files, images) = self.queued_with.pop_front().unwrap_or_default();
        self.emit(Op::Queued { prompts: queued });
        self.prompt(next, files, images);
    }

    fn cancel(&mut self) {
        let Some(session) = self.thread.session.clone() else { return };
        self.notify("session/cancel", json!({ "sessionId": session }));
        let waiting: Vec<u64> = self.asks.keys().copied().collect();
        for request in waiting {
            self.answer(request, None, Some("Cancelled".into()));
        }
        if !self.thread.queued.is_empty() {
            self.queued_with.clear();
            self.emit(Op::Queued { prompts: Vec::new() });
        }
    }

    /// `option` None declines: with the agent's own "reject", else as cancelled.
    fn answer(&mut self, request: u64, option: Option<String>, why: Option<String>) {
        self.asked_at.remove(&request);
        let Some(rpc) = self.asks.remove(&request) else { return };
        let Some((ix, mut tool)) = self.thread.entries.iter().enumerate().find_map(|(ix, e)| match e {
            Entry::Tool(t) if t.permission.as_ref().is_some_and(|p| p.request == request) => Some((ix, t.clone())),
            _ => None,
        }) else {
            return;
        };
        let options = tool.permission.take().map(|p| p.options).unwrap_or_default();
        let picked = match &option {
            Some(id) => options.iter().find(|o| &o.id == id),
            None if why.is_none() => options.iter().find(|o| o.kind == "reject_once").or_else(|| options.iter().find(|o| !o.allows())),
            None => None,
        };
        let outcome = match picked {
            Some(o) => json!({ "outcome": { "outcome": "selected", "optionId": o.id } }),
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        tool.decided = Some(why.unwrap_or_else(|| picked.map_or("Declined".into(), |o| o.name.clone())));
        self.respond(rpc, outcome);
        self.emit(Op::Set { ix, entry: Entry::Tool(tool) });
        if self.asks.is_empty() && self.thread.status == Status::Asking {
            self.emit(Op::Status { status: Status::Working });
        }
    }

    fn choose(&mut self, id: String, value: String) {
        let Some(session) = self.thread.session.clone() else { return };
        let Some(option) = self.thread.options.iter().find(|o| o.id == id).cloned() else { return };
        let call = Call::Choose { id: id.clone(), value: value.clone() };
        match option.legacy.as_deref() {
            Some("mode") => self.call(call, "session/set_mode", json!({ "sessionId": session, "modeId": value })),
            Some("model") => self.call(call, "session/set_model", json!({ "sessionId": session, "modelId": value })),
            _ => self.call(call, "session/set_config_option", json!({ "sessionId": session, "configId": id, "value": value })),
        };
    }

    // ------------------------------------------------------------- the agent

    fn from_agent(&mut self, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        match (method, msg.get("id").cloned()) {
            (Some(method), Some(id)) => self.agent_request(&method, id, msg.get("params").cloned().unwrap_or(Value::Null)),
            (Some(method), None) => {
                if method == "session/update" {
                    if let Some(update) = msg.pointer("/params/update") {
                        self.update(update);
                    }
                }
            }
            (None, Some(id)) => {
                let Some(call) = id.as_u64().and_then(|id| self.calls.remove(&id)) else { return };
                match msg.get("error") {
                    Some(error) => self.failed(call, error),
                    None => self.answered(call, msg.get("result").cloned().unwrap_or(Value::Null)),
                }
            }
            _ => {}
        }
    }

    fn answered(&mut self, call: Call, result: Value) {
        match call {
            Call::Initialize => {
                self.starting = None;
                self.load_session = result.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
                let images = result.pointer("/agentCapabilities/promptCapabilities/image").and_then(Value::as_bool).unwrap_or(false);
                if images != self.thread.images {
                    self.emit(Op::Images { yes: images });
                }
                self.auth_methods = auth_methods(&result);
                if let Some(info) = result.get("agentInfo") {
                    let name = info.get("title").or_else(|| info.get("name")).and_then(Value::as_str).unwrap_or_default();
                    let version = info.get("version").and_then(Value::as_str).unwrap_or_default();
                    let name = format!("{name} {version}").trim().to_string();
                    if !name.is_empty() {
                        self.emit(Op::Agent { name });
                    }
                }
                self.open_session();
            }
            Call::NewSession | Call::LoadSession => {
                self.replaying = false;
                // A new session where one was to be gone on with: told what it needs.
                let first = match call {
                    Call::NewSession => self.otherwise.take(),
                    _ => {
                        self.otherwise = None;
                        None
                    }
                };
                if let Some(id) = result.get("sessionId").and_then(Value::as_str).map(str::to_string).or_else(|| self.spec.resume.clone()) {
                    if self.thread.session.as_deref() != Some(id.as_str()) {
                        self.emit(Op::Session { id });
                    }
                }
                let options = config_options(&result);
                if !options.is_empty() || !self.thread.options.is_empty() {
                    self.emit(Op::Options { options });
                }
                let status = if self.thread.entries.iter().any(|e| matches!(e, Entry::User { .. })) { Status::Done } else { Status::Idle };
                self.emit(Op::Status { status });
                if !self.thread.auth.is_empty() {
                    self.emit(Op::Auth { methods: Vec::new() });
                }
                for (id, value) in self.spec.options.clone() {
                    let known = self.thread.options.iter().find(|o| o.id == id);
                    if known.is_some_and(|o| o.current != value && o.values.iter().any(|v| v.value == value)) {
                        self.choose(id, value);
                    }
                }
                if let Some(first) = first {
                    self.prompt(first, Vec::new(), Vec::new());
                }
                self.next_queued();
            }
            Call::Prompt => {
                self.prompting = false;
                self.flush();
                if self.thread.status.live() && self.thread.status != Status::SignIn {
                    self.emit(Op::Status { status: Status::Done });
                }
                self.next_queued();
            }
            Call::Choose { id, value } => {
                let options = config_options(&result);
                if options.is_empty() {
                    self.emit(Op::Choose { id, value });
                } else {
                    self.emit(Op::Options { options });
                }
            }
            Call::Authenticate => {
                self.emit(Op::Auth { methods: Vec::new() });
                self.open_session();
            }
        }
    }

    fn failed(&mut self, call: Call, error: &Value) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or_default();
        let message = error.get("message").and_then(Value::as_str).unwrap_or("failed").to_string();
        let message = match error.pointer("/data/message").or_else(|| error.get("data")).and_then(Value::as_str) {
            Some(more) if !message.contains(more) => format!("{message}: {more}"),
            _ => message,
        };
        match call {
            Call::LoadSession => {
                // The agent no longer has it: a new one, the thread kept for reading.
                self.replaying = false;
                self.notice(format!("The agent could not continue its session ({message}); a new one was started."));
                self.new_session();
            }
            _ if code == AUTH_REQUIRED => {
                self.prompting = false;
                self.notice(message);
                self.emit(Op::Auth { methods: self.auth_methods.clone() });
                self.emit(Op::Status { status: Status::SignIn });
            }
            Call::Prompt => {
                self.prompting = false;
                self.notice(message);
                self.emit(Op::Status { status: Status::Done });
                self.next_queued();
            }
            Call::Initialize | Call::NewSession => {
                self.starting = None;
                self.notice(message);
                self.emit(Op::Status { status: Status::Exited(None) });
            }
            Call::Authenticate | Call::Choose { .. } => self.notice(message),
        }
    }

    fn open_session(&mut self) {
        // A session never prompted is kept by no agent: there is nothing to
        // load. One asked for by id (another agent's, carried on) may be.
        let prompted = self.thread.entries.iter().any(|e| matches!(e, Entry::User { .. }));
        match self.spec.resume.clone().or_else(|| self.thread.session.clone()) {
            Some(id) if self.load_session && (prompted || self.spec.resume.is_some()) => {
                // Its replay is the journal's again, unless this window has none.
                self.replaying = !self.thread.entries.is_empty();
                let servers = self.mcp_servers();
                self.call(Call::LoadSession, "session/load", json!({ "sessionId": id, "cwd": self.spec.cwd, "mcpServers": servers }));
            }
            _ => self.new_session(),
        }
    }

    fn new_session(&mut self) {
        let servers = self.mcp_servers();
        self.call(Call::NewSession, "session/new", json!({ "cwd": self.spec.cwd, "mcpServers": servers }));
    }

    /// ideae's own server, on stdio, which every ACP agent takes.
    fn mcp_servers(&self) -> Value {
        match &self.spec.mcp {
            Some((command, args, env)) => json!([{
                "name": "ideae",
                "command": command,
                "args": args,
                "env": env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
            }]),
            None => json!([]),
        }
    }

    fn agent_request(&mut self, method: &str, rpc: Value, params: Value) {
        match method {
            "session/request_permission" => {
                let options: Vec<PermissionOption> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            id: o.get("optionId")?.as_str()?.to_string(),
                            name: o.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                            kind: o.get("kind").and_then(Value::as_str).unwrap_or_default().to_string(),
                        })
                    })
                    .collect();
                let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
                let ix = self.tool_update(&call, true);
                self.next_request += 1;
                let request = self.next_request;
                self.asks.insert(request, rpc);
                if let Some(Entry::Tool(mut tool)) = self.thread.entries.get(ix).cloned() {
                    tool.permission = Some(Permission { request, options: options.clone() });
                    self.emit(Op::Set { ix, entry: Entry::Tool(tool) });
                }
                let yes = options.iter().find(|o| o.kind == "allow_once").or_else(|| options.iter().find(|o| o.allows())).map(|o| o.id.clone());
                let decided = match super::policy::Policy::parse(&self.thread.policy) {
                    _ if self.spec.unasked => Some("Allowed: this variant works unasked".to_string()),
                    super::policy::Policy::All => Some("Allowed: Allow all".to_string()),
                    super::policy::Policy::Rules => match self.thread.entries.get(ix) {
                        Some(Entry::Tool(tool)) => super::policy::decide(&self.spec.cwd, tool, &call).map(|why| format!("Allowed by ideae: {why}")),
                        _ => None,
                    },
                    super::policy::Policy::Ask => None,
                };
                if let (Some(why), Some(_)) = (&decided, &yes) {
                    return self.answer(request, yes, Some(why.clone()));
                }
                self.asked_at.insert(request, Instant::now());
                self.emit(Op::Status { status: Status::Asking });
                // Nobody at a window: the desktop is told, or it waits unseen.
                if !self.window_here {
                    let what = match self.thread.entries.get(ix) {
                        Some(Entry::Tool(t)) if !t.title.is_empty() => t.title.clone(),
                        Some(Entry::Tool(t)) => t.kind.clone(),
                        _ => "a tool".into(),
                    };
                    self.notify_desktop(&format!("Allow: {what}?"));
                }
            }
            "fs/read_text_file" => {
                let path = PathBuf::from(params.get("path").and_then(Value::as_str).unwrap_or_default());
                let line = params.get("line").and_then(Value::as_u64).map(|n| n as usize);
                let limit = params.get("limit").and_then(Value::as_u64).map(|n| n as usize);
                self.next_request += 1;
                let request = self.next_request;
                (self.out)(ToWindow::Read { request, path: path.clone() });
                self.reads.insert(request, PendingRead { rpc, path, line, limit, until: Instant::now() + BUFFER_WAIT });
            }
            "fs/write_text_file" => {
                let path = PathBuf::from(params.get("path").and_then(Value::as_str).unwrap_or_default());
                let content = params.get("content").and_then(Value::as_str).unwrap_or_default();
                let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, content));
                match written {
                    Ok(()) => {
                        self.respond(rpc, Value::Null);
                        (self.out)(ToWindow::Wrote { path });
                    }
                    Err(e) => self.respond_error(rpc, -32603, &e.to_string()),
                }
            }
            m if m.starts_with("terminal/") => self.terminal_request(m, rpc, &params),
            _ => self.respond_error(rpc, -32601, &format!("{method} is not supported")),
        }
    }

    fn terminal_request(&mut self, method: &str, rpc: Value, params: &Value) {
        let id = params.get("terminalId").and_then(Value::as_str).unwrap_or_default().to_string();
        let exit_status = |exit: &Option<(Option<i32>, Option<String>)>| exit.as_ref().map(|(code, signal)| json!({ "exitCode": code, "signal": signal }));
        match method {
            "terminal/create" => {
                let Some(tx) = self.tx.clone() else { return self.respond_error(rpc, -32603, "no host loop") };
                let command = params.get("command").and_then(Value::as_str).unwrap_or_default();
                let args = strings(params.get("args"));
                let mut env = crate::agents::run_made(self.spec.env.clone());
                env.extend(params.get("env").and_then(Value::as_array).into_iter().flatten().filter_map(|v| {
                    Some((v.get("name")?.as_str()?.to_string(), v.get("value")?.as_str()?.to_string()))
                }));
                let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(|| self.spec.cwd.clone());
                let limit = params.get("outputByteLimit").and_then(Value::as_u64).map(|n| n as usize);
                self.next_terminal += 1;
                let id = format!("t{}", self.next_terminal);
                match super::terminal::Term::start(id.clone(), command, &args, &env, &cwd, limit, tx) {
                    Ok(term) => {
                        self.terminals.insert(id.clone(), term);
                        self.respond(rpc, json!({ "terminalId": id }));
                    }
                    Err(e) => self.respond_error(rpc, -32603, &format!("{command}: {e}")),
                }
            }
            "terminal/output" => match self.terminals.get(&id) {
                Some(term) => {
                    let (output, truncated) = term.output();
                    let status = exit_status(&term.exit);
                    self.respond(rpc, json!({ "output": output, "truncated": truncated, "exitStatus": status }));
                }
                None => self.respond_error(rpc, -32602, "no such terminal"),
            },
            "terminal/wait_for_exit" => match self.terminals.get_mut(&id) {
                Some(term) => match &term.exit {
                    Some((code, signal)) => {
                        let result = json!({ "exitCode": code, "signal": signal });
                        self.respond(rpc, result);
                    }
                    None => term.waiting.push(rpc),
                },
                None => self.respond_error(rpc, -32602, "no such terminal"),
            },
            "terminal/kill" => {
                if let Some(term) = self.terminals.get(&id) {
                    term.kill();
                }
                self.respond(rpc, Value::Null);
            }
            "terminal/release" => {
                // Its last output first, onto its card for good.
                self.show_terminals();
                self.shown_at.remove(&id);
                if let Some(term) = self.terminals.remove(&id) {
                    term.kill();
                }
                self.respond(rpc, Value::Null);
            }
            _ => self.respond_error(rpc, -32601, &format!("{method} is not supported")),
        }
    }

    fn term_exited(&mut self, id: String, code: Option<i32>, signal: Option<String>) {
        let Some(term) = self.terminals.get_mut(&id) else { return };
        term.exit = Some((code, signal.clone()));
        let waiting = std::mem::take(&mut term.waiting);
        for rpc in waiting {
            self.respond(rpc, json!({ "exitCode": code, "signal": signal }));
        }
        // Its last output, whatever the agent does next.
        self.show_terminals();
    }

    /// What the commands printed since, onto the cards of their tool calls.
    /// Live, it goes to the window alone; its last output, once it ended,
    /// to the journal too (written there at every look, a long build's
    /// output filled it).
    fn show_terminals(&mut self) {
        let mut changed: Vec<(usize, String, bool)> = Vec::new();
        for (id, term) in self.terminals.iter_mut() {
            let Some(&ix) = self.shown_at.get(id) else { continue };
            if term.shown == usize::MAX {
                continue;
            }
            let written = term.written();
            if written != term.shown || term.exit.is_some() {
                let last = term.exit.is_some();
                term.shown = if last { usize::MAX } else { written };
                changed.push((ix, term.tail(OUTPUT_KEPT), last));
            }
        }
        for (ix, output, last) in changed {
            if let Some(Entry::Tool(tool)) = self.thread.entries.get(ix) {
                if tool.output != output {
                    let mut tool = tool.clone();
                    tool.output = output;
                    let op = Op::Set { ix, entry: Entry::Tool(tool) };
                    if last {
                        self.emit(op);
                    } else {
                        self.flush();
                        self.thread.apply(op.clone());
                        (self.out)(ToWindow::Op { op });
                    }
                }
            }
        }
    }

    fn finish_read(&mut self, read: PendingRead, buffer: Option<String>) {
        let text = match buffer {
            Some(text) => Ok(text),
            None => std::fs::read_to_string(&read.path),
        };
        match text {
            Ok(text) => {
                let text = match (read.line, read.limit) {
                    (None, None) => text,
                    (line, limit) => {
                        let from = line.unwrap_or(1).saturating_sub(1);
                        text.split_inclusive('\n').skip(from).take(limit.unwrap_or(usize::MAX)).collect()
                    }
                };
                self.respond(read.rpc, json!({ "content": text }));
            }
            Err(e) => self.respond_error(read.rpc, -32002, &format!("{}: {e}", read.path.display())),
        }
    }

    fn expire_reads(&mut self) {
        let now = Instant::now();
        let late: Vec<u64> = self.reads.iter().filter(|(_, r)| r.until <= now).map(|(id, _)| *id).collect();
        for id in late {
            if let Some(read) = self.reads.remove(&id) {
                self.finish_read(read, None);
            }
        }
    }

    fn update(&mut self, update: &Value) {
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();
        // A loaded session's conversation, told again: the journal has it.
        // What it offers now (commands, choices, usage) is still news.
        let told_again = matches!(kind, "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" | "tool_call" | "tool_call_update" | "plan");
        if told_again && self.replaying {
            return;
        }
        let text = || update.get("content").map(block_text).unwrap_or_default();
        match kind {
            "agent_message_chunk" => self.stream(text(), |t| Entry::Agent { text: t }),
            "agent_thought_chunk" => self.stream(text(), |t| Entry::Thought { text: t }),
            "user_message_chunk" => self.stream(text(), |t| Entry::User { text: t }),
            "tool_call" | "tool_call_update" => {
                self.tool_update(update, false);
            }
            "plan" => {
                let items = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|e| PlanItem {
                        text: e.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
                        status: e.get("status").and_then(Value::as_str).unwrap_or("pending").to_string(),
                    })
                    .collect();
                let last_prompt = self.thread.entries.iter().rposition(|e| matches!(e, Entry::User { .. }));
                let plan = self.thread.entries.iter().rposition(|e| matches!(e, Entry::Plan { .. })).filter(|&ix| last_prompt.is_none_or(|p| ix > p));
                match plan {
                    Some(ix) => self.emit(Op::Set { ix, entry: Entry::Plan { items } }),
                    None => self.emit(Op::Push { entry: Entry::Plan { items } }),
                }
            }
            "available_commands_update" => {
                let commands = update
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        Some(SlashCommand {
                            name: c.get("name")?.as_str()?.to_string(),
                            description: c.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                        })
                    })
                    .collect();
                self.emit(Op::Commands { commands });
            }
            "current_mode_update" => {
                let value = update.get("currentModeId").and_then(Value::as_str).unwrap_or_default().to_string();
                if let Some(option) = self.thread.option("mode") {
                    let id = option.id.clone();
                    self.emit(Op::Choose { id, value });
                }
            }
            "config_option_update" => {
                let options = config_options(update);
                if !options.is_empty() {
                    self.emit(Op::Options { options });
                }
            }
            "usage_update" => {
                let cost = update.get("cost").and_then(|c| {
                    let amount = c.get("amount")?.as_f64()?;
                    let currency = c.get("currency").and_then(Value::as_str).unwrap_or("USD");
                    Some(if currency == "USD" { format!("${amount:.2}") } else { format!("{amount:.2} {currency}") })
                });
                let usage = Usage {
                    used: update.get("used").and_then(Value::as_u64).unwrap_or_default(),
                    size: update.get("size").and_then(Value::as_u64).unwrap_or_default(),
                    cost,
                };
                let spent = usage.cost.as_deref().and_then(super::spend::usd).map(|c| c - self.cost_base);
                if self.thread.usage.as_ref() != Some(&usage) {
                    self.emit(Op::Usage { usage });
                }
                // Its budget spent: the turn stops here.
                if let (Some(spent), Some(budget), false) = (spent, self.spec.budget, self.over_budget) {
                    if spent >= budget {
                        self.over_budget = true;
                        self.cancel();
                        self.notice(format!("Stopped: this agent's daily limit is reached (${spent:.2} in this run, ${budget:.2} were left today). Raise it in its profile (Settings → Agents) and restart it to go on."));
                    }
                }
            }
            "session_info_update" => {
                if let Some(title) = update.get("title").and_then(Value::as_str) {
                    self.emit(Op::Title { title: title.to_string() });
                }
            }
            _ => {}
        }
    }

    /// Streamed text: onto the entry being written, else a new one.
    fn stream(&mut self, text: String, make: impl Fn(String) -> Entry) {
        if text.is_empty() {
            return;
        }
        let last = self.thread.entries.len().checked_sub(1);
        let same_kind = last.and_then(|ix| self.thread.entries.get(ix)).is_some_and(|e| std::mem::discriminant(e) == std::mem::discriminant(&make(String::new())));
        match (last, same_kind) {
            (Some(ix), true) => match &mut self.streaming {
                Some((at, pending)) if *at == ix => pending.push_str(&text),
                _ => {
                    self.flush();
                    self.streaming = Some((ix, text));
                }
            },
            _ => self.emit(Op::Push { entry: make(text) }),
        }
    }

    /// A tool call, new or updated, from what the agent sent of it.
    fn tool_update(&mut self, call: &Value, asked: bool) -> usize {
        let id = call.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
        let (ix, mut tool) = match self.tools.get(&id).and_then(|&ix| match self.thread.entries.get(ix) {
            Some(Entry::Tool(tool)) => Some((ix, tool.clone())),
            _ => None,
        }) {
            Some(found) => found,
            None => (usize::MAX, Tool { id: id.clone(), status: "pending".into(), ..Default::default() }),
        };
        if let Some(title) = call.get("title").and_then(Value::as_str) {
            tool.title = title.to_string();
        }
        if let Some(kind) = call.get("kind").and_then(Value::as_str) {
            tool.kind = kind.to_string();
        }
        if let Some(status) = call.get("status").and_then(Value::as_str) {
            tool.status = status.to_string();
        }
        if let Some(locations) = call.get("locations").and_then(Value::as_array).filter(|l| !l.is_empty()) {
            tool.locations = locations
                .iter()
                .filter_map(|l| Some(Location { path: l.get("path")?.as_str()?.to_string(), line: l.get("line").and_then(Value::as_u64).map(|n| n as u32) }))
                .collect();
        }
        if let Some(content) = call.get("content").and_then(Value::as_array).filter(|c| !c.is_empty()) {
            let mut output = String::new();
            let mut diffs = Vec::new();
            for item in content {
                match item.get("type").and_then(Value::as_str) {
                    Some("diff") => diffs.push(Diff {
                        path: item.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                        old: item.get("oldText").and_then(Value::as_str).map(str::to_string),
                        new: item.get("newText").and_then(Value::as_str).unwrap_or_default().to_string(),
                    }),
                    Some("terminal") => {
                        tool.terminal = item.get("terminalId").and_then(Value::as_str).map(str::to_string);
                        if let (Some(term), true) = (&tool.terminal, ix != usize::MAX) {
                            self.shown_at.insert(term.clone(), ix);
                        }
                    }
                    Some("content") => {
                        let text = item.get("content").map(block_text).unwrap_or_default();
                        if !text.is_empty() {
                            if !output.is_empty() {
                                output.push('\n');
                            }
                            output.push_str(&text);
                        }
                    }
                    _ => {}
                }
            }
            if !diffs.is_empty() {
                tool.diffs = diffs;
            }
            if !output.is_empty() {
                tool.output = tail(&output, OUTPUT_KEPT);
            }
        }
        if !asked && matches!(tool.status.as_str(), "completed" | "failed") && tool.permission.is_some() {
            tool.permission = None;
        }
        if ix == usize::MAX {
            let ix = self.thread.entries.len();
            if let Some(term) = &tool.terminal {
                self.shown_at.insert(term.clone(), ix);
            }
            self.tools.insert(id, ix);
            self.emit(Op::Push { entry: Entry::Tool(tool) });
            ix
        } else {
            self.emit(Op::Set { ix, entry: Entry::Tool(tool) });
            ix
        }
    }

    fn agent_gone(&mut self) {
        let mid_turn = std::mem::take(&mut self.prompting);
        self.starting = None;
        let Some(mut agent) = self.agent.take() else { return };
        #[cfg_attr(not(unix), allow(unused_mut))]
        let (mut code, mut signal): (Option<Option<i32>>, Option<i32>) = (None, None);
        for _ in 0..20 {
            if let Ok(Some(status)) = agent.child.try_wait() {
                code = Some(status.code());
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    signal = status.signal();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if code.is_none() {
            // It closed its output and runs on: of no use any more.
            end(&mut agent.child);
        }
        // Its error stream may hold the reason a moment after.
        std::thread::sleep(Duration::from_millis(50));
        let (said, broken_npx) = {
            let stderr = agent.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (last_lines(&stderr), half_installed_npx(&String::from_utf8_lossy(&stderr.iter().copied().collect::<Vec<u8>>())))
        };
        // A command of its own that ended processes: likely what ended it
        // (an agent cleaning up may take its own adapter for a stray one).
        let own_kill = self.thread.entries.iter().rev().take(8).find_map(|e| match e {
            Entry::Tool(t) if t.kind == "execute" && !matches!(t.status.as_str(), "completed") && ends_processes(&t.title) => Some(t.title.chars().take(80).collect::<String>()),
            _ => None,
        });
        // What it said last, when it reads as an error: an agent's own
        // debug lines say nothing of why it ended.
        let error_said = said.clone().filter(|s| s.to_lowercase().contains("error") || s.contains("panic") || s.contains("Traceback"));
        let words = match (code.flatten(), signal) {
            (Some(0), _) => None,
            // The shell that started it found no such program.
            (Some(127), _) => Some(missing_program(&self.spec.command)),
            (Some(code), _) => Some(error_said.or(said).unwrap_or_else(|| format!("exit code {code}"))),
            (None, Some(signal)) => Some(match &own_kill {
                Some(command) => format!("its process was ended (signal {signal}), it seems by its own command `{command}`"),
                None => format!("its process was ended by signal {signal}, not by the app"),
            }),
            (None, None) => said,
        };
        let abnormal = signal.is_some() || code.flatten().is_some_and(|c| c != 0);
        let asked: Vec<u64> = self.asks.keys().copied().collect();
        for request in asked {
            self.asks.remove(&request);
        }
        let stuck: Vec<usize> = (0..self.thread.entries.len())
            .filter(|&ix| matches!(&self.thread.entries[ix], Entry::Tool(t) if t.permission.is_some() || matches!(t.status.as_str(), "pending" | "in_progress")))
            .collect();
        for ix in stuck {
            if let Some(Entry::Tool(mut tool)) = self.thread.entries.get(ix).cloned() {
                let asked = tool.permission.take().is_some();
                if asked {
                    tool.decided = Some("The agent ended".into());
                }
                let running = matches!(tool.status.as_str(), "pending" | "in_progress");
                if running {
                    tool.status = "failed".into();
                }
                if asked || running {
                    self.emit(Op::Set { ix, entry: Entry::Tool(tool) });
                }
            }
        }
        self.calls.clear();
        self.reads.clear();
        for term in self.terminals.values() {
            term.kill();
        }
        // npx's copy of it cut short at an earlier start (npm never mends
        // that): cleared, and started again, once.
        if let (Some(dir), Some(tx), false) = (broken_npx, self.tx.clone(), self.npx_mended) {
            self.npx_mended = true;
            if std::fs::remove_dir_all(&dir).is_ok() {
                self.notice("npx kept a copy of this agent whose download was cut short at an earlier start, which npm does not mend: it was cleared, and the agent is downloaded and started again.".into());
                self.emit(Op::Status { status: Status::Starting });
                return self.start(tx);
            }
        }
        // Ended in the middle of its work, and not a moment ago: once more,
        // going on with its session.
        // Or ended by something it did not choose (a signal, a failure), its
        // turn closed in time or not.
        let again = (mid_turn || abnormal) && self.thread.session.is_some() && self.restarted.is_none_or(|at| at.elapsed() > RESTART_EVERY);
        if let (true, Some(tx)) = (again, self.tx.clone()) {
            self.restarted = Some(Instant::now());
            let why = words.map(|w| format!(" ({})", w.lines().last().unwrap_or_default())).unwrap_or_default();
            let when = if mid_turn { "in the middle of its turn" } else { "unexpectedly" };
            self.notice(format!("The agent stopped {when}{why}; it was started again and goes on with its session. Its last turn may need to be asked again."));
            self.emit(Op::Status { status: Status::Starting });
            return self.start(tx);
        }
        self.emit(Op::Status { status: Status::Exited(words) });
    }

    // -------------------------------------------------------------- plumbing

    fn call(&mut self, call: Call, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.calls.insert(id, call);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_error(&mut self, id: Value, code: i64, message: &str) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn send(&mut self, msg: Value) {
        let Some(agent) = &mut self.agent else { return };
        let mut line = serde_json::to_vec(&msg).unwrap_or_default();
        line.push(b'\n');
        if agent.stdin.write_all(&line).and_then(|_| agent.stdin.flush()).is_err() {
            // It is gone; its end is told when its output closes.
        }
    }

    fn notice(&mut self, text: String) {
        self.emit(Op::Push { entry: Entry::Notice { text } });
    }

    /// Applies a change, keeps it in the journal and tells the window.
    fn emit(&mut self, op: Op) {
        self.flush();
        self.record(op);
    }

    fn record(&mut self, op: Op) {
        if let Some(journal) = &mut self.journal {
            let line = wire::line(&op);
            if journal.write_all(&line).is_ok() {
                self.journal_bytes += line.len() as u64;
            }
        }
        self.thread.apply(op.clone());
        (self.out)(ToWindow::Op { op });
    }

    /// Streamed text that waited, as one change.
    fn flush(&mut self) {
        if let Some((ix, text)) = self.streaming.take() {
            self.record(Op::Append { ix, text });
        }
    }

    /// A journal that grew large (a command's output shown live is written
    /// again with every change) is written anew as one snapshot.
    fn compact_journal(&mut self) {
        if self.journal_bytes < JOURNAL_MOST || self.journal_bytes < self.snapshot_bytes * 4 || self.streaming.is_some() {
            return;
        }
        let Some(path) = self.spec.journal.clone() else { return };
        // Closed first: Windows does not replace a file that is open.
        self.journal = None;
        match write_snapshot(&path, &self.thread) {
            Some((file, bytes)) => {
                self.journal = Some(file);
                self.journal_bytes = bytes;
                self.snapshot_bytes = bytes;
            }
            None => {
                self.journal = std::fs::OpenOptions::new().append(true).open(&path).ok();
                self.snapshot_bytes = self.journal_bytes;
            }
        }
    }

    fn reindex_tools(&mut self) {
        self.tools = self
            .thread
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, e)| match e {
                Entry::Tool(tool) => Some((tool.id.clone(), ix)),
                _ => None,
            })
            .collect();
    }

    /// The agent and the commands it runs, ended together: asked, then
    /// killed, before this host goes (a thread left to kill later goes
    /// with it).
    fn stop_agent(&mut self) {
        // Its input closed: some agents end on that alone.
        let mut agent = self.agent.take().map(|a| {
            drop(a.stdin);
            a.child
        });
        #[cfg(unix)]
        {
            let groups: Vec<i32> = self.terminals.values().map(|t| t.group() as i32).chain(agent.as_ref().map(|c| c.id() as i32)).collect();
            for &group in &groups {
                // SAFETY: the agent's and its commands' own process groups.
                unsafe { libc::kill(-group, libc::SIGTERM) };
            }
            let until = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < until {
                let agent_done = agent.as_mut().is_none_or(|c| c.try_wait().ok().flatten().is_some());
                if agent_done && self.terminals.values().all(|t| t.ended()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            for &group in &groups {
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            if let Some(child) = &mut agent {
                let _ = child.wait();
            }
        }
        #[cfg(not(unix))]
        {
            for term in self.terminals.values() {
                term.kill();
            }
            if let Some(child) = &mut agent {
                end(child);
            }
        }
    }
}

fn spawn_agent(spec: &HostSpec, tx: mpsc::Sender<Event>) -> std::io::Result<Agent> {
    let env = crate::agents::run_made(spec.env.clone());
    let mut command = if cfg!(windows) {
        // Quoted words whole: a program under `C:\Users\Jane Doe\…`.
        let words = command_words(&spec.command);
        let mut command = crate::procs::command(words.first().map(String::as_str).unwrap_or_default());
        command.args(&words[1.min(words.len())..]);
        command
    } else {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(format!("exec {}", spec.command));
        command
    };
    command
        .current_dir(&spec.cwd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env_remove(crate::agent::SOCK_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own group, ended whole: the adapter and the agent it runs.
        command.process_group(0);
    }
    let mut child = crate::procs::spawn_tied(&mut command)?;
    let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
    let stderr_pipe = child.stderr.take();
    let stderr: Arc<Mutex<VecDeque<u8>>> = Arc::default();
    let log_path = spec.journal.as_deref().map(log_path);
    // Its program alone: the rest of its line may hold what is private.
    let program = spec.command.split_whitespace().find(|w| !w.contains('=')).unwrap_or_default().to_string();
    if let Some(mut pipe) = stderr_pipe {
        let kept = stderr.clone();
        std::thread::spawn(move || {
            // What it says on its error stream, for when it goes wrong: in a
            // file of the user's alone, one older part kept past LOG_MOST.
            let mut log = log_path.as_deref().and_then(|p| {
                let mut options = std::fs::OpenOptions::new();
                options.create(true).append(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                options.open(p).ok()
            });
            if let Some(log) = &mut log {
                let _ = writeln!(log, "--- started at {} (unix time): {program}", chrono_now());
            }
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let (Some(file), Some(path)) = (&mut log, log_path.as_deref()) {
                    let _ = file.write_all(&buf[..n]);
                    if file.metadata().is_ok_and(|m| m.len() > LOG_MOST) {
                        let _ = std::fs::rename(path, path.with_extension("log.old"));
                        log = private_file(path).ok();
                    }
                }
                let mut kept = kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                kept.extend(&buf[..n]);
                let excess = kept.len().saturating_sub(STDERR_KEPT);
                kept.drain(..excess);
            }
        });
    }
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            let Ok(line) = line else { break };
            if let Some(msg) = wire::parse::<Value>(&line) {
                if tx.send(Event::Agent(msg)).is_err() {
                    return;
                }
            }
        }
        let _ = tx.send(Event::AgentGone);
    });
    Ok(Agent { child, stdin, stderr })
}

fn end(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: signals the agent's own process group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        for _ in 0..20 {
            if child.try_wait().ok().flatten().is_some() {
                // What it started may take a moment longer.
                std::thread::sleep(Duration::from_millis(100));
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = child.wait();
    }
    // The tree: npx.cmd's node and what the adapter started, not the wrapper alone.
    #[cfg(not(unix))]
    {
        let _ = crate::procs::command("taskkill").args(["/PID", &child.id().to_string(), "/T", "/F"]).status();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A command line's words, double quotes keeping spaces in one.
fn command_words(line: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

fn last_lines(bytes: &VecDeque<u8>) -> Option<String> {
    let text = String::from_utf8_lossy(&bytes.iter().copied().collect::<Vec<u8>>()).into_owned();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(3);
    Some(lines[start..].join("\n")).filter(|s| !s.is_empty())
}

/// The files now as a tree and what differs from `tree`; None when git no
/// longer has it (its ref gone with an archive).
fn restorable(cwd: &Path, name: &str, tree: &str) -> Option<(String, Vec<(char, String)>)> {
    let exists = crate::procs::command("git").current_dir(cwd).args(["cat-file", "-e", tree]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !exists {
        return None;
    }
    let now = super::checkpoint::take(cwd, name)?;
    let changed = super::checkpoint::changes(cwd, tree, &now);
    Some((now, changed))
}

/// The agent's log, next to its journal.
pub fn log_path(journal: &Path) -> PathBuf {
    journal.with_extension("log")
}

/// Seconds since the epoch, as the log's marks.
fn chrono_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A new file only the user reads.
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    // Made before with another mode: made the user's now.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// A command line that ends processes.
fn ends_processes(command: &str) -> bool {
    command.split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|').any(|w| matches!(w, "kill" | "pkill" | "killall" | "taskkill"))
}

/// The folder of npx's cache an agent's start failed on, when it is a
/// download cut short: `_npx/<hash>` with packages but no package.json,
/// which npm reports as ENOENT at every start after and never mends.
fn half_installed_npx(stderr: &str) -> Option<PathBuf> {
    // "… ENOENT: no such file or directory, open '<cache>/_npx/<hash>/package.json'"
    let at = stderr.find("ENOENT")?;
    let line = stderr[at..].lines().find(|l| l.contains("_npx") && l.contains("package.json") && l.contains("open '"))?;
    let path = line.split("open '").nth(1)?.split('\'').next()?;
    let dir = Path::new(path).parent()?.to_path_buf();
    // Only that: a folder of npx's own cache, without what would make it whole.
    let in_npx = dir.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "_npx");
    (in_npx && dir.is_dir() && !dir.join("package.json").exists()).then_some(dir)
}

/// What a missing program needs, said for the programs agents are run by.
fn missing_program(command: &str) -> String {
    let program = command.split_whitespace().find(|w| !w.contains('=')).unwrap_or(command);
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    match name {
        "npx" | "node" | "npm" => format!("`{name}` was not found: this agent needs Node.js (nodejs.org)"),
        "uvx" | "uv" => format!("`{name}` was not found: this agent needs uv (docs.astral.sh/uv)"),
        _ => format!("`{name}` was not found: is it installed, and on PATH?"),
    }
}

/// The thread kept from earlier runs, the journal open to go on with, and
/// its size. It is written anew as one snapshot, so it does not grow
/// without end.
fn open_journal(path: Option<&Path>) -> (Thread, Option<std::fs::File>, u64) {
    let Some(path) = path else { return (Thread::default(), None, 0) };
    let mut thread = Thread::default();
    if let Ok(text) = std::fs::read(path) {
        let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
        let mut unread = 0;
        for (n, line) in lines.iter().enumerate() {
            match wire::parse::<Op>(line) {
                Some(op) => thread.apply(op),
                // The last line cut short by a crash is expected; others are not.
                None if n + 1 < lines.len() || n == 0 => unread += 1,
                None => {}
            }
        }
        // Not read whole (another build's format, a bug): kept aside, not
        // written over by what little was read.
        if unread > 0 {
            let aside = path.with_extension(format!("jsonl.unread-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())));
            if let Ok(mut copy) = private_file(&aside) {
                let _ = copy.write_all(&text);
            }
            thread.entries.push(Entry::Notice { text: format!("Part of the conversation kept here could not be read; it is kept as {}.", aside.display()) });
        }
    }
    // Stale from the run before: nothing is asked or running now.
    thread.status = Status::Starting;
    thread.queued.clear();
    thread.auth.clear();
    for entry in &mut thread.entries {
        if let Entry::Tool(tool) = entry {
            tool.permission = None;
            if matches!(tool.status.as_str(), "pending" | "in_progress") {
                tool.status = "failed".into();
            }
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        // Conversations, command output and all: the user's alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let (file, bytes) = write_snapshot(path, &thread).map_or((None, 0), |(f, b)| (Some(f), b));
    (thread, file, bytes)
}

/// The journal as one snapshot, put in place whole: the one before stays
/// until the new one is on disk. The file is left open to append to.
fn write_snapshot(path: &Path, thread: &Thread) -> Option<(std::fs::File, u64)> {
    // As `Op::Reset`, written from the thread as it is: no copy of it.
    #[derive(serde::Serialize)]
    #[serde(tag = "op")]
    enum Snapshot<'a> {
        Reset { thread: &'a Thread },
    }
    let line = wire::line(&Snapshot::Reset { thread });
    let new = path.with_extension("jsonl.new");
    let mut file = private_file(&new).ok()?;
    file.write_all(&line).ok()?;
    file.sync_data().ok()?;
    std::fs::rename(&new, path).ok()?;
    let file = std::fs::OpenOptions::new().append(true).open(path).ok()?;
    Some((file, line.len() as u64))
}

fn auth_methods(init: &Value) -> Vec<AuthMethod> {
    init.get("authMethods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let terminal = match m.get("type").and_then(Value::as_str) {
                Some("terminal") => Some(strings(m.get("args"))),
                _ => m.pointer("/_meta/terminal-auth").map(|t| strings(t.get("args"))),
            };
            Some(AuthMethod {
                id: m.get("id")?.as_str()?.to_string(),
                name: m.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                description: m.get("description").and_then(Value::as_str).map(str::to_string),
                terminal,
            })
        })
        .collect()
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// The choices a session offers: its config options, or an older agent's
/// modes and models made into two.
fn config_options(result: &Value) -> Vec<ConfigOption> {
    if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
        return options
            .iter()
            .filter_map(|o| {
                let mut values = Vec::new();
                for v in o.get("options").and_then(Value::as_array).into_iter().flatten() {
                    // Grouped: { group, name, options: [...] }.
                    match v.get("options").and_then(Value::as_array) {
                        Some(group) => values.extend(group.iter().filter_map(option_value)),
                        None => values.extend(option_value(v)),
                    }
                }
                let current = match o.get("currentValue") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return None,
                };
                Some(ConfigOption {
                    id: o.get("id")?.as_str()?.to_string(),
                    name: o.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    category: o.get("category").and_then(Value::as_str).unwrap_or_default().to_string(),
                    current,
                    values,
                    legacy: None,
                })
            })
            .collect();
    }
    let mut options = Vec::new();
    if let Some(modes) = result.get("modes") {
        options.push(ConfigOption {
            id: "mode".into(),
            name: "Mode".into(),
            category: "mode".into(),
            current: modes.get("currentModeId").and_then(Value::as_str).unwrap_or_default().to_string(),
            values: modes
                .get("availableModes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    Some(OptionValue {
                        value: m.get("id")?.as_str()?.to_string(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect(),
            legacy: Some("mode".into()),
        });
    }
    if let Some(models) = result.get("models") {
        options.push(ConfigOption {
            id: "model".into(),
            name: "Model".into(),
            category: "model".into(),
            current: models.get("currentModelId").and_then(Value::as_str).unwrap_or_default().to_string(),
            values: models
                .get("availableModels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    Some(OptionValue {
                        value: m.get("modelId")?.as_str()?.to_string(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect(),
            legacy: Some("model".into()),
        });
    }
    options
}

fn option_value(v: &Value) -> Option<OptionValue> {
    Some(OptionValue {
        value: match v.get("value")? {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
        name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        description: v.get("description").and_then(Value::as_str).map(str::to_string),
    })
}

/// A content block as text: what an image or a resource is, by its name.
fn block_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        Some("image") => "[image]".into(),
        Some("resource_link") => block.get("name").or_else(|| block.get("uri")).and_then(Value::as_str).unwrap_or_default().to_string(),
        Some("resource") => block
            .pointer("/resource/text")
            .or_else(|| block.pointer("/resource/uri"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn tail(text: &str, keep: usize) -> String {
    if text.len() <= keep {
        return text.to_string();
    }
    let mut from = text.len() - keep;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    format!("…{}", &text[from..])
}

// ------------------------------------------------------------------ entries

/// `ideae --acp-host`: the spec in [`SPEC_ENV`], the window on standard
/// input and output (through the daemon).
pub fn run_process() {
    // The spec: the first line of input (as the daemon starts it), or
    // the variable (tests).
    let mut stdin = std::io::stdin().lock();
    let from_env = std::env::var(SPEC_ENV).ok();
    let spec: Option<HostSpec> = match from_env {
        Some(text) => serde_json::from_str(&text).ok(),
        None => {
            let mut first = String::new();
            let _ = stdin.read_line(&mut first);
            serde_json::from_str(first.trim()).ok()
        }
    };
    let Some(spec) = spec else {
        eprintln!("ideae --acp-host: no spec on its first line or in {SPEC_ENV}");
        std::process::exit(2);
    };
    // SAFETY: before any thread is started.
    unsafe { std::env::remove_var(SPEC_ENV) };
    drop(stdin);
    // A line of its own for the first message, whatever was printed before.
    println!();
    let (tx, rx) = mpsc::channel();
    #[cfg(unix)]
    on_termination(tx.clone());
    let input = tx.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().split(b'\n') {
            let Ok(line) = line else { break };
            if let Some(msg) = wire::parse::<ToHost>(&line) {
                if input.send(Event::Window(msg)).is_err() {
                    return;
                }
            }
        }
        let _ = input.send(Event::WindowGone);
    });
    let out = Box::new(|msg: ToWindow| {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&wire::line(&msg));
        let _ = stdout.flush();
    });
    Host::new(spec, out).serve(tx, rx);
}

/// Where a signal to end is written, for the thread that tells the host.
#[cfg(unix)]
static ENDING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// SIGTERM or SIGHUP (the daemon ending the session, or itself) ends the
/// host as a window's Stop does: its agent and their commands with it,
/// which are in groups of their own and would be left running.
#[cfg(unix)]
fn on_termination(tx: mpsc::Sender<Event>) {
    extern "C" fn handler(_: libc::c_int) {
        let fd = ENDING.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: write is async-signal-safe; one byte to our own pipe.
            unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
        }
    }
    let mut fds = [0i32; 2];
    // SAFETY: a pipe of this process, its ends kept for its life.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    ENDING.store(fds[1], std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        // SAFETY: the read end of the pipe made above.
        if unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), 1) } == 1 {
            let _ = tx.send(Event::WindowGone);
        }
    });
    // SAFETY: the handler only writes to the pipe.
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

/// A host on a thread of this process, for a window with no daemon: what
/// the window sends, and what it hears.
pub fn spawn_in_process(spec: HostSpec) -> (mpsc::Sender<Event>, async_channel::Receiver<ToWindow>) {
    let (tx, rx) = mpsc::channel();
    let (out_tx, out_rx) = async_channel::unbounded();
    let serve_tx = tx.clone();
    std::thread::Builder::new()
        .name("acp-host".into())
        .spawn(move || {
            let out = Box::new(move |msg: ToWindow| {
                let _ = out_tx.send_blocking(msg);
            });
            Host::new(spec, out).serve(serve_tx, rx);
        })
        .expect("a thread for the agent's host");
    (tx, out_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// npx's cache left half-installed by a start cut short: found from what
    /// npm says, and only that folder.
    #[test]
    fn a_download_cut_short_in_npx_s_cache_is_found() {
        let cache = std::env::temp_dir().join(format!("ideae-npx-{}", std::process::id())).join("_npx");
        let broken = cache.join("51a65f6ed666c7d8");
        std::fs::create_dir_all(broken.join("node_modules")).unwrap();
        let said = format!(
            "npm error code ENOENT\nnpm error syscall open\nnpm error path {p}\nnpm error enoent Could not read package.json: Error: ENOENT: no such file or directory, open '{p}'\n",
            p = broken.join("package.json").display()
        );
        assert_eq!(half_installed_npx(&said), Some(broken.clone()));
        // Whole after all, or another error: left alone.
        std::fs::write(broken.join("package.json"), "{}").unwrap();
        assert_eq!(half_installed_npx(&said), None);
        assert_eq!(half_installed_npx("npm error code E404\nnpm error 404 Not Found"), None);
        let _ = std::fs::remove_dir_all(cache.parent().unwrap());
    }

    /// A journal it cannot read whole is kept aside, never written over.
    #[test]
    fn a_journal_not_read_whole_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("ideae-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let push = |text: &str| String::from_utf8(wire::line(&Op::Push { entry: Entry::User { text: text.into() } })).unwrap();
        std::fs::write(&path, format!("{{\"op\":\"Reset\",\"thread\":{{\"what\":1}},\"broken\n{}{}{{\"op\":\"Push\"", push("one"), push("two"))).unwrap();
        let (thread, _file, _) = open_journal(Some(&path));
        assert!(matches!(&thread.entries[..2], [Entry::User { .. }, Entry::User { .. }]), "what could be read: {:?}", thread.entries);
        let aside: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("unread")).collect();
        assert_eq!(aside.len(), 1, "kept aside");
        assert!(std::fs::read_to_string(aside[0].path()).unwrap().contains("broken"));

        // A last line cut short by a crash alone is no reason.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, format!("{}{{\"op\":\"Pu", push("one"))).unwrap();
        let (thread, _file, _) = open_journal(Some(&path));
        assert_eq!(thread.entries.len(), 1);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
