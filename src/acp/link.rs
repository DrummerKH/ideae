//! The window's end of an agent's host: in the daemon, where the agent
//! outlives the window, or on a thread of the window when there is none.
//! Either way the window sends [`ToHost`] and hears [`ToWindow`].

use super::host;
use super::wire::{self, HostSpec, ToHost, ToWindow};

#[cfg(unix)]
/// The program that hosts an agent: this app, unless `IDEAE_ACP_HOST`
/// names another (a JSON list: the test binary, in tests).
pub fn host_program() -> Vec<String> {
    if let Some(program) = std::env::var("IDEAE_ACP_HOST").ok().and_then(|p| serde_json::from_str::<Vec<String>>(&p).ok()) {
        return program;
    }
    let exe = crate::desktop::exe().unwrap_or_default();
    vec![exe.display().to_string(), "--acp-host".into()]
}

enum To {
    #[cfg(unix)]
    Daemon { daemon: std::sync::Arc<crate::daemon::client::Daemon>, id: String, remap: Option<Remap> },
    Local(std::sync::mpsc::Sender<host::Event>),
}

pub struct Link {
    to: To,
    pub events: async_channel::Receiver<ToWindow>,
}

impl Link {
    /// The host of session `id` (the task's): attached when the daemon runs
    /// it already, else started.
    pub fn open(id: String, spec: HostSpec) -> Link {
        // A remote task's on its host, in its daemon there.
        #[cfg(unix)]
        if let Some(daemon) = crate::daemon::client::for_path(&spec.cwd) {
            return Self::in_daemon(daemon, id, spec);
        }
        let _ = id;
        // A remote task's agent runs on its host only: none here, in a folder that is no path here.
        if crate::hosts::is_remote(&spec.cwd) {
            return Self::not_there("Its host is not connected yet: Restart once it is");
        }
        let (tx, events) = host::spawn_in_process(spec);
        let link = Link { to: To::Local(tx), events };
        link.send(ToHost::Attach { version: wire::VERSION });
        link
    }

    /// A link to no host, which says why.
    fn not_there(why: &str) -> Link {
        let (tx, events) = async_channel::unbounded();
        let status = super::thread::Status::Exited(Some(why.to_string()));
        let _ = tx.try_send(ToWindow::Op { op: super::thread::Op::Status { status } });
        Link { to: To::Local(std::sync::mpsc::channel().0), events }
    }

    #[cfg(unix)]
    fn in_daemon(daemon: std::sync::Arc<crate::daemon::client::Daemon>, id: String, spec: HostSpec) -> Link {
        use crate::daemon::client::PipeEvent;
        // The spec, keys and all, as its first line: not in its environment,
        // which other programs of the user can read for as long as it runs.
        let (spec, program, remap) = match (crate::hosts::split(&spec.cwd), daemon.there()) {
            (Some((host, there)), Some((exe, _))) => (there_spec(spec, &there), vec![exe.display().to_string(), "--acp-host".into()], Some(Remap::new(&host, &there))),
            // Its ideae-server there not known (not put there by this window): nothing of here runs there.
            (Some(_), None) => return Self::not_there("Its host is not connected yet: Restart once it is"),
            _ => (spec, host_program(), None),
        };
        let first = String::from_utf8(wire::line(&spec)).unwrap_or_default();
        let pipe_spec = crate::daemon::proto::Spec { cwd: spec.cwd.clone(), env: Vec::new(), input: Some(first), program };
        // Its thread is in its journal: a host its daemon lost is started again, not ended.
        let pipe = daemon.pipe_again(id.clone(), pipe_spec);
        let remap_out = remap.clone();
        let (tx, events) = async_channel::unbounded();
        let (attach_daemon, attach_id) = (daemon.clone(), id.clone());
        std::thread::Builder::new()
            .name("acp-link".into())
            .spawn(move || {
                let mut lines = wire::Lines::default();
                let attach = || attach_daemon.write(&attach_id, &wire::line(&ToHost::Attach { version: wire::VERSION }));
                attach();
                while let Ok(event) = pipe.recv_blocking() {
                    match event {
                        PipeEvent::Bytes(bytes) => {
                            for line in lines.feed(&bytes) {
                                let line = remap.as_ref().map_or(line.clone(), |r| r.here(&line));
                                if let Some(msg) = wire::parse::<ToWindow>(&line) {
                                    if tx.send_blocking(msg).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        PipeEvent::Reattached => {
                            lines = wire::Lines::default();
                            attach();
                        }
                        PipeEvent::Exited => {
                            let status = super::thread::Status::Exited(Some("The agent's host ended".into()));
                            let _ = tx.send_blocking(ToWindow::Op { op: super::thread::Op::Status { status } });
                            return;
                        }
                    }
                }
            })
            .expect("a thread for the agent's link");
        Link { to: To::Daemon { daemon, id, remap: remap_out }, events }
    }

    pub fn send(&self, msg: ToHost) {
        match &self.to {
            #[cfg(unix)]
            To::Daemon { daemon, id, remap } => {
                let line = wire::line(&msg);
                match remap {
                    Some(remap) => daemon.write(id, &remap.there(&line)),
                    None => daemon.write(id, &line),
                }
            }
            To::Local(tx) => {
                let _ = tx.send(host::Event::Window(msg));
            }
        }
    }

    /// Ends the agent and its host.
    pub fn stop(&self) {
        self.send(ToHost::Stop);
        #[cfg(unix)]
        if let To::Daemon { daemon, id, .. } = &self.to {
            daemon.kill(id);
        }
    }

    /// The daemon's name for it, kept when the daemon prunes.
    pub fn session(&self) -> Option<&str> {
        match &self.to {
            #[cfg(unix)]
            To::Daemon { id, .. } => Some(id),
            To::Local(_) => None,
        }
    }
}

/// A host on a thread of the window ends with its last link: it holds its
/// own sender, so nothing else would tell it. One in the daemon goes on,
/// as it is meant to.
impl Drop for Link {
    #[cfg_attr(not(unix), allow(irrefutable_let_patterns))]
    fn drop(&mut self) {
        if let To::Local(tx) = &self.to {
            let _ = tx.send(host::Event::Window(ToHost::Stop));
        }
    }
}

/// A remote task's host spec, as its host there runs it: in its folder
/// there, its journal there ([`super::journal_path`]), its agent started
/// by the login shell there (its PATH: npx, ~/.local/bin). Its MCP server
/// is the ideae-server there already ([`crate::mcp::server_for`]).
#[cfg(unix)]
fn there_spec(mut spec: HostSpec, there: &std::path::Path) -> HostSpec {
    spec.cwd = there.to_path_buf();
    spec.journal = spec.journal.map(|j| crate::hosts::there(&j));
    spec.journal_before = spec.journal_before.map(|j| crate::hosts::there(&j));
    spec.login_shell = true;
    spec
}

/// The paths of a remote task in the host's messages: as they are there
/// (under its folder there) and as the window keys them, each way.
#[cfg(unix)]
#[derive(Clone)]
struct Remap {
    there: String,
    key: String,
}

#[cfg(unix)]
impl Remap {
    fn new(host: &str, there: &std::path::Path) -> Self {
        let there = there.display().to_string();
        Remap { key: crate::hosts::join(host, std::path::Path::new(&there)).display().to_string(), there }
    }

    /// Only what is a path (a location's, a diff's, a file read, written
    /// or mentioned): what the agent says of one stays as it said it.
    fn rewrite(value: &mut serde_json::Value, from: &str, to: &str, path: bool) {
        match value {
            serde_json::Value::String(s) if path => {
                if let Some(rest) = s.strip_prefix(from).filter(|rest| rest.is_empty() || rest.starts_with('/')) {
                    *s = format!("{to}{rest}");
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| Self::rewrite(v, from, to, path)),
            serde_json::Value::Object(map) => map.iter_mut().for_each(|(k, v)| Self::rewrite(v, from, to, matches!(k.as_str(), "path" | "files" | "cwd"))),
            _ => {}
        }
    }

    fn line(line: &[u8], from: &str, to: &str) -> Vec<u8> {
        match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(mut value) => {
                Self::rewrite(&mut value, from, to, false);
                let mut out = serde_json::to_vec(&value).unwrap_or_default();
                out.push(b'\n');
                out
            }
            Err(_) => line.to_vec(),
        }
    }

    /// A line of the host's, its paths there keyed as here.
    fn here(&self, line: &[u8]) -> Vec<u8> {
        Self::line(line, &self.there, &self.key)
    }

    /// A line to the host, its paths keyed here as they are there.
    fn there(&self, line: &[u8]) -> Vec<u8> {
        Self::line(line, &self.key, &self.there)
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn a_remote_hosts_paths_are_keyed_each_way() {
        let remap = super::Remap::new("box", std::path::Path::new("/home/me/wt"));
        let told = br#"{"op":{"Location":{"path":"/home/me/wt/src/a.rs","other":"/home/me/wt2/x","cmd":"/model","text":"/home/me/wt/src/a.rs:12 has it"}}}"#;
        let here: serde_json::Value = serde_json::from_slice(&remap.here(told)).unwrap();
        assert_eq!(here["op"]["Location"]["path"], "ssh:/box/home/me/wt/src/a.rs");
        assert_eq!(here["op"]["Location"]["other"], "/home/me/wt2/x", "another folder's is left");
        assert_eq!(here["op"]["Location"]["cmd"], "/model");
        assert_eq!(here["op"]["Location"]["text"], "/home/me/wt/src/a.rs:12 has it", "what is said of a path is no path");
        let prompt: serde_json::Value = serde_json::from_slice(&remap.there(br#"{"m":"Prompt","text":"ssh:/box/home/me/wt is it","files":["ssh:/box/home/me/wt/a.rs"]}"#)).unwrap();
        assert_eq!(prompt["files"][0], "/home/me/wt/a.rs");
        assert_eq!(prompt["text"], "ssh:/box/home/me/wt is it");
        let there: serde_json::Value = serde_json::from_slice(&remap.there(&serde_json::to_vec(&here).unwrap())).unwrap();
        assert_eq!(there["op"]["Location"]["path"], "/home/me/wt/src/a.rs");
    }
}
