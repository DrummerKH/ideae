//! What ssh asks of the user (a password, a key's passphrase, whether to
//! trust a host it has not met), asked in the window: ssh runs
//! `ideae --askpass <question>` (SSH_ASKPASS), which asks the window over
//! its socket and prints the answer for ssh. Nothing is kept: ssh's master
//! connection to the host ([`crate::daemon::remote`]) is what spares asking
//! again.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Where the window's socket is, for the ssh it starts.
pub const SOCK_ENV: &str = "IDEAE_ASKPASS_SOCK";
/// Set for that ssh: ssh runs its askpass with the question alone, no flag.
pub const MODE_ENV: &str = "IDEAE_ASKPASS";

/// How long a question waits for its answer.
const PATIENCE: Duration = Duration::from_secs(600);

static SOCKET: OnceLock<PathBuf> = OnceLock::new();
static ASKING: AtomicUsize = AtomicUsize::new(0);

/// A question ssh asks, and where its answer goes (none: cancelled).
pub struct Ask {
    pub prompt: String,
    pub reply: mpsc::Sender<Option<String>>,
}

impl Ask {
    /// A yes or no (a host not met), not a secret: shown as it is typed.
    pub fn is_secret(&self) -> bool {
        !self.prompt.contains("(yes/no")
    }
}

/// The window's socket, once it listens.
pub fn socket() -> Option<&'static PathBuf> {
    SOCKET.get()
}

/// Whether a question is open now: whoever waits on ssh waits on a person.
pub fn asking() -> bool {
    ASKING.load(Ordering::Relaxed) > 0
}

/// Listens for ssh's questions; each goes to `on_ask` from a thread of its own.
pub fn listen(on_ask: impl Fn(Ask) + Send + Sync + 'static) -> std::io::Result<()> {
    let dir = ideae_core::paths::own_runtime_dir()?;
    let path = dir.join(format!("askpass-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let _ = SOCKET.set(path);
    let on_ask = std::sync::Arc::new(on_ask);
    std::thread::Builder::new().name("askpass".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let on_ask = on_ask.clone();
            std::thread::spawn(move || answer(stream, &*on_ask));
        }
    })?;
    Ok(())
}

fn answer(stream: UnixStream, on_ask: &dyn Fn(Ask)) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    let prompt = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["prompt"].as_str().map(str::to_string)).unwrap_or_default();
    let (reply, answered) = mpsc::channel();
    ASKING.fetch_add(1, Ordering::Relaxed);
    on_ask(Ask { prompt, reply });
    let said = answered.recv_timeout(PATIENCE).ok().flatten();
    ASKING.fetch_sub(1, Ordering::Relaxed);
    let _ = writeln!(writer, "{}", serde_json::json!({ "answer": said }));
}

/// `ideae --askpass <question>`, run by ssh: the window's answer on
/// stdout; none (cancelled, no window), and ssh is told no.
pub fn run(args: &[String]) -> i32 {
    let prompt = args.join(" ");
    let Some(socket) = std::env::var_os(SOCK_ENV) else { return 1 };
    let Ok(mut stream) = UnixStream::connect(socket) else { return 1 };
    if writeln!(stream, "{}", serde_json::json!({ "prompt": prompt })).is_err() {
        return 1;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return 1;
    }
    match serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["answer"].as_str().map(str::to_string)) {
        Some(answer) => {
            println!("{answer}");
            0
        }
        None => 1,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_question_of_ssh_is_answered_by_the_window() {
        use std::io::{BufRead as _, BufReader, Write as _};
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        if super::socket().is_none() {
            super::listen(move |ask| {
                seen.lock().unwrap().push(ask.prompt.clone());
                let said = (ask.prompt != "Cancel?").then(|| format!("secret for {}", ask.prompt));
                let _ = ask.reply.send(said);
            })
            .unwrap();
        }
        let ask = |prompt: &str| {
            let mut stream = std::os::unix::net::UnixStream::connect(super::socket().unwrap()).unwrap();
            writeln!(stream, "{}", serde_json::json!({ "prompt": prompt })).unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["answer"].as_str().map(str::to_string)
        };
        assert_eq!(ask("me@box's password: ").as_deref(), Some("secret for me@box's password: "));
        assert_eq!(ask("Cancel?"), None);
        assert!(!super::asking(), "nothing open once answered");
        assert!(!super::Ask { prompt: "Are you sure you want to continue connecting (yes/no/[fingerprint])?".into(), reply: std::sync::mpsc::channel().0 }.is_secret());
    }
}
