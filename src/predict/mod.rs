//! Edit prediction: a small model on this computer guesses what comes
//! next at the cursor, shown as ghost text that Tab takes.
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
    input::{CompletionProvider, Rope, RopeExt as _},
    notification::Notification,
    setting::{SettingField, SettingGroup, SettingItem},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use lsp_types::{CompletionContext, CompletionResponse, InlineCompletionContext, InlineCompletionItem, InlineCompletionResponse};

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

    fn inline_completion(&self, text: &Rope, cursor: usize, _: InlineCompletionContext, window: &mut Window, cx: &mut App) -> Task<anyhow::Result<InlineCompletionResponse>> {
        let none = || Task::ready(Ok(InlineCompletionResponse::Array(Vec::new())));
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
            let item = job.await.ok().flatten();
            Ok(InlineCompletionResponse::Array(item.into_iter().collect()))
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
    fn run(self) -> anyhow::Result<Option<InlineCompletionItem>> {
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
        let suggestion = edit.and_then(|edit| prompt::suggestion(&self.text, self.cursor, edit));
        if std::env::var_os("IDEAE_DEBUG").is_some() {
            eprintln!(
                "predict: {} ms, prompt {} tokens ({} cached), {:?}",
                started.elapsed().as_millis(),
                answer.prompt_n,
                answer.cached_n,
                suggestion.as_ref().map(|s| &s.insert)
            );
            if suggestion.is_none() && std::env::var_os("IDEAE_DEBUG_PREDICT").is_some() {
                eprintln!("predict: asked\n{}\npredict: answered\n{}", prompt.text, answer.content);
            }
        }
        let Some(suggestion) = suggestion else { return Ok(None) };
        let range = suggestion.replaces_to.map(|end| lsp_types::Range::new(self.text.offset_to_position(self.cursor), self.text.offset_to_position(end)));
        Ok(Some(InlineCompletionItem { insert_text: suggestion.insert, filter_text: None, range, command: None, insert_text_format: None }))
    }
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
            "A model on this computer suggests what you type next, as gray text at the cursor: Tab takes it, Esc drops it. Off, nothing of it runs.",
        ))
        .item(SettingItem::new("Model", model).description(
            "Auto: Sweep 1.5B on a graphics card, Sweep 0.5B on the processor without one. Zeta 2.1 is bigger and slower, and wants a card with 8 GB. Downloaded once, when first used.",
        ))
}

#[cfg(test)]
mod tests {
    use super::{Arc, Duration, Instant, Model, Rope, http, prompt, server};

    /// A real llama-server and model, given by `IDEAE_PREDICT_SERVER` and
    /// `IDEAE_PREDICT_MODEL` (nothing is downloaded): it starts, predicts
    /// the body of a function, and stops generating when a request is
    /// dropped. Without the two, it passes without doing anything.
    #[test]
    #[ignore]
    fn a_local_model_predicts_at_the_cursor() {
        let (Some(server), Some(model)) = (std::env::var_os("IDEAE_PREDICT_SERVER"), std::env::var_os("IDEAE_PREDICT_MODEL")) else {
            eprintln!("IDEAE_PREDICT_SERVER and IDEAE_PREDICT_MODEL are not set: skipped");
            return;
        };
        let (server, model) = (std::path::PathBuf::from(server), std::path::PathBuf::from(model));
        let format = if model.to_string_lossy().contains("zeta") { prompt::Format::Zeta } else { prompt::Format::Sweep };
        let devices = server::list_devices(&server).unwrap();
        let device = server::choose(Some(Model::Sweep05), &devices).1;
        let port = server::free_port().unwrap();
        let mut child = std::process::Command::new(&server)
            .args(server::server_args(&model, port, device.as_ref()))
            .current_dir(server.parent().unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        struct Kill<'a>(&'a mut std::process::Child);
        impl Drop for Kill<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Kill(&mut child);
        let since = Instant::now();
        while !matches!(http::status(port, "/health"), Ok(200)) {
            assert!(child.0.try_wait().unwrap().is_none(), "llama-server ended");
            assert!(since.elapsed() < Duration::from_secs(60), "llama-server not ready in a minute");
            std::thread::sleep(Duration::from_millis(50));
        }
        let loaded = since.elapsed();

        let before = "struct Point {\n    x: f64,\n    y: f64,\n}\n\nimpl Point {\n}\n";
        let file = "struct Point {\n    x: f64,\n    y: f64,\n}\n\nimpl Point {\n    fn new(x: f64, y: f64) -> Self {\n        \n    }\n}\n";
        let text = Rope::from(file);
        let cursor = file.find("        \n").unwrap() + 8;
        let mut tracker = prompt::Tracker::default();
        let seen = tracker.see(&"src/point.rs".into(), &text, 7, Some(Rope::from(before)), Instant::now());
        let ask = prompt::Ask { format, path: "src/point.rs", root: "geometry", text: &text, cursor, seen: &seen, with_context: device.is_some() };
        let built = prompt::build(&ask);
        let body = serde_json::to_vec(&serde_json::json!({
            "prompt": built.text, "n_predict": 400, "temperature": 0, "cache_prompt": true, "id_slot": 0, "stream": true, "stop": format.stops(),
        }))
        .unwrap();
        let mut timings = Vec::new();
        let mut shown = None;
        for _ in 0..3 {
            let asked = Instant::now();
            let answer = http::complete(port, &body, &http::Cancel::default(), || true).unwrap();
            timings.push(asked.elapsed().as_millis());
            let edit = prompt::parse(&built, &text, &answer.content).expect("an answer in the format");
            shown = prompt::suggestion(&text, cursor, edit);
        }
        let shown = shown.expect("a suggestion at the cursor");
        assert!(shown.insert.contains("Self") || shown.insert.contains("Point"), "{shown:?}");
        eprintln!("loaded in {loaded:?}; predictions in {timings:?} ms; on {:?}: {shown:?}", device.map(|d| d.name));

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
        assert!(http::complete(port, &long, &cancel, || true).is_err());
        dropper.join().unwrap();
        let asked = Instant::now();
        http::complete(port, &body, &http::Cancel::default(), || true).unwrap();
        let after = asked.elapsed();
        eprintln!("the next prediction after a dropped one: {after:?}");
        assert!(after < Duration::from_secs(10));
        drop(child);
    }
}
