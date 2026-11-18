//! Commands an agent runs through the app (ACP's `terminal/*`): started by
//! the host, their output kept (its end, up to the agent's limit) for the
//! agent to read and for the chat to show live, ended with the agent.

use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::host::Event;

/// How long a command asked to end has before it is killed.
#[cfg_attr(not(unix), allow(dead_code))]
const GRACE: Duration = Duration::from_secs(2);

pub struct Term {
    pid: u32,
    output: Arc<Mutex<Output>>,
    /// Its process has ended (set before the host is told).
    #[cfg_attr(not(unix), allow(dead_code))]
    ended: Arc<AtomicBool>,
    pub exit: Option<(Option<i32>, Option<String>)>,
    /// `terminal/wait_for_exit` requests answered once it ends.
    pub waiting: Vec<serde_json::Value>,
    /// How much of its output the chat was shown.
    pub shown: usize,
}

#[derive(Default)]
struct Output {
    text: String,
    truncated: bool,
    limit: usize,
    /// Grows with every byte: tells the chat there is more to show.
    written: usize,
}

impl Term {
    pub fn start(id: String, command: &str, args: &[String], env: &[(String, String)], cwd: &Path, limit: Option<usize>, tx: mpsc::Sender<Event>) -> std::io::Result<Term> {
        // A whole line with no arguments: the shell runs it.
        let mut cmd = if args.is_empty() && command.contains(char::is_whitespace) {
            if cfg!(windows) {
                let mut c = crate::procs::command("cmd");
                c.args(["/C", command]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c", command]);
                c
            }
        } else {
            let mut c = crate::procs::command(command);
            c.args(args);
            c
        };
        cmd.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = crate::procs::spawn_tied(&mut cmd)?;
        let output = Arc::new(Mutex::new(Output { limit: limit.unwrap_or(1 << 20).max(1024), ..Default::default() }));
        let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)]
            .into_iter()
            .flatten()
            .map(|mut pipe| {
                let output = output.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    // A character cut between two reads, waiting for its end.
                    let mut cut: Vec<u8> = Vec::new();
                    while let Ok(n) = pipe.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        cut.extend_from_slice(&buf[..n]);
                        let text = take_text(&mut cut);
                        let mut out = output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        out.text.push_str(&text);
                        out.written += n;
                        // Cut when well past the limit: not a move of it all at every read.
                        if out.text.len() > out.limit + out.limit / 2 {
                            let mut from = out.text.len() - out.limit;
                            while !out.text.is_char_boundary(from) {
                                from += 1;
                            }
                            out.text.drain(..from);
                            out.truncated = true;
                        }
                    }
                    if !cut.is_empty() {
                        output.lock().unwrap_or_else(std::sync::PoisonError::into_inner).text.push_str(&String::from_utf8_lossy(&cut));
                    }
                })
            })
            .collect();
        let pid = child.id();
        let ended = Arc::new(AtomicBool::new(false));
        let ended_by = ended.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            ended_by.store(true, Ordering::Release);
            // Its last output; but what it left running in the background
            // (`npm run dev &`) keeps the pipes open: not waited for.
            let until = std::time::Instant::now() + Duration::from_millis(500);
            while readers.iter().any(|r| !r.is_finished()) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            let (code, signal) = match status {
                Ok(status) => {
                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt as _;
                        status.signal().map(|s| format!("signal {s}"))
                    };
                    #[cfg(not(unix))]
                    let signal = None;
                    (status.code(), signal)
                }
                Err(_) => (None, None),
            };
            let _ = tx.send(Event::TermExited { id, code, signal });
        });
        Ok(Term { pid, output, ended, exit: None, waiting: Vec::new(), shown: 0 })
    }

    /// What it printed (its end when long), and whether the start was cut.
    pub fn output(&self) -> (String, bool) {
        let out = self.output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (out.text.clone(), out.truncated)
    }

    /// The end of what it printed, at most `most` bytes, copied alone.
    pub fn tail(&self, most: usize) -> String {
        let out = self.output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut from = out.text.len().saturating_sub(most);
        while !out.text.is_char_boundary(from) {
            from += 1;
        }
        let cut = from > 0 || out.truncated;
        format!("{}{}", if cut { "…" } else { "" }, &out.text[from..])
    }

    /// How many bytes it has written so far.
    pub fn written(&self) -> usize {
        self.output.lock().unwrap_or_else(std::sync::PoisonError::into_inner).written
    }

    /// Its process group (unix), which what it left running is in too.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn group(&self) -> u32 {
        self.pid
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Ends it and what it started: asked first, killed if it stays. What
    /// it left in the background is in its group after it ended: that is
    /// ended too (a group's id is not taken again while it has members).
    pub fn kill(&self) {
        #[cfg(unix)]
        {
            let (pid, ended) = (self.pid as i32, self.ended.clone());
            // SAFETY: signals the command's own process group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
            std::thread::spawn(move || {
                std::thread::sleep(GRACE);
                let _ = ended;
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            });
        }
        #[cfg(not(unix))]
        let _ = crate::procs::command("taskkill").args(["/PID", &self.pid.to_string(), "/T", "/F"]).status();
    }
}

/// The text in `bytes` up to a character cut at their end, which is kept
/// for the next read; what is not UTF-8 at all is replaced.
fn take_text(bytes: &mut Vec<u8>) -> String {
    let keep = match std::str::from_utf8(bytes) {
        Ok(_) => 0,
        // An incomplete character at the very end: not an error yet.
        Err(e) if e.error_len().is_none() => bytes.len() - e.valid_up_to(),
        Err(_) => {
            // Its last bytes may still start a character.
            let tail = bytes.len().saturating_sub(3);
            (tail..bytes.len()).find(|&i| bytes[i] >= 0xC0 && std::str::from_utf8(&bytes[i..]).is_err_and(|e| e.error_len().is_none())).map_or(0, |i| bytes.len() - i)
        }
    };
    let rest = bytes.split_off(bytes.len() - keep);
    let text = String::from_utf8_lossy(bytes).into_owned();
    *bytes = rest;
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_cut_between_reads_comes_whole() {
        let word = "привет ✓".as_bytes();
        let mut cut = Vec::new();
        let mut text = String::new();
        for chunk in word.chunks(3) {
            cut.extend_from_slice(chunk);
            text.push_str(&take_text(&mut cut));
        }
        assert_eq!(text, "привет ✓");
        let mut bad = vec![b'a', 0xFF, b'b', 0xD0];
        assert_eq!(take_text(&mut bad), "a\u{FFFD}b");
        assert_eq!(bad, vec![0xD0], "the start of a character waits");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_will_not_end_is_killed() {
        let (tx, rx) = mpsc::channel();
        let dir = std::env::temp_dir();
        let term = Term::start("t".into(), "trap '' TERM; echo ready; sleep 30", &[], &[], &dir, None, tx).unwrap();
        let started = std::time::Instant::now();
        while !term.output().0.contains("ready") && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        term.kill();
        match rx.recv_timeout(GRACE + Duration::from_secs(3)) {
            Ok(super::super::host::Event::TermExited { .. }) => {}
            _ => panic!("still running after SIGKILL"),
        }
    }
}
