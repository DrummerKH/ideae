//! Terminals that outlive the window: a daemon process runs them (see
//! `ideae_core::daemon`), windows attach. Unix only; elsewhere terminals
//! stay in the window.

pub use ideae_core::daemon::{bridge, proto, server};
pub mod client;
pub mod install;
pub mod remote;

/// No window to show it: an agent that wants the user says so on the
/// desktop. The daemon calls it ([`server::NOTIFY`]).
pub fn notify(hook: &proto::Hook) {
    // The window may have changed it since this daemon started.
    let saved = crate::state::load();
    if !saved.settings.notifications || !saved.tells_of(std::path::Path::new(&hook.task)) {
        return;
    }
    let Some(status) = hook.event().status().filter(|s| s.is_attention()) else { return };
    let words = saved.settings.notify_said && saved.settings.show_said && saved.presenting.is_none();
    let said = hook.said.as_deref().filter(|_| words);
    let body = crate::said::notice(&status, said, words);
    let task = std::path::Path::new(&hook.task).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path = std::path::PathBuf::from(&hook.task);
    crate::desktop::notify_with(&task, &body, move || crate::desktop::open_window_on(&path));
}

#[cfg(all(test, unix))]
pub mod tests {
    use std::time::{Duration, Instant};

    /// A daemon for a test, a process of its own (this test binary run
    /// again), for the `IDEAE_STATE` the test set. Not a thread of the test's:
    /// a daemon left alone exits its process, which would end the whole run,
    /// with success, a minute after its test. Asked to shut down when
    /// dropped (its sessions with it), killed if it does not.
    pub struct TestDaemon(std::process::Child);

    pub fn start() -> TestDaemon {
        let exe = std::env::current_exe().unwrap();
        let child = std::process::Command::new(exe)
            .args(["--exact", "daemon::tests::daemon_process", "--ignored", "--nocapture", "--test-threads=1"])
            .env("IDEAE_TEST_DAEMON", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("the test's daemon");
        TestDaemon(child)
    }

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            use super::proto::{self, Msg};
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(proto::socket_path()) {
                let _ = proto::write_msg(&mut stream, &Msg::Hello { protocol: proto::PROTOCOL, version: "test".into() });
                let _ = proto::write_msg(&mut stream, &Msg::Shutdown);
            }
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until && matches!(self.0.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The daemon of [`start`].
    #[test]
    #[ignore]
    fn daemon_process() {
        if std::env::var_os("IDEAE_TEST_DAEMON").is_some() {
            super::server::run(false);
        }
    }
}
