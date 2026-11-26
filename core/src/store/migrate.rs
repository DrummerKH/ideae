//! The store made from a `state.json` of before it: its projects and tasks,
//! the work of each task out of its session (the tabs stay the window's),
//! its crew, and Global's fields that describe the machine. Nothing of it is
//! dropped: what the store has no field for stays in its `extra`.
//!
//! The window passes its state as it loaded it (older fields already read
//! into their layers), after writing into each project's own layer what its
//! space set of the machine's fields (the window knows how a space's
//! worktrees folder becomes a project's).

use serde_json::{Map, Value};

use super::{Machine, Project, Task, TaskWork, VERSION};

/// Global's fields that describe the machine where the work runs: its
/// Machine layer. Where its worktrees go, the language servers that run
/// there, whether it is kept awake while agents work, its agents (with
/// their profiles). What is a person's habit (agents' and questions'
/// timers, who may start agents, fetching, a request's status, how long
/// archives stay) is not: it stays theirs, in Global and their spaces.
pub const MACHINE_FIELDS: &[&str] = &[
    "agent",
    "default_agent",
    "quick_agent",
    "agents_welcomed",
    "worktrees_dir",
    "lsp_off",
    "lsp_choice",
    "lsp_separate",
    "lsp_idle_minutes",
    "agent_lsp",
    "keep_awake",
    "keep_awake_on_battery",
];

/// A project's fields that are a window's: where it sorts it, folded or
/// not; and those written for older builds only, from its layer.
pub(super) const PROJECT_WINDOW: &[&str] = &["space", "space_id", "collapsed", "agent", "base", "worktrees", "lsp_off", "pr_off", "check_to_agent"];

/// A task's fields that are a window's: whether it was open there, its tabs.
const TASK_WINDOW: &[&str] = &["open", "session"];

pub fn from_state(state: &Value) -> Result<Machine, String> {
    let mut machine = Machine { version: VERSION, ..Default::default() };
    let settings = state.get("settings").and_then(Value::as_object);
    if let Some(settings) = settings {
        let mine: Map<String, Value> = MACHINE_FIELDS.iter().filter_map(|k| Some((k.to_string(), settings.get(*k)?.clone()))).collect();
        machine.settings = Value::Object(mine);
        machine.profiles = settings.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
    }
    for project in state.get("projects").and_then(Value::as_array).into_iter().flatten() {
        let Some(project) = project.as_object() else { continue };
        let mut fields = project.clone();
        for k in PROJECT_WINDOW {
            fields.remove(*k);
        }
        let tasks = fields.remove("tasks").and_then(|t| t.as_array().cloned()).unwrap_or_default();
        let mut made: Project = serde_json::from_value(Value::Object(fields)).map_err(|e| format!("a project: {e}"))?;
        for task in tasks {
            let Value::Object(mut fields) = task else { continue };
            let session = fields.remove("session");
            for k in TASK_WINDOW {
                fields.remove(*k);
            }
            let mut task: Task = serde_json::from_value(Value::Object(fields)).map_err(|e| format!("a task of {}: {e}", made.path.display()))?;
            if let Some(Value::Object(session)) = session {
                let work = ["review", "viewed", "bookmarks"].iter().filter_map(|k| Some((k.to_string(), session.get(*k)?.clone()))).collect();
                task.work = serde_json::from_value::<TaskWork>(Value::Object(work)).map_err(|e| format!("the work of {}: {e}", task.path.display()))?;
            }
            machine.last_task += 1;
            task.id = machine.last_task;
            made.tasks.push(task);
        }
        machine.projects.push(made);
    }
    if let Some(crew) = state.get("crew") {
        machine.crew = serde_json::from_value(crew.clone()).map_err(|e| format!("its crew: {e}"))?;
    }
    machine.last_crew = machine.crew.iter().map(|c| c.id).max().unwrap_or(0);
    Ok(machine)
}
