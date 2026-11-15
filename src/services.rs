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

pub const TEMPLATE: &str = r#"# Services ideae can start for each task (Run ▾ in the branch bar).
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
    use super::{Config, task_env};

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
