use super::*;

// ---------------------------------------------------------------- setup and check

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The setup's terminal, among the service tabs: no service has this name.
pub(super) const SETUP_TAB: &str = "ideae-setup";
/// The most of a check's output kept on the task, and sent to the agent.
const SHOWN_LINES: usize = 400;
const TO_AGENT_LINES: usize = 80;

/// What the window asks of the workspace: the project's settings, or a
/// setup or a check run again (through the trust of the project's file).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoAsk {
    Settings,
    Setup,
    Check,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Running,
    Passed,
    /// The exit code, when it ended with one.
    Failed(Option<i32>),
    /// Its terminal was closed before it ended.
    Stopped,
}

pub struct CheckRun {
    pub state: RunState,
    pub command: String,
    pub output: SharedString,
    pub at: u64,
}

#[derive(Default)]
pub struct Auto {
    /// What the project has set for its tasks, as last read.
    pub configured: crate::project_config::TaskSetup,
    pub setup: Option<RunState>,
    _setup_poll: Option<Task<()>>,
    pub check: Option<CheckRun>,
    /// The check that runs: the future that waits for it, and what stops
    /// its processes.
    check_job: Option<(Task<()>, Arc<AtomicBool>)>,
    /// What the worktree held when the last check started.
    checked: Option<String>,
    output_open: bool,
    ci_busy: bool,
    /// Failed checks sent to the agent by themselves since a turn of yours.
    fixes: u32,
    /// The agent's turn now is one such a check started.
    fixing: bool,
    /// How many in a row, as the project is set on this machine; 0: none.
    fix_most: u32,
}

impl Auto {
    /// Ends what runs for the task: its check. The setup is a terminal,
    /// ended with the others.
    pub fn stop(&mut self) {
        if let Some((_, cancel)) = self.check_job.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl TaskView {
    /// The marks the task's row has for its setup and its check.
    pub fn auto_marks(&self) -> Vec<(&'static str, Hsla)> {
        let muted = hsla(0., 0., 0.55, 1.);
        let setup = match &self.auto.setup {
            Some(RunState::Failed(_)) => Some(("✗ setup", gpui_kit::red())),
            Some(RunState::Running) => Some(("setting up…", muted)),
            _ => None,
        };
        let check = match self.auto.check.as_ref().map(|c| &c.state) {
            Some(RunState::Failed(_)) => Some(("✗ check", gpui_kit::red())),
            Some(RunState::Running) => Some(("checking…", muted)),
            Some(RunState::Passed) => Some(("✓ check", gpui_kit::green())),
            _ => None,
        };
        setup.into_iter().chain(check).collect()
    }

    /// Runs the project's setup in a terminal of the task, behind what is
    /// in front: the agent starts at the same time and is not held up.
    pub fn start_setup(&mut self, commands: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if commands.is_empty() {
            return;
        }
        if let Some(ix) = self.service_ix(SETUP_TAB, cx) {
            if let TabKind::Terminal(view) = &self.tabs[ix].kind {
                TermView::end(&view.clone(), cx);
            }
            self.remove_tab(ix, window, cx);
        }
        use sha2::Digest as _;
        let key: String = sha2::Sha256::digest(self.root.display().to_string().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        let dir = crate::state::runtime_dir().join("setup");
        let dir = dir.join(key);
        let script = dir.join(if cfg!(windows) { "ideae-setup.cmd" } else { "ideae-setup.sh" });
        let status = dir.join("status");
        self.auto.setup = Some(RunState::Running);
        cx.emit(TaskEvent::Changed);
        cx.notify();
        let (job_script, job_status, job_root) = (script.clone(), status.clone(), self.root.clone());
        self.auto._setup_poll = Some(cx.spawn_in(window, async move |this, cx| {
            let written: std::io::Result<(PathBuf, PathBuf)> = cx
                .background_spawn(async move {
                    // A remote task's on its host: beside its worktree's git files there.
                    if crate::hosts::is_remote(&job_root) {
                        let place = |name: &str| -> std::io::Result<PathBuf> {
                            let there = crate::git::run(&job_root, "git", &["rev-parse", "--path-format=absolute", "--git-path", name]).map_err(std::io::Error::other)?;
                            Ok(crate::hostfs::rekey(&job_root, PathBuf::from(there.trim())))
                        };
                        let (script, status) = (place("ideae-setup.sh")?, place("ideae-setup.status")?);
                        let status_there = crate::hosts::there(&status).display().to_string();
                        let _ = crate::hostfs::run(&job_root, "rm", &["-f", &status_there], &[], None);
                        crate::hostfs::write(&script, crate::project_config::setup_script(&commands, Path::new(&status_there)).as_bytes())?;
                        return Ok((script, status));
                    }
                    std::fs::create_dir_all(&dir)?;
                    let _ = std::fs::remove_file(&job_status);
                    std::fs::write(&job_script, crate::project_config::setup_script(&commands, &job_status))?;
                    Ok((job_script, job_status))
                })
                .await;
            let (script, status) = match written {
                Ok(places) => places,
                Err(e) => {
                    let _ = this.update(cx, |t, cx| {
                        t.auto.setup = Some(RunState::Failed(None));
                        t.services_error = Some(format!("Setup: {e}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let opened = this.update_in(cx, |t, window, cx| {
                // By its path where the terminal runs: a remote task's on its host.
                let script = crate::hosts::there(&script);
                let line = if cfg!(windows) { format!("cmd /c \"{}\"", script.display()) } else { format!("sh {}", shell_quote(&script.display().to_string())) };
                t.terminal_seq += 1;
                let tab = Self::make_terminal(
                    &t.root,
                    t.hook_socket.as_ref(),
                    Term::Service { title: "⚙ setup".into(), seq: t.terminal_seq, name: SETUP_TAB.to_string(), cwd: t.root.clone() },
                    Some(line),
                    t.service_env(),
                    None,
                    window,
                    cx,
                );
                let at = t.tabs.iter().rposition(|tab| matches!(tab.kind, TabKind::Terminal(_))).map_or(0, |ix| ix + 1);
                t.tabs.insert(at, tab);
                cx.notify();
            });
            if opened.is_err() {
                return;
            }
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let read_status = status.clone();
                let code = cx.background_spawn(async move { crate::hostfs::read_to_string(&read_status).ok() }).await;
                let ended = this.update_in(cx, |t, window, cx| {
                    let open = t.service_ix(SETUP_TAB, cx);
                    // An empty file is one being written.
                    let state = match (code.as_deref().map(str::trim).filter(|c| !c.is_empty()), open) {
                        (Some("0"), _) => RunState::Passed,
                        (Some(code), _) => RunState::Failed(code.parse().ok()),
                        (None, None) => RunState::Stopped,
                        (None, Some(_)) => return false,
                    };
                    t.setup_ended(state, window, cx);
                    true
                });
                if !matches!(ended, Ok(false)) {
                    // The script and its status are of no more use.
                    if let Some(dir) = status.parent().map(Path::to_path_buf) {
                        cx.background_spawn(async move { std::fs::remove_dir_all(dir) }).detach();
                    }
                    return;
                }
            }
        }));
    }

    fn setup_ended(&mut self, state: RunState, window: &mut Window, cx: &mut Context<Self>) {
        match &state {
            // Done well, its terminal has nothing to say: the Run panel tells it went.
            RunState::Passed => {
                if let Some(ix) = self.service_ix(SETUP_TAB, cx) {
                    if let TabKind::Terminal(view) = &self.tabs[ix].kind {
                        TermView::end(&view.clone(), cx);
                    }
                    self.remove_tab(ix, window, cx);
                }
                // A server that could not import the build before the setup made it
                // whole (a sibling checkout, generated files) is started again.
                if !crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed) && self.lsp.clients().iter().any(|client| client.problem().is_some()) {
                    self.set_language_servers(false, cx);
                    self.set_language_servers(true, cx);
                }
            }
            RunState::Failed(code) => {
                let code = code.map(|c| format!(" (exit {c})")).unwrap_or_default();
                let name = self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                window.push_notification(
                    Notification::error(format!("The agent goes on; its terminal \"⚙ setup\" has the output{code}.")).title(format!("Setup failed in {name}")),
                    cx,
                );
            }
            _ => {}
        }
        self.auto.setup = Some(state);
        self.auto._setup_poll = None;
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Runs the project's check in the worktree, off the UI thread: one at
    /// a time, the one before stopped. Not again for a worktree as it was
    /// at the last one, unless `again`.
    /// The one after a turn (`again` false) may go back to the agent by
    /// itself when it fails, as the project says.
    pub fn start_check(&mut self, command: String, again: bool, window: &mut Window, cx: &mut Context<Self>) {
        if command.trim().is_empty() {
            return;
        }
        self.auto.stop();
        let cancel = Arc::new(AtomicBool::new(false));
        let last = if again { None } else { self.auto.checked.clone() };
        let (root, env, job_cancel) = (self.root.clone(), self.service_env(), cancel.clone());
        let job = cx.spawn_in(window, async move |this, cx| {
            let print_root = root.clone();
            let print = cx.background_spawn(async move { crate::project_config::fingerprint(&print_root) }).await;
            if print.is_some() && print == last {
                // A turn that changed nothing: no fix came of it.
                let _ = this.update(cx, |t, _| t.auto.fixing = false);
                return;
            }
            let started = this.update(cx, |t, cx| {
                t.auto.checked = print;
                t.auto.check = Some(CheckRun { state: RunState::Running, command: command.clone(), output: SharedString::default(), at: crate::state::now() });
                cx.emit(TaskEvent::Changed);
                cx.notify();
            });
            if started.is_err() {
                return;
            }
            let commands = vec![command.clone()];
            let outcome = cx
                .background_spawn(crate::procs::on_thread(move || {
                    crate::project_config::run(&commands, &root, &env, Duration::from_secs(30 * 60), &job_cancel)
                }))
                .await;
            if outcome.cancelled {
                return;
            }
            let _ = this.update_in(cx, |t, window, cx| {
                let mut output = crate::project_config::tail(&outcome.output, SHOWN_LINES);
                if outcome.timed_out {
                    output.push_str("\n… stopped after 30 minutes");
                }
                let state = if outcome.ok() { RunState::Passed } else { RunState::Failed(outcome.code) };
                let failed = matches!(state, RunState::Failed(_));
                t.auto.check = Some(CheckRun { state, command, output: output.into(), at: crate::state::now() });
                t.auto.check_job = None;
                if !again {
                    t.check_after_turn(failed, window, cx);
                }
                cx.emit(TaskEvent::Changed);
                cx.notify();
            });
        });
        self.auto.check_job = Some((job, cancel));
    }

    /// The end of the failed check's output to the agent, to fix.
    pub(super) fn send_check_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.failed_check_text() else { return };
        self.tell_agent(text, "Check output", window, cx);
    }

    pub fn set_check_to_agent(&mut self, most: u32) {
        self.auto.fix_most = most;
    }

    /// Failed checks the agent was sent by themselves in a row, and past
    /// the most when you were told.
    #[cfg(test)]
    pub fn check_fixes(&self) -> u32 {
        self.auto.fixes
    }

    fn failed_check_text(&self) -> Option<String> {
        let check = self.auto.check.as_ref().filter(|c| matches!(c.state, RunState::Failed(_)))?;
        Some(format!(
            "The check `{}` failed after your last turn. The end of its output:\n```\n{}\n```\nFix what makes it fail.",
            check.command,
            crate::digest::for_agent(&check.output, TO_AGENT_LINES)
        ))
    }

    /// A check after the agent's turn ended. Failed, it goes back to the
    /// agent by itself as many times in a row as the project says
    /// (`check_to_agent`), then you are told; a turn of yours starts the
    /// count again. Not while you type to the agent, nor into a question
    /// it asks.
    fn check_after_turn(&mut self, failed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ours = std::mem::take(&mut self.auto.fixing);
        if !failed || !ours {
            self.auto.fixes = 0;
        }
        let most = self.auto.fix_most;
        if !failed || most == 0 {
            return;
        }
        let name = self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if self.auto.fixes >= most {
            if self.auto.fixes == most {
                self.auto.fixes += 1;
                let tries = if most == 1 { "one try".to_string() } else { format!("{most} tries") };
                window.push_notification(Notification::warning(format!("Still failing after the agent's {tries}: your turn.")).title(format!("Check in {name}")), cx);
            }
            return;
        }
        let typing = self.agent_terminal(Who::Main, cx).and_then(|s| s.read(cx).typed_at()).is_some_and(|at| at.elapsed() < Duration::from_secs(30));
        if typing || self.agent.status != crate::agent::AgentStatus::Done || self.agent.has_queued_message() {
            return;
        }
        let Some(text) = self.failed_check_text() else { return };
        // Not written by the user: said so, as it shows in the agent's input.
        let text = format!("[Sent by ideae, not typed: the check failed after your turn; try {} of {most}.]\n{text}", self.auto.fixes + 1);
        if matches!(self.message_agent(text, window, cx), Told::Now) {
            self.auto.fixes += 1;
            self.auto.fixing = true;
            self.action = Some((true, format!("The failed check went to the agent ({} of {most})", self.auto.fixes)));
            cx.notify();
        }
    }

    fn tell_agent(&mut self, text: String, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.action = Some(match self.message_agent(text, window, cx) {
            Told::Now => (true, format!("{what} sent to the agent")),
            Told::Later => (true, format!("{what} goes to the agent when its turn ends")),
            Told::Not => (false, "The agent is not running".to_string()),
        });
        if let Some((ok, said)) = self.action.clone() {
            let note = if ok { Notification::info(said) } else { Notification::warning(said) };
            window.push_notification(note, cx);
        }
        cx.notify();
    }

    /// The failed CI job's log, its end, to the agent with what to do.
    pub(super) fn send_ci_failure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.auto.ci_busy, true) {
            return;
        }
        cx.notify();
        let (root, branch) = (self.root.clone(), self.branch.to_string());
        cx.spawn_in(window, async move |this, cx| {
            let log = cx.background_spawn(crate::procs::on_thread(move || crate::pr::failed_log(&root, &branch))).await;
            let _ = this.update_in(cx, |t, window, cx| {
                t.auto.ci_busy = false;
                match log {
                    Ok((url, log)) => {
                        let text = format!(
                            "CI failed on this branch ({url}). The end of the failed job's log:\n```\n{log}\n```\nFind the cause and fix it; run the failing step here first where you can."
                        );
                        t.tell_agent(text, "CI failure", window, cx);
                    }
                    Err(e) => window.push_notification(Notification::warning(e).title("CI log"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// In the branch bar: how the setup and the check went, a click away
    /// from the Run panel; and a failed CI, to the agent.
    pub(super) fn render_auto_badges(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        for (ix, (mark, color)) in self.auto_marks().into_iter().enumerate() {
            let tip = match self.auto.check.as_ref().filter(|_| mark.ends_with("check")) {
                Some(check) => format!("{} · the Run panel has its output", check.command),
                None if mark.starts_with('✗') => "The setup failed: its terminal, ⚙ setup, has the output; the Run panel runs it again".to_string(),
                None => "The project's setup for new tasks, in the terminal ⚙ setup".to_string(),
            };
            out.push(
                Button::new(("auto-mark", ix))
                    .xsmall()
                    .ghost()
                    .label(mark)
                    .text_color(color)
                    .tooltip(tip)
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.show_run_panel(cx);
                    }))
                    .into_any_element(),
            );
        }
        if self.auto.check.as_ref().is_some_and(|c| matches!(c.state, RunState::Failed(_))) {
            out.push(
                Button::new("check-to-agent")
                    .xsmall()
                    .ghost()
                    .label("→ agent")
                    .tooltip("Send the errors in the check's output and its end to the agent, to fix")
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.send_check_to_agent(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        let ci_red = self.pr.as_ref().is_some_and(|pr| pr.checks == crate::pr::Checks::Failed && matches!(pr.state, crate::pr::PrState::Open | crate::pr::PrState::Draft));
        if ci_red {
            out.push(
                Button::new("ci-to-agent")
                    .xsmall()
                    .ghost()
                    .label(if self.auto.ci_busy { "CI ✗ reading log…" } else { "CI ✗ → agent" })
                    .text_color(gpui_kit::red())
                    .disabled(self.auto.ci_busy)
                    .tooltip("Send the end of the failed CI job's log to the agent, to fix (gh run view --log-failed)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.send_ci_failure(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        out
    }

    pub fn set_configured(&mut self, configured: crate::project_config::TaskSetup, cx: &mut Context<Self>) {
        if self.auto.configured != configured {
            self.auto.configured = configured;
            cx.notify();
        }
    }

    /// The project's settings changed: services and all read again.
    pub fn reread_settings(&mut self, cx: &mut Context<Self>) {
        self.refresh_services(cx);
    }

    pub fn has_agent(&self) -> bool {
        self.agent.launch.is_some()
    }

    pub(super) fn show_run_panel(&mut self, cx: &mut Context<Self>) {
        self.side_over = true;
        self.open_panel(RightPane::Run, cx);
    }

    /// The Run panel's top: what new tasks of the project get, and how it
    /// went in this one.
    pub(super) fn render_auto(&self, mut panel: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        let muted = cx.theme().muted_foreground;
        let c = &self.auto.configured;
        let ask = |id: &'static str, label: &'static str, tip: &'static str, what: AutoAsk| {
            Button::new(id).xsmall().ghost().label(label).tooltip(tip).on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(TaskEvent::Auto(what));
            }))
        };
        let state_label = |state: &RunState| -> (String, Hsla) {
            match state {
                RunState::Running => ("running…".into(), muted),
                RunState::Passed => ("passed".into(), gpui_kit::green()),
                RunState::Failed(Some(code)) => (format!("failed (exit {code})"), gpui_kit::red()),
                RunState::Failed(None) => ("failed".into(), gpui_kit::red()),
                RunState::Stopped => ("stopped".into(), muted),
            }
        };
        let row = |id: &'static str| h_flex().id(id).px_3().pt_1().gap_2().min_w_0();
        let command = |line: String| div().px_3().pb_1().min_w_0().truncate().text_xs().text_color(muted).child(line);
        panel = panel.child(
            h_flex()
                .px_3()
                .pt_2()
                .child(div().flex_1().text_xs().text_color(muted).child("EACH TASK"))
                .child(ask("project-settings", "Project settings…", "Setup, teardown, check, services and files to copy, for every task of the project", AutoAsk::Settings)),
        );
        if !c.runs_anything() && self.auto.setup.is_none() && self.auto.check.is_none() {
            return panel.child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(muted)
                    .child("Nothing runs by itself: no setup for new worktrees, no check when the agent finishes."),
            );
        }
        let setup_line = if c.setup.is_empty() { "none".to_string() } else { c.setup.join(" · ") };
        panel = panel.child(
            row("auto-setup")
                .child(div().flex_1().child("Setup"))
                .when_some(self.auto.setup.as_ref(), |r, state| {
                    let (label, color) = state_label(state);
                    r.child(div().flex_none().text_xs().text_color(color).child(label))
                })
                .when(self.service_ix(SETUP_TAB, cx).is_some(), |r| {
                    r.child(Button::new("setup-output").xsmall().ghost().label("Output").on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(ix) = this.service_ix(SETUP_TAB, cx) {
                            this.activate(ix, window, cx);
                        }
                    })))
                })
                .when(!c.setup.is_empty() && self.auto.setup != Some(RunState::Running), |r| {
                    r.child(ask("setup-again", "Run", "Run the setup in this worktree again", AutoAsk::Setup))
                }),
        );
        panel = panel.child(command(setup_line));
        let check = self.auto.check.as_ref();
        let check_line = if c.check.trim().is_empty() { check.map_or_else(|| "none".to_string(), |c| c.command.clone()) } else { c.check.clone() };
        panel = panel.child(
            row("auto-check")
                .child(div().flex_1().child("Check"))
                .when_some(check, |r, run| {
                    let (label, color) = state_label(&run.state);
                    let when = if run.state == RunState::Running { String::new() } else { format!(" {}", crate::sidebar::ago(run.at)) };
                    r.child(div().flex_none().text_xs().text_color(color).child(format!("{label}{when}")))
                })
                .when(check.is_some_and(|c| !c.output.is_empty()), |r| {
                    let open = self.auto.output_open;
                    r.child(Button::new("check-output").xsmall().ghost().label(if open { "Hide" } else { "Output" }).on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.auto.output_open = !this.auto.output_open;
                        cx.notify();
                    })))
                })
                .when(!c.check.trim().is_empty(), |r| {
                    r.child(ask("check-again", "Run", "Run the check now, changed or not", AutoAsk::Check))
                }),
        );
        panel = panel.child(command(check_line));
        if let Some(run) = check.filter(|c| matches!(c.state, RunState::Failed(_))) {
            let _ = run;
            panel = panel.child(
                h_flex().px_3().pb_1().child(
                    Button::new("check-send")
                        .xsmall()
                        .outline()
                        .label("Send to agent")
                        .tooltip("The errors in the output and its end, and a word to fix it")
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.send_check_to_agent(window, cx);
                        })),
                ),
            );
        }
        if let Some(run) = check.filter(|c| self.auto.output_open && !c.output.is_empty()) {
            panel = panel.child(
                div()
                    .id("check-output-text")
                    .mx_3()
                    .mb_2()
                    .p_2()
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .whitespace_normal()
                    .child(run.output.clone()),
            );
        }
        panel
    }
}
