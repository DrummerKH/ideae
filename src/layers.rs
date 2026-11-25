//! Settings in layers: Global (Settings), a space's, the team's (the
//! repository's `.ideae.toml`) and a project's own. A value is the one of
//! the most specific layer that sets it; a list with ids (integrations) is
//! put together from all of them, each layer adding to it, changing an
//! entry by its id or turning one off.
//!
//! Resolved once per project when a layer changes (`Workspace::resolve_projects`)
//! and kept as an `Arc`: nothing here is worked out while drawing.

use serde::{Deserialize, Serialize};

/// Where a value comes from, the least specific first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
pub enum Layer {
    #[default]
    Default,
    Global,
    Space,
    Team,
    Project,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Default => "default",
            Layer::Global => "Settings",
            Layer::Space => "space",
            Layer::Team => "team (.ideae.toml)",
            Layer::Project => "project",
        }
    }
}

/// A setting that layers set: its id (the field's name), what it is
/// called, and the layers it may be set in.
pub struct Field {
    pub id: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    pub layers: &'static [Layer],
}

impl Field {
    pub fn allowed(&self, layer: Layer) -> bool {
        self.layers.contains(&layer)
    }
}

/// One entry of a list put together by id: what a layer says of it.
/// A layer that names an id no layer before it did adds the entry, and
/// must give its `kind` and `url`; one that names a known id changes the
/// fields it gives; `disabled` takes the entry out.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What it is called where it is shown; its host when not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The ticket keys' prefixes it owns (`DEV`, `OPS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// How you read it, yours alone: `token` (in the keyring, by host and
    /// account), `cli` (gh, glab). A team's file never says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Whose token: Jira Cloud's email; empty for a token on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The address the token was given for: one a team's file changes since
    /// is not sent it until you say so again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound: Option<String>,
    /// A task's agent may read its tickets with your token (the MCP tool
    /// `ticket`); off unless you turn it on, never by a team's file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_read: Option<bool>,
}

impl Entry {
    fn over(&mut self, layer: &Entry) {
        if let Some(kind) = &layer.kind {
            self.kind = Some(kind.clone());
        }
        if let Some(url) = &layer.url {
            self.url = Some(url.clone());
        }
        if let Some(name) = &layer.name {
            self.name = Some(name.clone());
        }
        if let Some(keys) = &layer.keys {
            self.keys = Some(keys.clone());
        }
        for (field, value) in [(&mut self.auth, &layer.auth), (&mut self.account, &layer.account), (&mut self.bound, &layer.bound)] {
            if value.is_some() {
                *field = value.clone();
            }
        }
        if layer.agent_read.is_some() {
            self.agent_read = layer.agent_read;
        }
    }
}

/// Declares the layered settings: each one's type, default, title, what
/// it does and the layers it may be set in. Out of it come [`Overrides`]
/// (what one layer sets, every field optional), [`Values`] (what applies),
/// [`Sources`] (which layer each came from) and [`FIELDS`].
macro_rules! layered {
    ($( $name:ident : $ty:ty = $default:expr, $title:literal, $about:literal, [$($layer:ident),+]; )*) => {
        /// What one layer sets; what it does not set is left to the layers under it.
        #[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
        #[serde(default)]
        pub struct Overrides {
            $(
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $name: Option<$ty>,
            )*
            #[serde(skip_serializing_if = "Vec::is_empty")]
            pub integrations: Vec<Entry>,
        }

        /// What applies, every setting given.
        #[derive(Clone, PartialEq, Debug)]
        pub struct Values {
            $( pub $name: $ty, )*
        }

        impl Default for Values {
            fn default() -> Self {
                Self { $( $name: $default, )* }
            }
        }

        /// Which layer each value of [`Values`] came from.
        #[derive(Clone, Copy, PartialEq, Debug, Default)]
        pub struct Sources {
            $( pub $name: Layer, )*
        }

        impl Sources {
            pub fn of(&self, id: &str) -> Layer {
                match id {
                    $( stringify!($name) => self.$name, )*
                    _ => Layer::Default,
                }
            }
        }

        pub const FIELDS: &[Field] = &[
            $( Field { id: stringify!($name), title: $title, about: $about, layers: &[$(Layer::$layer),+] }, )*
        ];

        impl Overrides {
            /// How many settings it holds, its list's entries counted.
            pub fn count(&self) -> usize {
                0 $( + usize::from(self.$name.is_some()) )* + self.integrations.len()
            }

            pub fn is_empty(&self) -> bool {
                self.count() == 0
            }

            pub fn is_set(&self, id: &str) -> bool {
                match id {
                    $( stringify!($name) => self.$name.is_some(), )*
                    _ => false,
                }
            }

            pub fn clear(&mut self, id: &str) {
                match id {
                    $( stringify!($name) => self.$name = None, )*
                    _ => {}
                }
            }

            /// Only what `layer` may hold: a team's file cannot choose
            /// whose agent runs, nor a project the folder of all worktrees.
            pub fn allowed_in(mut self, layer: Layer) -> Self {
                $(
                    if ![$(Layer::$layer),+].contains(&layer) {
                        self.$name = None;
                    }
                )*
                self
            }

            fn apply(&self, layer: Layer, values: &mut Values, sources: &mut Sources) {
                $(
                    if let Some(value) = &self.$name && [$(Layer::$layer),+].contains(&layer) {
                        values.$name = value.clone();
                        sources.$name = layer;
                    }
                )*
            }
        }
    };
}

/// A request's title unless a layer says otherwise.
pub const DEFAULT_PR_TITLE: &str = "{key} {title}";

layered! {
    agent: String = String::new(),
        "Default agent", "The agent new tasks run, unless the new-task dialog chooses another.",
        [Global, Space, Project];
    worktrees_dir: String = String::new(),
        "Worktrees folder", "New tasks' worktrees go in a folder of each project's name in it; empty: beside each repository.",
        [Global, Space];
    worktrees: String = String::new(),
        "Worktrees in", "The folder this project's new tasks' worktrees go in; empty: as the folder above says.",
        [Project];
    base: String = String::new(),
        "New tasks from", "The branch new tasks start from (develop, origin/release); empty: the remote's default branch.",
        [Team, Project];
    pull_requests: bool = true,
        "Pull request status", "Tasks' pull requests are asked of gh: their state, CI and review on each task.",
        [Global, Space, Project];
    language_servers: bool = true,
        "Language servers", "Completion, types and go to definition from a server. Which servers run is in Settings → Language servers.",
        [Space, Project];
    check_to_agent: u32 = 0,
        "Failed check back to the agent", "How many times in a row a failed check goes back to the agent by itself; 0: only by a click.",
        [Space, Project];
    branch: String = crate::naming::DEFAULT_TEMPLATE.to_string(),
        "Branch of a ticket's task", "How a task whose title starts with a ticket's key names its branch: {key}, {summary} (camelCase words), {slug} (dashed words), {rand}.",
        [Global, Space, Team, Project];
    pr_title: String = DEFAULT_PR_TITLE.to_string(),
        "Pull request's title", "The title of a pull or merge request of a task with a ticket: {key} and {title}, the commit's subject or the task's title. Its ticket's tracker finds it by the key.",
        [Global, Space, Team, Project];
    closes: String = "refs".to_string(),
        "Ticket in a pull request", "What a request's description says of its tickets: fixes (the tracker may close them when it is merged), refs, or none. Asked each time; this is the first choice.",
        [Global, Space, Team, Project];
}

/// What applies to a project: its values and where each came from, and
/// its integrations, each with the layer that added it.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Resolved {
    pub values: Values,
    pub from: Sources,
    pub integrations: Vec<(Entry, Layer)>,
    /// Entries left out, and why: shown, never a reason to stop.
    pub errors: Vec<String>,
}

/// The layers of a project, the least specific first; a project in no
/// space sees Global and its own alone.
pub fn resolve(global: &Overrides, space: Option<&Overrides>, team: Option<&Overrides>, project: &Overrides) -> Resolved {
    let mut resolved = Resolved::default();
    let layers = [(Layer::Global, Some(global)), (Layer::Space, space), (Layer::Team, team), (Layer::Project, Some(project))];
    for (layer, overrides) in layers {
        let Some(overrides) = overrides else { continue };
        overrides.apply(layer, &mut resolved.values, &mut resolved.from);
        for entry in &overrides.integrations {
            merge_entry(&mut resolved, entry, layer);
        }
    }
    resolved.integrations.retain(|(entry, _)| !entry.disabled);
    resolved
}

fn merge_entry(resolved: &mut Resolved, entry: &Entry, layer: Layer) {
    if entry.id.trim().is_empty() {
        resolved.errors.push(format!("An integration without an id ({}) is left out.", layer.label()));
        return;
    }
    if let Some((known, _)) = resolved.integrations.iter_mut().find(|(e, _)| e.id == entry.id) {
        known.over(entry);
        known.disabled = entry.disabled;
        return;
    }
    if entry.disabled {
        return;
    }
    let given = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.trim().is_empty());
    if !given(&entry.kind) || !given(&entry.url) {
        resolved.errors.push(format!("Integration \"{}\" ({}) needs a kind and a url to be added; it is left out.", entry.id, layer.label()));
        return;
    }
    resolved.integrations.push((entry.clone(), layer));
}

/// The Global layer: what Settings hold of the layered settings.
pub fn global_layer(settings: &crate::state::Settings) -> Overrides {
    Overrides {
        agent: Some(settings.default_agent.clone()),
        worktrees_dir: Some(settings.worktrees_dir.clone()),
        pull_requests: Some(settings.pr_status),
        integrations: settings.integrations.clone(),
        // Empty: the default, not set here.
        branch: Some(settings.branch_template.clone()).filter(|t| !t.trim().is_empty()),
        pr_title: Some(settings.pr_title.clone()).filter(|t| !t.trim().is_empty()),
        closes: Some(settings.pr_closes.clone()).filter(|t| !t.trim().is_empty()),
        ..Default::default()
    }
}

/// Puts what a layer sets into Settings, where Global keeps it: a space's
/// settings moved there when the space goes.
pub fn into_global(overrides: &Overrides, settings: &mut crate::state::Settings) {
    if let Some(agent) = &overrides.agent {
        settings.default_agent = agent.clone();
    }
    if let Some(dir) = &overrides.worktrees_dir {
        settings.worktrees_dir = dir.clone();
    }
    if let Some(on) = overrides.pull_requests {
        settings.pr_status = on;
    }
    if let Some(t) = &overrides.branch {
        settings.branch_template = t.clone();
    }
    if let Some(t) = &overrides.pr_title {
        settings.pr_title = t.clone();
    }
    if let Some(t) = &overrides.closes {
        settings.pr_closes = t.clone();
    }
    for entry in &overrides.integrations {
        match settings.integrations.iter_mut().find(|e| e.id == entry.id) {
            Some(known) => *known = entry.clone(),
            None => settings.integrations.push(entry.clone()),
        }
    }
}

/// The team's layer out of a `.ideae.toml`: `[project]` and
/// `[integrations.<id>]`, what a team may set and no more.
pub fn team_layer(text: &str) -> Result<Overrides, String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct File {
        project: Overrides,
        integrations: std::collections::BTreeMap<String, Entry>,
    }
    let file: File = toml::from_str(text).map_err(|e| format!("{}: {e}", crate::services::FILE))?;
    let mut layer = file.project.allowed_in(Layer::Team);
    layer.integrations = file
        .integrations
        .into_iter()
        .map(|(id, mut entry)| {
            entry.id = id;
            // Whose token, and for where: yours to say, not the file's.
            (entry.auth, entry.account, entry.bound, entry.agent_read) = (None, None, None, None);
            entry
        })
        .collect();
    Ok(layer)
}

/// What changed for a project between two resolutions, as told when it
/// moves to another space: each setting whose value is not what it was,
/// and the integrations it lost and got.
pub fn changes(before: &Resolved, after: &Resolved, describe: impl Fn(&str, &Values) -> String) -> Vec<String> {
    let mut told = Vec::new();
    for field in FIELDS {
        let (was, is) = (describe(field.id, &before.values), describe(field.id, &after.values));
        if was != is {
            told.push(format!("{}: {is} (was {was})", field.title));
        }
    }
    let ids = |r: &Resolved| r.integrations.iter().map(|(e, _)| e.id.clone()).collect::<std::collections::BTreeSet<_>>();
    let (had, has) = (ids(before), ids(after));
    let lost: Vec<String> = had.difference(&has).cloned().collect();
    let got: Vec<String> = has.difference(&had).cloned().collect();
    if !lost.is_empty() {
        told.push(format!("Loses: {}", lost.join(", ")));
    }
    if !got.is_empty() {
        told.push(format!("Gets: {}", got.join(", ")));
    }
    told
}

// ---------------------------------------------------------------- what applies where

/// Each project's resolution, set by the window when it changes, read from
/// any thread or view by a task's root (a task view's request, its commit box).
static PROJECTS: std::sync::RwLock<Option<std::collections::HashMap<std::path::PathBuf, std::sync::Arc<Resolved>>>> = std::sync::RwLock::new(None);

pub fn set_project(project: &std::path::Path, resolved: std::sync::Arc<Resolved>) {
    PROJECTS.write().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(project.to_path_buf(), resolved);
}

/// What applies to the project a task's root is of; the defaults when none is known.
pub fn of_root(root: &std::path::Path) -> std::sync::Arc<Resolved> {
    let project = crate::project_config::project_of(root);
    PROJECTS.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|all| all.get(&project).or_else(|| all.get(root)).cloned()).unwrap_or_default()
}

/// The title of a task's request: the template with its key and the title
/// the commits give; as it is when that title has the key already.
pub fn request_title(template: &str, key: &str, title: &str) -> String {
    if key.is_empty() || title.contains(key) {
        return title.to_string();
    }
    let template = if template.contains("{title}") { template } else { DEFAULT_PR_TITLE };
    template.replace("{key}", key).replace("{title}", title).trim().to_string()
}

// ---------------------------------------------------------------- the state of before

/// A project's settings as `state.json` had them before layers, in fields
/// of their own. Read into its `Overrides`, and written back from them so
/// that an older build reads what it wrote.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct OldFields {
    pub agent: Option<String>,
    pub base: String,
    pub worktrees: String,
    pub lsp_off: bool,
    pub pr_off: bool,
    pub check_to_agent: u32,
}

impl OldFields {
    pub fn from_overrides(o: &Overrides) -> Self {
        Self {
            agent: o.agent.clone(),
            base: o.base.clone().unwrap_or_default(),
            worktrees: o.worktrees.clone().unwrap_or_default(),
            lsp_off: o.language_servers == Some(false),
            pr_off: o.pull_requests == Some(false),
            check_to_agent: o.check_to_agent.unwrap_or(0),
        }
    }

    /// Takes into `o` what the old fields say otherwise than `o` would
    /// have written them: an older build changed them since.
    pub fn reconcile(&self, o: &mut Overrides) {
        let written = Self::from_overrides(o);
        if written.agent != self.agent {
            o.agent = self.agent.clone();
        }
        if written.base != self.base {
            o.base = Some(self.base.clone()).filter(|b| !b.is_empty());
        }
        if written.worktrees != self.worktrees {
            o.worktrees = Some(self.worktrees.clone()).filter(|w| !w.is_empty());
        }
        if written.lsp_off != self.lsp_off {
            o.language_servers = self.lsp_off.then_some(false);
        }
        if written.pr_off != self.pr_off {
            o.pull_requests = self.pr_off.then_some(false);
        }
        if written.check_to_agent != self.check_to_agent {
            o.check_to_agent = Some(self.check_to_agent).filter(|n| *n > 0);
        }
    }
}

/// A new id for a space: short, never one of `taken`.
pub fn new_space_id(taken: &[&str]) -> String {
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64 ^ u64::from(std::process::id());
    loop {
        // xorshift: distinct enough for a handful of spaces.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let id = format!("s{:08x}", seed as u32);
        if !taken.contains(&id.as_str()) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: Option<&str>, url: Option<&str>) -> Entry {
        Entry { id: id.into(), kind: kind.map(Into::into), url: url.map(Into::into), ..Default::default() }
    }

    #[test]
    fn the_most_specific_layer_that_sets_a_value_wins() {
        let global = Overrides { agent: Some("claude".into()), pull_requests: Some(true), ..Default::default() };
        let space = Overrides { agent: Some("codex".into()), language_servers: Some(false), ..Default::default() };
        let project = Overrides { language_servers: Some(true), ..Default::default() };
        let r = resolve(&global, Some(&space), None, &project);
        assert_eq!((r.values.agent.as_str(), r.from.agent), ("codex", Layer::Space));
        assert_eq!((r.values.language_servers, r.from.language_servers), (true, Layer::Project));
        assert_eq!((r.values.pull_requests, r.from.pull_requests), (true, Layer::Global));
        assert_eq!((r.values.check_to_agent, r.from.check_to_agent), (0, Layer::Default));
        // In no space: Global and its own alone.
        let alone = resolve(&global, None, None, &project);
        assert_eq!((alone.values.agent.as_str(), alone.from.agent), ("claude", Layer::Global));
    }

    #[test]
    fn a_layer_sets_only_what_it_may() {
        // A team's file choosing the agent, or a space the base branch: not theirs to choose.
        let team = Overrides { agent: Some("codex".into()), base: Some("develop".into()), ..Default::default() };
        let space = Overrides { base: Some("main".into()), ..Default::default() };
        let r = resolve(&Overrides::default(), Some(&space), Some(&team), &Overrides::default());
        assert_eq!((r.values.agent.as_str(), r.from.agent), ("", Layer::Default));
        assert_eq!((r.values.base.as_str(), r.from.base), ("develop", Layer::Team));
        let mine = Overrides { base: Some("release".into()), ..Default::default() };
        assert_eq!(resolve(&Overrides::default(), None, Some(&team), &mine).values.base, "release", "yours over the team's");
        assert_eq!(team.clone().allowed_in(Layer::Team), Overrides { base: Some("develop".into()), ..Default::default() });
    }

    #[test]
    fn a_list_is_put_together_by_id() {
        let global = Overrides { integrations: vec![entry("acme", Some("jira"), Some("https://acme.example.atlassian.net"))], ..Default::default() };
        let space = Overrides {
            integrations: vec![
                Entry { id: "acme".into(), keys: Some(vec!["DEV".into()]), ..Default::default() },
                entry("gl", Some("gitlab"), Some("https://gitlab.example.com")),
                // An unknown id without what it is: told, left out.
                entry("half", Some("jira"), None),
            ],
            ..Default::default()
        };
        let project = Overrides { integrations: vec![Entry { id: "gl".into(), disabled: true, ..Default::default() }], ..Default::default() };
        let r = resolve(&global, Some(&space), None, &project);
        assert_eq!(r.integrations.len(), 1, "gl is turned off in the project");
        let (acme, from) = &r.integrations[0];
        assert_eq!((acme.keys.clone(), acme.url.as_deref(), *from), (Some(vec!["DEV".to_string()]), Some("https://acme.example.atlassian.net"), Layer::Global));
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].contains("half"));
        // Turned off where it is added: as if it were not.
        let off = Overrides { integrations: vec![Entry { disabled: true, ..entry("x", Some("jira"), Some("https://x.example.com")) }], ..Default::default() };
        assert!(resolve(&off, None, None, &Overrides::default()).integrations.is_empty());
        // Turned off above, on again below.
        let again = Overrides { integrations: vec![Entry { id: "gl".into(), ..Default::default() }], ..Default::default() };
        let mut space_only = space.clone();
        space_only.integrations.retain(|e| e.id == "gl");
        let r = resolve(&Overrides::default(), Some(&space_only), Some(&project), &again);
        assert_eq!(r.integrations.iter().map(|(e, _)| e.id.as_str()).collect::<Vec<_>>(), ["gl"]);
    }

    #[test]
    fn the_team_file_sets_what_a_team_may() {
        let text = r#"
[task]
check = "cargo test"

[project]
base = "develop"
agent = "codex"

[integrations.acme]
kind = "jira"
url = "https://example.atlassian.net"
keys = ["DEV"]
"#;
        let layer = team_layer(text).unwrap();
        assert_eq!(layer.base.as_deref(), Some("develop"));
        assert_eq!(layer.agent, None, "whose agent runs is not the team's to say");
        assert_eq!(layer.integrations, vec![Entry { keys: Some(vec!["DEV".into()]), ..entry("acme", Some("jira"), Some("https://example.atlassian.net")) }]);
        assert_eq!(team_layer("[task]\ncheck = \"x\"\n").unwrap(), Overrides::default());
        assert!(team_layer("[project\n").is_err());
    }

    #[test]
    fn the_old_fields_are_read_and_written_back() {
        // A state of before layers: its fields become the project's layer.
        let old = OldFields { agent: Some("codex".into()), base: "develop".into(), worktrees: String::new(), lsp_off: true, pr_off: false, check_to_agent: 2 };
        let mut o = Overrides::default();
        old.reconcile(&mut o);
        assert_eq!(o, Overrides { agent: Some("codex".into()), base: Some("develop".into()), language_servers: Some(false), check_to_agent: Some(2), ..Default::default() });
        assert_eq!(OldFields::from_overrides(&o), old, "written back as they were read");
        // An older build turned language servers on again and changed nothing else: only that is taken.
        let changed = OldFields { lsp_off: false, ..old.clone() };
        let mut kept = Overrides { pull_requests: Some(true), ..o.clone() };
        changed.reconcile(&mut kept);
        assert_eq!(kept, Overrides { language_servers: None, pull_requests: Some(true), ..o });
    }

    #[test]
    fn a_move_tells_what_changes() {
        let work = Overrides { agent: Some("codex".into()), integrations: vec![entry("acme", Some("jira"), Some("https://a.example.com"))], ..Default::default() };
        let home = Overrides { integrations: vec![entry("lin", Some("linear"), Some("https://linear.app/x"))], ..Default::default() };
        let global = Overrides { agent: Some("claude".into()), ..Default::default() };
        let (before, after) = (resolve(&global, Some(&work), None, &Overrides::default()), resolve(&global, Some(&home), None, &Overrides::default()));
        let told = changes(&before, &after, |id, v| match id {
            "agent" => v.agent.clone(),
            _ => String::new(),
        });
        assert_eq!(told, ["Default agent: claude (was codex)", "Loses: acme", "Gets: lin"]);
    }

    #[test]
    fn a_request_s_title_has_its_key() {
        assert_eq!(request_title(DEFAULT_PR_TITLE, "DEV-1", "Fix the login"), "DEV-1 Fix the login");
        assert_eq!(request_title("[{key}] {title}", "DEV-1", "Fix"), "[DEV-1] Fix");
        assert_eq!(request_title(DEFAULT_PR_TITLE, "DEV-1", "DEV-1 Fix"), "DEV-1 Fix", "once");
        assert_eq!(request_title("{key}", "DEV-1", "Fix"), "DEV-1 Fix", "no {{title}}: the default");
        assert_eq!(request_title(DEFAULT_PR_TITLE, "", "Fix"), "Fix");
    }

    #[test]
    fn space_ids_differ() {
        let a = new_space_id(&[]);
        let b = new_space_id(&[&a]);
        assert_ne!(a, b);
        assert!(a.starts_with('s') && a.len() == 9);
    }
}
