//! Terminals that outlive the window: a daemon process runs them, windows
//! attach, from this machine or over ssh (the bridge).

pub mod bridge;
pub mod calls;
pub mod pipe;
pub mod proto;
pub mod server;

/// `ideae --hook` in a terminal of the daemon: reports there.
pub fn send_hook(socket: &str, hook: proto::Hook) {
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) {
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
        let _ = proto::write_msg(&mut stream, &proto::Msg::Hook(hook));
    }
}

/// Starts `<this binary> --daemon` in a session of its own, so it stays
/// when whoever started it goes.
pub fn start() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    let exe = crate::paths::exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid only changes the child's session.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map(|_| ())
}
