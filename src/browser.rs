//! A browser tab: a page in a shared headless Chrome, shown through CDP
//! screencast frames, with mouse and keyboard sent back over CDP.
//!
//! Chrome listens on a local DevTools port, so Playwright MCP can drive the
//! very page you see: `--cdp-endpoint http://127.0.0.1:<port>`.

use std::{
    io::ErrorKind,
    net::TcpStream,
    os::unix::process::CommandExt as _,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::ContextMenuExt as _,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

// ---------------------------------------------------------------- chrome

struct Chrome {
    port: u16,
    ws_path: String,
    _child: Child,
    profile: PathBuf,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

static CHROME: OnceLock<Mutex<Option<Arc<Chrome>>>> = OnceLock::new();

/// The app's Chrome, started on first use.
fn chrome() -> Result<Arc<Chrome>> {
    let slot = CHROME.get_or_init(|| Mutex::new(None));
    let mut slot = slot.lock().unwrap();
    if let Some(chrome) = slot.as_ref() {
        return Ok(chrome.clone());
    }
    let program = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]
        .into_iter()
        .find(|p| Command::new(p).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()))
        .ok_or_else(|| anyhow!("no Chrome or Chromium found"))?;
    let profile = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(crate::state::APP_ID)
        .join(format!("chrome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile)?;
    let mut command = Command::new(program);
    command
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--hide-scrollbars",
            "about:blank",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Chrome must not outlive the app, even when the app is killed.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = command.spawn().with_context(|| format!("starting {program}"))?;

    // Chrome writes the port it picked and the browser socket path here.
    let port_file = profile.join("DevToolsActivePort");
    let started = Instant::now();
    let (port, ws_path) = loop {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse() {
                    break (port, path.trim().to_string());
                }
            }
        }
        if started.elapsed() > Duration::from_secs(15) {
            return Err(anyhow!("Chrome did not open its DevTools port"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let chrome = Arc::new(Chrome { port, ws_path, _child: child, profile });
    *slot = Some(chrome.clone());
    Ok(chrome)
}

/// The DevTools endpoint for Playwright MCP (`--cdp-endpoint`), once Chrome runs.
pub fn endpoint() -> Option<String> {
    let slot = CHROME.get()?.lock().unwrap();
    slot.as_ref().map(|c| format!("http://127.0.0.1:{}", c.port))
}

// ---------------------------------------------------------------- page session

enum Out {
    Command(&'static str, Value),
    /// A command whose result the UI waits for.
    Call(&'static str, Value, mpsc::Sender<Value>),
    Close,
}

enum In {
    Frame(Vec<u8>),
    Url(String),
    Error(String),
}

/// One page: a thread owning the CDP socket.
fn spawn_page(url: String, in_tx: async_channel::Sender<In>) -> mpsc::Sender<Out> {
    let (out_tx, out_rx) = mpsc::channel::<Out>();
    std::thread::Builder::new()
        .name("cdp-page".into())
        .spawn(move || {
            if let Err(e) = run_page(&url, &in_tx, out_rx) {
                let _ = in_tx.send_blocking(In::Error(format!("{e:#}")));
            }
        })
        .expect("spawn cdp thread");
    out_tx
}

struct Socket {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    session: Option<String>,
}

impl Socket {
    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let mut msg = json!({ "id": self.next_id, "method": method, "params": params });
        if let Some(session) = &self.session {
            msg["sessionId"] = json!(session);
        }
        self.ws.send(Message::text(msg.to_string()))?;
        Ok(self.next_id)
    }

    /// Sends and waits for the reply, setting other messages aside.
    fn call(&mut self, method: &str, params: Value, aside: &mut Vec<Value>) -> Result<Value> {
        let id = self.send(method, params)?;
        loop {
            let Some(msg) = self.read()? else { continue };
            if msg["id"].as_u64() == Some(id) {
                if let Some(error) = msg.get("error") {
                    return Err(anyhow!("{method}: {}", error["message"]));
                }
                return Ok(msg["result"].clone());
            }
            aside.push(msg);
        }
    }

    fn read(&mut self) -> Result<Option<Value>> {
        match self.ws.read() {
            Ok(Message::Text(text)) => Ok(serde_json::from_str(text.as_str()).ok()),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn run_page(url: &str, in_tx: &async_channel::Sender<In>, out_rx: mpsc::Receiver<Out>) -> Result<()> {
    let chrome = chrome()?;
    let (ws, _) = tungstenite::connect(format!("ws://127.0.0.1:{}{}", chrome.port, chrome.ws_path))?;
    let mut socket = Socket { ws, next_id: 0, session: None };
    let mut aside = Vec::new();

    let target = socket.call("Target.createTarget", json!({ "url": url }), &mut aside)?;
    let target_id = target["targetId"].as_str().unwrap_or_default().to_string();
    let attached = socket.call("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), &mut aside)?;
    socket.session = attached["sessionId"].as_str().map(str::to_string);
    socket.call("Page.enable", json!({}), &mut aside)?;

    // Short reads so commands from the UI go out promptly.
    if let MaybeTlsStream::Plain(stream) = socket.ws.get_mut() {
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    }

    let mut replies: std::collections::HashMap<u64, mpsc::Sender<Value>> = Default::default();
    let handle = |msg: Value, socket: &mut Socket, replies: &mut std::collections::HashMap<u64, mpsc::Sender<Value>>| -> Result<()> {
        if let Some(reply) = msg["id"].as_u64().and_then(|id| replies.remove(&id)) {
            let _ = reply.send(msg["result"].clone());
            return Ok(());
        }
        match msg["method"].as_str() {
            Some("Page.screencastFrame") => {
                let params = &msg["params"];
                socket.send("Page.screencastFrameAck", json!({ "sessionId": params["sessionId"] }))?;
                if let Some(data) = params["data"].as_str() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        let _ = in_tx.try_send(In::Frame(bytes));
                    }
                }
            }
            Some("Page.frameNavigated") if msg["params"]["frame"]["parentId"].is_null() => {
                if let Some(url) = msg["params"]["frame"]["url"].as_str() {
                    let _ = in_tx.try_send(In::Url(url.to_string()));
                }
            }
            _ => {}
        }
        Ok(())
    };
    for msg in aside.drain(..) {
        handle(msg, &mut socket, &mut replies)?;
    }

    loop {
        loop {
            match out_rx.try_recv() {
                Ok(Out::Command(method, params)) => {
                    socket.send(method, params)?;
                }
                Ok(Out::Call(method, params, reply)) => {
                    let id = socket.send(method, params)?;
                    replies.insert(id, reply);
                }
                Ok(Out::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                    socket.session = None;
                    let _ = socket.send("Target.closeTarget", json!({ "targetId": target_id }));
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if let Some(msg) = socket.read()? {
            handle(msg, &mut socket, &mut replies)?;
        }
    }
}

// ---------------------------------------------------------------- view

actions!(
    ideae_browser,
    [BrowserBack, BrowserForward, BrowserReload, BrowserCopy, BrowserPaste, BrowserCopyUrl, BrowserOpenExternal]
);

pub enum BrowserEvent {
    /// The page's URL changed, so the tab title should too.
    Navigated,
}

impl EventEmitter<BrowserEvent> for BrowserView {}

pub struct BrowserView {
    out: mpsc::Sender<Out>,
    url_input: Entity<InputState>,
    url: SharedString,
    frame: Option<Arc<RenderImage>>,
    error: Option<String>,
    focus: FocusHandle,
    /// Where the page is drawn, for mapping the pointer, and the size the
    /// page was last told, to resize it only on change.
    bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    sent_size: std::rc::Rc<std::cell::Cell<(u32, u32, u32)>>,
    _frames: Task<()>,
    /// A sharp PNG of the page once screencast frames stop coming: the JPEG
    /// frames are for motion.
    _sharpen: Task<()>,
}

impl Drop for BrowserView {
    fn drop(&mut self) {
        let _ = self.out.send(Out::Close);
    }
}

impl BrowserView {
    pub fn url(&self) -> String {
        self.url.to_string()
    }

    pub fn new(url: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (in_tx, in_rx) = async_channel::bounded::<In>(4);
        let out = spawn_page(url.to_string(), in_tx);
        let url_input = cx.new(|cx| InputState::new(window, cx).placeholder("http://localhost:3000"));
        url_input.update(cx, |input, cx| input.set_value(url.to_string(), window, cx));
        cx.subscribe_in(&url_input, window, |this, input, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let mut url = input.read(cx).value().trim().to_string();
                if !url.contains("://") && !url.is_empty() {
                    url = format!("http://{url}");
                }
                this.command("Page.navigate", json!({ "url": url }));
            }
        })
        .detach();

        let frames = cx.spawn(async move |this, cx| {
            while let Ok(message) = in_rx.recv().await {
                let update = match message {
                    In::Frame(jpeg) => {
                        let image = cx.background_spawn(async move { decode(&jpeg, image::ImageFormat::Jpeg) }).await;
                        let Some(image) = image else { continue };
                        Some(Ok(image))
                    }
                    In::Url(url) => Some(Err(Ok(url))),
                    In::Error(e) => Some(Err(Err(e))),
                };
                let ok = this.update(cx, |this, cx| {
                    match update {
                        Some(Ok(image)) => {
                            this.show_frame(image, cx);
                            this.sharpen_later(cx);
                        }
                        Some(Err(Ok(url))) => {
                            this.url = url.into();
                            cx.emit(BrowserEvent::Navigated);
                        }
                        Some(Err(Err(e))) => this.error = Some(e),
                        None => {}
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        });

        Self {
            out,
            url_input,
            url: url.to_string().into(),
            frame: None,
            error: None,
            focus: cx.focus_handle(),
            bounds: Default::default(),
            sent_size: Default::default(),
            _frames: frames,
            _sharpen: Task::ready(()),
        }
    }

    pub fn title(&self) -> SharedString {
        let url = self.url.as_ref();
        url.split_once("://").map_or(url, |(_, rest)| rest).trim_end_matches('/').to_string().into()
    }

    fn show_frame(&mut self, image: Arc<RenderImage>, cx: &mut Context<Self>) {
        if let Some(old) = self.frame.replace(image) {
            cx.drop_image(old, None);
        }
        cx.notify();
    }

    /// After 300 ms without new frames, replace the last JPEG with a PNG.
    fn sharpen_later(&mut self, cx: &mut Context<Self>) {
        let out = self.out.clone();
        self._sharpen = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let image = cx
                .background_spawn(async move {
                    let (tx, rx) = mpsc::channel();
                    out.send(Out::Call("Page.captureScreenshot", json!({ "format": "png" }), tx)).ok()?;
                    let result = rx.recv_timeout(Duration::from_secs(5)).ok()?;
                    let png = base64::engine::general_purpose::STANDARD.decode(result["data"].as_str()?).ok()?;
                    decode(&png, image::ImageFormat::Png)
                })
                .await;
            if let Some(image) = image {
                let _ = this.update(cx, |this, cx| this.show_frame(image, cx));
            }
        });
    }

    /// The page's selected text, from the CDP thread.
    fn eval_string(&self, expression: &str) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.out
            .send(Out::Call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), tx))
            .ok()?;
        let result = rx.recv_timeout(Duration::from_secs(2)).ok()?;
        result["result"]["value"].as_str().map(str::to_string)
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.eval_string("window.getSelection().toString()").filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.command("Input.insertText", json!({ "text": text }));
        }
    }

    fn command(&self, method: &'static str, params: Value) {
        let _ = self.out.send(Out::Command(method, params));
    }

    fn pointer(&self, position: Point<Pixels>) -> (f32, f32) {
        let b = self.bounds.get();
        (f32::from(position.x - b.origin.x), f32::from(position.y - b.origin.y))
    }

    fn mouse(&self, kind: &str, position: Point<Pixels>, button: Option<MouseButton>, clicks: usize, modifiers: &Modifiers) {
        let (x, y) = self.pointer(position);
        let button = match button {
            Some(MouseButton::Left) => "left",
            Some(MouseButton::Right) => "right",
            Some(MouseButton::Middle) => "middle",
            _ => "none",
        };
        self.command(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": button, "clickCount": clicks, "modifiers": cdp_modifiers(modifiers) }),
        );
    }

    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        if m.control && !m.alt && !m.shift {
            match keystroke.key.as_str() {
                "c" => return self.copy(cx),
                "v" => return self.paste(cx),
                _ => {}
            }
        }
        let modifiers = cdp_modifiers(&keystroke.modifiers);
        let shortcut = keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;
        if let (Some(text), false) = (&keystroke.key_char, shortcut) {
            self.command("Input.insertText", json!({ "text": text }));
            return;
        }
        let (key, code, vk) = match keystroke.key.as_str() {
            "enter" => ("Enter", "Enter", 13),
            "backspace" => ("Backspace", "Backspace", 8),
            "tab" => ("Tab", "Tab", 9),
            "escape" => ("Escape", "Escape", 27),
            "delete" => ("Delete", "Delete", 46),
            "left" => ("ArrowLeft", "ArrowLeft", 37),
            "up" => ("ArrowUp", "ArrowUp", 38),
            "right" => ("ArrowRight", "ArrowRight", 39),
            "down" => ("ArrowDown", "ArrowDown", 40),
            "home" => ("Home", "Home", 36),
            "end" => ("End", "End", 35),
            "pageup" => ("PageUp", "PageUp", 33),
            "pagedown" => ("PageDown", "PageDown", 34),
            other if other.chars().count() == 1 => {
                // A shortcut such as Ctrl+A: send the key with its modifiers.
                let c = other.chars().next().unwrap().to_ascii_uppercase();
                let code = format!("Key{c}");
                for kind in ["rawKeyDown", "keyUp"] {
                    self.command(
                        "Input.dispatchKeyEvent",
                        json!({ "type": kind, "key": other, "code": code, "windowsVirtualKeyCode": c as u32, "modifiers": modifiers }),
                    );
                }
                return;
            }
            _ => return,
        };
        for kind in ["rawKeyDown", "keyUp"] {
            self.command(
                "Input.dispatchKeyEvent",
                json!({ "type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": modifiers }),
            );
        }
        if key == "Enter" {
            self.command("Input.dispatchKeyEvent", json!({ "type": "char", "text": "\r", "modifiers": modifiers }));
        }
    }
}

fn cdp_modifiers(m: &Modifiers) -> u32 {
    (m.alt as u32) | (m.control as u32) << 1 | (m.platform as u32) << 2 | (m.shift as u32) << 3
}

/// A JPEG or PNG frame to the BGRA image gpui draws.
fn decode(bytes: &[u8], format: image::ImageFormat) -> Option<Arc<RenderImage>> {
    let mut rgba = image::load_from_memory_with_format(bytes, format).ok()?.into_rgba8();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let sent = self.sent_size.clone();
        let out = self.out.clone();
        let scale = window.scale_factor();
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &BrowserBack, _, _| {
                this.command("Runtime.evaluate", json!({ "expression": "history.back()" }))
            }))
            .on_action(cx.listener(|this, _: &BrowserForward, _, _| {
                this.command("Runtime.evaluate", json!({ "expression": "history.forward()" }))
            }))
            .on_action(cx.listener(|this, _: &BrowserReload, _, _| this.command("Page.reload", json!({}))))
            .on_action(cx.listener(|this, _: &BrowserCopy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &BrowserPaste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &BrowserCopyUrl, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(this.url.to_string()))
            }))
            .on_action(cx.listener(|this, _: &BrowserOpenExternal, _, cx| cx.open_url(&this.url)))
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(Button::new("back").xsmall().ghost().label("←").on_click(cx.listener(|this, _, _, _| {
                        this.command("Runtime.evaluate", json!({ "expression": "history.back()" }))
                    })))
                    .child(Button::new("forward").xsmall().ghost().label("→").on_click(cx.listener(|this, _, _, _| {
                        this.command("Runtime.evaluate", json!({ "expression": "history.forward()" }))
                    })))
                    .child(Button::new("reload").xsmall().ghost().label("⟳").on_click(cx.listener(|this, _, _, _| {
                        this.command("Page.reload", json!({}))
                    })))
                    .child(div().flex_1().child(Input::new(&self.url_input).small()))
                    .when_some(endpoint(), |this, endpoint| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("CDP {endpoint}")),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_3().py_1().text_xs().text_color(gpui_kit::red()).child(error))
            })
            .child(
                div()
                    .id("page")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .bg(gpui_kit::white())
                    .track_focus(&self.focus)
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, window, cx| {
                        this.focus.focus(window, cx);
                        this.mouse("mousePressed", e.position, Some(e.button), e.click_count, &e.modifiers)
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, e: &MouseDownEvent, window, cx| {
                        // Focus first, so the context menu's actions reach this view.
                        this.focus.focus(window, cx);
                        this.mouse("mousePressed", e.position, Some(e.button), e.click_count, &e.modifiers)
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &MouseUpEvent, _, _| {
                        this.mouse("mouseReleased", e.position, Some(e.button), e.click_count, &e.modifiers)
                    }))
                    .on_mouse_up(MouseButton::Right, cx.listener(|this, e: &MouseUpEvent, _, _| {
                        this.mouse("mouseReleased", e.position, Some(e.button), e.click_count, &e.modifiers)
                    }))
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, _| {
                        this.mouse("mouseMoved", e.position, e.pressed_button, 0, &e.modifiers)
                    }))
                    .on_scroll(cx.listener(|this, e: &ScrollEvent, window, _| {
                        let delta = e.delta.pixel_delta(window.line_height());
                        let (x, y) = this.pointer(e.position);
                        this.command(
                            "Input.dispatchMouseEvent",
                            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": -f32::from(delta.x), "deltaY": -f32::from(delta.y) }),
                        );
                    }))
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                        this.key(e, cx);
                        cx.stop_propagation();
                    }))
                    .child(
                        // Tracks the page area: Chrome renders at exactly this
                        // size and the window's scale, so frames are 1:1.
                        canvas(
                            move |b, _, _| {
                                bounds.set(b);
                                let size = (f32::from(b.size.width) as u32, f32::from(b.size.height) as u32, (scale * 100.) as u32);
                                if size != sent.get() && size.0 > 0 && size.1 > 0 {
                                    sent.set(size);
                                    // A running screencast keeps its old size: restart it.
                                    let _ = out.send(Out::Command("Page.stopScreencast", json!({})));
                                    let _ = out.send(Out::Command(
                                        "Emulation.setDeviceMetricsOverride",
                                        json!({ "width": size.0, "height": size.1, "deviceScaleFactor": scale, "mobile": false }),
                                    ));
                                    let _ = out.send(Out::Command(
                                        "Page.startScreencast",
                                        json!({ "format": "jpeg", "quality": 85, "maxWidth": (size.0 as f32 * scale) as u32, "maxHeight": (size.1 as f32 * scale) as u32 }),
                                    ));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .when_some(self.frame.clone(), |this, frame| {
                        // 1:1, never stretched: while a resize waits for frames
                        // of the new size, the old one stays sharp at its size.
                        let size = frame.size(0);
                        let (w, h) = (size.width.0 as f32 / scale, size.height.0 as f32 / scale);
                        this.child(img(frame).absolute().top_0().left_0().w(px(w)).h(px(h)))
                    })
                    // Headless Chrome has no menu of its own: that is browser UI.
                    .context_menu(|menu, _, _| {
                        menu.menu("Back", Box::new(BrowserBack))
                            .menu("Forward", Box::new(BrowserForward))
                            .menu("Reload", Box::new(BrowserReload))
                            .separator()
                            .menu("Copy", Box::new(BrowserCopy))
                            .menu("Paste", Box::new(BrowserPaste))
                            .separator()
                            .menu("Copy page address", Box::new(BrowserCopyUrl))
                            .menu("Open in system browser", Box::new(BrowserOpenExternal))
                    })
            )
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
