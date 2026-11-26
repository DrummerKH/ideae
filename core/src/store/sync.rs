//! A window's changes brought into the store, and the store's into a
//! window. A window says what it had (`base`: the machine as it last took
//! it in) and what it has now (`wanted`); only what differs between the two
//! is its change, applied to the store as it is now (another window may
//! have changed it since). Tasks are matched by path within their project
//! (a window keys them so); ids stay the store's; a field the window does
//! not know (another build's) is never taken away.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Map, Value};

use super::{Machine, Op, Project, Refused, Store, Task, WindowId};

/// What of `wanted` differs from `current`: its fields set, those `current`
/// has and `wanted` has not set to null (their default) unless the window
/// does not know them (`unknown`); `skip` left alone.
fn changed<T: Serialize>(current: &T, wanted: &T, unknown: &Map<String, Value>, skip: &[&str]) -> Map<String, Value> {
    let (Ok(Value::Object(current)), Ok(Value::Object(wanted))) = (serde_json::to_value(current), serde_json::to_value(wanted)) else { return Map::new() };
    let mut out = Map::new();
    for (k, v) in &wanted {
        if !skip.contains(&k.as_str()) && current.get(k) != Some(v) {
            out.insert(k.clone(), v.clone());
        }
    }
    for k in current.keys() {
        if !skip.contains(&k.as_str()) && !wanted.contains_key(k) && !unknown.contains_key(k) {
            out.insert(k.clone(), Value::Null);
        }
    }
    out
}

/// A list's items by what tells one from another (a question's id, a
/// link's address): a window's change to it is the items it added, removed
/// and changed from `base`, made to the store's list `now` (another window's
/// items there kept; one it removed is not brought back by a change).
fn merged_list(base: &Value, wanted: &Value, now: &Value, key: fn(&Value) -> Value) -> Value {
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let (base, wanted, mut out) = (list(base), list(wanted), list(now));
    let find = |items: &[Value], k: &Value| items.iter().position(|i| key(i) == *k);
    for b in &base {
        if find(&wanted, &key(b)).is_none() {
            out.retain(|i| key(i) != key(b));
        }
    }
    for w in &wanted {
        let k = key(w);
        match (find(&base, &k).map(|i| &base[i]), find(&out, &k)) {
            (Some(b), _) if b == w => {}
            (Some(_), Some(at)) => out[at] = w.clone(),
            (Some(_), None) => {}
            (None, Some(at)) => out[at] = w.clone(),
            (None, None) => out.push(w.clone()),
        }
    }
    Value::Array(out)
}

/// A map's keys added, removed and changed from `base` to `wanted`, made to `now`.
fn merged_map(base: &Value, wanted: &Value, now: &Value) -> Value {
    let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
    let (base, wanted, mut out) = (map(base), map(wanted), map(now));
    for k in base.keys() {
        if !wanted.contains_key(k) {
            out.remove(k);
        }
    }
    for (k, v) in &wanted {
        if base.get(k) != Some(v) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn key_of(v: &Value, fields: &[&str]) -> Value {
    Value::Array(fields.iter().map(|f| v.get(*f).cloned().unwrap_or(Value::Null)).collect())
}

/// A task's fields a window changed, with its lists merged into the
/// store's rather than put over them: two windows ask questions of one
/// task, add links to it, write review notes in it.
fn merged_task_fields(base: &Task, wanted: &Task, now: &Task, mut fields: Map<String, Value>) -> Map<String, Value> {
    let (Ok(base), Ok(wanted), Ok(now)) = (serde_json::to_value(base), serde_json::to_value(wanted), serde_json::to_value(now)) else { return fields };
    let get = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    let lists: [(&str, fn(&Value) -> Value); 3] = [("questions", |v| key_of(v, &["id"])), ("links", |v| key_of(v, &["what", "key", "url"])), ("sessions", |v| key_of(v, &["profile", "started"]))];
    for (field, key) in lists {
        if fields.contains_key(field) {
            fields.insert(field.into(), merged_list(&get(&base, field), &get(&wanted, field), &get(&now, field), key));
        }
    }
    if fields.contains_key("work") {
        let (b, w, n) = (get(&base, "work"), get(&wanted, "work"), get(&now, "work"));
        let review = merged_list(&get(&b, "review"), &get(&w, "review"), &get(&n, "review"), |v| key_of(v, &["path", "line", "code", "text"]));
        let viewed = merged_map(&get(&b, "viewed"), &get(&w, "viewed"), &get(&n, "viewed"));
        let bookmarks = merged_map(&get(&b, "bookmarks"), &get(&w, "bookmarks"), &get(&n, "bookmarks"));
        fields.insert("work".into(), serde_json::json!({ "review": review, "viewed": viewed, "bookmarks": bookmarks }));
    }
    // Ids the store gave are never given again: the count goes up only.
    if let Some(asked) = fields.get("asked").and_then(Value::as_u64) {
        let kept = now.get("asked").and_then(Value::as_u64).unwrap_or(0);
        fields.insert("asked".into(), Value::from(asked.max(kept)));
    }
    fields
}

fn project<'a>(machine: &'a Machine, path: &std::path::Path) -> Option<&'a Project> {
    machine.projects.iter().find(|p| p.path == path)
}

fn task<'a>(project: Option<&'a Project>, path: &std::path::Path) -> Option<&'a Task> {
    project?.tasks.iter().find(|t| t.path == path)
}

/// Applies to `store` what window `by` changed from `base` to `wanted` (its
/// projects, tasks, crew and the machine's settings and profiles, as
/// `migrate::from_state` makes them of its state). What the store refused
/// is told back.
pub fn sync(store: &mut Store, by: WindowId, base: &Machine, wanted: &Machine) -> Vec<Refused> {
    let mut refused = Vec::new();
    let mut apply = |store: &mut Store, op: Op| {
        if let Err(e) = store.apply(by, op) {
            refused.push(e);
        }
    };
    if base.settings != wanted.settings {
        apply(store, Op::Settings { settings: wanted.settings.clone() });
    }
    if base.profiles != wanted.profiles {
        apply(store, Op::Profiles { profiles: wanted.profiles.clone() });
    }
    if base.crew != wanted.crew {
        let as_json = |crew: &Vec<super::CrewRecord>| serde_json::to_value(crew).unwrap_or_default();
        let merged = merged_list(&as_json(&base.crew), &as_json(&wanted.crew), &as_json(&store.machine.crew), |v| key_of(v, &["id"]));
        if let Ok(crew) = serde_json::from_value(merged) {
            apply(store, Op::Crew { crew });
        }
    }
    // Projects the window removed.
    for gone in base.projects.iter().filter(|p| project(wanted, &p.path).is_none()) {
        if project(&store.machine, &gone.path).is_some() {
            apply(store, Op::RemoveProject { path: gone.path.clone() });
        }
    }
    for wanted_project in &wanted.projects {
        let was = project(base, &wanted_project.path);
        if project(&store.machine, &wanted_project.path).is_none() {
            // One the window had that another removed meanwhile stays removed (it takes that in next).
            if was.is_some() {
                continue;
            }
            apply(store, Op::AddProject { path: wanted_project.path.clone(), settings: Value::Null });
        }
        let empty = Project::default();
        let from = was.unwrap_or(&empty);
        let Some(current) = project(&store.machine, &wanted_project.path) else { continue };
        let fields = changed(from, wanted_project, &current.extra, &["path", "tasks"]);
        if !fields.is_empty() {
            apply(store, Op::SetProject { path: wanted_project.path.clone(), fields });
        }
        // Tasks the window removed.
        let gone: Vec<u64> = was
            .into_iter()
            .flat_map(|p| &p.tasks)
            .filter(|t| task(Some(wanted_project), &t.path).is_none())
            .filter_map(|t| task(project(&store.machine, &wanted_project.path), &t.path).map(|t| t.id))
            .collect();
        for id in gone {
            apply(store, Op::DeleteTask { id });
        }
        for wanted_task in &wanted_project.tasks {
            let base_task = task(was, &wanted_task.path);
            match task(project(&store.machine, &wanted_project.path), &wanted_task.path) {
                Some(current) => {
                    // Its change from what the window had; one the window never had is all of it.
                    let id = current.id;
                    let fields = match base_task {
                        Some(base_task) => merged_task_fields(base_task, wanted_task, current, changed(base_task, wanted_task, &current.extra, &["id", "path"])),
                        None => changed(current, wanted_task, &current.extra, &["id", "path"]),
                    };
                    if !fields.is_empty() {
                        apply(store, Op::SetTask { id, fields });
                    }
                }
                // New in the window. One it had that another window removed meanwhile stays removed.
                None if base_task.is_none() => apply(store, Op::CreateTask { project: wanted_project.path.clone(), task: wanted_task.clone(), after: None }),
                None => {}
            }
        }
        // Its order, when the window changed it: the window's, the store's others after.
        let order = |p: Option<&Project>| p.map(|p| p.tasks.iter().map(|t| t.path.clone()).collect::<Vec<PathBuf>>()).unwrap_or_default();
        let wanted_order = order(Some(wanted_project));
        let Some(current) = project(&store.machine, &wanted_project.path) else { continue };
        let window_moved = was.is_none_or(|w| order(Some(w)) != wanted_order);
        let store_order: Vec<PathBuf> = order(Some(current)).into_iter().filter(|p| wanted_order.contains(p)).collect();
        let wanted_there: Vec<PathBuf> = wanted_order.iter().filter(|p| current.tasks.iter().any(|t| &t.path == *p)).cloned().collect();
        if window_moved && store_order != wanted_there {
            let ids: Vec<u64> = wanted_order.iter().filter_map(|p| current.tasks.iter().find(|t| &t.path == p).map(|t| t.id)).collect();
            let mut after = None;
            for id in ids {
                apply(store, Op::MoveTask { id, after });
                after = Some(id);
            }
        }
    }
    refused
}

/// What changed from `base` (what a window has) to `now` (the store): each
/// task's fields that differ, the tasks and projects come and gone, the
/// order of each project's tasks; for the window to take in.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// By project: fields of the project set.
    pub projects: BTreeMap<PathBuf, Map<String, Value>>,
    pub projects_added: Vec<Project>,
    pub projects_removed: Vec<PathBuf>,
    /// By task path: its fields set (a field null: its default).
    pub tasks: BTreeMap<PathBuf, Map<String, Value>>,
    /// Added, in their project, with all they have.
    pub tasks_added: Vec<(PathBuf, Task)>,
    pub tasks_removed: Vec<PathBuf>,
    /// Projects whose tasks' order changed: their order now.
    pub orders: BTreeMap<PathBuf, Vec<PathBuf>>,
    pub crew: bool,
    /// The crew's records as the window had them from the store: one of
    /// its own not among them is not yet brought, not one removed
    /// elsewhere; one it changed since is its own yet.
    pub crew_before: Vec<super::CrewRecord>,
    pub settings: bool,
    pub profiles: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes { crew_before: self.crew_before.clone(), ..Changes::default() }
    }
}

pub fn changes(base: &Machine, now: &Machine) -> Changes {
    let mut out = Changes {
        crew: base.crew != now.crew,
        crew_before: base.crew.clone(),
        settings: base.settings != now.settings,
        profiles: base.profiles != now.profiles,
        ..Default::default()
    };
    for gone in base.projects.iter().filter(|p| project(now, &p.path).is_none()) {
        out.projects_removed.push(gone.path.clone());
    }
    for p in &now.projects {
        let Some(was) = project(base, &p.path) else {
            out.projects_added.push(p.clone());
            continue;
        };
        let fields = changed(was, p, &Map::new(), &["path", "tasks"]);
        if !fields.is_empty() {
            out.projects.insert(p.path.clone(), fields);
        }
        for t in was.tasks.iter().filter(|t| task(Some(p), &t.path).is_none()) {
            out.tasks_removed.push(t.path.clone());
        }
        for t in &p.tasks {
            match task(Some(was), &t.path) {
                Some(before) => {
                    let fields = changed(before, t, &Map::new(), &["id", "path"]);
                    if !fields.is_empty() {
                        out.tasks.insert(t.path.clone(), fields);
                    }
                }
                None => out.tasks_added.push((p.path.clone(), t.clone())),
            }
        }
        let order: Vec<PathBuf> = p.tasks.iter().map(|t| t.path.clone()).collect();
        if was.tasks.iter().map(|t| &t.path).ne(order.iter()) {
            out.orders.insert(p.path.clone(), order);
        }
    }
    out
}
