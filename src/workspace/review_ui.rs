//! Review…: a colleague's request or branch as a task of its own, to review.
//! The requests open now, those asking your review first; then origin's
//! branches no request is of. The prompt is shown and goes as it stands;
//! the task is marked a review (Push asks first, Review again).

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use super::{Workspace, agents_ui};
use crate::git::OriginBranch;
use crate::integrations::{Integration, OpenRequest};

/// Branches listed at most: more are found by typing.
const BRANCHES_SHOWN: usize = 40;

/// What can be chosen to review.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Pick {
    Request(OpenRequest),
    Branch(OriginBranch),
}

/// A task of the project, as far as finding the one of a pick goes.
#[derive(Clone, Debug, Default)]
pub(super) struct TaskOf {
    branch: String,
    path: PathBuf,
    /// The links of its requests.
    requests: Vec<String>,
    /// The number of its branch's request.
    number: Option<u64>,
}

pub(super) fn tasks_of(tasks: &[super::TaskEntry]) -> Vec<TaskOf> {
    tasks
        .iter()
        .filter(|t| !t.main)
        .map(|t| TaskOf {
            branch: t.branch.clone(),
            path: t.path.clone(),
            requests: t.links.iter().filter(|l| l.what == "pr").map(|l| l.url.clone()).collect(),
            number: t.pr.as_ref().map(|pr| pr.number),
        })
        .collect()
}

/// The task already there for a pick: a request's by its link or number
/// (a fork's head is `request-N` here, or a name of origin's that is not
/// its), or on its head where origin has it; a branch's on that branch.
pub(super) fn task_for<'a>(tasks: &'a [TaskOf], pick: &Pick) -> Option<&'a PathBuf> {
    let found = match pick {
        Pick::Request(r) => tasks.iter().find(|t| t.requests.contains(&r.url) || t.number == Some(r.number) || (!r.cross && t.branch == r.head)),
        Pick::Branch(b) => tasks.iter().find(|t| t.branch == b.name),
    };
    found.map(|t| &t.path)
}

/// What the host and origin told, as the dialog shows it.
#[derive(Default)]
struct Told {
    /// None while asked; Err what the host said.
    requests: Option<Result<Vec<OpenRequest>, String>>,
    /// origin's as last fetched, then as fetched now.
    branches: Option<Vec<OriginBranch>>,
    /// origin's default branch: none to review.
    base: Option<String>,
    /// The rows for the filter they were listed for: listed again when it
    /// or what was told changes, not at every frame.
    listed: Option<(String, Rc<(Vec<Pick>, usize)>)>,
}

impl Told {
    fn rows(&mut self, filter: &str) -> Rc<(Vec<Pick>, usize)> {
        match &self.listed {
            Some((was, listed)) if was == filter => listed.clone(),
            _ => {
                let requests = self.requests.as_ref().and_then(|r| r.as_ref().ok()).map(Vec::as_slice).unwrap_or_default();
                let listed = Rc::new(rows(requests, self.branches.as_deref().unwrap_or_default(), self.base.as_deref(), filter));
                self.listed = Some((filter.to_string(), listed.clone()));
                listed
            }
        }
    }
}

/// The rows as listed: requests asking your review, then the others, yours
/// last; then origin's branches of no open request, but its default. Those
/// `filter` matches (number, title, branch, author); of the branches at most
/// `BRANCHES_SHOWN`, with how many more there are.
pub(super) fn rows(requests: &[OpenRequest], branches: &[OriginBranch], base: Option<&str>, filter: &str) -> (Vec<Pick>, usize) {
    let filter = filter.trim().to_lowercase();
    let matches = |texts: &[&str]| filter.is_empty() || texts.iter().any(|t| t.to_lowercase().contains(&filter));
    let mut listed: Vec<&OpenRequest> = requests.iter().filter(|r| matches(&[&format!("#{}", r.number), &r.title, &r.head, &r.author])).collect();
    listed.sort_by_key(|r| (!r.for_me, r.mine));
    let mut picks: Vec<Pick> = listed.into_iter().cloned().map(Pick::Request).collect();
    let requested: HashSet<&str> = requests.iter().filter(|r| !r.cross).map(|r| r.head.as_str()).collect();
    let (mut shown, mut more) = (0, 0);
    for b in branches.iter().filter(|b| Some(b.name.as_str()) != base && !requested.contains(b.name.as_str()) && matches(&[&b.name, &b.author])) {
        if shown < BRANCHES_SHOWN {
            picks.push(Pick::Branch(b.clone()));
            shown += 1;
        } else {
            more += 1;
        }
    }
    (picks, more)
}

/// Whether a prompt names what was chosen: a request by its number or
/// link, a branch by its name.
fn names(prompt: &str, pick: &Pick) -> bool {
    match pick {
        Pick::Request(r) => {
            let number = format!("#{}", r.number);
            // #12 is not #1.
            prompt.contains(&r.url) || prompt.match_indices(&number).any(|(at, _)| prompt.get(at + number.len()..).is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit())))
        }
        Pick::Branch(b) => prompt.contains(&b.name),
    }
}

/// What the agent is told, shown in the dialog to change: the review asked,
/// the ticket its title or branch names, and not to push.
pub(super) fn review_prompt(pick: &Pick, integrations: &[Integration], base: &str) -> String {
    // A key its tracker owns, never guessed: "UTF-8" in a title is none.
    let issue = |text: &str| {
        crate::integrations::tickets::keys_in(text)
            .into_iter()
            .find_map(|key| crate::integrations::tickets::owner(&key, integrations).map(|owner| format!("{} issue {key}", owner.kind.label())))
    };
    match pick {
        Pick::Request(r) => {
            let what = if r.url.contains("/-/merge_requests/") { "MR" } else { "PR" };
            match issue(&format!("{} {}", r.head, r.title)) {
                Some(issue) => format!("Review {issue}, {what} #{} ({}). Do not push.", r.number, r.url),
                None => format!("Review {what} #{} ({}). Do not push.", r.number, r.url),
            }
        }
        Pick::Branch(b) => match issue(&b.name) {
            Some(issue) => format!("Review {issue}: the branch {} against origin/{base}. Do not push.", b.name),
            None => format!("Review the branch {} against origin/{base}. Do not push.", b.name),
        },
    }
}

impl Workspace {
    /// Review…: a request or a branch of origin chosen, its prompt shown.
    pub fn review_dialog(&mut self, project: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.review_dialog_with(project, None, window, cx);
    }

    /// Review… with `find` in its filter: from New task, a branch or key
    /// origin has. A remote project's once its host is connected: all it
    /// reads is read there.
    pub fn review_dialog_with(&mut self, project: usize, find: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.projects.get(project).map(|p| p.path.clone()) else { return };
        if crate::hosts::is_remote(&repo) {
            let key = repo.clone();
            return self.with_host(&repo, window, cx, Box::new(move |this, window, cx| {
                if let Some(project) = this.projects.iter().position(|p| p.path == key) {
                    this.open_review_dialog(project, find, window, cx);
                }
            }));
        }
        self.open_review_dialog(project, find, window, cx);
    }

    fn open_review_dialog(&mut self, project: usize, find: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(project) else { return };
        let (name, repo) = (p.name.clone(), p.path.clone());
        let integrations = Rc::new(crate::integrations::of_resolved(&p.resolved));
        // What has a task: Switch to it, not a second one.
        let tasks: Rc<Vec<TaskOf>> = Rc::new(tasks_of(&p.tasks));
        // The project's own base, where it names one: reviewed against, not listed.
        let project_base = Some(p.values().base.trim().trim_start_matches("origin/").to_string()).filter(|b| !b.is_empty());
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter: number, title, branch or author"));
        if let Some(find) = find {
            filter.update(cx, |f, cx| f.set_value(find, window, cx));
        }
        let prompt = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 8).placeholder("For the agent: sent as it stands"));
        let agent = Rc::new(RefCell::new(self.project_agent(project)));
        let told: Rc<RefCell<Told>> = Rc::default();
        let chosen: Rc<RefCell<Option<Pick>>> = Rc::default();
        // What choosing put in the prompt: a prompt of yours is not replaced.
        let filled: Rc<RefCell<String>> = Rc::default();
        let listening = Rc::new(vec![cx.subscribe_in(&filter, window, |_, _, event: &InputEvent, window, _| {
            if matches!(event, InputEvent::Change) {
                window.refresh();
            }
        })]);
        // Asked off the UI thread: origin's branches as last fetched, then the
        // host's requests, then origin fetched for the branches as they are now.
        {
            let (repo, told, chosen, filled, prompt, integrations) = (repo.clone(), told.clone(), chosen.clone(), filled.clone(), prompt.clone(), integrations.clone());
            cx.spawn_in(window, async move |_, cx| {
                let job = repo.clone();
                let (branches, base) = cx.background_spawn(crate::procs::on_thread(move || (crate::git::origin_branches(&job), crate::git::remote_default(&job)))).await;
                {
                    let mut told = told.borrow_mut();
                    told.branches = Some(branches);
                    told.base = project_base.or(base);
                    told.listed = None;
                }
                let _ = cx.update(|window, _| window.refresh());
                let job = repo.clone();
                let requests = cx
                    .background_spawn(crate::procs::on_thread(move || crate::integrations::code_host(&job).and_then(|host| host.open_requests(&job)).map_err(|e| e.to_string())))
                    .await;
                // A branch chosen before its request was listed is the request:
                // its task is made as one, its link and title with it.
                let became = match (&*chosen.borrow(), &requests) {
                    (Some(Pick::Branch(b)), Ok(requests)) => requests.iter().find(|r| !r.cross && r.head == b.name).cloned().map(Pick::Request),
                    _ => None,
                };
                {
                    let mut told = told.borrow_mut();
                    told.requests = Some(requests);
                    told.listed = None;
                }
                if let Some(pick) = became {
                    let base = told.borrow().base.clone().unwrap_or_else(|| "main".into());
                    let text = review_prompt(&pick, &integrations, &base);
                    *chosen.borrow_mut() = Some(pick);
                    let _ = cx.update(|window, cx| {
                        if prompt.read(cx).value() == filled.borrow().as_str() {
                            *filled.borrow_mut() = text.clone();
                            prompt.update(cx, |p, cx| p.set_value(text, window, cx));
                        }
                    });
                }
                let _ = cx.update(|window, _| window.refresh());
                let job = repo.clone();
                let fetched = cx
                    .background_spawn(crate::procs::on_thread(move || crate::git::fetch_all(&job).map(|_| crate::git::origin_branches(&job))))
                    .await;
                if let Ok(branches) = fetched {
                    let mut told = told.borrow_mut();
                    told.branches = Some(branches);
                    told.listed = None;
                    drop(told);
                    let _ = cx.update(|window, _| window.refresh());
                }
            })
            .detach();
        }
        let this = cx.weak_entity();
        let start = {
            let (chosen, prompt, agent, this) = (chosen.clone(), prompt.clone(), agent.clone(), this.clone());
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let Some(pick) = chosen.borrow().clone() else { return };
                let text = prompt.read(cx).value().trim().to_string();
                let agent = agent.borrow().clone();
                window.close_dialog(cx);
                let _ = this.update(cx, |w, cx| w.start_review(project, pick, text, agent, window, cx));
            })
        };
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let _ = &listening;
            let (filter, prompt, told, chosen, filled, agent, start, focused) =
                (filter.clone(), prompt.clone(), told.clone(), chosen.clone(), filled.clone(), agent.clone(), start.clone(), focused.clone());
            let (integrations, tasks) = (integrations.clone(), tasks.clone());
            let picked = chosen.borrow().clone();
            let has_task = picked.as_ref().is_some_and(|p| task_for(&tasks, p).is_some());
            let nothing = picked.is_none();
            let several = crate::state::with_settings(|s| s.several_agents());
            let ok = start.clone();
            dialog
                .title(format!("Review in {name}"))
                .w(px(720.))
                .on_ok(move |_, window, cx| {
                    ok(window, cx);
                    false
                })
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let filter = filter.clone();
                        window.defer(cx, move |window, cx| filter.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    let muted = cx.theme().muted_foreground;
                    let listed = told.borrow_mut().rows(&filter.read(cx).value());
                    let (picks, more) = (&listed.0, listed.1);
                    let told = told.borrow();
                    let base = told.base.clone().unwrap_or_else(|| "main".into());
                    let mut list = v_flex().id("review-list").gap_0p5().max_h(px(380.)).overflow_y_scroll();
                    list = list.child(div().pt_1().text_xs().text_color(muted).child(match &told.requests {
                        None => "Requests: asking the code host…".to_string(),
                        Some(Err(why)) => format!("Requests: {why}"),
                        Some(Ok(all)) if all.is_empty() => "Requests: none open".to_string(),
                        Some(Ok(_)) => "Requests open, those asking your review first".to_string(),
                    }));
                    let mut branches_said = false;
                    for (ix, pick) in picks.iter().enumerate() {
                        if matches!(pick, Pick::Branch(_)) && !branches_said {
                            branches_said = true;
                            list = list.child(div().pt_2().text_xs().text_color(muted).child("Branches on origin with no request open"));
                        }
                        let on = picked.as_ref() == Some(pick);
                        let (head, detail, marks): (String, String, Vec<&str>) = match pick {
                            Pick::Request(r) => (
                                format!("#{} {}", r.number, r.title),
                                format!("{} · {}", r.author, r.head),
                                [(r.for_me, "asks your review"), (r.draft, "draft"), (r.mine, "yours"), (r.cross, "from a fork")].into_iter().filter(|(on, _)| *on).map(|(_, mark)| mark).collect(),
                            ),
                            Pick::Branch(b) => (b.name.clone(), format!("last commit by {}, {}", b.author, b.when), Vec::new()),
                        };
                        let task_mark = task_for(&tasks, pick).is_some().then_some("has a task");
                        let (pick, chosen, filled, prompt, integrations, base) = (pick.clone(), chosen.clone(), filled.clone(), prompt.clone(), integrations.clone(), base.clone());
                        list = list.child(
                            v_flex()
                                .id(("review-row", ix))
                                .test_support()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .when(on, |row| row.bg(cx.theme().accent))
                                .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().min_w_0().truncate().text_sm().child(head))
                                        .children(marks.into_iter().chain(task_mark).map(|mark| div().flex_none().text_xs().text_color(muted).child(mark))),
                                )
                                .child(div().text_xs().text_color(muted).truncate().child(detail))
                                .on_click(move |_, window, cx| {
                                    *chosen.borrow_mut() = Some(pick.clone());
                                    let text = review_prompt(&pick, &integrations, &base);
                                    // What choosing wrote follows the choice; what you wrote stays.
                                    let (current, was) = (prompt.read(cx).value().to_string(), filled.replace(text.clone()));
                                    let now = if current.trim().is_empty() || current == was {
                                        Some(text)
                                    } else if !was.is_empty() && current.contains(&was) {
                                        Some(current.replacen(&was, &text, 1))
                                    } else {
                                        None
                                    };
                                    if let Some(now) = now {
                                        prompt.update(cx, |p, cx| p.set_value(now, window, cx));
                                    }
                                    window.refresh();
                                }),
                        );
                    }
                    if more > 0 {
                        list = list.child(div().pt_1().text_xs().text_color(muted).child(format!("and {more} more branches: type to narrow")));
                    }
                    if told.branches.is_none() {
                        list = list.child(div().pt_2().text_xs().text_color(muted).child("Branches: reading origin…"));
                    }
                    content.child(
                        v_flex()
                            .gap_2()
                            .child(Input::new(&filter))
                            .child(list)
                            .child(Textarea::new(&prompt))
                            // A prompt of yours that names another request or branch goes as it stands.
                            .when_some(picked.as_ref().filter(|pick| !names(&prompt.read(cx).value(), pick)), |form, pick| {
                                let what = match pick {
                                    Pick::Request(r) => format!("#{}", r.number),
                                    Pick::Branch(b) => b.name.clone(),
                                };
                                form.child(div().text_xs().text_color(cx.theme().warning).child(format!("The prompt does not name {what}: it goes as it stands.")))
                            })
                            .when(several, |form| {
                                form.child(h_flex().gap_2().items_center().child(div().text_sm().child("Agent")).child(agents_ui::picker("review-agent", agent.clone())))
                            })
                            .child(div().text_xs().text_color(muted).child(
                                "A task on its branch, following origin's, marked a review: Push asks first, and what a colleague pushes since is offered as Review again.",
                            )),
                    )
                })
                .footer({
                    let start = start.clone();
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("review-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(
                            Button::new("review-start")
                                .primary()
                                .label(if has_task { "Switch to its task" } else { "Review" })
                                .disabled(nothing)
                                .on_click(move |_, window, cx| start(window, cx)),
                        )
                })
        });
    }

    /// The task of what was chosen: its own if it has one; a request's
    /// through its head (fetched, a fork's too), a branch's on origin's.
    /// Marked a review either way; `prompt` sent as it stands.
    pub(super) fn start_review(&mut self, project: usize, pick: Pick, prompt: String, agent: String, window: &mut Window, cx: &mut Context<Self>) {
        let there = self.projects.get(project).and_then(|p| task_for(&tasks_of(&p.tasks), &pick).cloned());
        if let Some(path) = there {
            return self.switch(&path, window, cx);
        }
        let agents = vec![agent];
        match pick {
            Pick::Request(r) => {
                let kind = crate::integrations::request_of_url(&r.url).map_or(crate::integrations::Kind::GitHub, |(kind, _)| kind);
                self.create_request_task(project, kind, r.number, r.url, prompt, agents, true, window, cx);
            }
            Pick::Branch(b) => {
                let title = format!("Review {}", b.name);
                self.create_task_on_branch(project, b.name, title, prompt, agents, true, true, window, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BRANCHES_SHOWN, Pick, review_prompt, rows};
    use crate::git::OriginBranch;
    use crate::integrations::OpenRequest;

    fn request(number: u64, head: &str, for_me: bool, mine: bool) -> OpenRequest {
        OpenRequest { number, title: format!("Title {number}"), url: format!("https://github.com/o/r/pull/{number}"), head: head.into(), author: "ann".into(), for_me, mine, ..Default::default() }
    }

    fn branch(name: &str) -> OriginBranch {
        OriginBranch { name: name.into(), author: "bob".into(), when: "2 days ago".into() }
    }

    /// Requests asking your review first, yours last; then origin's branches
    /// of no request, but its default; a filter over all of it.
    #[test]
    fn requests_asking_you_come_first_then_branches_of_none() {
        let requests = [request(1, "a", false, true), request(2, "b", false, false), request(3, "c", true, false)];
        let branches = [branch("main"), branch("a"), branch("DEV-7_fix"), branch("c")];
        let (picks, more) = rows(&requests, &branches, Some("main"), "");
        let names: Vec<String> = picks.iter().map(|p| match p { Pick::Request(r) => format!("#{}", r.number), Pick::Branch(b) => b.name.clone() }).collect();
        assert_eq!((names, more), (vec!["#3".to_string(), "#2".into(), "#1".into(), "DEV-7_fix".into()], 0));
        let (picks, _) = rows(&requests, &branches, Some("main"), "dev-7");
        assert_eq!(picks, [Pick::Branch(branch("DEV-7_fix"))], "the filter, any case");
        let (picks, _) = rows(&requests, &branches, Some("main"), "#2");
        assert_eq!(picks, [Pick::Request(request(2, "b", false, false))]);

        let many: Vec<OriginBranch> = (0..BRANCHES_SHOWN + 5).map(|n| branch(&format!("b{n}"))).collect();
        let (picks, more) = rows(&[], &many, None, "");
        assert_eq!((picks.len(), more), (BRANCHES_SHOWN, 5));
    }

    /// The prompt names the review, the ticket in its branch or title that
    /// a tracker set up owns, and says not to push.
    #[test]
    fn the_prompt_names_the_request_and_its_ticket() {
        use crate::integrations::{Integration, Kind};
        use crate::layers::Layer;
        let jira = [Integration { id: "jira".into(), kind: Kind::Jira, name: "Jira".into(), base: "https://jira.example.com".into(), keys: vec!["DEV".into()], from: Layer::Global, account: String::new(), connected: false, agent_read: false }];
        let r = OpenRequest { head: "DEV-7_fixLogin".into(), ..request(12, "", false, false) };
        assert_eq!(review_prompt(&Pick::Request(r.clone()), &jira, "main"), "Review Jira issue DEV-7, PR #12 (https://github.com/o/r/pull/12). Do not push.");
        assert_eq!(review_prompt(&Pick::Request(r), &[], "main"), "Review PR #12 (https://github.com/o/r/pull/12). Do not push.", "a key no tracker owns is none");
        let utf = OpenRequest { title: "Support UTF-8".into(), ..request(14, "utf", false, false) };
        assert!(review_prompt(&Pick::Request(utf), &jira, "main").starts_with("Review PR #14 "));
        assert_eq!(review_prompt(&Pick::Branch(branch("DEV-9_x")), &jira, "develop"), "Review Jira issue DEV-9: the branch DEV-9_x against origin/develop. Do not push.");
        let plain = request(13, "fix-login", false, false);
        assert_eq!(review_prompt(&Pick::Request(plain), &[], "main"), "Review PR #13 (https://github.com/o/r/pull/13). Do not push.");
        let mr = OpenRequest { url: "https://gitlab.com/g/p/-/merge_requests/4".into(), ..request(4, "x", false, false) };
        assert!(review_prompt(&Pick::Request(mr), &[], "main").starts_with("Review MR #4 "));
        assert_eq!(review_prompt(&Pick::Branch(branch("spike")), &[], "master"), "Review the branch spike against origin/master. Do not push.");
    }

    /// A pick's task: a request's by its link or number, whatever its branch
    /// is named here; a fork's head is no branch of origin's.
    #[test]
    fn a_request_s_task_is_found_by_its_link_not_its_branch() {
        use super::{TaskOf, names, task_for};
        let task = |branch: &str, path: &str, requests: &[&str], number: Option<u64>| TaskOf { branch: branch.into(), path: path.into(), requests: requests.iter().map(|r| r.to_string()).collect(), number };
        let tasks = [task("request-9", "/w/request-9", &["https://github.com/o/r/pull/9"], None), task("fix", "/w/fix", &[], None), task("DEV-7", "/w/DEV-7", &[], Some(7))];
        let fork = OpenRequest { cross: true, ..request(9, "main", false, false) };
        assert_eq!(task_for(&tasks, &Pick::Request(fork)).map(|p| p.to_str().unwrap()), Some("/w/request-9"), "by its link");
        let fork_fix = OpenRequest { cross: true, ..request(11, "fix", false, false) };
        assert_eq!(task_for(&tasks, &Pick::Request(fork_fix)), None, "a fork's fix is not origin's");
        assert_eq!(task_for(&tasks, &Pick::Request(request(7, "DEV-7", false, false))).map(|p| p.to_str().unwrap()), Some("/w/DEV-7"));
        assert_eq!(task_for(&tasks, &Pick::Branch(branch("fix"))).map(|p| p.to_str().unwrap()), Some("/w/fix"));
        assert!(names("Review PR #12 now", &Pick::Request(request(12, "x", false, false))));
        assert!(!names("Review PR #12 now", &Pick::Request(request(1, "x", false, false))), "#12 is not #1");
    }
}

// They script repositories with bash, which Windows lacks or has as WSL's.
#[cfg(all(test, unix))]
mod dialog_tests {
    use crate::state::{ProjectState, State};
    use crate::workspace::Workspace;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};

    /// A branch of origin chosen in Review…: its prompt shown, and the task
    /// made on it is a review, named after it.
    #[gpui_kit::test]
    fn a_branch_chosen_is_reviewed_in_a_task_of_its_own(cx: &mut TestAppContext) {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        // The main thread's terminal runs on a thread of its own.
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-review-dialog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |script: &str| assert!(std::process::Command::new("bash").arg("-ec").arg(script).current_dir(&dir).status().unwrap().success(), "{script}");
        sh("git init -q --bare -b main remote.git
            git clone -q remote.git theirs 2>/dev/null; cd theirs; git config user.email someone@else; git config user.name Someone
            echo one > f.txt; git add f.txt; git commit -qm base; git push -q origin main 2>/dev/null
            git checkout -q -b DEV-7_fix; echo x > t.txt; git add t.txt; git commit -qm theirs; git push -q origin DEV-7_fix 2>/dev/null
            cd ..; git clone -q remote.git repo 2>/dev/null; cd repo; git config user.email t@t; git config user.name t");
        let repo = dir.join("repo");
        // SAFETY: set before anything of the app reads them, one window at a time.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        let saved = State { projects: vec![ProjectState { path: repo.clone(), ..Default::default() }], ..Default::default() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&saved).unwrap()).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        let (handle, workspace) = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(1000.), px(800.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(Vec::new(), Some(None), None, window, cx))).expect("a test window")
        });
        let wait = |cx: &mut TestAppContext, what: &str, done: &mut dyn FnMut(&mut TestAppContext) -> bool| {
            for _ in 0..1000 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                cx.executor().advance_clock(std::time::Duration::from_millis(10));
            }
            panic!("waited in vain for {what}");
        };
        // From New task: its key has a branch on origin, said apart, with Review instead.
        cx.update_window(handle.into(), |_, window, cx| {
            workspace.update(cx, |w, cx| w.new_task_dialog(0, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.input("DEV-7", cx)).unwrap();
        wait(cx, "what origin has of DEV-7", &mut |cx| {
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                window.try_find("on-origin").is_some()
            })
            .unwrap()
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("review-instead", cx);
            window.render_frame(cx);
        })
        .unwrap();
        // origin's branches are read off the UI thread; main is no branch to review.
        wait(cx, "origin's branches", &mut |cx| {
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                window.try_find(("review-row", 0usize)).is_some()
            })
            .unwrap()
        });
        let second = cx.update_window(handle.into(), |_, window, _| window.try_find(("review-row", 1usize)).is_some()).unwrap();
        assert!(!second, "DEV-7_fix alone: main is the base, and the filter is DEV-7");
        cx.update_window(handle.into(), |_, window, cx| {
            window.click(("review-row", 0usize), cx);
            window.render_frame(cx);
            window.click("review-start", cx);
        })
        .unwrap();
        // Its row is there at once, its worktree on the way: that is git's test
        // (a task's terminals do not run in this one).
        let made = workspace.update(cx, |w, _| w.projects[0].tasks.iter().filter(|t| !t.main).map(|t| (t.title.clone(), t.branch.clone(), t.review, t.prompt.clone())).collect::<Vec<_>>());
        assert_eq!(
            made,
            [("Review DEV-7_fix".to_string(), "DEV-7_fix".to_string(), true, "Review the branch DEV-7_fix against origin/main. Do not push.".to_string())],
            "a review of the branch, its prompt as shown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
