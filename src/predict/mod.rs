//! Edit prediction: a small model on this computer guesses what comes
//! next at the cursor, shown as ghost text, and the edits around it that
//! follow, shown where they are; Tab takes them.
//!
//! llama.cpp's server runs the model, started on first use and ended with
//! the app (server.rs); what it and the model are, and their download
//! (assets.rs); what it is asked and what its answer changes (prompt.rs);
//! HTTP to it (http.rs). Nothing is downloaded before the user says yes.

pub mod assets;
mod http;
pub mod prompt;
pub mod server;
#[cfg(test)]
mod routing_tests;

use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{CompletionProvider, EditPrediction, Rope, RopeExt as _},
    notification::Notification,
    setting::{SettingField, SettingGroup, SettingItem},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use lsp_types::{CompletionContext, CompletionResponse, InlineCompletionContext, InlineCompletionItem};

use assets::Model;
use server::Status;

/// What the window knows of the server, told by its thread.
struct Ui {
    status: Status,
    /// The server was asked to run since the setting last changed.
    asked: bool,
    /// The offer to turn it on was shown in this run.
    offered: bool,
}

impl Global for Ui {}

fn init(cx: &mut App) {
    if cx.has_global::<Ui>() {
        return;
    }
    cx.set_global(Ui { status: Status::Off, asked: false, offered: false });
    let (tx, rx) = async_channel::unbounded();
    server::report_to(tx);
    cx.spawn(async move |cx| {
        while let Ok(status) = rx.recv().await {
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("predict: {status:?}");
            }
            let _ = cx.update(|cx| {
                cx.global_mut::<Ui>().status = status;
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn chosen_model() -> Option<Model> {
    crate::state::with_settings(|s| Model::by_id(&s.prediction_model))
}

/// Turned on or off, or another model: the server follows at once.
pub fn settings_changed(old: &crate::state::Settings, new: &crate::state::Settings, cx: &mut App) {
    if old.edit_prediction == new.edit_prediction && old.prediction_model == new.prediction_model {
        return;
    }
    init(cx);
    let on = new.edit_prediction == Some(true);
    cx.global_mut::<Ui>().asked = on;
    if on {
        server::run(Model::by_id(&new.prediction_model));
    } else {
        server::stop();
    }
}

/// Saving power: the model leaves the GPU and memory; it starts again at
/// the first prediction asked after.
pub fn power_saving(on: bool, cx: &mut App) {
    if !on {
        return;
    }
    init(cx);
    cx.global_mut::<Ui>().asked = false;
    server::stop();
}

pub fn quit() {
    server::quit();
}

/// The language server's completions as they were, and predictions.
/// Every file's editor has one, with a language server or without.
pub struct Provider {
    inner: Option<Rc<dyn CompletionProvider>>,
    path: Arc<str>,
    root: Arc<str>,
    /// The file as it was opened, the start of its first change.
    first: RefCell<Option<Rope>>,
}

pub fn provider(inner: Option<Rc<dyn CompletionProvider>>, path: &Path, root: &Path, text: Rope, cx: &mut App) -> Rc<dyn CompletionProvider> {
    init(cx);
    let shown = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let root = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let on = crate::state::with_settings(|s| s.edit_prediction == Some(true));
    Rc::new(Provider { inner, path: shown.into(), root: root.into(), first: RefCell::new(on.then_some(text)) })
}

static TRACKER: Mutex<Option<prompt::Tracker>> = Mutex::new(None);
/// The latest request: one before it stops at its next piece.
static LATEST: AtomicU64 = AtomicU64::new(0);

impl CompletionProvider for Provider {
    fn completions(&self, text: &Rope, offset: usize, trigger: CompletionContext, window: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        match &self.inner {
            Some(inner) => inner.completions(text, offset, trigger, window, cx),
            None => Task::ready(Ok(CompletionResponse::Array(Vec::new()))),
        }
    }

    fn resolve_completions(
        &self,
        indices: Vec<usize>,
        completions: Rc<RefCell<Box<[lsp_types::request::Completion]>>>,
        cx: &mut App,
    ) -> Task<anyhow::Result<bool>> {
        match &self.inner {
            Some(inner) => inner.resolve_completions(indices, completions, cx),
            None => Task::ready(Ok(false)),
        }
    }

    fn completion_inserted(&self, item: &lsp_types::CompletionItem, before: &Rope, range: std::ops::Range<usize>, after: &Rope, window: &mut Window, cx: &mut App) {
        if let Some(inner) = &self.inner {
            inner.completion_inserted(item, before, range, after, window, cx);
        }
    }

    fn is_completion_trigger(&self, offset: usize, new_text: &str, cx: &mut App) -> bool {
        self.inner.as_ref().is_some_and(|inner| inner.is_completion_trigger(offset, new_text, cx))
    }

    fn inline_completion_debounce(&self) -> Duration {
        match server::ready() {
            Some((_, _, true)) => Duration::from_millis(150),
            _ => Duration::from_millis(300),
        }
    }

    fn edit_prediction(&self, text: &Rope, cursor: usize, _: InlineCompletionContext, window: &mut Window, cx: &mut App) -> Task<anyhow::Result<Option<EditPrediction>>> {
        let none = || Task::ready(Ok(None));
        // Debug aid for screenshots: IDEAE_PREDICT_GHOST="text" is the suggestion, no model asked.
        if let Some(ghost) = std::env::var("IDEAE_PREDICT_GHOST").ok().filter(|g| !g.is_empty()) {
            let item = InlineCompletionItem { range: None, insert_text: ghost, filter_text: None, command: None, insert_text_format: None };
            return Task::ready(Ok(Some(EditPrediction { ghost: Some(item), ..EditPrediction::default() })));
        }
        match crate::state::with_settings(|s| s.edit_prediction) {
            None => {
                offer(window, cx);
                return none();
            }
            Some(false) => return none(),
            Some(true) => {}
        }
        if crate::lsp::POWER_SAVING.load(Ordering::Relaxed) {
            return none();
        }
        let Some((port, format, gpu)) = server::ready() else {
            init(cx);
            let ui = cx.global_mut::<Ui>();
            if !ui.asked {
                ui.asked = true;
                server::run(chosen_model());
            }
            return none();
        };
        let ask = Request {
            path: self.path.clone(),
            root: self.root.clone(),
            text: text.clone(),
            cursor,
            first: self.first.borrow_mut().take(),
            port,
            format,
            gpu,
            id: LATEST.fetch_add(1, Ordering::SeqCst) + 1,
            cancel: Arc::default(),
        };
        let guard = http::CancelOnDrop(ask.cancel.clone());
        let job = cx.background_spawn(crate::procs::on_thread(move || ask.run()));
        cx.spawn(async move |_| {
            let _guard = guard;
            Ok(job.await.ok().flatten())
        })
    }
}

struct Request {
    path: Arc<str>,
    root: Arc<str>,
    text: Rope,
    cursor: usize,
    first: Option<Rope>,
    port: u16,
    format: prompt::Format,
    gpu: bool,
    id: u64,
    cancel: Arc<http::Cancel>,
}

impl Request {
    fn run(self) -> anyhow::Result<Option<EditPrediction>> {
        let started = Instant::now();
        let line = self.text.offset_to_point(self.cursor).row;
        let seen = {
            let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
            tracker.get_or_insert_default().see(&self.path, &self.text, line, self.first.clone(), started)
        };
        let ask = prompt::Ask {
            format: self.format,
            path: &self.path,
            root: &self.root,
            text: &self.text,
            cursor: self.cursor,
            seen: &seen,
            with_context: self.gpu,
        };
        let prompt = prompt::build(&ask);
        let n_predict = if self.format == prompt::Format::Sweep { 512 } else { 400 };
        let body = serde_json::json!({
            "prompt": prompt.text,
            "n_predict": n_predict,
            "temperature": 0,
            "cache_prompt": true,
            "id_slot": 0,
            "stream": true,
            "stop": self.format.stops(),
            "return_tokens": false,
        });
        let body = serde_json::to_vec(&body)?;
        let answer = http::complete(self.port, &body, &self.cancel, || LATEST.load(Ordering::SeqCst) == self.id)?;
        let edit = if answer.cut { None } else { prompt::parse(&prompt, &self.text, &answer.content) };
        let prediction = edit.and_then(|edit| prompt::prediction(&self.text, self.cursor, edit));
        if std::env::var_os("IDEAE_DEBUG").is_some() {
            eprintln!(
                "predict: {} ms, prompt {} tokens ({} cached), {:?}",
                started.elapsed().as_millis(),
                answer.prompt_n,
                answer.cached_n,
                prediction.as_ref().map(|p| &p.edits)
            );
            if prediction.is_none() && std::env::var_os("IDEAE_DEBUG_PREDICT").is_some() {
                eprintln!("predict: asked\n{}\npredict: answered\n{}", prompt.text, answer.content);
            }
        }
        Ok(prediction.map(|p| shown(&self.text, self.cursor, p)))
    }
}

/// A prediction as the editor takes it: the edits elsewhere only when
/// there are any besides the one at the cursor.
fn shown(text: &Rope, cursor: usize, prediction: prompt::Prediction) -> EditPrediction {
    let ghost = prediction.ghost.map(|g| InlineCompletionItem {
        range: g.replaces_to.map(|end| lsp_types::Range::new(text.offset_to_position(cursor), text.offset_to_position(end))),
        insert_text: g.insert,
        filter_text: None,
        command: None,
        insert_text_format: None,
    });
    if ghost.is_some() && prediction.edits.len() == 1 {
        return EditPrediction { ghost, ..EditPrediction::default() };
    }
    EditPrediction { ghost, edits: prediction.edits, caret: prediction.caret }
}

/// Changes the settings as Settings' Save does.
fn set(cx: &mut App, change: impl FnOnce(&mut crate::state::Settings) + 'static) {
    let Some(target) = cx.try_global::<crate::menu::Target>() else { return };
    let workspace = target.workspace.clone();
    let mut settings = crate::state::settings();
    change(&mut settings);
    let _ = workspace.update(cx, |w, cx| w.apply_settings(settings, cx));
}

/// Once, the first time a prediction would be asked: what it is, what it
/// downloads, on or not. Not answered, it is asked again in the next run.
fn offer(window: &mut Window, cx: &mut App) {
    init(cx);
    if std::mem::replace(&mut cx.global_mut::<Ui>().offered, true) {
        return;
    }
    let note = Notification::new().title("Edit prediction").autohide(false).content(|_, _, cx| {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .child(div().text_sm().text_color(muted).child(
                "A small model on this computer can suggest what you type next, as gray text that Tab takes. \
                 Turning it on downloads llama.cpp (about 36 MB) and the model: 1.5 GB with a graphics card, 0.5 GB without. \
                 Settings → Editor turns it off again.",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("predict-on").small().primary().label("Turn on").on_click(cx.listener(|note, _, window, cx| {
                        set(cx, |s| s.edit_prediction = Some(true));
                        note.dismiss(window, cx);
                    })))
                    .child(Button::new("predict-off").small().ghost().label("No, thanks").on_click(cx.listener(|note, _, window, cx| {
                        set(cx, |s| s.edit_prediction = Some(false));
                        note.dismiss(window, cx);
                    }))),
            )
            .into_any_element()
    });
    window.defer(cx, move |window, cx| window.push_notification(note, cx));
}

/// How edit prediction is, for the file's header; nothing once it runs.
pub fn badge(cx: &App) -> Option<AnyElement> {
    if !crate::state::with_settings(|s| s.edit_prediction == Some(true)) {
        return None;
    }
    let (text, failed) = match &cx.try_global::<Ui>()?.status {
        Status::Downloading { what, done, of } => (format!("prediction: downloading {what} {}%", done * 100 / (*of).max(1)), None),
        Status::Starting => ("prediction: starting…".to_string(), None),
        Status::Failed(why) => ("prediction failed".to_string(), Some(why.clone())),
        Status::Off | Status::Ready { .. } => return None,
    };
    let muted = cx.theme().muted_foreground;
    Some(
        div()
            .id("predict-status")
            .flex_none()
            .text_color(if failed.is_some() { gpui_kit::red() } else { muted })
            .when_some(failed, |s, why| {
                s.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(why.clone()).build(window, cx))
            })
            .child(text)
            .into_any_element(),
    )
}

/// Settings → Editor: on or off, and which model.
pub fn settings_group(draft: &Rc<RefCell<crate::state::Settings>>) -> SettingGroup {
    let on = {
        let (read, write) = (draft.clone(), draft.clone());
        let options = [("on", "On"), ("off", "Off")].map(|(v, l)| (SharedString::from(v), SharedString::from(l))).into();
        SettingField::dropdown(
            options,
            move |_| SharedString::from(if read.borrow().edit_prediction == Some(true) { "on" } else { "off" }),
            move |value, _| write.borrow_mut().edit_prediction = Some(value == "on"),
        )
        .default_value(SharedString::from("off"))
    };
    let model = {
        let (read, write) = (draft.clone(), draft.clone());
        let options = [
            ("auto", "Auto"),
            ("sweep-1.5b", "Sweep 1.5B · 1.5 GB"),
            ("sweep-0.5b", "Sweep 0.5B · 0.5 GB"),
            ("zeta-2.1", "Zeta 2.1 · 5 GB"),
        ]
        .map(|(v, l)| (SharedString::from(v), SharedString::from(l)))
        .into();
        SettingField::dropdown(options, move |_| read.borrow().prediction_model.clone().into(), move |value, _| {
            write.borrow_mut().prediction_model = value.to_string()
        })
        .default_value(SharedString::from("auto"))
    };
    SettingGroup::new()
        .title("Edit prediction")
        .item(SettingItem::new("Edit prediction", on).description(
            "A model on this computer suggests what you type next, as gray text at the cursor, and the edits around it that should follow, in red and green where they are: Tab takes them, Esc drops them. Off, nothing of it runs.",
        ))
        .item(SettingItem::new("Model", model).description(
            "Auto: Sweep 1.5B on a graphics card, Sweep 0.5B on the processor without one. Zeta 2.1 is bigger and slower, and wants a card with 8 GB. Downloaded once, when first used.",
        ))
}

#[cfg(test)]
mod tests {
    use super::{Arc, Duration, Instant, Model, Rope, http, prompt, server};
    use gpui_kit::component::input::RopeExt as _;

    /// llama-server with the model of `IDEAE_PREDICT_SERVER` and
    /// `IDEAE_PREDICT_MODEL`, ended when dropped.
    struct Local {
        child: std::process::Child,
        port: u16,
        format: prompt::Format,
        gpu: bool,
        loaded: Duration,
    }

    impl Drop for Local {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn local() -> Option<Local> {
        let (Some(server), Some(model)) = (std::env::var_os("IDEAE_PREDICT_SERVER"), std::env::var_os("IDEAE_PREDICT_MODEL")) else {
            eprintln!("IDEAE_PREDICT_SERVER and IDEAE_PREDICT_MODEL are not set: skipped");
            return None;
        };
        let (server, model) = (std::path::PathBuf::from(server), std::path::PathBuf::from(model));
        let format = if model.to_string_lossy().contains("zeta") { prompt::Format::Zeta } else { prompt::Format::Sweep };
        let devices = server::list_devices(&server).unwrap();
        let device = server::choose(Some(Model::Sweep05), &devices).1;
        let port = server::free_port().unwrap();
        let child = std::process::Command::new(&server)
            .args(server::server_args(&model, port, device.as_ref()))
            .current_dir(server.parent().unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let since = Instant::now();
        let mut local = Local { child, port, format, gpu: device.is_some(), loaded: Duration::ZERO };
        while !matches!(http::status(port, "/health"), Ok(200)) {
            assert!(local.child.try_wait().unwrap().is_none(), "llama-server ended");
            assert!(since.elapsed() < Duration::from_secs(60), "llama-server not ready in a minute");
            std::thread::sleep(Duration::from_millis(50));
        }
        local.loaded = since.elapsed();
        Some(local)
    }

    impl Local {
        /// The model asked about `text` at `cursor`, `before` being the file
        /// before the change being made; its answer, and how long it took.
        fn ask(&self, path: &str, before: &str, text: &str, cursor: usize) -> (Option<prompt::Prediction>, Duration) {
            let text = Rope::from(text);
            let mut tracker = prompt::Tracker::default();
            let line = text.offset_to_point(cursor).row;
            let path: Arc<str> = path.into();
            let seen = tracker.see(&path, &text, line, Some(Rope::from(before)), Instant::now());
            let ask = prompt::Ask { format: self.format, path: &path, root: "demo", text: &text, cursor, seen: &seen, with_context: self.gpu };
            let built = prompt::build(&ask);
            let body = serde_json::to_vec(&serde_json::json!({
                "prompt": built.text, "n_predict": 512, "temperature": 0, "cache_prompt": true, "id_slot": 0, "stream": true, "stop": self.format.stops(),
            }))
            .unwrap();
            let asked = Instant::now();
            let answer = http::complete(self.port, &body, &http::Cancel::default(), || true).unwrap();
            let took = asked.elapsed();
            let answer = prompt::parse(&built, &text, &answer.content).expect("an answer in the format");
            (prompt::prediction(&text, cursor, answer), took)
        }
    }

    /// A real llama-server and model, given by `IDEAE_PREDICT_SERVER` and
    /// `IDEAE_PREDICT_MODEL` (nothing is downloaded): it starts, predicts
    /// the body of a function, and stops generating when a request is
    /// dropped. Without the two, it passes without doing anything.
    #[test]
    #[ignore]
    fn a_local_model_predicts_at_the_cursor() {
        let Some(local) = local() else { return };
        let before = "struct Point {\n    x: f64,\n    y: f64,\n}\n\nimpl Point {\n}\n";
        let file = "struct Point {\n    x: f64,\n    y: f64,\n}\n\nimpl Point {\n    fn new(x: f64, y: f64) -> Self {\n        \n    }\n}\n";
        let cursor = file.find("        \n").unwrap() + 8;
        let mut timings = Vec::new();
        let mut shown = None;
        for _ in 0..3 {
            let (prediction, took) = local.ask("src/point.rs", before, file, cursor);
            timings.push(took.as_millis());
            shown = prediction.and_then(|p| p.ghost);
        }
        let shown = shown.expect("a suggestion at the cursor");
        assert!(shown.insert.contains("Self") || shown.insert.contains("Point"), "{shown:?}");
        eprintln!("loaded in {:?}; predictions in {timings:?} ms; gpu {}: {shown:?}", local.loaded, local.gpu);

        // Dropped after a moment: the server is free again soon after.
        let cancel = Arc::new(http::Cancel::default());
        let dropper = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            })
        };
        let long = serde_json::to_vec(&serde_json::json!({
            "prompt": "// the numbers from 1 to 5000, one per line\n1\n2\n3\n", "n_predict": 4000, "temperature": 0, "stream": true, "id_slot": 0,
        }))
        .unwrap();
        assert!(http::complete(local.port, &long, &cancel, || true).is_err());
        dropper.join().unwrap();
        let (_, after) = local.ask("src/point.rs", before, file, cursor);
        eprintln!("the next prediction after a dropped one: {after:?}");
        assert!(after < Duration::from_secs(10));
    }

    /// What the local model predicts after a change: the next edits it
    /// offers, printed for a person to judge; one at least away from the
    /// cursor. Without the model, skipped.
    #[test]
    #[ignore]
    fn a_local_model_predicts_the_next_edits() {
        let Some(local) = local() else { return };
        let cases: [(&str, &str, &str, &str, &str); 7] = [
            (
                "a parameter renamed",
                "src/area.rs",
                "fn area(w: f64, h: f64) -> f64 {\n    let a = w * h;\n    println!(\"{} {}\", w, h);\n    a\n}\n",
                "fn area(width: f64, h: f64) -> f64 {\n    let a = w * h;\n    println!(\"{} {}\", w, h);\n    a\n}\n",
                "width",
            ),
            (
                "a field added",
                "src/point.rs",
                "pub struct Point {\n    pub x: f64,\n    pub y: f64,\n}\n\nimpl Point {\n    pub fn new(x: f64, y: f64) -> Self {\n        Self { x, y }\n    }\n}\n",
                "pub struct Point {\n    pub x: f64,\n    pub y: f64,\n    pub z: f64,\n}\n\nimpl Point {\n    pub fn new(x: f64, y: f64) -> Self {\n        Self { x, y }\n    }\n}\n",
                "pub z: f64,",
            ),
            (
                "the start of a line fixed",
                "src/show.rs",
                "fn show(user: &User) {\n    let name = user.name();\n    println!(\"{name}\");\n}\n",
                "fn show(user: &User) {\n    let email = user.name();\n    println!(\"{name}\");\n}\n",
                "email",
            ),
            (
                "a variable renamed",
                "src/shop.py",
                "def total(items):\n    count = 0\n    for item in items:\n        count += item.price\n    return count\n",
                "def total(items):\n    amount = 0\n    for item in items:\n        count += item.price\n    return count\n",
                "amount",
            ),
            (
                "a parameter added",
                "src/greet.rs",
                "fn greet(name: &str) {\n    println!(\"hi {name}\");\n}\n\nfn main() {\n    greet(\"ann\");\n    greet(\"bob\");\n}\n",
                "fn greet(name: &str, loud: bool) {\n    println!(\"hi {name}\");\n}\n\nfn main() {\n    greet(\"ann\");\n    greet(\"bob\");\n}\n",
                "loud: bool",
            ),
            (
                "a list renamed",
                "src/cart.js",
                "const items = [];\nfunction add(item) {\n  items.push(item);\n}\nfunction count() {\n  return items.length;\n}\n",
                "const list = [];\nfunction add(item) {\n  items.push(item);\n}\nfunction count() {\n  return items.length;\n}\n",
                "list",
            ),
            (
                "a type changed",
                "src/id.rs",
                "pub struct User {\n    pub id: u32,\n}\n\npub fn id(user: &User) -> u32 {\n    user.id\n}\n",
                "pub struct User {\n    pub id: u64,\n}\n\npub fn id(user: &User) -> u32 {\n    user.id\n}\n",
                "u64",
            ),
        ];
        let mut elsewhere = 0;
        for (name, path, before, after, typed) in cases {
            let cursor = after.find(typed).unwrap() + typed.len();
            let (prediction, took) = local.ask(path, before, after, cursor);
            elsewhere += usize::from(prediction.as_ref().is_some_and(|p| p.edits.iter().any(|(r, _)| r.start != cursor)));
            let shown = prediction.map(|p| {
                let edits: Vec<String> = p.edits.iter().map(|(r, s)| format!("{:?} -> {s:?}", &after[r.clone()])).collect();
                format!("ghost {:?}; edits {edits:?}", p.ghost.map(|g| g.insert))
            });
            eprintln!("{name}: {} ms: {shown:?}", took.as_millis());
        }
        assert!(elsewhere > 0, "no edit away from the cursor was offered");
    }
}
