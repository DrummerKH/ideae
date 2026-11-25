//! A task's links in the window: its ticket opened in the system's
//! browser, its key and link copied, a link added by hand.

use std::path::Path;

use gpui_kit::component::{
    Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    v_flex,
};
use gpui_kit::{TestSupportExt as _, *};

use super::Workspace;

impl Workspace {
    /// The integrations of the project a task is of.
    pub fn integrations_of(&self, path: &Path) -> Vec<crate::integrations::Integration> {
        self.find(path).map(|(pi, _)| crate::integrations::of_resolved(&self.projects[pi].resolved)).unwrap_or_default()
    }

    /// Opens a task's ticket, its first, where links open: the system's
    /// browser, never the task's own Chrome, whose sessions are its agent's.
    pub fn open_ticket(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ticket) = self.find(path).and_then(|(pi, ti)| self.projects[pi].tasks[ti].ticket().cloned()) else { return };
        if ticket.url.is_empty() {
            let note = format!("{} looks like a ticket, but no integration owns its prefix: add one in Settings → Integrations, and its tickets get links.", ticket.key);
            window.push_notification(Notification::info(note).title("No link for this ticket"), cx);
            return;
        }
        self.open_link(path, &ticket.url, cx);
    }

    /// Opens a link of a task's ticket or request: in the system's browser,
    /// never the task's own Chrome, whose sessions are its agent's.
    pub fn open_link(&mut self, _path: &Path, url: &str, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    /// Adds a link to a task: a ticket's address or key, or any address.
    pub fn add_link_dialog(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("https://… or a ticket's key (DEV-123)"));
        let this = cx.weak_entity();
        let path = path.to_path_buf();
        let focused = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, window, cx| {
            if !focused.replace(true) {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            let add = {
                let (input, this, path) = (input.clone(), this.clone(), path.clone());
                move |window: &mut Window, cx: &mut App| {
                    let text = input.read(cx).value().trim().to_string();
                    window.close_dialog(cx);
                    let _ = this.update(cx, |w, cx| {
                        w.add_link(&path, &text, cx);
                        w.tell_commit_keys(window, cx);
                    });
                }
            };
            let (ok, button) = (add.clone(), add);
            dialog
                .title("Add a link")
                .w(px(520.))
                .on_ok(move |_, window, cx| {
                    ok(window, cx);
                    false
                })
                .child(v_flex().gap_2().child(div().id("link-input").test_support().child(Input::new(&input))))
                .footer(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("link-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("link-add").primary().label("Add").on_click(move |_, window, cx| button(window, cx))),
                )
        });
    }

    /// What `add_link_dialog` adds: a ticket by its link or key, else the address.
    pub fn add_link(&mut self, path: &Path, text: &str, cx: &mut Context<Self>) {
        let integrations = self.integrations_of(path);
        let mut found = crate::integrations::tickets::links_for_task("", text, &integrations);
        if found.is_empty()
            && let Some(f) = crate::integrations::tickets::ticket_in(text, &integrations).filter(|f| f.own_issue.is_none())
        {
            found.push(crate::integrations::tickets::link_of(&f.ticket));
        }
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &mut self.projects[pi].tasks[ti];
        for link in found {
            if !t.links.iter().any(|l| (l.is_ticket() && link.is_ticket() && l.key == link.key) || (!l.url.is_empty() && l.url == link.url)) {
                t.links.push(link);
            }
        }
        let links = t.links.clone();
        if let Some(view) = t.view.clone() {
            view.update(cx, |v, _| v.links = links);
        }
        self.save_later(cx);
        cx.notify();
    }

    /// A new task for the ticket of another: its key in the title (its
    /// links go with it), and its agent told the earlier branch and request.
    pub fn new_task_for_ticket(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        let Some(ticket) = t.ticket().cloned() else { return };
        let key = ticket.key.clone();
        let request = t.links.iter().find(|l| l.what == "pr").map(|l| format!(", its request {}", l.url)).unwrap_or_default();
        let link = if ticket.url.is_empty() { String::new() } else { format!(" ({})", ticket.url) };
        let title = format!("{key} follow-up");
        let prompt = format!(
            "A follow-up of {key}{link}. The earlier task's branch was {}{request}. Read the ticket and what that branch did (git log {}), then do what is left.",
            t.branch, t.branch
        );
        self.new_task_dialog_with(pi, Some((title, prompt)), window, cx);
    }

    /// A task on a colleague's pull or merge request, from its link: its
    /// head fetched into a branch of its name, a worktree on it. Marked as
    /// a review: its agent is asked to review, not to commit.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_request_task(
        &mut self,
        project: usize,
        kind: crate::integrations::Kind,
        number: u64,
        url: String,
        prompt: String,
        agents: Vec<String>,
        given: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.projects.get_mut(project) else { return };
        let repo = p.path.clone();
        let placeholder = crate::git::task_worktree_dir(&repo, &format!("request-{number}.making-{}", crate::state::now()));
        p.collapsed = false;
        p.tasks.push(super::TaskEntry {
            path: placeholder.clone(),
            title: format!("#{number}"),
            last_active: crate::state::now(),
            busy: Some(format!("fetching #{number}…").into()),
            created: crate::state::now(),
            agent: super::agents_ui::pick_profile(agents.first().map_or("", String::as_str)),
            links: vec![crate::state::TaskLink { what: "pr".into(), key: format!("#{number}"), url: url.clone(), ..Default::default() }],
            links_read: true,
            review: true,
            fresh: true,
            ..Default::default()
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let copy_env = crate::state::settings().copy_env;
            let (job, link) = (repo.clone(), url.clone());
            let made = cx
                .background_spawn(crate::procs::on_thread(move || {
                    crate::integrations::request_is_of(&job, &link)?;
                    let host = crate::integrations::code_host(&job).map_err(|e| e.to_string())?;
                    let info = host.request_info(&job, number).map_err(|e| e.to_string())?;
                    let (dir, branch) = crate::git::request_worktree(&job, kind, number, &info.head, info.cross, copy_env)?;
                    Ok::<_, String>((info, dir, branch))
                }))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some((pi, ti)) = this.find(&placeholder) else { return };
                match made {
                    Ok((info, dir, branch)) => {
                        let prompt = if prompt.trim().is_empty() {
                            let from = if info.cross { " from a fork" } else { "" };
                            format!("Request #{number} ({url}), branch {branch}{from}, is a colleague's: review it. Read its diff against its base and what it is for; say what is wrong or could be better, file and line. Commit nothing unless asked.")
                        } else {
                            prompt
                        };
                        let t = &mut this.projects[pi].tasks[ti];
                        t.path = dir.clone();
                        t.branch = branch;
                        t.title = format!("#{number} {}", info.title);
                        t.busy = None;
                        t.prompt = prompt.clone();
                        t.pr = Some(info.pr.clone());
                        t.set_pr_link(&info.pr);
                        this.switch(&dir, window, cx);
                        this.task_made(&dir, window, cx);
                        if let Some(view) = this.entry_mut(&dir).and_then(|e| e.view.clone()) {
                            let send = crate::state::settings().send_new_prompt || given;
                            view.update(cx, |t, cx| t.queue_prompt(prompt, send, window, cx));
                        }
                    }
                    Err(e) => {
                        this.projects[pi].tasks.remove(ti);
                        window.push_notification(Notification::error(e).title(format!("Could not open #{number}")), cx);
                    }
                }
                this.save_later(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// A ticket asked for from outside the window (`ideae ticket DEV-1`,
    /// `ideae://ticket/DEV-1`): its task comes to the front; none, New task
    /// opens with its key, in the project whose integration owns it.
    pub fn open_asked(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(crate::deep_link::Asked::Ticket(key)) = crate::deep_link::parse(line) else { return };
        window.activate_window();
        // What is being typed in a dialog is not thrown away by a link from
        // outside (any page can open one): it waits for a click.
        if window.has_active_dialog(cx) {
            let (me, line) = (cx.entity().downgrade(), line.to_string());
            let note = Notification::info(format!("{key} was asked for from outside the app.")).title("A ticket asked for").autohide(false).content(move |_, _, cx| {
                let (me, line, note) = (me.clone(), line.clone(), cx.entity().downgrade());
                gpui_kit::component::h_flex()
                    .child(Button::new("asked-open").xsmall().label("Open it (closes the dialog)").on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let _ = me.update(cx, |w, cx| w.open_asked_now(&line, window, cx));
                        let _ = note.update(cx, |note, cx| note.dismiss(window, cx));
                    }))
                    .into_any_element()
            });
            window.push_notification(note, cx);
            return;
        }
        self.open_asked_now(line, window, cx);
    }

    fn open_asked_now(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(crate::deep_link::Asked::Ticket(key)) = crate::deep_link::parse(line) else { return };
        let has = |t: &super::TaskEntry| !t.main && (t.links.iter().any(|l| l.is_ticket() && l.key.eq_ignore_ascii_case(&key)) || crate::integrations::tickets::key_at_start(&t.title).is_some_and(|k| k.eq_ignore_ascii_case(&key)));
        if let Some(path) = self.projects.iter().flat_map(|p| &p.tasks).find(|t| has(t)).map(|t| t.path.clone()) {
            return self.focus_task(&path, window, cx);
        }
        let owner = self.projects.iter().position(|p| crate::integrations::tickets::owner(&key, &crate::integrations::of_resolved(&p.resolved)).is_some());
        let Some(pi) = owner.or_else(|| self.active_project()).or_else(|| self.first_project()) else {
            window.push_notification(Notification::info(format!("No project to make a task for {key} in: add one first.")), cx);
            return;
        };
        self.new_task_dialog_with(pi, Some((key, String::new())), window, cx);
    }

    /// The branch a ticket's task would have, copied without making one:
    /// for a branch made by hand, or in another tool.
    pub fn copy_branch_for_ticket_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pi) = self.active_project().or_else(|| self.first_project()) else { return };
        let (template, integrations) = (self.projects[pi].values().branch.clone(), crate::integrations::of_resolved(&self.projects[pi].resolved));
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("DEV-123 Fix login, or a ticket's link"));
        // Kept by the dialog and gone with it: the branch shown follows what is typed.
        let heard = std::rc::Rc::new(window.subscribe(&input, cx, |_, event: &gpui_kit::component::input::InputEvent, window, _| {
            if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                window.refresh();
            }
        }));
        let focused = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, window, cx| {
            let _ = &heard;
            if !focused.replace(true) {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            let typed = input.read(cx).value().trim().to_string();
            // A link: its key, the words a Linear link has.
            let title = match crate::integrations::tickets::ticket_in(&typed, &integrations).filter(|f| f.pasted.is_some()) {
                Some(f) => format!("{} {}", f.ticket.key, f.words),
                None => typed,
            };
            let branch = if crate::integrations::tickets::key_at_start(&title).is_some() { crate::naming::branch_name_as(&title, &template) } else { String::new() };
            let (copied, shown) = (branch.clone(), branch.clone());
            dialog
                .title("Copy a ticket's branch name")
                .w(px(520.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(Input::new(&input))
                        .child(div().text_xs().child(if shown.is_empty() { "Its key first: DEV-123 and a few words.".to_string() } else { format!("Branch: {shown}") })),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("branch-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("branch-copy").primary().label("Copy").on_click(move |_, window, cx| {
                            if !copied.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                                window.close_dialog(cx);
                            }
                        })),
                )
        });
    }

    /// Today's tasks for a stand-up, as Markdown: by ticket, each with its
    /// title and its request's state; from what ideae knows, nothing asked.
    pub fn copy_todays_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = crate::state::now();
        let since = crate::local_time::at(now, 0, 0);
        let mut rows: Vec<(String, String)> = Vec::new();
        for p in self.projects.iter().enumerate().filter(|(pi, _)| self.in_space(*pi)).map(|(_, p)| p) {
            for t in p.tasks.iter().filter(|t| !t.main && t.side.is_none() && (t.last_active >= since || t.done_at >= since)) {
                rows.push((t.ticket().map(|l| l.key.clone()).unwrap_or_default(), standup_line(t)));
            }
        }
        let text = standup(rows);
        if text.is_empty() {
            window.push_notification(Notification::info("No task was worked on today."), cx);
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(Notification::info("Today's tasks copied, as Markdown."), cx);
    }

    /// A task made from a tracker's link that no integration knows: asks,
    /// once a host, whether to add one, and for where. Never in the way:
    /// the task is made already.
    pub fn ask_for_integration(&mut self, pi: usize, link: &crate::state::TaskLink, window: &mut Window, cx: &mut Context<Self>) {
        use crate::integrations::{Kind, host_of, tickets};
        if !link.integration.is_empty() || link.url.is_empty() || !crate::state::with_settings(|s| s.tickets) {
            return;
        }
        let Some(kind) = Kind::parse(&link.tracker).filter(|k| k.keyed()) else { return };
        let Some(base) = tickets::base_of_link(kind, &link.url) else { return };
        let asked = format!("integration:{}", host_of(&base));
        if crate::state::with_settings(|s| s.hints_off.contains(&asked)) {
            return;
        }
        let prefix = link.key.split_once('-').map(|(p, _)| p.to_string()).unwrap_or_default();
        let Some(p) = self.projects.get(pi) else { return };
        let space = p.space.clone().and_then(|id| self.space_name(Some(&id)).map(|name| (id, name)));
        let (project, name) = (p.path.clone(), p.name.clone());
        let taken: Vec<String> = crate::integrations::of_resolved(&p.resolved).into_iter().map(|i| i.id).collect();
        let host = host_of(&base).split(['.', ':']).next().unwrap_or("x").to_ascii_lowercase();
        let mut id = format!("{}-{host}", kind.word());
        while taken.contains(&id) {
            id.push('2');
        }
        let entry = crate::layers::Entry { id, kind: Some(kind.word().into()), url: Some(base.clone()), keys: Some(vec![prefix.clone()]), ..Default::default() };
        let this = cx.weak_entity();
        let said = format!("Add {} {}, owning {prefix}-…? Its keys get links everywhere it applies.", kind.label(), host_of(&base));
        let note = Notification::info(said).title("A tracker not set up").content(move |note, _, cx| {
            let me = cx.entity().downgrade();
            let button = |id: &'static str, label: String, to: Where| {
                let (this, me, entry, project) = (this.clone(), me.clone(), entry.clone(), project.clone());
                Button::new(id).xsmall().outline().label(label).on_click(move |_, window, cx| {
                    let _ = this.update(cx, |w, cx| w.add_integration_to(&project, entry.clone(), to.clone(), window, cx));
                    let _ = me.update(cx, |note, cx| note.dismiss(window, cx));
                })
            };
            let _ = note;
            let mut row = h_flex().gap_1().flex_wrap().child(button("int-project", format!("For {name}"), Where::Project));
            if let Some((id, space)) = &space {
                row = row.child(button("int-space", format!("For space {space}"), Where::Space(id.clone())));
            }
            row.child(button("int-everywhere", "Everywhere".into(), Where::Global))
                .child(button("int-team", "In the team's .ideae.toml".into(), Where::Team))
                .child(button("int-never", "Not again".into(), Where::Never(asked.clone())))
                .into_any_element()
        })
        .autohide(false);
        window.push_notification(note, cx);
    }

    /// Puts an integration where it was asked for.
    fn add_integration_to(&mut self, project: &Path, entry: crate::layers::Entry, to: Where, window: &mut Window, cx: &mut Context<Self>) {
        let put = |list: &mut Vec<crate::layers::Entry>| {
            if !list.iter().any(|e| e.id == entry.id) {
                list.push(entry.clone());
            }
        };
        match to {
            Where::Global => {
                let mut settings = crate::state::settings();
                put(&mut settings.integrations);
                self.apply_settings(settings, cx);
            }
            Where::Space(id) => {
                if let Some(space) = self.spaces.iter_mut().find(|s| s.id == id) {
                    put(&mut space.settings.integrations);
                }
                self.resolve_projects(cx);
            }
            Where::Project => {
                if let Some(p) = self.projects.iter_mut().find(|p| p.path == project) {
                    put(&mut p.settings.integrations);
                }
                self.resolve_projects(cx);
            }
            Where::Team => {
                let (dir, written) = (project.to_path_buf(), entry.clone());
                let job = dir.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let done = cx.background_spawn(async move { crate::project_config::write_integration(&job, &written) }).await;
                    let _ = this.update_in(cx, |w, window, cx| match done {
                        // Read again at once, not at the next minute's look.
                        Ok(()) => {
                            if let Some(p) = w.projects.iter_mut().find(|p| p.path == dir) {
                                p.team_read = None;
                            }
                            w.read_team_layers(cx);
                        }
                        Err(e) => window.push_notification(Notification::error(e).title(".ideae.toml"), cx),
                    });
                })
                .detach();
            }
            Where::Never(asked) => {
                let mut settings = crate::state::settings();
                if !settings.hints_off.contains(&asked) {
                    settings.hints_off.push(asked);
                }
                self.apply_settings(settings, cx);
            }
        }
        // The tasks of before: their keys may have an owner now.
        for p in &mut self.projects {
            let integrations = crate::integrations::of_resolved(&p.resolved);
            for t in &mut p.tasks {
                for link in t.links.iter_mut().filter(|l| l.is_ticket() && l.integration.is_empty()) {
                    if let Some(i) = crate::integrations::tickets::owner(&link.key, &integrations) {
                        link.integration = i.id.clone();
                        link.tracker = i.kind.word().into();
                        if let Some(t) = i.tracker() {
                            link.url = t.url_for(&link.key);
                        }
                    }
                }
            }
        }
        self.save_later(cx);
        cx.notify();
    }
}

impl Workspace {
    /// What changed of a task's request since it was last asked, told as
    /// it happens and not again: its CI failed (a task out of view), it was
    /// merged, new review comments came. Nothing asked of the host for it:
    /// what the poll of requests brought.
    pub(super) fn request_changed(&mut self, path: &Path, old: &crate::pr::Pr, new: Option<&crate::pr::Pr>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::pr::{Checks, PrState};
        let Some(new) = new.filter(|new| new.number == old.number) else { return };
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        let label = t.label();
        let ticket = t.ticket().cloned().filter(|l| !l.url.is_empty() && crate::state::with_settings(|s| s.tickets));
        let in_view = self.active.as_deref() == Some(path) && window.is_window_active();
        let (said, buttons): (String, Vec<(&'static str, Then)>) = if new.state == PrState::Merged && old.state != PrState::Merged {
            self.ticket_after_merge(path, window, cx);
            let mut buttons = vec![("Archive task", Then::Archive)];
            if let Some(ticket) = &ticket {
                buttons.push(("Open ticket", Then::Open(ticket.url.clone())));
            }
            (format!("#{} merged: {label}", new.number), buttons)
        } else if new.checks == Checks::Failed && old.checks != Checks::Failed && !in_view {
            (format!("#{} CI failed: {label}", new.number), vec![("Show task", Then::Show)])
        } else if new.open_feedback > old.open_feedback && !in_view {
            let n = new.open_feedback - old.open_feedback;
            (format!("#{} {n} new review comment{}: {label}", new.number, if n == 1 { "" } else { "s" }), vec![("Show task", Then::Show)])
        } else {
            return;
        };
        if !window.is_window_active() && crate::state::with_settings(|s| s.notifications) {
            let (tx, at) = (self.open_task_tx.clone(), path.to_path_buf());
            crate::desktop::notify_with("ideae", &said, move || {
                let _ = tx.send_blocking((at, None));
            });
        }
        let (this, at) = (cx.weak_entity(), path.to_path_buf());
        let note = Notification::info(said).content(move |_, _, cx| {
            let me = cx.entity().downgrade();
            let mut row = h_flex().gap_1();
            for (ix, (label, then)) in buttons.iter().enumerate() {
                let (this, at, me, then) = (this.clone(), at.clone(), me.clone(), then.clone());
                row = row.child(Button::new(("request-note", ix)).xsmall().outline().label(*label).on_click(move |_, window, cx| {
                    match &then {
                        Then::Archive => {
                            let _ = this.update(cx, |w, cx| w.archive_task(&at, window, cx));
                        }
                        Then::Open(url) => {
                            let _ = this.update(cx, |w, cx| w.open_link(&at, url, cx));
                        }
                        Then::Show => {
                            let _ = this.update(cx, |w, cx| w.switch(&at, window, cx));
                        }
                    }
                    let _ = me.update(cx, |note, cx| note.dismiss(window, cx));
                }));
            }
            row.into_any_element()
        });
        window.push_notification(note, cx);
    }
}

/// What a button of a request's notice does.
#[derive(Clone)]
enum Then {
    Archive,
    Open(String),
    Show,
}

/// One task's line: its title, linked to its ticket, and its request.
fn standup_line(t: &super::TaskEntry) -> String {
    let title = t.label();
    let title = match t.ticket().filter(|l| !l.url.is_empty()) {
        Some(l) => format!("[{}]({}){}", l.key, l.url, title.strip_prefix(&l.key).unwrap_or(&format!(" {title}"))),
        None => title,
    };
    match &t.pr {
        Some(pr) => format!("{title} · [{}]({})", pr.label(), pr.url),
        None => title,
    }
}

/// Lines grouped by their ticket, those of none last.
fn standup(mut rows: Vec<(String, String)>) -> String {
    rows.sort_by(|a, b| (a.0.is_empty(), &a.0).cmp(&(b.0.is_empty(), &b.0)));
    rows.into_iter().map(|(_, line)| format!("- {line}")).collect::<Vec<_>>().join("\n")
}

/// Where an integration offered goes.
#[derive(Clone)]
enum Where {
    Global,
    Space(String),
    Project,
    Team,
    Never(String),
}
