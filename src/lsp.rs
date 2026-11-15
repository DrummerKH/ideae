//! Minimal stdio LSP client: just enough for hover and go-to-definition on
//! read-only files. Calls block, so run them on the background executor.

use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
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

pub struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    ready: (Mutex<Option<Result<(), String>>>, Condvar),
    opened: Mutex<HashSet<String>>,
    progress: Progress,
    child: Mutex<Child>,
}

/// Which server handles a file, by extension: (language id, program, args),
/// or why there is none, for the editor's status line.
pub fn server_for(
    path: &Path,
) -> Result<(&'static str, &'static str, &'static [&'static str]), String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let entry: (&str, &str, &[&str]) = match ext {
        "go" => ("go", "gopls", &[]),
        "rs" => ("rust", "rust-analyzer", &[]),
        "ts" | "tsx" => ("typescript", "typescript-language-server", &["--stdio"]),
        "js" | "jsx" => ("javascript", "typescript-language-server", &["--stdio"]),
        "php" => ("php", "intelephense", &["--stdio"]),
        "kt" | "kts" => ("kotlin", "kotlin-lsp", &["--stdio"]),
        _ => return Err(format!("no language server for .{ext}")),
    };
    if !runs(entry.1) {
        return Err(format!("{} not installed", entry.1));
    }
    Ok(entry)
}

/// Whether the server is there. Found on PATH is enough, except for
/// rust-analyzer: rustup puts a proxy on PATH that fails when the component
/// is missing, so that one has to answer `--version`. Others cannot be asked
/// (intelephense crashes without a transport flag).
fn runs(program: &str) -> bool {
    static CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    *cache.get_or_insert_with(HashMap::new).entry(program.to_string()).or_insert_with(|| {
        if program != "rust-analyzer" {
            return on_path(program);
        }
        Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(program)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// The directory a server should treat as its workspace: the nearest ancestor
/// of `file` holding one of the server's project markers, no higher than
/// `limit`. A monorepo root is rarely what rust-analyzer or tsserver want.
pub fn project_root(program: &str, file: &Path, limit: &Path) -> PathBuf {
    let markers: &[&str] = match program {
        "gopls" => &["go.work", "go.mod"],
        "rust-analyzer" => &["Cargo.toml"],
        "typescript-language-server" => &["tsconfig.json", "package.json"],
        "intelephense" => &["composer.json"],
        "kotlin-lsp" => &["settings.gradle.kts", "settings.gradle"],
        _ => &[],
    };
    let mut dir = file.parent();
    while let Some(d) = dir {
        if markers.iter().any(|m| d.join(m).is_file()) {
            return d.to_path_buf();
        }
        if d == limit {
            break;
        }
        dir = d.parent();
    }
    limit.to_path_buf()
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
        "css" | "scss" => "css",
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

pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
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
    String::from_utf8(out).ok().map(PathBuf::from)
}

impl LspClient {
    /// Starts the server; initialization runs on its own thread, and every
    /// call waits for it.
    pub fn spawn(program: &str, args: &[&str], root: &Path) -> Result<Arc<Self>> {
        let mut command = Command::new(program);
        command.args(args).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Dies with the app even when it exits without running destructors.
        // SAFETY: prctl only sets a flag of the child.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {program}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();
        let progress: Progress = Arc::default();

        let reader_stdin = stdin.clone();
        let reader_pending = pending.clone();
        let reader_progress = progress.clone();
        thread::Builder::new()
            .name(format!("lsp-read-{program}"))
            .spawn(move || read_loop(stdout, reader_stdin, reader_pending, reader_progress))?;

        let client = Arc::new(Self {
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            ready: (Mutex::new(None), Condvar::new()),
            opened: Mutex::default(),
            progress,
            child: Mutex::new(child),
        });

        let init = client.clone();
        let root = root.to_path_buf();
        thread::Builder::new()
            .name(format!("lsp-init-{program}"))
            .spawn(move || {
                let result = init.initialize(&root).map_err(|e| e.to_string());
                let (lock, cvar) = &init.ready;
                *lock.lock().unwrap() = Some(result);
                cvar.notify_all();
            })?;
        Ok(client)
    }

    fn initialize(&self, root: &Path) -> Result<()> {
        let uri = path_to_uri(root);
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.raw_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": name }],
                "capabilities": {
                    // The editor counts columns in characters.
                    "general": { "positionEncodings": ["utf-32", "utf-16"] },
                    "textDocument": {
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "definition": { "linkSupport": true },
                        "synchronization": { "didSave": false },
                    },
                    "workspace": { "workspaceFolders": true, "configuration": true },
                    "window": { "workDoneProgress": true },
                },
            }),
        )?;
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

impl Drop for LspClient {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
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

fn read_loop(stdout: impl Read, stdin: Arc<Mutex<ChildStdin>>, pending: Pending, progress: Progress) {
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let client = LspClient::spawn("gopls", &[], &root).unwrap();
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

        let client = LspClient::spawn("rust-analyzer", &[], &root).unwrap();
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
