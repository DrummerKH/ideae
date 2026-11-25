//! Settings over a layer: Global, a space's, a project's. On a narrower
//! one the dialog shows only what that layer may set, each with where its
//! value comes from and a ↺ while it is set there; on Global, a setting a
//! space or a project sets otherwise says so.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::layers::{self, Field, Layer, Overrides, Resolved};
use crate::state;

#[derive(Clone, Debug, PartialEq)]
pub struct SpaceDraft {
    pub id: String,
    pub name: String,
    pub settings: Overrides,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectDraft {
    pub path: PathBuf,
    pub name: String,
    pub space: Option<String>,
    pub settings: Overrides,
    pub team: Overrides,
}

/// The layers under Global as the dialog edits them, applied on Save.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layers {
    pub spaces: Vec<SpaceDraft>,
    pub projects: Vec<ProjectDraft>,
}

pub type LayersDraft = Rc<RefCell<Layers>>;

/// Which layer the dialog shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Scope {
    #[default]
    Global,
    Space(usize),
    Project(usize),
}

impl Layers {
    fn space_of(&self, project: &ProjectDraft) -> Option<&SpaceDraft> {
        project.space.as_ref().and_then(|id| self.spaces.iter().find(|s| &s.id == id))
    }

    /// What the layers under `scope` give, its own left out: what it
    /// falls back to, and where each value comes from.
    pub fn under(&self, settings: &state::Settings, scope: Scope) -> Resolved {
        let global = layers::global_layer(settings);
        match scope {
            Scope::Global => layers::resolve(&Overrides::default(), None, None, &Overrides::default()),
            Scope::Space(_) => layers::resolve(&global, None, None, &Overrides::default()),
            Scope::Project(pi) => match self.projects.get(pi) {
                Some(p) => layers::resolve(&global, self.space_of(p).map(|s| &s.settings), Some(&p.team), &Overrides::default()),
                None => Resolved::default(),
            },
        }
    }

    fn own(&self, scope: Scope) -> Option<&Overrides> {
        match scope {
            Scope::Global => None,
            Scope::Space(si) => self.spaces.get(si).map(|s| &s.settings),
            Scope::Project(pi) => self.projects.get(pi).map(|p| &p.settings),
        }
    }

    fn own_mut(&mut self, scope: Scope) -> Option<&mut Overrides> {
        match scope {
            Scope::Global => None,
            Scope::Space(si) => self.spaces.get_mut(si).map(|s| &mut s.settings),
            Scope::Project(pi) => self.projects.get_mut(pi).map(|p| &mut p.settings),
        }
    }

    pub fn label(&self, scope: Scope) -> String {
        match scope {
            Scope::Global => "Global".into(),
            Scope::Space(si) => self.spaces.get(si).map(|s| format!("Space: {}", s.name)).unwrap_or_default(),
            Scope::Project(pi) => self.projects.get(pi).map(|p| format!("Project: {}", p.name)).unwrap_or_default(),
        }
    }

    /// Who sets a setting otherwise than Global, to tell on Global's page.
    pub fn overridden(&self, id: &str) -> Option<String> {
        let mut who: Vec<String> = Vec::new();
        who.extend(self.spaces.iter().filter(|s| s.settings.is_set(id)).map(|s| format!("space {}", s.name)));
        who.extend(self.projects.iter().filter(|p| p.settings.is_set(id) || p.team.is_set(id)).map(|p| format!("project {}", p.name)));
        match who.len() {
            0 => None,
            1..=3 => Some(format!("Set otherwise by {}: edit it there, or reset it there.", who.join(", "))),
            n => Some(format!("Set otherwise by {}, and {} more: edit it there, or reset it there.", who[..3].join(", "), n - 3)),
        }
    }

    /// Where a value of `scope`'s comes from, as its row says it.
    fn source(&self, scope: Scope, from: Layer) -> String {
        match (from, scope) {
            (Layer::Space, Scope::Project(pi)) => self.projects.get(pi).and_then(|p| self.space_of(p)).map(|s| format!("space {}", s.name)).unwrap_or_else(|| "its space".into()),
            (layer, _) => layer.label().to_string(),
        }
    }
}

/// The switch over the dialog: Global, each space, each project.
pub fn switch(layers: &LayersDraft, scope: &Rc<Cell<Scope>>) -> impl IntoElement {
    let label = layers.borrow().label(scope.get());
    let (layers, scope) = (layers.clone(), scope.clone());
    Button::new("settings-layer").small().outline().label(format!("{label} ▾")).dropdown_menu(move |menu, _, _| {
        let all = layers.borrow();
        let now = scope.get();
        let mut menu = menu;
        let go = |menu: gpui_kit::component::menu::PopupMenu, to: Scope, text: String| {
            let scope = scope.clone();
            menu.item(PopupMenuItem::new(text).checked(now == to).on_click(move |_, window, _| {
                scope.set(to);
                window.refresh();
            }))
        };
        menu = go(menu, Scope::Global, "Global: every project".into());
        if !all.spaces.is_empty() {
            menu = menu.separator();
            for (si, s) in all.spaces.iter().enumerate() {
                menu = go(menu, Scope::Space(si), format!("Space: {}", s.name));
            }
        }
        if !all.projects.is_empty() {
            menu = menu.separator();
            for (pi, p) in all.projects.iter().enumerate() {
                menu = go(menu, Scope::Project(pi), format!("Project: {}", p.name));
            }
        }
        menu
    })
}

/// The page of a space's or a project's layer.
pub fn page(draft: &Rc<RefCell<state::Settings>>, layers: &LayersDraft, scope: Scope) -> SettingPage {
    let layer = match scope {
        Scope::Global => Layer::Global,
        Scope::Space(_) => Layer::Space,
        Scope::Project(_) => Layer::Project,
    };
    let all = layers.borrow();
    let under = all.under(&draft.borrow(), scope);
    let items: Vec<SettingItem> = layers::FIELDS.iter().filter(|f| f.allowed(layer)).map(|f| item(f, draft, layers, scope, &under)).collect();
    let (title, about) = match scope {
        Scope::Space(si) => (
            all.spaces.get(si).map(|s| s.name.clone()).unwrap_or_default(),
            "What this space sets applies to its projects, over Settings; a project's own settings go over it. A project in no space has Settings and its own alone.",
        ),
        _ => (
            all.projects.get(match scope { Scope::Project(pi) => pi, _ => 0 }).map(|p| p.name.clone()).unwrap_or_default(),
            "What this project sets is kept on this machine and goes over its space, its team's .ideae.toml and Settings.",
        ),
    };
    SettingPage::new(title)
        .default_open(true)
        .resettable(false)
        .description(about)
        .group(SettingGroup::new().items(items))
        .group(integrations_group(draft, layers, scope))
}

/// One setting of a layer: its control shows what applies, its own value
/// or the one under it; a change sets it here, ↺ gives it back.
fn item(field: &'static Field, draft: &Rc<RefCell<state::Settings>>, layers: &LayersDraft, scope: Scope, under: &Resolved) -> SettingItem {
    let all = layers.borrow();
    let own = all.own(scope).cloned().unwrap_or_default();
    let inherited = crate::workspace::Workspace::describe_value(field.id, &under.values);
    let from = all.source(scope, under.from.of(field.id));
    let said = if own.is_set(field.id) {
        format!("{} Set here; without it: {inherited} ({from}).", field.about)
    } else {
        format!("{} {inherited}, from {from}.", field.about)
    };
    let id = field.id;
    // Reads and writes this layer's value of the field.
    let get = {
        let layers = layers.clone();
        move || layers.borrow().own(scope).cloned().unwrap_or_default()
    };
    let set = {
        let layers = layers.clone();
        move |change: &dyn Fn(&mut Overrides)| {
            if let Some(own) = layers.borrow_mut().own_mut(scope) {
                change(own);
            }
        }
    };
    let dirty = {
        let get = get.clone();
        move |_: &App| get().is_set(id)
    };
    let reset = {
        let set = set.clone();
        move |window: &mut Window, _: &mut App| {
            set(&|o| o.clear(id));
            window.refresh();
        }
    };
    let values = under.values.clone();
    let row = match id {
        "agent" => {
            let options: Vec<(SharedString, SharedString)> = draft.borrow().agents.iter().map(|p| (SharedString::from(p.id.clone()), SharedString::from(p.name.clone()))).collect();
            let (get, set) = (get.clone(), set.clone());
            SettingItem::new(
                field.title,
                SettingField::dropdown(options, move |_| get().agent.unwrap_or_else(|| values.agent.clone()).into(), move |v: SharedString, _| set(&|o| o.agent = Some(v.to_string())))
                    .on_reset(dirty, reset),
            )
        }
        "pull_requests" | "language_servers" | "remote_browser" => {
            let (get, set) = (get.clone(), set.clone());
            let fallback = match id {
                "pull_requests" => values.pull_requests,
                "language_servers" => values.language_servers,
                _ => values.remote_browser,
            };
            SettingItem::new(
                field.title,
                SettingField::switch(
                    move |_| {
                        let o = get();
                        match id {
                            "pull_requests" => o.pull_requests,
                            "language_servers" => o.language_servers,
                            _ => o.remote_browser,
                        }
                        .unwrap_or(fallback)
                    },
                    move |on, _| {
                        set(&|o| match id {
                            "pull_requests" => o.pull_requests = Some(on),
                            "language_servers" => o.language_servers = Some(on),
                            _ => o.remote_browser = Some(on),
                        })
                    },
                )
                .on_reset(dirty, reset),
            )
        }
        "links_open_in" => {
            let (get, set) = (get.clone(), set.clone());
            let options: Vec<(SharedString, SharedString)> = LINKS_OPEN_IN.iter().map(|(k, v)| (SharedString::from(*k), SharedString::from(*v))).collect();
            SettingItem::new(
                field.title,
                SettingField::dropdown(options, move |_| get().links_open_in.unwrap_or_else(|| values.links_open_in.clone()).into(), move |v: SharedString, _| set(&|o| o.links_open_in = Some(v.to_string())))
                    .on_reset(dirty, reset),
            )
        }
        "closes" => {
            let (get, set) = (get.clone(), set.clone());
            let options: Vec<(SharedString, SharedString)> = CLOSES.iter().map(|(k, v)| (SharedString::from(*k), SharedString::from(*v))).collect();
            SettingItem::new(
                field.title,
                SettingField::dropdown(options, move |_| get().closes.unwrap_or_else(|| values.closes.clone()).into(), move |v: SharedString, _| set(&|o| o.closes = Some(v.to_string())))
                    .on_reset(dirty, reset),
            )
        }
        "check_to_agent" => {
            let (get, set) = (get.clone(), set.clone());
            SettingItem::new(
                field.title,
                SettingField::number_input(
                    NumberFieldOptions { min: 0., max: 5., step: 1. },
                    move |_| f64::from(get().check_to_agent.unwrap_or(values.check_to_agent)),
                    move |n, _| set(&|o| o.check_to_agent = Some(n.clamp(0., 5.) as u32)),
                )
                .on_reset(dirty, reset),
            )
        }
        // Text: empty is not set here.
        _ => {
            let (get, set) = (get.clone(), set.clone());
            SettingItem::new(
                field.title,
                SettingField::input(
                    move |_| text_of(&get(), id).unwrap_or_default().into(),
                    move |v: SharedString, _| {
                        let v = v.trim().to_string();
                        set(&|o| put_text(o, id, Some(v.clone()).filter(|v| !v.is_empty())))
                    },
                )
                .on_reset(dirty, reset),
            )
        }
    };
    row.description(said).reset_here()
}

fn text_of(o: &Overrides, id: &str) -> Option<String> {
    match id {
        "branch" => o.branch.clone(),
        "pr_title" => o.pr_title.clone(),
        "worktrees_dir" => o.worktrees_dir.clone(),
        "worktrees" => o.worktrees.clone(),
        "base" => o.base.clone(),
        "stand_hosts" => o.stand_hosts.clone(),
        _ => None,
    }
}

fn put_text(o: &mut Overrides, id: &str, value: Option<String>) {
    match id {
        "branch" => o.branch = value,
        "pr_title" => o.pr_title = value,
        "worktrees_dir" => o.worktrees_dir = value,
        "worktrees" => o.worktrees = value,
        "base" => o.base = value,
        "stand_hosts" => o.stand_hosts = value,
        _ => {}
    }
}

// ---------------------------------------------------------------- integrations

use crate::integrations::Kind;
use crate::layers::Entry;

/// Where a task's links may open.
pub const LINKS_OPEN_IN: [(&str, &str); 2] = [("tab", "A tab of your own browser"), ("system", "The system's browser")];

/// What a request may say of its tickets.
pub const CLOSES: [(&str, &str); 3] = [("fixes", "Fixes: closes them when merged"), ("refs", "Refs: mentions them"), ("none", "Nothing")];

/// Global's page of integrations: those for every project, and the switch
/// over all of tickets.
pub fn integrations_page(draft: &Rc<RefCell<state::Settings>>, layers: &LayersDraft) -> SettingPage {
    let text = |id: &'static str, get: fn(&state::Settings) -> String, put: fn(&mut state::Settings, String)| {
        let (read, write) = (draft.clone(), draft.clone());
        let field = layers::FIELDS.iter().find(|f| f.id == id).expect("a field");
        let about = match layers.borrow().overridden(id) {
            Some(note) => format!("{} {note}", field.about),
            None => field.about.to_string(),
        };
        SettingItem::new(field.title, SettingField::input(move |_| get(&read.borrow()).into(), move |v: SharedString, _| put(&mut write.borrow_mut(), v.trim().to_string()))).description(about)
    };
    let closes = {
        let (read, write) = (draft.clone(), draft.clone());
        let options: Vec<(SharedString, SharedString)> = CLOSES.iter().map(|(k, v)| (SharedString::from(*k), SharedString::from(*v))).collect();
        SettingItem::new(
            "Ticket in a pull request",
            SettingField::dropdown(options, move |_| SharedString::from(Some(read.borrow().pr_closes.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| "refs".into())), move |v: SharedString, _| {
                write.borrow_mut().pr_closes = v.to_string()
            }),
        )
        .description("What a request's description says of its tickets; asked each time, this the first choice.")
    };
    let keys = SettingGroup::new()
        .title("The key where trackers look")
        .item(text("branch", |s| if s.branch_template.is_empty() { crate::naming::DEFAULT_TEMPLATE.into() } else { s.branch_template.clone() }, |s, v| {
            s.branch_template = if v == crate::naming::DEFAULT_TEMPLATE { String::new() } else { v }
        }))
        .item(text("pr_title", |s| if s.pr_title.is_empty() { layers::DEFAULT_PR_TITLE.into() } else { s.pr_title.clone() }, |s, v| {
            s.pr_title = if v == layers::DEFAULT_PR_TITLE { String::new() } else { v }
        }))
        .item(closes);
    let (read, write) = (draft.clone(), draft.clone());
    let off = SettingItem::new(
        "Tickets",
        SettingField::switch(move |_| read.borrow().tickets, move |on, _| write.borrow_mut().tickets = on).default_value(true),
    )
    .description("Off: no ticket marks on tasks, no questions about trackers, no IDEAE_TICKET variables; integrations in a team's .ideae.toml stay silent.");
    SettingPage::new("Integrations")
        .default_open(true)
        .description(
            "Trackers (Jira, Linear, YouTrack, GitHub and GitLab issues) and code hosts (GitHub, GitLab). A tracker owns the prefixes of its keys (DEV, OPS): a key is its tracker's, never guessed. A space or a project adds its own over these; Settings for: switches to them.",
        )
        .group(SettingGroup::new().item(off))
        .group(integrations_group(draft, layers, Scope::Global))
        .group(keys)
}

/// A layer's integrations: its own, with Edit and Remove, and those under
/// it, with where each comes from and a switch to turn it off here.
fn integrations_group(draft: &Rc<RefCell<state::Settings>>, layers: &LayersDraft, scope: Scope) -> SettingGroup {
    let (draft, layers) = (draft.clone(), layers.clone());
    let list = SettingItem::render(move |_, _, cx| {
        let muted = cx.theme().muted_foreground;
        let own: Vec<Entry> = match scope {
            Scope::Global => draft.borrow().integrations.clone(),
            _ => layers.borrow().own(scope).map(|o| o.integrations.clone()).unwrap_or_default(),
        };
        let under: Vec<(Entry, Layer)> = match scope {
            Scope::Global => Vec::new(),
            _ => {
                let all = layers.borrow();
                let mut under = all.under(&draft.borrow(), scope).integrations;
                // Turned off below: still listed, to turn on again here.
                for e in own.iter().filter(|e| e.disabled) {
                    if !under.iter().any(|(u, _)| u.id == e.id) {
                        if let Some(found) = all.under_with_off(&draft.borrow(), scope, &e.id) {
                            under.push(found);
                        }
                    }
                }
                under
            }
        };
        let source = |from: Layer| layers.borrow().source(scope, from);
        let mut rows = v_flex().gap_1();
        let mut shown = 0usize;
        for (entry, from) in &under {
            let here = own.iter().find(|e| e.id == entry.id);
            let off = here.is_some_and(|e| e.disabled);
            let merged = here.map(|h| over(entry, h)).unwrap_or_else(|| entry.clone());
            let (id, draft2, layers2) = (entry.id.clone(), draft.clone(), layers.clone());
            let toggle = Button::new(SharedString::from(format!("integration-off-{shown}")))
                .xsmall()
                .ghost()
                .label(if off { "Turn on here" } else { "Turn off here" })
                .on_click(move |_, window, _| {
                    let mut all = layers2.borrow_mut();
                    if let Some(o) = all.own_mut(scope) {
                        match o.integrations.iter().position(|e| e.id == id) {
                            Some(at) if o.integrations[at].disabled => {
                                o.integrations.remove(at);
                            }
                            Some(at) => o.integrations[at].disabled = true,
                            None => o.integrations.push(Entry { id: id.clone(), disabled: true, ..Default::default() }),
                        }
                    }
                    let _ = &draft2;
                    window.refresh();
                });
            let edit = {
                let (layers, draft, entry) = (layers.clone(), draft.clone(), merged.clone());
                Button::new(SharedString::from(format!("integration-edit-{shown}"))).xsmall().ghost().label("Change here…").on_click(move |_, window, cx| {
                    edit_dialog(Some(entry.clone()), draft.clone(), layers.clone(), scope, window, cx)
                })
            };
            rows = rows.child(row(shown, &merged, format!("from {}{}", source(*from), if here.is_some() && !off { ", changed here" } else { "" }), off, muted, [edit, toggle]));
            shown += 1;
        }
        for (ix, entry) in own.iter().enumerate().filter(|(_, e)| !e.disabled && !under.iter().any(|(u, _)| u.id == e.id)) {
            let edit = {
                let (layers, draft, entry) = (layers.clone(), draft.clone(), entry.clone());
                Button::new(SharedString::from(format!("integration-edit-{shown}"))).xsmall().ghost().label("Edit…").on_click(move |_, window, cx| {
                    edit_dialog(Some(entry.clone()), draft.clone(), layers.clone(), scope, window, cx)
                })
            };
            let remove = {
                let (layers, draft) = (layers.clone(), draft.clone());
                Button::new(SharedString::from(format!("integration-remove-{shown}"))).xsmall().ghost().label("Remove").on_click(move |_, window, _| {
                    match scope {
                        Scope::Global => {
                            draft.borrow_mut().integrations.remove(ix);
                        }
                        _ => {
                            if let Some(o) = layers.borrow_mut().own_mut(scope) {
                                o.integrations.remove(ix);
                            }
                        }
                    }
                    window.refresh();
                })
            };
            rows = rows.child(row(shown, entry, "set here".into(), false, muted, [edit, remove]));
            shown += 1;
        }
        if shown == 0 {
            rows = rows.child(div().text_xs().text_color(muted).child("None yet. Without one, links of Jira Cloud, Linear, GitHub and GitLab issues are still known; a bare key only looks like a ticket."));
        }
        let (draft, layers) = (draft.clone(), layers.clone());
        rows.child(h_flex().pt_1().child(Button::new("integration-add").small().outline().label("Add integration…").on_click(move |_, window, cx| {
            edit_dialog(None, draft.clone(), layers.clone(), scope, window, cx)
        })))
    });
    SettingGroup::new().title("Integrations").item(list)
}

/// An entry with what a layer changes of it.
fn over(under: &Entry, layer: &Entry) -> Entry {
    let mut e = under.clone();
    for (field, value) in [(&mut e.kind, &layer.kind), (&mut e.url, &layer.url), (&mut e.name, &layer.name)] {
        if value.is_some() {
            *field = value.clone();
        }
    }
    if layer.keys.is_some() {
        e.keys = layer.keys.clone();
    }
    e
}

fn row(ix: usize, entry: &Entry, from: String, off: bool, muted: Hsla, buttons: [Button; 2]) -> impl IntoElement {
    let kind = entry.kind.as_deref().and_then(Kind::parse).map_or("?", |k| k.label());
    let keys = entry.keys.clone().unwrap_or_default().join(", ");
    let name = entry.name.clone().unwrap_or_else(|| entry.id.clone());
    let reading = match (entry.auth.as_deref(), entry.bound.as_deref() == entry.url.as_deref()) {
        (Some("token"), true) => " · reads with your token",
        (Some("token"), false) => " · paused: its address changed, give the token again",
        _ => "",
    };
    let about = format!("{kind} · {}{}{reading}", entry.url.as_deref().unwrap_or_default(), if keys.is_empty() { String::new() } else { format!(" · {keys}") });
    h_flex()
        .id(SharedString::from(format!("integration-row-{ix}")))
        .test_support()
        .gap_2()
        .py_1()
        .items_center()
        .when(off, |row| row.opacity(0.5))
        .child(v_flex().flex_1().min_w_0().child(div().text_sm().child(if off { format!("{name} (off here)") } else { name })).child(div().text_xs().text_color(muted).truncate().child(about)))
        .child(div().flex_none().text_xs().text_color(muted).child(from))
        .children(buttons)
}

/// An id for a new integration: its kind and host, made unique.
fn new_id(kind: Kind, url: &str, taken: &[String]) -> String {
    let host = crate::integrations::host_of(url).split(['.', ':']).next().unwrap_or("x").to_ascii_lowercase();
    let base = format!("{}-{host}", kind.word());
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// Adds an integration to a layer, or changes one: its kind, address,
/// keys and name. Opened over Settings; what it saves is Settings' until Save.
fn edit_dialog(editing: Option<Entry>, draft: Rc<RefCell<state::Settings>>, layers: LayersDraft, scope: Scope, window: &mut Window, cx: &mut App) {
    let start = editing.clone().unwrap_or_default();
    let kind = Rc::new(Cell::new(start.kind.as_deref().and_then(Kind::parse).unwrap_or(Kind::Jira)));
    let field = |value: Option<String>, hint: &str, window: &mut Window, cx: &mut App| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(hint.to_string()));
        input.update(cx, |i, cx| i.set_value(value.unwrap_or_default(), window, cx));
        input
    };
    let url = field(start.url.clone(), "https://example.atlassian.net", window, cx);
    let keys = field(start.keys.clone().map(|k| k.join(", ")), "DEV, OPS (Jira, Linear, YouTrack); owner/repo for issues", window, cx);
    let name = field(start.name.clone(), "what it is called; its host when empty", window, cx);
    let account = field(start.account.clone(), "Jira Cloud: your email; empty for a personal access token", window, cx);
    let token = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(if start.auth.as_deref() == Some("token") { "kept: type to replace it" } else { "optional: to read tickets' titles and status" }));
    let error: Rc<RefCell<Option<String>>> = Rc::default();
    let agent_read = Rc::new(Cell::new(start.agent_read == Some(true)));
    // The token kept is forgotten on its button, at once: not sent anywhere again.
    let forgotten = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, _, cx| {
        let (forgotten, agent_read) = (forgotten.clone(), agent_read.clone());
        let (kind, url, keys, name, error, draft, layers, editing) = (kind.clone(), url.clone(), keys.clone(), name.clone(), error.clone(), draft.clone(), layers.clone(), editing.clone());
        let (account, token) = (account.clone(), token.clone());
        let muted = cx.theme().muted_foreground;
        let save = {
            let (kind, url, keys, name, error, editing) = (kind.clone(), url.clone(), keys.clone(), name.clone(), error.clone(), editing.clone());
            let (account, token, forgotten, agent_read) = (account.clone(), token.clone(), forgotten.clone(), agent_read.clone());
            move |window: &mut Window, cx: &mut App| {
                let address = url.read(cx).value().trim().to_string();
                let Some(base) = crate::integrations::normal_base(&address) else {
                    *error.borrow_mut() = Some("An address is needed: https://host, or https://host/path for a server on a path.".into());
                    window.refresh();
                    return;
                };
                let keys: Vec<String> = keys.read(cx).value().split([',', ' ']).map(|k| k.trim().trim_end_matches('-').to_string()).filter(|k| !k.is_empty()).collect();
                let name = Some(name.read(cx).value().trim().to_string()).filter(|n| !n.is_empty());
                let taken: Vec<String> = {
                    let mut all: Vec<String> = draft.borrow().integrations.iter().map(|e| e.id.clone()).collect();
                    let l = layers.borrow();
                    all.extend(l.spaces.iter().flat_map(|s| s.settings.integrations.iter().map(|e| e.id.clone())));
                    all.extend(l.projects.iter().flat_map(|p| p.settings.integrations.iter().chain(&p.team.integrations).map(|e| e.id.clone())));
                    all
                };
                let id = editing.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| new_id(kind.get(), &base, &taken));
                // A token given: kept by host and account, for this very address; saving
                // again with the address changed is your say that it goes there too.
                let account = account.read(cx).value().trim().to_string();
                let token = token.read(cx).value().trim().to_string();
                let had = editing.as_ref().and_then(|e| e.auth.clone()).filter(|_| !forgotten.get());
                let auth = if token.is_empty() { had } else { Some("token".to_string()) };
                if !token.is_empty() {
                    let kept = crate::integrations::secret_store::write(&crate::integrations::secret_id(&base, &account), &token, cx);
                    let handle = window.window_handle();
                    cx.spawn(async move |cx| {
                        if let Some(note) = kept.await {
                            let _ = handle.update(cx, |_, window, cx| window.push_notification(gpui_kit::component::notification::Notification::warning(note).title("Token"), cx));
                        }
                    })
                    .detach();
                }
                let bound = auth.is_some().then(|| base.clone());
                let entry = Entry {
                    id: id.clone(),
                    kind: Some(kind.get().word().to_string()),
                    url: Some(base),
                    name,
                    keys: Some(keys),
                    disabled: false,
                    account: Some(account).filter(|a| !a.is_empty()),
                    auth,
                    bound,
                    agent_read: agent_read.get().then_some(true),
                };
                let put = |list: &mut Vec<Entry>| match list.iter_mut().find(|e| e.id == id) {
                    Some(known) => *known = entry.clone(),
                    None => list.push(entry.clone()),
                };
                match scope {
                    Scope::Global => put(&mut draft.borrow_mut().integrations),
                    _ => {
                        if let Some(o) = layers.borrow_mut().own_mut(scope) {
                            put(&mut o.integrations);
                        }
                    }
                }
                window.close_dialog(cx);
                window.refresh();
            }
        };
        let save = Rc::new(save);
        let now = kind.get();
        let mut kinds = h_flex().gap_1();
        for k in Kind::ALL {
            let kind = kind.clone();
            kinds = kinds.child(Button::new(SharedString::from(format!("integration-kind-{}", k.word()))).xsmall().outline().selected(now == k).label(k.label()).on_click(move |_, window, _| {
                kind.set(k);
                window.refresh();
            }));
        }
        let hint = match now {
            Kind::Jira => "Jira Cloud (https://your.atlassian.net) or Server/Data Center (https://corp.example.com/jira). Keys: the projects' prefixes.",
            Kind::Linear => "Your workspace: https://linear.app/your-team. Keys: the teams' prefixes.",
            Kind::YouTrack => "https://your.youtrack.cloud. Keys: the projects' prefixes.",
            Kind::GitHub => "https://github.com, or your GitHub Enterprise. Asked through gh, signed in there. Keys: owner/repo whose issues are tickets.",
            Kind::GitLab => "https://gitlab.com, or your own GitLab server. Asked through glab (glab auth login --hostname …). Keys: group/project whose issues are tickets.",
        };
        let fieldrow = |label: &'static str, input: &Entity<InputState>, id: &'static str| {
            h_flex().gap_3().child(div().w(px(90.)).flex_none().text_sm().child(label)).child(div().id(id).test_support().flex_1().child(Input::new(input)))
        };
        let (save_ok, save_btn) = (save.clone(), save.clone());
        dialog
            .title(if editing.is_some() { "Integration" } else { "Add an integration" })
            .w(px(560.))
            .on_ok(move |_, window, cx| {
                save_ok(window, cx);
                false
            })
            .child(
                v_flex()
                    .gap_2()
                    .child(kinds)
                    .child(div().text_xs().text_color(muted).child(hint))
                    .child(fieldrow("Address", &url, "integration-url"))
                    .child(fieldrow("Keys", &keys, "integration-keys"))
                    .child(fieldrow("Name", &name, "integration-name"))
                    .when(matches!(now, Kind::Jira | Kind::Linear | Kind::YouTrack), |form| {
                        form.child(fieldrow("Account", &account, "integration-account")).child(fieldrow("Token", &token, "integration-token"))
                    })
                    .when(matches!(now, Kind::Jira | Kind::Linear | Kind::YouTrack), |form| {
                        let flag = agent_read.clone();
                        form.child(
                            gpui_kit::component::checkbox::Checkbox::new("integration-agent-read")
                                .label("A task's agent may read its tickets with this token (the MCP tool ticket: its own tickets only)")
                                .checked(agent_read.get())
                                .on_click(move |on, window, _| {
                                    flag.set(*on);
                                    window.refresh();
                                }),
                        )
                    })
                    .when_some(editing.clone().filter(|e| e.auth.as_deref() == Some("token") && !forgotten.get()), |form, e| {
                        let forgotten = forgotten.clone();
                        form.child(h_flex().child(Button::new("integration-forget").xsmall().ghost().label("Forget the token").on_click(move |_, window, cx| {
                            let id = crate::integrations::secret_id(e.url.as_deref().unwrap_or_default(), e.account.as_deref().unwrap_or_default());
                            crate::integrations::secret_store::delete(&id, cx).detach();
                            forgotten.set(true);
                            window.refresh();
                        })))
                    })
                    .when_some(error.borrow().clone(), |form, e| form.child(div().text_xs().text_color(gpui_kit::red()).child(e)))
                    .child(div().text_xs().text_color(muted).child(match now {
                        Kind::Jira => "Its address is all ideae needs to make links. A token reads a ticket's title and status: Jira Cloud's from id.atlassian.com → Security → API tokens (no admin needed), with your email; Server/Data Center's personal access token alone. Kept in the system's keyring by host and account; nothing is asked of the network without one.",
                        Kind::Linear => "Its address is all ideae needs to make links. A personal API key (Linear → Settings → Security & access) reads a ticket's title and status. Kept in the system's keyring; nothing is asked of the network without one.",
                        Kind::YouTrack => "Its address is all ideae needs to make links. A permanent token (Profile → Account security) reads a ticket's title and status. Kept in the system's keyring.",
                        _ => "Read through gh or glab, with your sign-in there: no token here.",
                    })),
            )
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().flex_1())
                    .child(Button::new("integration-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("integration-save").primary().label(if editing.is_some() { "Keep" } else { "Add" }).on_click(move |_, window, cx| save_btn(window, cx))),
            )
    });
}

impl Layers {
    /// An integration under `scope` that its own layer turned off, as it would be were it on.
    fn under_with_off(&self, settings: &state::Settings, scope: Scope, id: &str) -> Option<(Entry, Layer)> {
        let mut copy = self.clone();
        if let Some(o) = copy.own_mut(scope) {
            o.integrations.retain(|e| e.id != id);
        }
        // What the layers under it give, with nothing of this one turned off.
        copy.under(settings, scope).integrations.into_iter().find(|(e, _)| e.id == id)
    }
}
