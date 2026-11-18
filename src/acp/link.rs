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
    Daemon { daemon: std::sync::Arc<crate::daemon::client::Daemon>, id: String },
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
        #[cfg(unix)]
        if let Some(daemon) = crate::daemon::client::get() {
            return Self::in_daemon(daemon, id, spec);
        }
        let _ = id;
        let (tx, events) = host::spawn_in_process(spec);
        let link = Link { to: To::Local(tx), events };
        link.send(ToHost::Attach { version: wire::VERSION });
        link
    }

    #[cfg(unix)]
    fn in_daemon(daemon: std::sync::Arc<crate::daemon::client::Daemon>, id: String, spec: HostSpec) -> Link {
        use crate::daemon::client::PipeEvent;
        // The spec, keys and all, as its first line: not in its environment,
        // which other programs of the user can read for as long as it runs.
        let first = String::from_utf8(wire::line(&spec)).unwrap_or_default();
        let pipe_spec = crate::daemon::proto::Spec { cwd: spec.cwd.clone(), env: Vec::new(), input: Some(first), program: host_program() };
        let pipe = daemon.pipe(id.clone(), pipe_spec);
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
        Link { to: To::Daemon { daemon, id }, events }
    }

    pub fn send(&self, msg: ToHost) {
        match &self.to {
            #[cfg(unix)]
            To::Daemon { daemon, id } => daemon.write(id, &wire::line(&msg)),
            To::Local(tx) => {
                let _ = tx.send(host::Event::Window(msg));
            }
        }
    }

    /// Ends the agent and its host.
    pub fn stop(&self) {
        self.send(ToHost::Stop);
        #[cfg(unix)]
        if let To::Daemon { daemon, id } = &self.to {
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
