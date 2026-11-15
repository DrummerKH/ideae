//! Minimal stdio LSP client: hover, go to definition, implementations and
//! references. Calls block, so run them on the background executor.

use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};

type Pending = Arc<Mutex<HashMap<i64, mpsc::Sender<Result<Value>>>>>;
/// Work-done progress the server has begun and not ended: token → title.
type Progress = Arc<Mutex<HashMap<String, String>>>;
type Import = Arc<Mutex<ImportState>>;

/// The server's import of the build: its errors so far, and how it ended.
#[derive(Default)]
struct ImportState {
    log: Vec<String>,
    failed: Option<String>,
}

pub struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    ready: (Mutex<Option<Result<(), String>>>, Condvar),
    opened: Mutex<HashSet<String>>,
    progress: Progress,
    import: Import,
    /// `capabilities` from the server's answer to `initialize`.
    capabilities: Mutex<Value>,
    child: Mutex<Option<Child>>,
}

/// Power saving: no language servers run, none start.
pub static POWER_SAVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The server for a file: its language id, its name, the executable to run
/// (a name on PATH or a path ideae installed it at) and its arguments.
pub struct Server {
    pub language_id: &'static str,
    pub program: &'static str,
    pub command: PathBuf,
    pub args: &'static [&'static str],
}

/// A language server ideae can run: the program, its arguments, and the
/// file extensions it serves with their LSP language ids. The one list:
/// Settings shows what is here.
pub struct ServerDef {
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub languages: &'static [(&'static str, &'static str)],
}

pub const SERVERS: &[ServerDef] = &[
    ServerDef { program: "gopls", args: &[], languages: &[("go", "go")] },
    ServerDef { program: "rust-analyzer", args: &[], languages: &[("rs", "rust")] },
    ServerDef {
        program: "typescript-language-server",
        args: &["--stdio"],
        languages: &[("ts", "typescript"), ("tsx", "typescriptreact"), ("js", "javascript"), ("jsx", "javascriptreact")],
    },
    ServerDef { program: "intelephense", args: &["--stdio"], languages: &[("php", "php")] },
    ServerDef { program: "kotlin-lsp", args: &["--stdio"], languages: &[("kt", "kotlin"), ("kts", "kotlin")] },
];

impl ServerDef {
    /// ".go", ".ts .tsx .js .jsx": what it is for, in Settings.
    pub fn extensions(&self) -> String {
        self.languages.iter().map(|(ext, _)| format!(".{ext}")).collect::<Vec<_>>().join(" ")
    }

    /// On PATH or installed by ideae.
    pub fn available(&self) -> bool {
        locate(self.program).is_some()
    }
}

fn known(path: &Path) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    SERVERS.iter().find_map(|def| {
        def.languages.iter().find(|(e, _)| *e == ext).map(|(_, language)| (*language, def.program, def.args))
    })
}

/// Which server handles a file, by extension, or why there is none, for the
/// editor's status line.
pub fn server_for(path: &Path) -> Result<Server, String> {
    if POWER_SAVING.load(Ordering::Relaxed) {
        return Err("language servers off (power saving)".into());
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let (language_id, program, args) = known(path).ok_or_else(|| format!("no language server for .{ext}"))?;
    if crate::state::settings().lsp_off.iter().any(|p| p == program) {
        return Err(format!("{program} off in Settings"));
    }
    let command = locate(program).ok_or_else(|| format!("{program} not installed"))?;
    Ok(Server { language_id, program, command, args })
}

/// The server a file would have, when it is neither on PATH nor installed.
pub fn missing_server(path: &Path) -> Option<&'static str> {
    if POWER_SAVING.load(Ordering::Relaxed) {
        return None;
    }
    known(path).map(|(_, program, _)| program).filter(|program| locate(program).is_none())
}

/// PATH first, then ideae's own copy.
fn locate(program: &str) -> Option<PathBuf> {
    if runs(program) {
        return Some(PathBuf::from(program));
    }
    crate::lsp_install::installed(program)
}

/// Whether the server is on PATH. Found there is enough, except for
/// rust-analyzer: rustup puts a proxy on PATH that fails when the component
/// is missing, so that one has to answer `--version`. Others cannot be asked
/// (intelephense crashes without a transport flag).
fn runs(program: &str) -> bool {
    static CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    if let Some(known) = CACHE.lock().unwrap().as_ref().and_then(|cache| cache.get(program).copied()) {
        return known;
    }
    // Not under the lock: asking takes a process, and the UI thread asks too.
    let found = on_path(program)
        && (program != "rust-analyzer"
            || crate::procs::command(program)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success()));
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(program.to_string(), found);
    found
}

/// Finds out which servers are there before a file asks, off the UI thread.
pub fn look_for_servers() {
    for def in SERVERS {
        runs(def.program);
    }
}

pub fn on_path(program: &str) -> bool {
    crate::procs::which(program).is_some()
}

/// The directory a server should treat as its workspace: the outermost
/// ancestor of `file` holding one of the server's project markers, no higher
/// than `limit`. Dependencies inside a project carry markers of their own
/// (vendor/*/Cargo.toml, vendor/*/composer.json, node_modules/*/package.json,
/// included Gradle builds), and rooting at them would start one more server
/// indexing it all again, gigabytes apiece. Projects side by side in a repo
/// without a root marker still get one server each.
pub fn project_root(program: &str, file: &Path, limit: &Path) -> PathBuf {
    let markers: &[&str] = match program {
        "gopls" => &["go.work", "go.mod"],
        "rust-analyzer" => &["Cargo.toml"],
        "typescript-language-server" => &["tsconfig.json", "package.json"],
        "intelephense" => &["composer.json"],
        "kotlin-lsp" => &["settings.gradle.kts", "settings.gradle"],
        _ => &[],
    };
    let mut found = None;
    let mut dir = file.parent();
    while let Some(d) = dir {
        if markers.iter().any(|m| d.join(m).is_file()) {
            found = Some(d.to_path_buf());
        }
        if d == limit {
            break;
        }
        dir = d.parent();
    }
    found.unwrap_or_else(|| limit.to_path_buf())
}

/// Highlighter language name for the editor, the same table gpui-kit's diff
/// uses to detect a file's language (its own is crate-private).
pub fn language_for(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    match name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "cargo.lock" => return "toml",
        ".editorconfig" | ".gitconfig" | ".npmrc" => return "ini",
        "jenkinsfile" => return "groovy",
        _ => {}
    }
    if name.starts_with("dockerfile") || name.starts_with("containerfile") || name.ends_with(".dockerfile") {
        return "dockerfile";
    }
    // .env, .env.local, .env.example: KEY=value lines and # comments.
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") {
        return "properties";
    }
    if name.ends_with(".conf") && path.to_string_lossy().contains("nginx") {
        return "nginx";
    }
    match name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or_default() {
        "go" => "go",
        "rs" => "rust",
        "kt" | "kts" => "kotlin",
        "java" => "java",
        "swift" => "swift",
        "scala" => "scala",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "php" | "phtml" => "php",
        "py" | "pyi" => "python",
        "rb" => "ruby",
        "lua" => "lua",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "svelte" => "svelte",
        "astro" => "astro",
        "json" | "jsonc" => "json",
        "md" | "mdx" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sql" => "sql",
        "graphql" | "gql" => "graphql",
        "proto" => "proto",
        "sh" | "bash" | "zsh" => "bash",
        "css" => "css",
        // tree-sitter-scss passes MSVC a flag it rejects; CSS's grammar
        // reads most of SCSS.
        "scss" | "sass" => "css",
        "less" => "less",
        "twig" => "twig",
        "dart" => "dart",
        "nix" => "nix",
        "hs" | "lhs" => "haskell",
        "r" => "r",
        "ml" | "mli" => "ocaml",
        "erl" | "hrl" => "erlang",
        "fish" => "fish",
        "glsl" | "vert" | "frag" | "geom" | "comp" | "tesc" | "tese" => "glsl",
        "sol" => "solidity",
        "jl" => "julia",
        "elm" => "elm",
        "gleam" => "gleam",
        "prisma" => "prisma",
        "html" | "htm" | "vue" => "html",
        "cmake" => "cmake",
        "diff" | "patch" => "diff",
        "xml" | "svg" | "plist" | "xsd" | "xsl" | "xslt" => "xml",
        "properties" => "properties",
        "gradle" | "groovy" => "groovy",
        "bat" | "cmd" => "batch",
        "ini" | "cfg" => "ini",
        "ps1" | "psm1" | "psd1" => "powershell",
        _ => "text",
    }
}

/// `/a/b.rs` → `file:///a/b.rs`; `C:\a\b.rs` → `file:///C:/a/b.rs`.
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = if cfg!(windows) { format!("/{}", text.replace('\\', "/")) } else { text.into_owned() };
    let mut uri = String::from("file://");
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'.' | b'_' | b'~' | b'+' | b'@' => uri.push(b as char),
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path = decode_uri_path(uri)?;
    if !cfg!(windows) {
        return Some(PathBuf::from(path));
    }
    // `/c:/a/b.rs`, as servers write it, is `C:\a\b.rs`, the drive in capitals
    // as the rest of the app has it.
    let path = path.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':')).unwrap_or(&path);
    let mut path = path.replace('/', "\\");
    if path.as_bytes().get(1) == Some(&b':') {
        path[..1].make_ascii_uppercase();
    }
    Some(PathBuf::from(path))
}

fn decode_uri_path(uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let mut out = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

impl LspClient {
    /// Starts the server; initialization runs on its own thread, and every
    /// call waits for it.
    pub fn spawn(program: &str, executable: &Path, args: &[&str], root: &Path, init_options: Value) -> Result<Arc<Self>> {
        let mut command = crate::procs::command(executable);
        command.args(args).envs(server_env(program)).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Dies with the app even when it exits without running destructors.
        let mut child = crate::procs::spawn_tied(&mut command)
            .with_context(|| format!("starting {program}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();
        let progress: Progress = Arc::default();

        let reader_stdin = stdin.clone();
        let reader_pending = pending.clone();
        let reader_progress = progress.clone();
        let import: Import = Arc::default();
        let reader_import = import.clone();
        thread::Builder::new()
            .name(format!("lsp-read-{program}"))
            .spawn(move || read_loop(stdout, reader_stdin, reader_pending, reader_progress, reader_import))?;

        let client = Arc::new(Self {
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            ready: (Mutex::new(None), Condvar::new()),
            opened: Mutex::default(),
            progress,
            import,
            capabilities: Mutex::new(Value::Null),
            child: Mutex::new(Some(child)),
        });

        let init = client.clone();
        let root = root.to_path_buf();
        thread::Builder::new()
            .name(format!("lsp-init-{program}"))
            .spawn(move || {
                let result = init.initialize(&root, init_options).map_err(|e| e.to_string());
                let (lock, cvar) = &init.ready;
                *lock.lock().unwrap() = Some(result);
                cvar.notify_all();
            })?;
        Ok(client)
    }

    fn initialize(&self, root: &Path, init_options: Value) -> Result<()> {
        let uri = path_to_uri(root);
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let answer = self.raw_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": name }],
                "initializationOptions": init_options,
                "capabilities": {
                    // The editor counts columns in characters.
                    "general": { "positionEncodings": ["utf-32", "utf-16"] },
                    "textDocument": {
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "definition": { "linkSupport": true },
                        "implementation": { "linkSupport": true },
                        "references": {},
                        "synchronization": { "didSave": false },
                    },
                    "workspace": { "workspaceFolders": true, "configuration": true },
                    "window": { "workDoneProgress": true },
                },
            }),
        )?;
        *self.capabilities.lock().unwrap() = answer["capabilities"].clone();
        self.notify("initialized", json!({}))
    }

    fn wait_ready(&self) -> Result<()> {
        let (lock, cvar) = &self.ready;
        let guard = lock.lock().unwrap();
        let (guard, _) = cvar
            .wait_timeout_while(guard, Duration::from_secs(60), |r| r.is_none())
            .unwrap();
        match guard.as_ref() {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(anyhow!("language server failed to start: {e}")),
            None => Err(anyhow!("language server did not initialize in time")),
        }
    }

    /// Title of some work the server is still doing, such as indexing.
    pub fn busy(&self) -> Option<String> {
        self.progress.lock().unwrap().values().next().cloned()
    }

    /// Why the server knows less of the project than it should: its import
    /// of the build failed, so what other modules and libraries declare is
    /// not found.
    pub fn problem(&self) -> Option<String> {
        self.import.lock().unwrap().failed.clone()
    }

    pub fn did_open(&self, path: &Path, language_id: &str, text: &str) -> Result<()> {
        self.wait_ready()?;
        let uri = path_to_uri(path);
        if !self.opened.lock().unwrap().insert(uri.clone()) {
            return Ok(());
        }
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
        )
    }

    /// The file's tab closed: the server lets go of the text, and opening
    /// the file again sends it anew.
    pub fn did_close(&self, path: &Path) -> Result<()> {
        let uri = path_to_uri(path);
        if !self.opened.lock().unwrap().remove(&uri) {
            return Ok(());
        }
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }))
    }

    /// Full-text sync: a change without a range replaces the whole document,
    /// which every server accepts whatever sync kind it prefers.
    pub fn did_change(&self, path: &Path, version: i32, text: &str) -> Result<()> {
        self.wait_ready()?;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": path_to_uri(path), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    pub fn did_save(&self, path: &Path) -> Result<()> {
        self.wait_ready()?;
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": path_to_uri(path) } }))
    }

    pub fn hover(&self, uri: &str, pos: lsp_types::Position) -> Result<Option<lsp_types::Hover>> {
        self.wait_ready()?;
        let value = self.raw_request("textDocument/hover", position_params(uri, pos))?;
        Ok(serde_json::from_value(value).unwrap_or(None))
    }

    pub fn definition(&self, uri: &str, pos: lsp_types::Position) -> Result<Vec<lsp_types::LocationLink>> {
        self.wait_ready()?;
        let value = self.raw_request("textDocument/definition", position_params(uri, pos))?;
        if value.is_null() {
            return Ok(vec![]);
        }
        let response: lsp_types::GotoDefinitionResponse = serde_json::from_value(value)?;
        let to_link = |l: lsp_types::Location| lsp_types::LocationLink {
            origin_selection_range: None,
            target_uri: l.uri,
            target_range: l.range,
            target_selection_range: l.range,
        };
        Ok(match response {
            lsp_types::GotoDefinitionResponse::Scalar(l) => vec![to_link(l)],
            lsp_types::GotoDefinitionResponse::Array(ls) => ls.into_iter().map(to_link).collect(),
            lsp_types::GotoDefinitionResponse::Link(links) => links,
        })
    }

    /// Whether the server offers `provider` (such as `referencesProvider`):
    /// `true` or options. Waits for initialization.
    pub fn supports(&self, provider: &str) -> bool {
        self.wait_ready().is_ok()
            && match &self.capabilities.lock().unwrap()[provider] {
                Value::Bool(b) => *b,
                Value::Null => false,
                _ => true,
            }
    }

    /// textDocument/implementation: the types implementing an interface or
    /// trait, the overrides of a method.
    pub fn implementation(&self, uri: &str, pos: lsp_types::Position) -> Result<Vec<lsp_types::Location>> {
        self.wait_ready()?;
        let value = self.raw_request("textDocument/implementation", position_params(uri, pos))?;
        if value.is_null() {
            return Ok(vec![]);
        }
        let response: lsp_types::GotoDefinitionResponse = serde_json::from_value(value)?;
        Ok(match response {
            lsp_types::GotoDefinitionResponse::Scalar(l) => vec![l],
            lsp_types::GotoDefinitionResponse::Array(ls) => ls,
            lsp_types::GotoDefinitionResponse::Link(links) => links
                .into_iter()
                .map(|l| lsp_types::Location { uri: l.target_uri, range: l.target_selection_range })
                .collect(),
        })
    }

    /// textDocument/references, without the declaration itself.
    pub fn references(&self, uri: &str, pos: lsp_types::Position) -> Result<Vec<lsp_types::Location>> {
        self.wait_ready()?;
        let mut params = position_params(uri, pos);
        params["context"] = json!({ "includeDeclaration": false });
        let value = self.raw_request("textDocument/references", params)?;
        Ok(serde_json::from_value::<Option<Vec<lsp_types::Location>>>(value)?.unwrap_or_default())
    }

    fn raw_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        write_message(&self.stdin, &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow!("{method}: no response"));
        self.pending.lock().unwrap().remove(&id);
        result?
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_message(&self.stdin, &json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }
}

impl LspClient {
    /// Ends the server now, even while files still hold the client.
    pub fn shut_down(&self) {
        let Some(mut child) = self.child.lock().unwrap().take() else { return };
        let _ = child.kill();
        // Reaping one that held gigabytes takes a while: not on this thread,
        // which is the UI's more often than not.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn position_params(uri: &str, pos: lsp_types::Position) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": pos.line, "character": pos.character } })
}

fn write_message(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut stdin = stdin.lock().unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

fn read_loop(stdout: impl Read, stdin: Arc<Mutex<ChildStdin>>, pending: Pending, progress: Progress, import: Import) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                length = v.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&body) else { continue };

        match (message.get("id"), message.get("method")) {
            // A request from the server. Answer so it never waits on us.
            (Some(id), Some(method)) => {
                let result = if method == "workspace/configuration" {
                    let n = message["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; n])
                } else {
                    Value::Null
                };
                let _ = write_message(&stdin, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            (Some(id), None) => {
                let Some(id) = id.as_i64() else { continue };
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let result = match message.get("error") {
                        Some(error) => Err(anyhow!("{}", error["message"].as_str().unwrap_or("lsp error"))),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            (None, Some(method)) if method == "$/progress" => {
                let params = &message["params"];
                let token = params["token"].to_string();
                match params["value"]["kind"].as_str() {
                    Some("begin") => {
                        let title = params["value"]["title"].as_str().unwrap_or("working").to_string();
                        progress.lock().unwrap().insert(token, title);
                    }
                    Some("end") => {
                        progress.lock().unwrap().remove(&token);
                    }
                    _ => {}
                }
            }
            // kotlin-lsp tells how its import of the Gradle or Maven build went.
            (None, Some(method)) if method == "intellij/importLog" => {
                let params = &message["params"];
                let mut import = import.lock().unwrap();
                if params["started"] == true {
                    import.log.clear();
                }
                let text = params["message"].as_str().unwrap_or_default();
                if params["type"].as_i64().is_some_and(|t| t <= 2) && !text.starts_with('\t') && import.log.len() < 200 {
                    import.log.push(text.trim_end().to_string());
                }
            }
            (None, Some(method)) if method == "intellij/workspaceImportState" => {
                let params = &message["params"];
                if params["phase"] != "FINISHED" {
                    continue;
                }
                let failed = params["folders"].as_array().and_then(|f| f.iter().find(|f| f["status"] == "FAILED"));
                let mut import = import.lock().unwrap();
                import.failed = failed.map(|folder| {
                    let what = folder["message"].as_str().unwrap_or("import failed");
                    match build_failure(&import.log) {
                        Some(reason) => format!("{what}: {reason}"),
                        None => what.to_string(),
                    }
                });
            }
            _ => {}
        }
    }
}

/// What Gradle says went wrong, from its output: the lines between
/// "* What went wrong:" and the next section.
fn build_failure(log: &[String]) -> Option<String> {
    let start = log.iter().position(|l| l.starts_with("* What went wrong"))? + 1;
    let reason: Vec<&str> = log[start..]
        .iter()
        .take_while(|l| !l.starts_with("* "))
        .map(|l| l.trim().trim_start_matches("> "))
        .filter(|l| !l.is_empty())
        .collect();
    (!reason.is_empty()).then(|| reason.join(" "))
}

/// Variables a server needs that the user's environment does not give.
/// Android's Gradle plugin fails to configure a project without the SDK,
/// which Android Studio names in an untracked local.properties: a new
/// worktree has none.
fn server_env(program: &str) -> Vec<(&'static str, PathBuf)> {
    if program != "kotlin-lsp" || ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().any(|v| std::env::var_os(v).is_some()) {
        return vec![];
    }
    let sdk = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Android/sdk"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Android").join("Sdk"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Android/Sdk"))
    };
    sdk.filter(|dir| dir.is_dir()).map(|dir| ("ANDROID_HOME", dir)).into_iter().collect()
}

#[cfg(test)]
mod import_tests {
    #[test]
    fn gradle_failure_reason() {
        let log: Vec<String> = [
            "FAILURE: Build failed with an exception.",
            "",
            "* What went wrong:",
            "A problem occurred configuring project ':app'.",
            "> SDK location not found.",
            "",
            "* Try:",
            "> Run with --info",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(super::build_failure(&log).as_deref(), Some("A problem occurred configuring project ':app'. SDK location not found."));
        assert_eq!(super::build_failure(&log[..2]), None);
    }
}

#[cfg(test)]
mod root_tests {
    use super::project_root;
    use std::path::Path;

    #[test]
    fn dependencies_share_the_project_server() {
        let base = std::env::temp_dir().join(format!("ideae-root-{}", std::process::id()));
        let vendored = base.join("vendor/dep/src");
        std::fs::create_dir_all(&vendored).unwrap();
        std::fs::write(base.join("Cargo.toml"), "").unwrap();
        std::fs::write(base.join("vendor/dep/Cargo.toml"), "").unwrap();
        // Two web apps side by side, no package.json above them.
        for app in ["apps/a", "apps/b"] {
            std::fs::create_dir_all(base.join(app).join("node_modules/lib")).unwrap();
            std::fs::write(base.join(app).join("package.json"), "").unwrap();
            std::fs::write(base.join(app).join("node_modules/lib/package.json"), "").unwrap();
        }
        let file = vendored.join("lib.rs");
        assert_eq!(project_root("rust-analyzer", &file, &base), base);
        let ts = "typescript-language-server";
        assert_eq!(project_root(ts, &base.join("apps/a/node_modules/lib/x.d.ts"), &base), base.join("apps/a"));
        assert_eq!(project_root(ts, &base.join("apps/b/src.ts"), &base), base.join("apps/b"));
        assert_eq!(project_root("rust-analyzer", &base.join("_"), Path::new(&base)), base);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let path = if cfg!(windows) { PathBuf::from(r"C:\Users\A B\src\main.rs") } else { PathBuf::from("/home/a b/src/main.rs") };
        let uri = path_to_uri(&path);
        assert!(!uri.contains(' ') && !uri.contains('\\'), "{uri}");
        assert_eq!(uri_to_path(&uri), Some(path));
        if cfg!(windows) {
            assert_eq!(uri, "file:///C:/Users/A%20B/src/main.rs");
            assert_eq!(uri_to_path("file:///c%3A/Users/x.rs"), Some(PathBuf::from(r"C:\Users\x.rs")));
        }
    }

    /// Needs gopls: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn gopls_definition_across_packages() {
        let root = std::env::temp_dir().join(format!("ideae-gopls-{}", std::process::id()));
        std::fs::create_dir_all(root.join("greet")).unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/m\n\ngo 1.21\n").unwrap();
        std::fs::write(root.join("greet/greet.go"), "package greet\n\n// Hello says hi.\nfunc Hello() string { return \"hi\" }\n").unwrap();
        let file = root.join("main.go");
        let text = "package main\n\nimport \"example.com/m/greet\"\n\nfunc main() { _ = greet.Hello() }\n";
        std::fs::write(&file, text).unwrap();
        let (line, row) = text.lines().enumerate().find(|(_, l)| l.contains("greet.Hello(")).unwrap();
        let col = row.find("Hello").unwrap() as u32;

        let client = LspClient::spawn("gopls", Path::new("gopls"), &[], &root, Value::Null).unwrap();
        client.did_open(&file, "go", text).unwrap();
        let links = client
            .definition(&path_to_uri(&file), lsp_types::Position::new(line as u32, col + 2))
            .unwrap();
        let target = uri_to_path(links[0].target_uri.as_str()).unwrap();
        assert_eq!(target, root.join("greet/greet.go"));
        assert_eq!(links[0].target_selection_range.start.line, 3);

        let hover = client.hover(&path_to_uri(&file), lsp_types::Position::new(line as u32, col + 2)).unwrap();
        assert!(format!("{hover:?}").contains("Hello"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Needs rust-analyzer: `cargo test -- --ignored`. The root comes from
    /// project_root, as in the app: the directory above is no cargo project.
    #[test]
    #[ignore]
    fn rust_analyzer_local_definition() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let file = crate_dir.join("src/git.rs");
        let root = project_root("rust-analyzer", &file, crate_dir.parent().unwrap());
        assert_eq!(root, crate_dir);

        let text = std::fs::read_to_string(&file).unwrap();
        let (use_line, row) = text.lines().enumerate().find(|(_, l)| l.contains("} else if line.starts_with")).unwrap();
        let col = row.find("line.").unwrap() as u32 + 1;
        let (decl_line, _) = text.lines().enumerate().find(|(_, l)| l.trim_start().starts_with("for line in patch.lines()")).unwrap();

        let client = LspClient::spawn("rust-analyzer", Path::new("rust-analyzer"), &[], &root, Value::Null).unwrap();
        client.did_open(&file, "rust", &text).unwrap();
        let uri = path_to_uri(&file);
        let started = std::time::Instant::now();
        let links = loop {
            let links = client.definition(&uri, lsp_types::Position::new(use_line as u32, col)).unwrap_or_default();
            if !links.is_empty() || started.elapsed() > Duration::from_secs(180) {
                break links;
            }
            thread::sleep(Duration::from_secs(2));
        };
        eprintln!("answered after {:?}", started.elapsed());
        assert!(!links.is_empty(), "no definition");
        assert_eq!(uri_to_path(links[0].target_uri.as_str()).unwrap(), file);
        assert_eq!(links[0].target_selection_range.start.line as usize, decl_line);
    }
}
