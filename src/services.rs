//! Services a project runs while you work on it (dev servers, workers),
//! described in `.ideae.toml` at the repository root:
//!
//! ```toml
//! [[service]]
//! name = "web"
//! command = "make dev"
//! port = 8092
//! url = "http://localhost:{port}"
//! env = { PORT = "{port}" }
//! ```
//!
//! Every task has a port slot (0 for the main checkout, 1, 2… for
//! worktrees), so the same service runs in several tasks at once:
//! `{port}` is `port + 100 × slot`. That only helps when the program takes
//! its port from the environment or its arguments.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

pub const FILE: &str = ".ideae.toml";
const PORT_STEP: u32 = 100;

pub const TEMPLATE: &str = r#"# Services ideae can start for each task (the Run panel, Alt+4).
# Every task gets its own ports: {port} is `port` + 100 × the task's slot
# (0 for the main checkout, 1, 2… for worktrees). Programs must take the
# port from the environment or their arguments for that to help.
# Terminals of a task, the agent's included, get IDEAE_PORT_<NAME>.

[[service]]
name = "web"
command = "npm run dev -- --port {port}"
port = 3000
url = "http://localhost:{port}"
# env = { PORT = "{port}" }
"#;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Service {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Relative to the task's root.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    service: Vec<Service>,
}

/// What the top of a new `.ideae.toml` says.
const HEADER: &str = "# Services ideae can start for each task (the Run panel, Alt+4).\n\
# Every task gets its own ports: {port} is `port` + 100 × the task's slot\n\
# (0 for the main checkout, 1, 2… for worktrees). Programs must take the\n\
# port from the environment or their arguments for that to help.\n\
# Terminals of a task, the agent's included, get IDEAE_PORT_<NAME>.\n";

/// A service the project seems to have, from the files every such project
/// has: package.json scripts, Makefile targets, a Procfile, a compose file.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub service: Service,
    /// Where it was found, for the list: "package.json", "Makefile"…
    pub source: &'static str,
}

fn found(name: &str, command: String, port: Option<u16>, source: &'static str) -> Found {
    Found {
        service: Service {
            name: name.to_string(),
            command,
            port,
            url: port.map(|_| "http://localhost:{port}".to_string()),
            env: BTreeMap::new(),
            cwd: None,
        },
        source,
    }
}

/// `--port 8080`, `--port=8080`, `PORT=8080`, `-p 8080` in a command line.
fn port_in(command: &str) -> Option<u16> {
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        for prefix in ["--port=", "PORT="] {
            if let Some(n) = w.strip_prefix(prefix) {
                return n.parse().ok();
            }
        }
        if matches!(*w, "--port" | "-p") {
            return words.get(i + 1)?.parse().ok();
        }
    }
    None
}

pub fn detect(root: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            let run = if root.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if root.join("yarn.lock").exists() {
                "yarn"
            } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
                "bun run"
            } else {
                "npm run"
            };
            for key in ["dev", "start", "serve", "preview", "storybook"] {
                let Some(script) = json["scripts"][key].as_str() else { continue };
                // Dev servers that take --port: each task on its own.
                let default = [("vite", 5173), ("next", 3000), ("astro", 4321), ("react-scripts", 3000), ("storybook", 6006)]
                    .into_iter()
                    .find(|(tool, _)| script.split_whitespace().next() == Some(*tool) || script.contains(&format!("{tool} ")))
                    .map(|(_, port)| port);
                let (command, port) = match (port_in(script), default) {
                    (Some(port), _) => (format!("{run} {key}"), Some(port)),
                    (None, Some(port)) => {
                        let sep = if run == "npm run" { " --" } else { "" };
                        (format!("{run} {key}{sep} --port {{port}}"), Some(port))
                    }
                    (None, None) => (format!("{run} {key}"), None),
                };
                out.push(found(key, command, port, "package.json"));
            }
        }
    }
    for makefile in ["Makefile", "makefile", "GNUmakefile"] {
        let Ok(text) = std::fs::read_to_string(root.join(makefile)) else { continue };
        for line in text.lines() {
            let Some((target, _)) = line.split_once(':') else { continue };
            if matches!(target.trim(), "run" | "dev" | "serve" | "start" | "up" | "watch") && !line.starts_with(['\t', ' ']) {
                out.push(found(target.trim(), format!("make {}", target.trim()), None, "Makefile"));
            }
        }
        break;
    }
    if let Ok(text) = std::fs::read_to_string(root.join("Procfile")) {
        for line in text.lines() {
            let Some((name, command)) = line.split_once(':') else { continue };
            if !name.trim().is_empty() && !name.starts_with('#') {
                out.push(found(name.trim(), command.trim().to_string(), port_in(command), "Procfile"));
            }
        }
    }
    if ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"].iter().any(|f| root.join(f).exists()) {
        out.push(found("compose", "docker compose up".into(), None, "compose file"));
    }
    // A name once: the first source wins.
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert(f.service.name.clone()));
    out
}

/// Adds `service` to the task's `.ideae.toml`, making the file when missing.
pub fn add(root: &Path, service: &Service) -> Result<(), String> {
    let path = root.join(FILE);
    let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| HEADER.to_string());
    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("\n[[service]]\nname = {}\ncommand = {}\n", quote(&service.name), quote(&service.command)));
    if let Some(port) = service.port {
        text.push_str(&format!("port = {port}\n"));
    }
    if let Some(url) = &service.url {
        text.push_str(&format!("url = {}\n", quote(url)));
    }
    std::fs::write(&path, text).map_err(|e| format!("{FILE}: {e}"))
}

/// The services of the task at `root`, or why the file could not be read.
pub fn load(root: &Path) -> Result<Vec<Service>, String> {
    let Ok(text) = std::fs::read_to_string(root.join(FILE)) else { return Ok(Vec::new()) };
    toml::from_str::<Config>(&text).map(|c| c.service).map_err(|e| format!("{FILE}: {e}"))
}

impl Service {
    pub fn port(&self, slot: u32) -> Option<u16> {
        let port = u32::from(self.port?) + PORT_STEP * slot;
        u16::try_from(port).ok()
    }

    /// `{port}` is this service's port, `{port.NAME}` another's.
    pub fn expand(&self, text: &str, all: &[Service], slot: u32) -> String {
        let mut out = text.to_string();
        for other in all {
            if let Some(port) = other.port(slot) {
                out = out.replace(&format!("{{port.{}}}", other.name), &port.to_string());
            }
        }
        match self.port(slot) {
            Some(port) => out.replace("{port}", &port.to_string()),
            None => out,
        }
    }

    pub fn url(&self, all: &[Service], slot: u32) -> Option<String> {
        self.url.as_deref().map(|u| self.expand(u, all, slot))
    }

    /// Its own variables, with the ports filled in.
    pub fn env(&self, all: &[Service], slot: u32) -> Vec<(String, String)> {
        self.env.iter().map(|(k, v)| (k.clone(), self.expand(v, all, slot))).collect()
    }
}

/// IDEAE_PORT_<NAME> for every service with a port, for a task's terminals.
pub fn task_env(all: &[Service], slot: u32) -> Vec<(String, String)> {
    let mut env = vec![("IDEAE_PORT_SLOT".to_string(), slot.to_string())];
    for service in all {
        if let Some(port) = service.port(slot) {
            let name: String =
                service.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
            env.push((format!("IDEAE_PORT_{name}"), port.to_string()));
        }
    }
    env
}

/// A TCP port one of `pids` listens on, with that process.
#[derive(Clone, Debug, PartialEq)]
pub struct Listening {
    pub port: u16,
    pub pid: i32,
    pub name: String,
}

/// The ports `pids` listen on, lowest first.
#[cfg(target_os = "linux")]
pub fn listening(pids: &[i32]) -> Vec<Listening> {
    // inode → port, from the listening sockets of the whole machine.
    let mut ports: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() > 9 && f[3] == "0A" {
                if let Some(port) = f[1].rsplit(':').next().and_then(|h| u16::from_str_radix(h, 16).ok()) {
                    ports.insert(format!("socket:[{}]", f[9]), port);
                }
            }
        }
    }
    let mut out: Vec<Listening> = Vec::new();
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            if let Some(&port) = ports.get(link.to_string_lossy().as_ref()) {
                if !out.iter().any(|l| l.port == port) {
                    let name = crate::procs::name(pid as u32).unwrap_or_default();
                    out.push(Listening { port, pid, name });
                }
            }
        }
    }
    out.sort_by_key(|l| l.port);
    out
}

/// `lsof` for the ports of `pids`.
#[cfg(target_os = "macos")]
pub fn listening(pids: &[i32]) -> Vec<Listening> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    let Ok(out) = crate::procs::command("lsof")
        .args(["-a", "-p", &list.join(","), "-iTCP", "-sTCP:LISTEN", "-P", "-n", "-Fpcn"])
        .output()
    else {
        return Vec::new();
    };
    let mut found: Vec<Listening> = Vec::new();
    let (mut pid, mut name) = (0, String::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.split_at_checked(1) {
            Some(("p", v)) => pid = v.parse().unwrap_or(0),
            Some(("c", v)) => name = v.to_string(),
            // "n*:3000" or "n127.0.0.1:3000".
            Some(("n", v)) => {
                if let Some(port) = v.rsplit(':').next().and_then(|p| p.parse().ok()) {
                    if !found.iter().any(|l| l.port == port) {
                        found.push(Listening { port, pid, name: name.clone() });
                    }
                }
            }
            _ => {}
        }
    }
    found.sort_by_key(|l| l.port);
    found
}

#[cfg(not(unix))]
pub fn listening(_: &[i32]) -> Vec<Listening> {
    Vec::new()
}

/// Something listens on `port` already: its process name when it can be
/// told, else just that it is taken.
pub fn port_taken(port: u16) -> Option<String> {
    let free = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() && std::net::TcpListener::bind(("0.0.0.0", port)).is_ok();
    if free {
        return None;
    }
    Some(owner(port).unwrap_or_else(|| "another program".into()))
}

#[cfg(target_os = "linux")]
fn owner(port: u16) -> Option<String> {
    let hex = format!(":{port:04X}");
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Local address ends in the port; state 0A is LISTEN.
            if f.len() > 9 && f[1].ends_with(&hex) && f[3] == "0A" {
                inodes.push(format!("socket:[{}]", f[9]));
            }
        }
    }
    if inodes.is_empty() {
        return None;
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            if inodes.iter().any(|i| link.as_os_str() == i.as_str()) {
                let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                return Some(format!("{} (pid {})", name.trim(), entry.file_name().to_string_lossy()));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn owner(_: u16) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Config, detect, load, task_env};

    #[test]
    fn finds_scripts_and_adds_them() {
        let root = std::env::temp_dir().join(format!("ideae-services-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), r#"{"scripts":{"dev":"vite","start":"node server.js --port 8080","test":"vitest"}}"#).unwrap();
        std::fs::write(root.join("Makefile"), "run:\n\tgo run .\nbuild:\n\tgo build\n").unwrap();
        let found = detect(&root);
        let names: Vec<_> = found.iter().map(|f| f.service.name.as_str()).collect();
        assert_eq!(names, ["dev", "start", "run"]);
        assert_eq!(found[0].service.command, "npm run dev -- --port {port}");
        assert_eq!(found[0].service.port, Some(5173));
        assert_eq!(found[1].service.port, Some(8080));
        super::add(&root, &found[0].service).unwrap();
        super::add(&root, &found[2].service).unwrap();
        let loaded = load(&root).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].url.as_deref(), Some("http://localhost:{port}"));
        assert_eq!(loaded[1].command, "make run");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ports_follow_the_slot() {
        let config: Config = toml::from_str(
            r#"
            [[service]]
            name = "web"
            command = "serve --port {port} --api {port.api}"
            port = 3000
            url = "http://localhost:{port}"
            env = { PORT = "{port}" }

            [[service]]
            name = "api-v2"
            command = "api"
            port = 8090
            "#,
        )
        .unwrap();
        let all = &config.service;
        let web = &all[0];
        assert_eq!(web.port(2), Some(3200));
        assert_eq!(web.expand(&web.command, all, 1), "serve --port 3100 --api {port.api}");
        assert_eq!(web.url(all, 0).as_deref(), Some("http://localhost:3000"));
        assert_eq!(web.env(all, 1), vec![("PORT".to_string(), "3100".to_string())]);
        let env = task_env(all, 1);
        assert!(env.contains(&("IDEAE_PORT_API_V2".to_string(), "8190".to_string())));
    }
}
