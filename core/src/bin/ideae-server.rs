//! What a server runs for windows on other machines: the daemon that keeps
//! its terminals and agents, and the bridge ssh starts to reach it. No
//! window, no gpui: it builds for any Linux or macOS.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        #[cfg(unix)]
        ["--daemon", "--stdio"] => ideae_core::daemon::bridge::run(),
        #[cfg(unix)]
        ["--daemon"] => ideae_core::daemon::server::run(false),
        #[cfg(unix)]
        ["--daemon", "--adopt"] => ideae_core::daemon::server::run(true),
        #[cfg(unix)]
        ["--acp-host"] => ideae_core::acp::host::run_process(),
        ["--mcp"] => ideae_core::mcp::run_server(),
        ["--hook", ..] => {
            let _ = ideae_core::hook::run(&args[1..].iter().map(|a| a.to_string()).collect::<Vec<_>>(), None);
        }
        ["--version"] => println!("ideae-server {} (protocol {})", ideae_core::build::version(), protocol()),
        _ => {
            eprintln!("usage: ideae-server --daemon [--stdio] | --version");
            std::process::exit(2);
        }
    }
}

fn protocol() -> u32 {
    #[cfg(unix)]
    return ideae_core::daemon::proto::PROTOCOL;
    #[cfg(not(unix))]
    0
}
