//! A port on this machine that leads to a database through a bastion: the
//! system's `ssh`, so its keys, agent, `known_hosts` and config all work.
//! One per (bastion, target), held by the sessions using it; the last to let
//! go ends it.
//!
//! A TCP port and not a unix socket: over a socket the driver takes the
//! channel for a safe one and sends the password in the clear, and past the
//! bastion it is not.

use std::{
    collections::HashMap,
    io::{BufRead as _, BufReader},
    net::TcpListener,
    process::{Child, Stdio},
    sync::{Arc, Condvar, Mutex, Weak},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};

use super::config::SshHop;

/// From starting ssh to its port listening: login, a key's agent, a slow link.
const READY_WITHIN: Duration = Duration::from_secs(25);

/// Another program can take the free port before ssh does.
const TRIES: usize = 3;

/// What ssh said last, for when it fails.
const LINES_KEPT: usize = 6;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    hop: SshHop,
    host: String,
    port: u16,
}

/// One slot per key, so that a slow login holds up only those waiting for this tunnel.
type Slot = Arc<Mutex<Weak<Tunnel>>>;

static TUNNELS: Mutex<Option<HashMap<Key, Slot>>> = Mutex::new(None);

/// The tests' ssh: the environment is not theirs to change while others run.
#[cfg(test)]
pub(super) static STAND_IN: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

pub struct Tunnel {
    child: Mutex<Option<Child>>,
    pub port: u16,
}

impl Tunnel {
    fn alive(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn end(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.end();
    }
}

/// The tunnel to `host:port`: the one open, or a new one. Blocks: not for the UI thread.
pub fn open(hop: &SshHop, host: &str, port: u16) -> Result<Arc<Tunnel>> {
    let key = Key { hop: hop.clone(), host: host.to_string(), port };
    let slot = {
        let mut tunnels = TUNNELS.lock().unwrap_or_else(|e| e.into_inner());
        let tunnels = tunnels.get_or_insert_with(HashMap::new);
        tunnels.retain(|k, slot| *k == key || Arc::strong_count(slot) > 1 || slot.lock().is_ok_and(|t| t.strong_count() > 0));
        tunnels.entry(key).or_default().clone()
    };
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tunnel) = held.upgrade().filter(|t| t.alive()) {
        return Ok(tunnel);
    }
    let tunnel = Arc::new(start(hop, host, port)?);
    *held = Arc::downgrade(&tunnel);
    Ok(tunnel)
}

pub fn quit() {
    let slots: Vec<Slot> = TUNNELS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|t| t.values().cloned().collect()).unwrap_or_default();
    for slot in slots {
        // A slot held is a login under way: its ssh is tied to the app.
        if let Ok(held) = slot.try_lock()
            && let Some(tunnel) = held.upgrade()
        {
            tunnel.end();
        }
    }
}

fn start(hop: &SshHop, host: &str, port: u16) -> Result<Tunnel> {
    let mut last = anyhow!("ssh did not start");
    for _ in 0..TRIES {
        let local = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port();
        match start_on(hop, host, port, local) {
            Ok(child) => return Ok(Tunnel { child: Mutex::new(Some(child)), port: local }),
            Err(Failure::PortTaken(e)) => last = e,
            Err(Failure::Other(e)) => return Err(e),
        }
    }
    Err(last)
}

enum Failure {
    PortTaken(anyhow::Error),
    Other(anyhow::Error),
}

pub(crate) fn arguments(hop: &SshHop, host: &str, port: u16, local: u16) -> Vec<String> {
    let target = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    let mut args: Vec<String> = ["-N", "-v", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=3"]
        .map(String::from)
        .into();
    args.extend(["-p".into(), hop.port.to_string(), "-L".into(), format!("127.0.0.1:{local}:{target}:{port}")]);
    if let Some(key) = &hop.key {
        args.extend(["-i".into(), key.display().to_string()]);
    }
    if !hop.user.is_empty() {
        args.extend(["-l".into(), hop.user.clone()]);
    }
    // A host read as an option could name a command for ssh to run.
    args.extend(["--".into(), hop.host.clone()]);
    args
}

#[derive(Default)]
struct Said {
    ready: bool,
    ended: bool,
    lines: Vec<String>,
}

fn start_on(hop: &SshHop, host: &str, port: u16, local: u16) -> Result<Child, Failure> {
    #[cfg(test)]
    let stand_in = STAND_IN.lock().unwrap_or_else(|e| e.into_inner()).clone().map(std::ffi::OsString::from);
    #[cfg(not(test))]
    let stand_in: Option<std::ffi::OsString> = None;
    let program = stand_in.or_else(|| std::env::var_os("IDEAE_SSH").filter(|_| cfg!(debug_assertions))).unwrap_or_else(|| "ssh".into());
    let mut command = crate::procs::command(program);
    command.args(arguments(hop, host, port, local)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = crate::procs::spawn_tied(&mut command).map_err(|e| Failure::Other(anyhow!("could not start ssh: {e}")))?;
    let stderr = child.stderr.take().ok_or_else(|| Failure::Other(anyhow!("ssh has no stderr")))?;

    // ssh's own word that it listens: a look at the port could not tell it
    // from another program there, and that one would be sent the login.
    let said = Arc::new((Mutex::new(Said::default()), Condvar::new()));
    let reader = said.clone();
    let listening = format!("Local forwarding listening on 127.0.0.1 port {local}");
    std::thread::spawn(move || {
        let (state, changed) = &*reader;
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if line.contains(&listening) {
                state.ready = true;
                changed.notify_all();
            } else if !line.starts_with("debug") && !state.ready {
                state.lines.push(line);
                let extra = state.lines.len().saturating_sub(LINES_KEPT);
                state.lines.drain(..extra);
            }
        }
        state.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
        changed.notify_all();
    });

    let (state, changed) = &*said;
    let deadline = Instant::now() + READY_WITHIN;
    let mut seen = state.lock().unwrap_or_else(|e| e.into_inner());
    while !seen.ready && !seen.ended {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        seen = changed.wait_timeout(seen, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    if seen.ready {
        drop(seen);
        return Ok(child);
    }
    let timed_out = !seen.ended;
    let lines = seen.lines.join("\n");
    drop(seen);
    let _ = child.kill();
    let _ = child.wait();
    if lines.contains("Address already in use") {
        return Err(Failure::PortTaken(anyhow!("SSH: {lines}")));
    }
    Err(Failure::Other(explain(hop, &lines, timed_out)))
}

fn explain(hop: &SshHop, said: &str, timed_out: bool) -> anyhow::Error {
    let login = if hop.user.is_empty() { hop.host.clone() } else { format!("{}@{}", hop.user, hop.host) };
    if said.contains("Host key verification failed") || said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return anyhow!("SSH: the key of {} is not a known one. Check it by connecting once in a terminal: ssh -p {} {login}", hop.host, hop.port);
    }
    if said.contains("Permission denied") {
        return anyhow!("SSH: {login} was refused. A key with a passphrase has to be in ssh-agent (ssh-add).\n{said}");
    }
    if timed_out {
        return anyhow!("SSH: no answer from {} in {} seconds\n{said}", hop.host, READY_WITHIN.as_secs());
    }
    if said.is_empty() { anyhow!("SSH: ended without saying why") } else { anyhow!("SSH: {said}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop() -> SshHop {
        SshHop { host: "bastion.example".into(), port: 2222, user: "me".into(), key: Some("/keys/id".into()) }
    }

    #[test]
    fn the_forward_listens_on_loopback_only_and_the_host_cannot_be_an_option() {
        let args = arguments(&SshHop { host: "-oProxyCommand=x".into(), ..hop() }, "db-internal", 3306, 41000);
        assert!(args.contains(&"127.0.0.1:41000:db-internal:3306".to_string()));
        assert_eq!(&args[args.len() - 2..], ["--", "-oProxyCommand=x"]);
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.iter().any(|a| a.contains("StrictHostKeyChecking")));
    }

    #[test]
    fn an_ipv6_target_is_in_brackets() {
        assert!(arguments(&hop(), "fd00::1", 3306, 41000).contains(&"127.0.0.1:41000:[fd00::1]:3306".to_string()));
    }

    #[test]
    fn an_unknown_host_key_says_how_to_check_it() {
        let text = explain(&hop(), "Host key verification failed.", false).to_string();
        assert!(text.contains("ssh -p 2222 me@bastion.example"));
    }
}
