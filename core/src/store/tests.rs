use super::*;
use serde_json::json;

fn store() -> Store {
    let mut store = Store::default();
    store.apply(1, Op::AddProject { path: "/p".into(), settings: Value::Null }).unwrap();
    let main = Task { path: "/p".into(), main: true, ..Default::default() };
    store.apply(1, Op::CreateTask { project: "/p".into(), task: main, after: None }).unwrap();
    store
}

fn task(path: &str, branch: &str) -> Task {
    Task { path: path.into(), branch: branch.into(), title: branch.into(), ..Default::default() }
}

fn created(store: &mut Store, task: Task) -> u64 {
    match store.apply(1, Op::CreateTask { project: "/p".into(), task, after: None }) {
        Ok(Applied::Task(id)) => id,
        other => panic!("{other:?}"),
    }
}

fn fields(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[test]
fn the_store_gives_what_must_be_one_machine_wide() {
    let mut store = store();
    let a = created(&mut store, task("/w/a", "a"));
    let b = created(&mut store, task("/w/b", "b"));
    assert_ne!(a, b);
    assert_eq!((store.task(a).unwrap().port_slot, store.task(b).unwrap().port_slot), (1, 2), "slots apart; 0 is the main checkout's");
    // A path or a branch another task has.
    assert_eq!(store.apply(2, Op::CreateTask { project: "/p".into(), task: task("/w/a", "c"), after: None }), Err(Refused::Taken("/w/a".into())));
    // A slot let go of is given again; one of before slots gets one when asked.
    store.apply(1, Op::DeleteTask { id: a }).unwrap();
    let old = created(&mut store, task("/w/old", "old"));
    assert_eq!(store.task(old).unwrap().port_slot, 1);
    store.task_mut(old).unwrap().port_slot = 0;
    assert_eq!(store.apply(1, Op::PortSlot { id: old }), Ok(Applied::Slot(1)));
    // Variants of a group are numbered by it.
    let in_group = |path: &str, branch: &str| Task { group: "g".into(), ..task(path, branch) };
    let v1 = created(&mut store, in_group("/w/v1", "v1"));
    let v2 = created(&mut store, in_group("/w/v2", "v2"));
    assert_eq!((store.task(v1).unwrap().variant, store.task(v2).unwrap().variant), (1, 2));
    // Questions and crew records: never one number twice.
    let ask = || QuestionState { title: "why".into(), ..Default::default() };
    assert_eq!(store.apply(1, Op::NewQuestion { id: b, question: ask() }), Ok(Applied::Question(1)));
    assert_eq!(store.apply(2, Op::NewQuestion { id: b, question: ask() }), Ok(Applied::Question(2)));
    assert_eq!(store.apply(1, Op::NewCrew { record: CrewRecord { parent: "/w/b".into(), kind: "question".into(), ..Default::default() } }), Ok(Applied::Crew(1)));
    assert_eq!(store.apply(1, Op::NewCrew { record: CrewRecord::default() }), Ok(Applied::Crew(2)));
    // An archive's key none of the project's archives has.
    let key = |taken: Vec<String>| store.clone_apply(Op::ArchiveKey { id: b, taken, at: 100 });
    assert_eq!(key(vec![]), Applied::Key("b-100".into()));
    assert_eq!(key(vec!["b-100".into()]), Applied::Key("b-100-2".into()));
}

impl Store {
    fn clone_apply(&self, op: Op) -> Applied {
        Store::new(self.machine.clone()).apply(1, op).unwrap()
    }
}

#[test]
fn a_window_sets_a_task_s_fields_but_not_what_the_store_gives() {
    let mut store = store();
    let a = created(&mut store, task("/w/a", "a"));
    let rev = store.machine.rev;
    store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "title": "Renamed", "starred": true, "seen_at": 7 })) }).unwrap();
    let t = store.task(a).unwrap();
    assert_eq!((t.title.as_str(), t.starred, t.seen_at), ("Renamed", true, 7));
    assert!(store.machine.rev > rev, "every change raises the rev");
    for kept in ["id", "path"] {
        let refused = store.apply(2, Op::SetTask { id: a, fields: fields(json!({ kept: 9 })) });
        assert_eq!(refused, Err(Refused::NotSettable(kept.into())));
    }
    // A number another task has is refused; a free one taken.
    let b = created(&mut store, task("/w/b", "b"));
    let slot_of_b = store.task(b).unwrap().port_slot;
    assert!(matches!(store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "port_slot": slot_of_b })) }), Err(Refused::Taken(_))));
    store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "port_slot": 9 })) }).unwrap();
    // Null: its default again.
    store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "note": "n" })) }).unwrap();
    store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "note": null })) }).unwrap();
    assert_eq!(store.task(a).unwrap().note, "");
    // A field this build does not know, set by another, kept.
    store.apply(2, Op::SetTask { id: a, fields: fields(json!({ "later": { "x": 1 } })) }).unwrap();
    store.apply(1, Op::SetTask { id: a, fields: fields(json!({ "title": "Again" })) }).unwrap();
    assert_eq!(store.task(a).unwrap().extra.get("later"), Some(&json!({ "x": 1 })));
    // Its path changes by its own op (a restore), keeping its id.
    store.apply(1, Op::Repath { id: a, path: "/w/a2".into() }).unwrap();
    assert_eq!(store.task_at(Path::new("/w/a2")).map(|t| t.id), Some(a));
}

#[test]
fn a_task_another_window_is_archiving_is_left_to_it() {
    let mut store = store();
    let a = created(&mut store, task("/w/a", "a"));
    store.apply(1, Op::Lease { what: Leased::Task(a) }).unwrap();
    assert_eq!(store.apply(2, Op::Lease { what: Leased::Task(a) }), Err(Refused::Leased));
    assert_eq!(store.apply(2, Op::DeleteTask { id: a }), Err(Refused::Leased));
    assert_eq!(store.apply(2, Op::RemoveProject { path: "/p".into() }), Err(Refused::Leased));
    // A deferred task coming back: its archive is leased, no task yet.
    store.apply(2, Op::Lease { what: Leased::Archive("x-1".into()) }).unwrap();
    assert_eq!(store.apply(1, Op::Lease { what: Leased::Archive("x-1".into()) }), Err(Refused::Leased));
    // The window gone, what it held goes.
    store.release_all(1);
    store.apply(2, Op::DeleteTask { id: a }).unwrap();
    assert!(store.task(a).is_none());
}

#[test]
fn tasks_keep_the_order_they_are_put_in() {
    let mut store = store();
    let main = store.task_at(Path::new("/p")).unwrap().id;
    let a = created(&mut store, task("/w/a", "a"));
    let b = store.apply(1, Op::CreateTask { project: "/p".into(), task: task("/w/b", "b"), after: Some(a) }).unwrap();
    let Applied::Task(b) = b else { panic!() };
    let order = |store: &Store| store.machine.projects[0].tasks.iter().map(|t| t.id).collect::<Vec<_>>();
    assert_eq!(order(&store), vec![a, b, main]);
    store.apply(1, Op::MoveTask { id: main, after: None }).unwrap();
    assert_eq!(order(&store), vec![main, a, b]);
}

#[test]
fn a_state_of_before_becomes_a_store_with_nothing_lost() {
    let state = json!({
        "appearance": "dark",
        "settings": { "theme": "One", "worktrees_dir": "~/wt", "lsp_off": true, "question_close_days": 5, "agents": [{ "id": "claude", "command": "claude" }] },
        "crew": [{ "id": 7, "parent": "/p", "kind": "question", "n": 1 }],
        "projects": [{
            "path": "/p", "space_id": "s1", "collapsed": true, "check": "make test",
            "settings": { "base": "develop" }, "base": "develop",
            "trusted": { "k": true }, "made_by_agents": { "/w/x": "/w/a" },
            "tasks": [
                { "path": "/p", "branch": "main", "main": true, "open": true },
                { "path": "/w/a", "branch": "a", "title": "A", "port_slot": 3, "starred": true, "seen_at": 9, "open": true,
                  "note": "only this variant", "questions": [{ "id": 1, "title": "why", "unread": true }], "asked": 1,
                  "links": [{ "what": "ticket", "key": "DEV-1" }], "sessions": [{ "profile": "claude", "id": "s", "started": 1 }],
                  "session": { "tabs": [{ "kind": "agent" }], "review": [{ "text": "fix", "line": 3 }], "viewed": { "a.rs": 5 }, "bookmarks": { "b.rs": [1] } },
                  "check": { "what": "make test", "state": "Passed", "seconds": 1.5, "output": "", "log": "", "tokens": 0, "asked": 0 },
                  "a_field_of_later": 1 }
            ]
        }]
    });
    let machine = migrate::from_state(&state).unwrap();
    let store = Store::new(machine.clone());
    let p = &machine.projects[0];
    assert_eq!((p.check.as_str(), p.trusted.get("k"), p.settings["base"].as_str()), ("make test", Some(&true), Some("develop")));
    assert!(p.extra.is_empty(), "the window's fields (space, collapsed) and older builds' mirrors stay out: {:?}", p.extra);
    let a = store.task_at(Path::new("/w/a")).unwrap();
    assert_eq!((a.title.as_str(), a.port_slot, a.starred, a.seen_at, a.asked), ("A", 3, true, 9, 1));
    assert_eq!(a.questions[0].title, "why");
    assert_eq!(a.links[0].key, "DEV-1");
    assert_eq!(a.sessions[0].id.as_deref(), Some("s"));
    assert_eq!(a.work.review[0].text, "fix");
    assert_eq!(a.work.viewed.get("a.rs"), Some(&5));
    assert_eq!(a.work.bookmarks.get("b.rs"), Some(&vec![1]));
    assert_eq!(a.check["state"], "Passed");
    assert_eq!(a.extra.get("a_field_of_later"), Some(&json!(1)), "a field the store does not know is kept");
    assert!(!a.extra.contains_key("open") && !a.extra.contains_key("session"), "the window's");
    assert_ne!(a.id, store.task_at(Path::new("/p")).unwrap().id);
    assert_eq!(machine.last_task, 2);
    assert_eq!((machine.crew[0].id, machine.last_crew), (7, 7));
    assert_eq!(machine.settings["worktrees_dir"], "~/wt");
    assert_eq!(machine.settings["lsp_off"], true);
    assert!(machine.settings.get("question_close_days").is_none(), "a person's habit, theirs");
    assert!(machine.settings.get("theme").is_none(), "the person's");
    assert_eq!(machine.profiles[0]["id"], "claude");
    // Through the file and back, all of it.
    let dir = std::env::temp_dir().join(format!("ideae-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    file::save(&dir, &machine).unwrap();
    assert_eq!(file::load(&dir).unwrap().as_ref(), Some(&machine));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_store_on_disk_survives_a_crash_and_is_kept_by_one() {
    let dir = std::env::temp_dir().join(format!("ideae-store-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(file::load(&dir).unwrap(), None, "none yet");
    let held = file::lock(&dir).unwrap();
    assert!(held.is_some());
    assert!(file::lock(&dir).unwrap().is_none(), "a second does not get it");
    let mut one = Machine { version: VERSION, rev: 1, ..Default::default() };
    file::save(&dir, &one).unwrap();
    one.rev = 2;
    file::save(&dir, &one).unwrap();
    assert_eq!(file::load(&dir).unwrap().map(|m| m.rev), Some(2));
    // A crash between the renames: the one before.
    std::fs::remove_file(file::path(&dir)).unwrap();
    assert_eq!(file::load(&dir).unwrap().map(|m| m.rev), Some(1));
    // Both unreadable: kept aside, none (made again of state.json).
    std::fs::write(file::path(&dir), b"{ half").unwrap();
    std::fs::write(dir.join("machine.json.bak"), b"").unwrap();
    assert_eq!(file::load(&dir).unwrap(), None);
    assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("machine.json.unreadable-")));
    // A newer ideae's: not read, so not written over.
    std::fs::write(file::path(&dir), serde_json::to_vec(&Machine { version: VERSION + 1, ..Default::default() }).unwrap()).unwrap();
    assert!(file::load(&dir).is_err());
    drop(held);
    // A process a test beside this one starts may hold it a moment, until it runs its program.
    let taken = (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(25));
        file::lock(&dir).unwrap().is_some()
    });
    assert!(taken, "let go of with its holder");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A real state, copied by hand: every field of every task and project is
/// in the store, or the window's. Prints counts only.
#[test]
#[ignore]
fn a_real_state_moves_whole() {
    let Some(at) = std::env::var_os("IDEAE_MIGRATE_CHECK") else { return };
    let state: Value = serde_json::from_slice(&std::fs::read(at).unwrap()).unwrap();
    let machine = migrate::from_state(&state).unwrap();
    let back = serde_json::to_value(&machine).unwrap();
    let (mut fields, mut lost) = (0, Vec::new());
    for (pi, project) in state["projects"].as_array().unwrap().iter().enumerate() {
        for (k, v) in project.as_object().unwrap() {
            if k == "tasks" || migrate::PROJECT_WINDOW.contains(&k.as_str()) {
                continue;
            }
            fields += 1;
            if back["projects"][pi].get(k) != Some(v) && !(v.is_null() || v == &json!({}) || v == &json!([]) || v == &json!("") || v == &json!(false)) {
                lost.push(format!("project.{k}"));
            }
        }
        for (ti, task) in project["tasks"].as_array().unwrap().iter().enumerate() {
            let got = &back["projects"][pi]["tasks"][ti];
            for (k, v) in task.as_object().unwrap() {
                if k == "open" {
                    continue;
                }
                if k == "session" {
                    for w in ["review", "viewed", "bookmarks"] {
                        if let Some(v) = v.get(w).filter(|v| *v != &json!([]) && *v != &json!({})) {
                            fields += 1;
                            if got["work"].get(w) != Some(v) {
                                lost.push(format!("task.session.{w}"));
                            }
                        }
                    }
                    continue;
                }
                fields += 1;
                if got.get(k) != Some(v) && !(v.is_null() || v == &json!({}) || v == &json!([]) || v == &json!("") || v == &json!(false) || v == &json!(0)) {
                    lost.push(format!("task.{k}"));
                }
            }
        }
    }
    let tasks: usize = machine.projects.iter().map(|p| p.tasks.len()).sum();
    eprintln!("{} projects, {tasks} tasks, {fields} fields compared, {} crew", machine.projects.len(), machine.crew.len());
    lost.sort();
    lost.dedup();
    assert!(lost.is_empty(), "not in the store: {lost:?}");
}

#[test]
fn a_window_s_changes_reach_the_store_as_ops() {
    let state = |tasks: Value| json!({ "settings": { "worktrees_dir": "~/wt" }, "projects": [{ "path": "/p", "check": "make", "tasks": tasks }] });
    let before = state(json!([
        { "path": "/p", "branch": "main", "main": true },
        { "path": "/w/a", "branch": "a", "title": "A", "note": "n", "port_slot": 1 },
        { "path": "/w/b", "branch": "b", "title": "B", "port_slot": 2 }
    ]));
    let mut store = Store::new(migrate::from_state(&before).unwrap());
    let (a, b) = (store.task_at(Path::new("/w/a")).unwrap().id, store.task_at(Path::new("/w/b")).unwrap().id);
    // Another build's field on a, which this window does not know.
    store.apply(9, Op::SetTask { id: a, fields: fields(json!({ "later": 1 })) }).unwrap();
    let rev = store.machine.rev;
    // Nothing changed: no op.
    let base = store.machine.clone();
    assert!(sync::sync(&mut store, 1, &base, &migrate::from_state(&before).unwrap()).is_empty());
    assert_eq!(store.machine.rev, rev);
    // Renamed, its note cleared, b gone, c new, the order changed, the machine's settings changed.
    let mut after = state(json!([
        { "path": "/w/c", "branch": "c", "title": "C", "port_slot": 2 },
        { "path": "/p", "branch": "main", "main": true },
        { "path": "/w/a", "branch": "a", "title": "A2", "port_slot": 1 }
    ]));
    after["settings"]["worktrees_dir"] = json!("~/other");
    let base = store.machine.clone();
    assert!(sync::sync(&mut store, 1, &base, &migrate::from_state(&after).unwrap()).is_empty());
    let t = store.task(a).unwrap();
    assert_eq!((t.title.as_str(), t.note.as_str()), ("A2", ""), "its id kept, its fields set");
    assert_eq!(t.extra.get("later"), Some(&json!(1)), "another build's field kept");
    assert!(store.task(b).is_none());
    let c = store.task_at(Path::new("/w/c")).unwrap();
    assert_eq!(c.port_slot, 2, "b's slot, let go of, taken by c");
    let order: Vec<_> = store.machine.projects[0].tasks.iter().map(|t| t.path.display().to_string()).collect();
    assert_eq!(order, ["/w/c", "/p", "/w/a"]);
    assert_eq!(store.machine.settings["worktrees_dir"], "~/other");
    assert_eq!(store.machine.projects[0].check, "make");
    // Synced again from the same: nothing more.
    let rev = store.machine.rev;
    let base = store.machine.clone();
    assert!(sync::sync(&mut store, 1, &base, &migrate::from_state(&after).unwrap()).is_empty());
    assert_eq!(store.machine.rev, rev);
}

#[test]
fn two_windows_change_one_store_and_neither_undoes_the_other() {
    let state = json!({ "projects": [{ "path": "/p", "tasks": [
        { "path": "/p", "branch": "main", "main": true },
        { "path": "/w/a", "branch": "a", "title": "A", "note": "n", "port_slot": 1 },
        { "path": "/w/b", "branch": "b", "title": "B", "port_slot": 2 }
    ] }] });
    let mut store = Store::new(migrate::from_state(&state).unwrap());
    let seen = store.machine.clone();
    let with = |base: &Machine, edit: &dyn Fn(&mut Machine)| {
        let mut m = base.clone();
        edit(&mut m);
        m
    };
    fn at<'a>(m: &'a mut Machine, path: &str) -> &'a mut Task {
        m.projects[0].tasks.iter_mut().find(|t| t.path == Path::new(path)).unwrap()
    }
    // Window 1 renames a; window 2, from the same base, stars b and adds c.
    let one = with(&seen, &|m| at(m, "/w/a").title = "A by one".into());
    let two = with(&seen, &|m| {
        at(m, "/w/b").starred = true;
        m.projects[0].tasks.push(Task { path: "/w/c".into(), branch: "c".into(), title: "C".into(), ..Default::default() });
    });
    assert!(sync::sync(&mut store, 1, &seen, &one).is_empty());
    assert!(sync::sync(&mut store, 2, &seen, &two).is_empty());
    let get = |store: &Store, path: &str| store.task_at(Path::new(path)).cloned().unwrap();
    assert_eq!(get(&store, "/w/a").title, "A by one", "window 2's base had A: its sync does not take one's rename back");
    assert!(get(&store, "/w/b").starred);
    assert_eq!(get(&store, "/w/c").title, "C");
    // What window 1 takes in: only window 2's, not its own.
    let changes = sync::changes(&one, &store.machine);
    assert_eq!(changes.tasks.keys().map(|p| p.display().to_string()).collect::<Vec<_>>(), ["/w/b"]);
    assert_eq!(changes.tasks[Path::new("/w/b")].get("starred"), Some(&json!(true)));
    assert_eq!(changes.tasks_added.len(), 1);
    assert_eq!(changes.orders.len(), 1, "c came in");
    // A task one window removed is not brought back by another that still had it unchanged.
    let base = store.machine.clone();
    let without_b = with(&base, &|m| m.projects[0].tasks.retain(|t| t.path != Path::new("/w/b")));
    assert!(sync::sync(&mut store, 1, &base, &without_b).is_empty());
    let still = with(&base, &|m| at(m, "/w/a").note = "two's note".into());
    assert!(sync::sync(&mut store, 2, &base, &still).is_empty());
    assert!(store.task_at(Path::new("/w/b")).is_none(), "gone stays gone");
    assert_eq!(get(&store, "/w/a").note, "two's note");
}

#[test]
fn two_windows_add_to_one_task_s_lists_and_both_stay() {
    let state = json!({ "crew": [{ "id": 1, "parent": "/w/a", "kind": "question" }], "projects": [{ "path": "/p", "tasks": [
        { "path": "/w/a", "branch": "a", "port_slot": 1, "links": [{ "what": "url", "url": "https://x" }],
          "session": { "review": [{ "path": "f.rs", "line": 1, "text": "old" }], "viewed": { "f.rs": 1 } } }
    ] }] });
    let mut store = Store::new(migrate::from_state(&state).unwrap());
    let seen = store.machine.clone();
    // Each asks a question: its id the store's, given as it is asked.
    let q = |store: &mut Store| match store.apply(1, Op::ReserveQuestion { task: "/w/a".into() }) {
        Ok(Applied::Question(id)) => id,
        other => panic!("{other:?}"),
    };
    let (q1, q2) = (q(&mut store), q(&mut store));
    assert_ne!(q1, q2);
    let crew = |store: &mut Store| match store.apply(1, Op::ReserveCrew) {
        Ok(Applied::Crew(id)) => id,
        other => panic!("{other:?}"),
    };
    let (c1, c2) = (crew(&mut store), crew(&mut store));
    assert_eq!((c1, c2), (2, 3), "above the crew kept");
    let edit = |base: &Machine, q: u32, link: &str, note: &str, c: u32| {
        let mut m = base.clone();
        let t = &mut m.projects[0].tasks[0];
        t.questions.push(QuestionState { id: q, title: format!("q{q}"), ..Default::default() });
        t.asked = q;
        t.links.push(TaskLink { what: "url".into(), url: link.into(), ..Default::default() });
        t.work.review.push(ReviewNote { path: "f.rs".into(), line: 2, text: note.into(), ..Default::default() });
        t.work.viewed.insert(format!("{note}.rs"), 2);
        m.crew.push(CrewRecord { id: c, parent: "/w/a".into(), kind: "task".into(), ..Default::default() });
        m
    };
    let one = edit(&seen, q1, "https://one", "one", c1);
    let two = edit(&seen, q2, "https://two", "two", c2);
    assert!(sync::sync(&mut store, 1, &seen, &one).is_empty());
    assert!(sync::sync(&mut store, 2, &seen, &two).is_empty());
    let t = store.task_at(Path::new("/w/a")).unwrap();
    assert_eq!(t.questions.iter().map(|q| q.id).collect::<Vec<_>>(), [q1, q2], "both questions");
    assert_eq!(t.asked, q2, "the count only goes up");
    assert_eq!(t.links.iter().map(|l| l.url.as_str()).collect::<Vec<_>>(), ["https://x", "https://one", "https://two"]);
    assert_eq!(t.work.review.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["old", "one", "two"]);
    assert_eq!(t.work.viewed.len(), 3);
    assert_eq!(store.machine.crew.iter().map(|c| c.id).collect::<Vec<_>>(), [1, c1, c2]);
    // One removes the old link and note; the other's stay.
    let base = store.machine.clone();
    let mut fewer = base.clone();
    fewer.projects[0].tasks[0].links.retain(|l| l.url != "https://x");
    fewer.projects[0].tasks[0].work.review.retain(|n| n.text != "old");
    assert!(sync::sync(&mut store, 1, &base, &fewer).is_empty());
    let t = store.task_at(Path::new("/w/a")).unwrap();
    assert_eq!(t.links.len(), 2);
    assert_eq!(t.work.review.len(), 2);
}

#[test]
fn an_older_build_s_state_and_another_window_s_change_are_both_kept() {
    let dir = std::env::temp_dir().join(format!("ideae-store-both-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("state.json");
    let state = json!({ "projects": [{ "path": "/p", "tasks": [{ "path": "/p", "branch": "main", "main": true }] }] });
    std::fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
    let open = |rev: u64| {
        // A process a test beside this one starts may hold the lock a moment.
        (0..40).find_map(|_| host::Host::open(&dir, &file, &state, rev, true).ok()).expect("opened")
    };
    let (mut h, _) = open(0);
    // Its own window mirrors (state.json written at that rev); another (over ssh) does not.
    let seen = h.machine().clone();
    let mut mine = seen.clone();
    mine.projects[0].check = "make".into();
    let (rev, _) = h.sync(1, &seen, &mine, true);
    let mut theirs = h.machine().clone();
    theirs.projects[0].tasks.push(Task { path: "/w/b".into(), branch: "b".into(), title: "Theirs".into(), ..Default::default() });
    let base = h.machine().clone();
    h.sync(2, &base, &theirs, false);
    assert_eq!(h.machine().mirrored, rev, "not moved by another machine's window");
    drop(h);
    // An older build wrote state.json since: both changed, the store's kept and that one told.
    let (h, note) = open(0);
    assert!(note.is_some_and(|n| n.contains("both changed")));
    assert!(h.store.task_at(Path::new("/w/b")).is_some(), "the other window's task kept");
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_another_window_removed_is_not_brought_back() {
    let state = json!({ "projects": [{ "path": "/p", "tasks": [{ "path": "/p", "branch": "main", "main": true }] }] });
    let mut store = Store::new(migrate::from_state(&state).unwrap());
    let seen = store.machine.clone();
    store.apply(2, Op::RemoveProject { path: "/p".into() }).unwrap();
    // A window that has not taken that in yet saves a change of its own.
    let mut wanted = seen.clone();
    wanted.projects[0].tasks[0].starred = true;
    assert!(sync::sync(&mut store, 1, &seen, &wanted).is_empty());
    assert!(store.machine.projects.is_empty(), "removed stays removed");
}
