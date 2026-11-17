//! One llama-server for the app, run by a thread of its own: downloads
//! what is missing, picks the model and the device, starts the server,
//! starts it again when it falls, ends it when turned off or at quit.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};

use super::assets::{self, Model, Progress};
use super::prompt::Format;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Off,
    Downloading { what: &'static str, done: u64, of: u64 },
    Starting,
    Ready { model: &'static str, gpu: bool },
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    /// Run the model chosen, or the one the machine suits (None).
    Run(Option<Model>),
    Stop,
}

static COMMANDS: OnceLock<Sender<Cmd>> = OnceLock::new();
static STATUS: OnceLock<async_channel::Sender<Status>> = OnceLock::new();
/// The ready server's port; 0 while there is none.
static PORT: AtomicU16 = AtomicU16::new(0);
static ZETA: AtomicBool = AtomicBool::new(false);
static GPU: AtomicBool = AtomicBool::new(false);
/// The server's and a download's process, ended at quit by their ids:
/// a Mac does not end a child with its parent.
static CHILDREN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// Where the manager tells how it goes; set once by the UI.
pub fn report_to(status: async_channel::Sender<Status>) {
    let _ = STATUS.set(status);
}

fn tell(status: Status) {
    if let Some(tx) = STATUS.get() {
        let _ = tx.try_send(status);
    }
}

fn send(cmd: Cmd) {
    let tx = COMMANDS.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        // A thread for the app's life: Linux ends a tied child when the
        // thread that started it ends, not only the process.
        std::thread::Builder::new().name("predict".into()).spawn(move || Manager::default().run(rx)).expect("starting the prediction thread");
        tx
    });
    let _ = tx.send(cmd);
}

pub fn run(model: Option<Model>) {
    send(Cmd::Run(model));
}

pub fn stop() {
    if COMMANDS.get().is_some() {
        send(Cmd::Stop);
    }
}

/// The port of the server when it is ready to answer, and how to ask it.
pub fn ready() -> Option<(u16, Format, bool)> {
    let port = PORT.load(Ordering::SeqCst);
    let format = if ZETA.load(Ordering::SeqCst) { Format::Zeta } else { Format::Sweep };
    (port != 0).then(|| (port, format, GPU.load(Ordering::SeqCst)))
}

/// Ends the server and a download under way, at once: the app quits.
pub fn quit() {
    PORT.store(0, Ordering::SeqCst);
    for slot in &CHILDREN {
        let pid = slot.swap(0, Ordering::SeqCst);
        #[cfg(unix)]
        if pid != 0 {
            // SAFETY: a signal to a child of ours, whose id is cleared once it is reaped.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}

/// Starts a child that ends with the app, kept to be ended at quit.
pub(super) fn tied(command: &mut Command) -> std::io::Result<Child> {
    let child = crate::procs::spawn_tied(command)?;
    if let Some(slot) = CHILDREN.iter().find(|s| s.load(Ordering::SeqCst) == 0) {
        slot.store(child.id(), Ordering::SeqCst);
    }
    Ok(child)
}

/// The child was waited for: its id may go to another process.
pub(super) fn untied(child: &Child) {
    for slot in &CHILDREN {
        let _ = slot.compare_exchange(child.id(), 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// A device llama.cpp can offload to, as `--list-devices` tells it.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub free_mib: u64,
}

/// `  Vulkan0: NVIDIA GeForce RTX 4070 Ti (12282 MiB, 10737 MiB free)`
pub fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .filter_map(|line| {
            let (id, rest) = line.trim().split_once(": ")?;
            if id.contains(' ') || !id.ends_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let open = rest.rfind(" (")?;
            let (name, memory) = (&rest[..open], rest[open + 2..].trim_end_matches(')'));
            let free = memory.split(", ").find(|m| m.ends_with(" free"))?;
            let free_mib = free.split_whitespace().next()?.parse().ok()?;
            Some(Device { id: id.to_string(), name: name.to_string(), free_mib })
        })
        .collect()
}

/// A graphics card of its own, worth running on. A GPU in the processor
/// (an AMD APU, Intel's) measured slower than the CPU itself.
pub fn discrete(device: &Device) -> bool {
    let id = device.id.to_ascii_lowercase();
    if id.starts_with("cuda") || id.starts_with("mtl") || id.starts_with("metal") {
        return true;
    }
    let name = device.name.to_ascii_lowercase();
    let card = ["nvidia", "geforce", "quadro", "tesla", "radeon rx", "radeon pro", "radeon vii", "instinct", "arc(tm) a", "arc(tm) b", "arc a", "arc b"];
    card.iter().any(|c| name.contains(c)) && !name.contains("llvmpipe")
}

/// The model to run and the device it runs on (None: the CPU).
pub fn choose(chosen: Option<Model>, devices: &[Device]) -> (Model, Option<Device>) {
    let card = devices.iter().filter(|d| discrete(d)).max_by_key(|d| d.free_mib);
    let model = chosen.unwrap_or(match card {
        Some(card) if card.free_mib >= Model::Sweep15.vram_mib() => Model::Sweep15,
        _ => Model::Sweep05,
    });
    (model, card.filter(|c| c.free_mib >= model.vram_mib()).cloned())
}

#[derive(Default)]
struct Manager {
    child: Option<Child>,
    wanted: Option<Option<Model>>,
    falls: u32,
    again_at: Option<Instant>,
    /// A command that came while a download or a start was under way.
    next: Option<Cmd>,
}

struct Watch<'a> {
    rx: &'a Receiver<Cmd>,
    next: &'a mut Option<Cmd>,
    what: &'static str,
    told: Instant,
}

impl<'a> Watch<'a> {
    fn new(rx: &'a Receiver<Cmd>, next: &'a mut Option<Cmd>, what: &'static str) -> Self {
        Self { rx, next, what, told: Instant::now() }
    }
}

impl Progress for Watch<'_> {
    fn progress(&mut self, done: u64, of: u64) {
        if self.told.elapsed() >= Duration::from_millis(500) || done == of {
            self.told = Instant::now();
            tell(Status::Downloading { what: self.what, done, of });
        }
    }

    fn stopped(&mut self) -> bool {
        if self.next.is_none() {
            match self.rx.try_recv() {
                Ok(cmd) => *self.next = Some(cmd),
                Err(TryRecvError::Disconnected) => *self.next = Some(Cmd::Stop),
                Err(TryRecvError::Empty) => {}
            }
        }
        self.next.is_some()
    }
}

impl Manager {
    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = match self.next.take() {
                Some(cmd) => Some(cmd),
                None if self.child.is_some() || self.again_at.is_some() => match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => Some(Cmd::Stop),
                },
                None => Some(rx.recv().unwrap_or(Cmd::Stop)),
            };
            match cmd {
                Some(Cmd::Run(model)) => {
                    if self.wanted != Some(model) || self.child.is_none() {
                        self.end();
                        self.wanted = Some(model);
                        self.falls = 0;
                        self.start(&rx);
                    }
                }
                Some(Cmd::Stop) => {
                    self.end();
                    self.wanted = None;
                    tell(Status::Off);
                }
                None => {}
            }
            self.watch(&rx);
        }
    }

    fn end(&mut self) {
        PORT.store(0, Ordering::SeqCst);
        self.again_at = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            untied(&child);
        }
    }

    /// A server that fell is started again, later each time; after a few
    /// falls in a row it is left, and the reason told.
    fn watch(&mut self, rx: &Receiver<Cmd>) {
        if let Some(child) = &mut self.child
            && let Ok(Some(status)) = child.try_wait()
        {
            untied(child);
            self.child = None;
            PORT.store(0, Ordering::SeqCst);
            self.falls += 1;
            if self.falls > 4 {
                tell(Status::Failed(format!("the server stopped ({status}): {}", last_log_line())));
            } else {
                tell(Status::Starting);
                self.again_at = Some(Instant::now() + Duration::from_secs(1 << self.falls));
            }
        }
        if self.again_at.is_some_and(|at| Instant::now() >= at) {
            self.again_at = None;
            self.start(rx);
        }
    }

    fn start(&mut self, rx: &Receiver<Cmd>) {
        let Some(wanted) = self.wanted else { return };
        tell(Status::Starting);
        match self.launch(wanted, rx) {
            Ok(status) => tell(status),
            Err(e) => {
                self.end();
                if self.next.is_none() {
                    tell(Status::Failed(format!("{e:#}")));
                }
            }
        }
    }

    fn launch(&mut self, wanted: Option<Model>, rx: &Receiver<Cmd>) -> Result<Status> {
        let server = match assets::server() {
            Some(server) => server,
            None => assets::install_runtime(&mut Watch::new(rx, &mut self.next, "llama.cpp"))?,
        };
        let devices = list_devices(&server)?;
        let (model, device) = choose(wanted, &devices);
        let path = match assets::model(model) {
            Some(path) => path,
            None => assets::install_model(model, &mut Watch::new(rx, &mut self.next, model.name()))?,
        };
        tell(Status::Starting);
        let port = free_port()?;
        let child = tied(&mut server_command(&server, &path, port, device.as_ref())?).context("starting llama-server")?;
        self.child = Some(child);
        let since = Instant::now();
        loop {
            if let Some(child) = &mut self.child
                && let Some(status) = child.try_wait()?
            {
                bail!("llama-server ended ({status}): {}", last_log_line());
            }
            if matches!(super::http::status(port, "/health"), Ok(200)) {
                break;
            }
            if since.elapsed() > Duration::from_secs(180) {
                bail!("llama-server did not get ready in 3 minutes");
            }
            if Watch::new(rx, &mut self.next, "").stopped() {
                bail!("stopped");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        ZETA.store(model.format() == Format::Zeta, Ordering::SeqCst);
        GPU.store(device.is_some(), Ordering::SeqCst);
        PORT.store(port, Ordering::SeqCst);
        Ok(Status::Ready { model: model.name(), gpu: device.is_some() })
    }
}

fn log_path() -> PathBuf {
    assets::root().join("server.log")
}

fn last_log_line() -> String {
    let log = std::fs::read_to_string(log_path()).unwrap_or_default();
    log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().chars().take(200).collect()
}

pub(super) fn list_devices(server: &Path) -> Result<Vec<Device>> {
    let out = crate::procs::command(server)
        .arg("--list-devices")
        .current_dir(server.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", server.display()))?;
    if !out.status.success() {
        bail!("llama-server --list-devices: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default());
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Ok(parse_devices(&text))
}

pub(super) fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

pub fn server_args(model: &Path, port: u16, device: Option<&Device>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-m", &model.to_string_lossy(), "--host", "127.0.0.1", "--port", &port.to_string(), "-np", "1", "-c", "8192", "--cache-ram", "256",
        "--spec-type", "ngram-simple", "--no-webui", "--sleep-idle-seconds", "300",
    ]
    .map(String::from)
    .into();
    match device {
        // Alone: the Vulkan build would spread the model over a GPU in the processor too.
        Some(device) => args.extend(["--device".into(), device.id.clone(), "-ngl".into(), "99".into()]),
        None => {
            let threads = std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).clamp(1, 4));
            args.extend(["--device".into(), "none".into(), "-ngl".into(), "0".into(), "-t".into(), threads.to_string()]);
        }
    }
    args
}

fn server_command(server: &Path, model: &Path, port: u16, device: Option<&Device>) -> Result<Command> {
    std::fs::create_dir_all(assets::root())?;
    let log = std::fs::File::create(log_path())?;
    let mut command = crate::procs::command(server);
    command
        .args(server_args(model, port, device))
        .current_dir(server.parent().ok_or_else(|| anyhow!("no folder"))?)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: &str = "load_backend: loaded Vulkan backend\nAvailable devices:\n  Vulkan0: NVIDIA GeForce RTX 4070 Ti (12282 MiB, 10737 MiB free)\n  Vulkan1: AMD Ryzen 9 7950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO) (31733 MiB, 31705 MiB free)\n";

    #[test]
    fn devices_are_read_from_the_list() {
        let devices = parse_devices(DESKTOP);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], Device { id: "Vulkan0".into(), name: "NVIDIA GeForce RTX 4070 Ti".into(), free_mib: 10737 });
        assert_eq!(devices[1].name, "AMD Ryzen 9 7950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO)");
        assert!(parse_devices("Available devices:\n").is_empty());
    }

    #[test]
    fn a_graphics_card_runs_the_bigger_model_and_a_gpu_in_the_processor_does_not_count() {
        let devices = parse_devices(DESKTOP);
        let (model, device) = choose(None, &devices);
        assert_eq!((model, device.map(|d| d.id)), (Model::Sweep15, Some("Vulkan0".into())));
        // Only the APU: the CPU, and the small model.
        let apu = parse_devices("  Vulkan0: AMD Radeon Graphics (RADV PHOENIX) (16384 MiB, 16000 MiB free)\n  Vulkan1: Intel(R) UHD Graphics 770 (ADL-S GT1) (15000 MiB, 15000 MiB free)\n");
        assert_eq!(choose(None, &apu), (Model::Sweep05, None));
        assert_eq!(choose(None, &[]), (Model::Sweep05, None));
        // Zeta asked for, on a card too small for it: on the CPU.
        let small = parse_devices("  Vulkan0: NVIDIA GeForce GTX 1650 (4096 MiB, 3900 MiB free)\n");
        assert_eq!(choose(Some(Model::Zeta21), &small).1, None);
        assert_eq!(choose(None, &small).0, Model::Sweep15);
        let mac = parse_devices("  MTL0: Apple M2 Pro (21845 MiB, 21845 MiB free)\n");
        assert_eq!(choose(None, &mac).1.map(|d| d.id), Some("MTL0".into()));
        let rx = parse_devices("  Vulkan0: AMD Radeon RX 7800 XT (RADV NAVI32) (16368 MiB, 16000 MiB free)\n");
        assert!(discrete(&rx[0]));
    }

    #[test]
    fn the_server_runs_on_the_card_alone_or_on_a_few_cores() {
        let card = Device { id: "Vulkan0".into(), name: "NVIDIA".into(), free_mib: 8000 };
        let args = server_args(Path::new("m.gguf"), 4242, Some(&card)).join(" ");
        assert!(args.contains("--port 4242") && args.contains("--device Vulkan0 -ngl 99") && args.contains("--sleep-idle-seconds 300"), "{args}");
        let args = server_args(Path::new("m.gguf"), 4242, None).join(" ");
        assert!(args.contains("--device none -ngl 0 -t "), "{args}");
    }
}
