//! System load and what the app and everything it started cost: a small
//! readout in the title bar, the process tree in its tooltip.
//!
//! CPU is a share of the whole machine (all cores = 100%), for processes as
//! for the system, so the numbers add up. Memory is what a process holds in
//! RAM: on Linux PSS where readable (shared pages are split between the
//! processes that map them), on macOS the resident size, on Windows the
//! working set.

use std::{collections::HashMap, path::PathBuf, rc::Rc, time::{Duration, Instant}};

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{prelude::FluentBuilder as _, *};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Editor,
    Daemon,
    Agent,
    /// What an agent started that analyzes code: Claude Code's LSP plugins.
    AgentLanguageServer,
    Terminal,
    LanguageServer,
    Browser,
    Other,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Editor => "editor",
            Kind::Daemon => "daemon",
            Kind::Agent => "agents",
            Kind::AgentLanguageServer => "agents' language servers",
            Kind::Terminal => "terminals",
            Kind::LanguageServer => "language servers",
            Kind::Browser => "browser",
            Kind::Other => "other",
        }
    }
}

#[derive(Clone)]
pub struct Proc {
    pub pid: i32,
    pub depth: usize,
    pub name: String,
    /// The task a terminal session belongs to (its worktree's name), on the
    /// session's first row.
    pub task: Option<String>,
    /// The task it runs for, on every row of the session.
    owner: Option<String>,
    pub kind: Kind,
    /// The app's child this process descends from (itself for those).
    top: i32,
    pub cpu: f32,
    pub mem: u64,
}

#[derive(Default)]
pub struct Snapshot {
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// The app's tree, parents before children.
    pub procs: Vec<Proc>,
    pub by_kind: Vec<(Kind, usize, f32, u64)>,
    /// Per task: its terminals, agent and what they started.
    pub by_task: Vec<(String, f32, u64)>,
    pub app_cpu: f32,
    pub app_mem: u64,
    /// The app itself: the window, its daemon, its language servers, Chrome.
    pub own_mem: u64,
}

struct Raw {
    ppid: i32,
    /// The session it is in; its own pid for a terminal's shell. Windows
    /// has no sessions: there a terminal's shell gets its own pid here.
    sid: i32,
    name: String,
    /// CPU time so far, which Linux has with the process list; elsewhere
    /// `os::usage` asks for it.
    #[cfg(target_os = "linux")]
    ticks: u64,
}

#[derive(Default)]
struct Sampler {
    cpu: Option<(u64, u64)>,
    ticks: HashMap<i32, u64>,
    at: Option<Instant>,
}

#[cfg(target_os = "linux")]
mod os {
    use std::collections::HashMap;

    use super::Raw;

    fn sysconf(name: libc::c_int) -> u64 {
        // SAFETY: sysconf only reads a constant.
        (unsafe { libc::sysconf(name) }).max(1) as u64
    }

    pub fn ticks_per_sec() -> f32 {
        sysconf(libc::_SC_CLK_TCK) as f32
    }

    /// Cores, and CPU ticks so far: all of them and the idle ones.
    pub fn cpu() -> (f32, Option<(u64, u64)>) {
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let cores = stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count().max(1) as f32;
        let ticks = stat.lines().next().map(|line| {
            let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            (v.iter().take(8).sum::<u64>(), v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0))
        });
        (cores, ticks)
    }

    /// RAM in all and in use.
    pub fn memory() -> (u64, u64) {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let field = |name: &str| {
            meminfo
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        let total = field("MemTotal:");
        (total, total.saturating_sub(field("MemAvailable:")))
    }

    pub fn processes(_me: i32) -> HashMap<i32, Raw> {
        let mut all = HashMap::new();
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            let (Some(open), Some(close)) = (text.find('('), text.rfind(')')) else { continue };
            let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
            if f.first() == Some(&"Z") {
                continue;
            }
            let num = |i: usize| f.get(i).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
            all.insert(
                pid,
                Raw {
                    ppid: num(1) as i32,
                    sid: num(3) as i32,
                    name: text[open + 1..close].to_string(),
                    ticks: (num(11) + num(12)) as u64,
                },
            );
        }
        all
    }

    /// CPU time so far and memory: PSS, or RSS when smaps_rollup cannot be read.
    pub fn usage(pid: i32, raw: &Raw) -> (u64, u64) {
        let pss = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok().and_then(|text| {
            Some(text.lines().find(|l| l.starts_with("Pss:"))?.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
        });
        let memory = pss.unwrap_or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/statm"))
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map_or(0, |pages| pages * sysconf(libc::_SC_PAGESIZE))
        });
        (raw.ticks, memory)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::collections::HashMap;

    use super::Raw;

    /// Nanoseconds.
    pub fn ticks_per_sec() -> f32 {
        1e9
    }

    #[allow(deprecated)]
    fn host() -> libc::mach_port_t {
        // SAFETY: returns the port every process has.
        unsafe { libc::mach_host_self() }
    }

    pub fn cpu() -> (f32, Option<(u64, u64)>) {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
        let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: host_statistics writes at most `count` integers into `info`.
        let ok = unsafe { libc::host_statistics(host(), libc::HOST_CPU_LOAD_INFO, (&raw mut info).cast(), &mut count) } == 0;
        let ticks = ok.then(|| {
            let total = info.cpu_ticks.iter().map(|&t| u64::from(t)).sum();
            (total, u64::from(info.cpu_ticks[libc::CPU_STATE_IDLE as usize]))
        });
        (cores, ticks)
    }

    /// In use as Activity Monitor has it: app memory, wired and compressed.
    pub fn memory() -> (u64, u64) {
        let mut total = 0u64;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: sysctlbyname writes `size` bytes into `total`; the name ends in NUL.
        unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&raw mut total).cast(), &mut size, std::ptr::null_mut(), 0) };
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: host_statistics64 writes at most `count` integers into `vm`.
        if unsafe { libc::host_statistics64(host(), libc::HOST_VM_INFO64, (&raw mut vm).cast(), &mut count) } != 0 {
            return (total, 0);
        }
        // SAFETY: sysconf only reads a constant.
        let page = (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(1) as u64;
        let pages = u64::from(vm.internal_page_count).saturating_sub(u64::from(vm.purgeable_count))
            + u64::from(vm.wire_count)
            + u64::from(vm.compressor_page_count);
        (total, pages * page)
    }

    pub fn processes(_me: i32) -> HashMap<i32, Raw> {
        let mut all = HashMap::new();
        for (pid, ppid, name) in crate::procs::running_with_parents() {
            all.insert(pid, Raw { ppid, sid: crate::procs::sid(pid), name });
        }
        all
    }

    /// CPU time so far and the resident size.
    #[allow(deprecated)]
    pub fn usage(pid: i32, _: &Raw) -> (u64, u64) {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage fills the struct of the flavor asked for.
        if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) } != 0 {
            return (0, 0);
        }
        // The times are in Mach ticks, which are not nanoseconds on Apple silicon.
        static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
        let (numer, denom) = *TIMEBASE.get_or_init(|| {
            let mut base = libc::mach_timebase_info { numer: 1, denom: 1 };
            // SAFETY: writes the two numbers.
            unsafe { libc::mach_timebase_info(&mut base) };
            (u64::from(base.numer.max(1)), u64::from(base.denom.max(1)))
        });
        let ticks = (info.ri_user_time + info.ri_system_time) as u128 * numer as u128 / denom as u128;
        (ticks as u64, info.ri_resident_size)
    }
}

#[cfg(windows)]
mod os {
    use std::collections::HashMap;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS},
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::{GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    use super::Raw;

    /// File time: 100 nanoseconds.
    pub fn ticks_per_sec() -> f32 {
        1e7
    }

    fn ticks(time: &FILETIME) -> u64 {
        u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime)
    }

    pub fn cpu() -> (f32, Option<(u64, u64)>) {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut idle, mut kernel, mut user) = (zero, zero, zero);
        // SAFETY: the three times are written to.
        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0;
        // The kernel's time has the idle time in it.
        (cores, ok.then(|| (ticks(&kernel) + ticks(&user), ticks(&idle))))
    }

    pub fn memory() -> (u64, u64) {
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: the struct says how long it is.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return (0, 0);
        }
        (status.ullTotalPhys, status.ullTotalPhys.saturating_sub(status.ullAvailPhys))
    }

    pub fn processes(me: i32) -> HashMap<i32, Raw> {
        let mut all = HashMap::new();
        // SAFETY: the snapshot handle is checked and closed here; each entry
        // says how long it is.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return all;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(&name).to_string();
                let (pid, ppid) = (entry.th32ProcessID as i32, entry.th32ParentProcessID as i32);
                // What the app started with a task in its environment is a terminal's shell.
                let shell = ppid == me && super::own_env(pid).is_some_and(|env| env.0.is_some());
                all.insert(pid, Raw { ppid, sid: if shell { pid } else { 0 }, name });
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        all
    }

    /// CPU time so far and the working set.
    pub fn usage(pid: i32, _: &Raw) -> (u64, u64) {
        // SAFETY: the handle is checked and closed here; the structs are
        // written to at their own size.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle.is_null() {
                return (0, 0);
            }
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            let time = if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
                ticks(&kernel) + ticks(&user)
            } else {
                0
            };
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let memory = if K32GetProcessMemoryInfo(handle, &mut counters, size) != 0 { counters.WorkingSetSize as u64 } else { 0 };
            CloseHandle(handle);
            (time, memory)
        }
    }
}

impl Sampler {
    fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let elapsed = self.at.map_or(0., |at| now.duration_since(at).as_secs_f32());
        self.at = Some(now);

        let mut snap = Snapshot::default();
        let (cores, cpu) = os::cpu();
        if let Some((total, idle)) = cpu {
            if let Some((pt, pi)) = self.cpu {
                let (dt, di) = (total.saturating_sub(pt), idle.saturating_sub(pi));
                if dt > 0 {
                    snap.cpu = 100. * (1. - di as f32 / dt as f32);
                }
            }
            self.cpu = Some((total, idle));
        }
        (snap.mem_total, snap.mem_used) = os::memory();

        let me = std::process::id() as i32;
        // Every process, to find the app's descendants.
        let all = os::processes(me);
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for (&pid, raw) in &all {
            children.entry(raw.ppid).or_default().push(pid);
        }
        for list in children.values_mut() {
            list.sort();
        }

        let tck = os::ticks_per_sec();
        let mut ticks = HashMap::new();
        // The app, its daemon, and the daemon's terminal sessions: found by
        // the socket in their environment, since a session handed to a newer
        // daemon is nobody's child any more.
        let (daemon, socket) = daemon_of_app();
        let mut roots: Vec<(i32, usize)> = vec![(me, 0)];
        if let Some(d) = daemon.filter(|d| all.contains_key(d)) {
            roots.push((d, 0));
        }
        if let Some(socket) = &socket {
            let candidates: std::collections::HashSet<i32> = all
                .iter()
                .filter(|(pid, raw)| raw.sid == **pid && raw.ppid != me && Some(**pid) != daemon)
                .filter(|(pid, _)| session_env(**pid, &children).2.as_deref() == Some(socket.as_str()))
                .map(|(pid, _)| *pid)
                .collect();
            // A session of its own only when nothing of ours is above it: an
            // agent's background shell (setsid) inherits the socket too, and
            // the daemon's sessions show under the daemon.
            let ours_above = |pid: i32| {
                let mut at = all.get(&pid).map(|r| r.ppid);
                for _ in 0..64 {
                    match at {
                        Some(p) if p == me || Some(p) == daemon || candidates.contains(&p) => return true,
                        Some(p) if p > 1 => at = all.get(&p).map(|r| r.ppid),
                        _ => return false,
                    }
                }
                false
            };
            let mut sessions: Vec<i32> = candidates.iter().copied().filter(|&pid| !ours_above(pid)).collect();
            sessions.sort();
            roots.extend(sessions.into_iter().map(|pid| (pid, 1)));
        }
        let root_pids: std::collections::HashSet<i32> = roots.iter().map(|r| r.0).collect();
        let mut stack: Vec<(i32, usize, Kind, Option<String>, i32)> =
            roots.iter().rev().map(|&(pid, depth)| (pid, depth, Kind::Terminal, None, pid)).collect();
        while let Some((pid, depth, inherited, task, top)) = stack.pop() {
            let top = if depth == 1 { pid } else { top };
            let Some(raw) = all.get(&pid) else { continue };
            // A terminal session: one we found on its own, or a session the
            // app or its daemon started.
            let session_root = (root_pids.contains(&pid) && pid != me && Some(pid) != daemon)
                || (raw.sid == pid && daemon.is_some_and(|d| raw.ppid == d));
            let kind = if pid == me {
                Kind::Editor
            } else if Some(pid) == daemon {
                Kind::Daemon
            } else if session_root {
                Kind::Terminal
            } else if raw.name == "claude" {
                Kind::Agent
            } else if matches!(inherited, Kind::Agent | Kind::AgentLanguageServer) && is_language_server(&raw.name) {
                Kind::AgentLanguageServer
            } else if raw.ppid == me {
                // The app's own children: terminal shells lead a session,
                // language servers and Chrome do not.
                if raw.sid == pid {
                    Kind::Terminal
                } else if matches!(raw.name.as_str(), "conhost" | "OpenConsole") {
                    // Windows: what a terminal draws through.
                    Kind::Terminal
                } else if raw.name.contains("chrom") {
                    Kind::Browser
                } else if raw.name == "git" || raw.name == "notify-send" {
                    Kind::Other
                } else {
                    Kind::LanguageServer
                }
            } else {
                inherited
            };
            let leader = Some(pid) != daemon && (session_root || (raw.ppid == me && raw.sid == pid));
            let (task, agent_tab) = if leader {
                let (task, agent, _) = session_env(pid, &children);
                (task, agent)
            } else {
                (task, false)
            };
            let kind = if agent_tab { Kind::Agent } else { kind };
            let (spent, mem) = os::usage(pid, raw);
            let prev = self.ticks.get(&pid).copied();
            ticks.insert(pid, spent);
            let cpu = match prev {
                Some(prev) if elapsed > 0. => 100. * spent.saturating_sub(prev) as f32 / (elapsed * tck * cores),
                _ => 0.,
            };
            snap.procs.push(Proc {
                pid,
                depth,
                name: raw.name.clone(),
                task: if leader { task.clone() } else { None },
                owner: task.clone(),
                kind,
                top,
                cpu,
                mem,
            });
            if let Some(kids) = children.get(&pid) {
                // Reversed, so the stack pops them in pid order. Sessions
                // found on their own are roots, under the daemon's row.
                for &kid in kids.iter().rev().filter(|k| !root_pids.contains(k)) {
                    stack.push((kid, depth + 1, kind, task.clone(), top));
                }
            }
        }
        self.ticks = ticks;

        let mut tasks: HashMap<String, (f32, u64)> = HashMap::new();
        for p in &snap.procs {
            if let Some(task) = &p.owner {
                let e = tasks.entry(task.clone()).or_default();
                e.0 += p.cpu;
                e.1 += p.mem;
            }
            if matches!(p.kind, Kind::Editor | Kind::Daemon | Kind::LanguageServer | Kind::Browser) && p.owner.is_none() {
                snap.own_mem += p.mem;
            }
        }
        let mut by_task: Vec<_> = tasks.into_iter().map(|(t, (c, m))| (t, c, m)).collect();
        by_task.sort_by(|a, b| b.2.cmp(&a.2));
        snap.by_task = by_task;

        let mut kinds: HashMap<Kind, (usize, f32, u64)> = HashMap::new();
        // Agents count once per task, not per process or session they run.
        let mut agent_tasks = std::collections::HashSet::new();
        for p in &snap.procs {
            let e = kinds.entry(p.kind).or_default();
            if p.kind != Kind::Agent || agent_tasks.insert(p.owner.clone().unwrap_or_else(|| p.top.to_string())) {
                e.0 += 1;
            }
            e.1 += p.cpu;
            e.2 += p.mem;
            snap.app_cpu += p.cpu;
            snap.app_mem += p.mem;
        }
        let mut by_kind: Vec<_> = kinds.into_iter().map(|(k, (n, c, m))| (k, n, c, m)).collect();
        by_kind.sort_by_key(|e| e.0);
        snap.by_kind = by_kind;
        snap
    }
}

/// Language servers by their process names, as Claude Code's LSP plugins
/// start them.
fn is_language_server(name: &str) -> bool {
    const NAMES: [&str; 12] = [
        "rust-analyzer", "gopls", "clangd", "pyright", "jdtls", "intellij-server",
        "kotlin-lsp", "typescript-lan", "tsserver", "vtsls", "lua-language-se", "solargraph",
    ];
    NAMES.iter().any(|n| name.starts_with(n)) || name.contains("language-server") || name.ends_with("-lsp")
}

/// The task in a process's environment, whether it is in the agent's tab,
/// and the app's socket. None when the environment cannot be read, or is
/// kept back as macOS does for the system's own programs.
fn own_env(pid: i32) -> Option<(Option<String>, bool, Option<String>)> {
    let env = crate::procs::environ(pid).filter(|env| !env.is_empty())?;
    let task_key = format!("{}=", crate::agent::TASK_ENV);
    let sock_key = format!("{}=", crate::agent::SOCK_ENV);
    let role = format!("{}=agent", crate::agent::ROLE_ENV);
    let (mut task, mut agent, mut sock) = (None, false, None);
    for var in env.split(|&b| b == 0).filter_map(|v| std::str::from_utf8(v).ok()) {
        if let Some(path) = var.strip_prefix(&task_key) {
            task = PathBuf::from(path).file_name().map(|n| n.to_string_lossy().into_owned());
        } else if let Some(s) = var.strip_prefix(&sock_key) {
            sock = Some(s.to_string());
        } else if var == role {
            agent = true;
        }
    }
    Some((task, agent, sock))
}

/// The same for a terminal's session. Its leader's own environment may be
/// out of reach (on macOS that is root's `login`, then a system shell);
/// what it started has inherited it: the nearest one that shows its
/// environment answers.
fn session_env(pid: i32, children: &HashMap<i32, Vec<i32>>) -> (Option<String>, bool, Option<String>) {
    const MOST: usize = 16;
    let mut queue = std::collections::VecDeque::from([pid]);
    let mut asked = 0;
    while let Some(at) = queue.pop_front() {
        match own_env(at) {
            Some(env) => return env,
            None => queue.extend(children.get(&at).into_iter().flatten()),
        }
        asked += 1;
        if asked == MOST {
            break;
        }
    }
    (None, false, None)
}

/// The daemon's pid and socket, when terminals run there.
#[cfg(unix)]
fn daemon_of_app() -> (Option<i32>, Option<String>) {
    match crate::daemon::client::get() {
        Some(d) => (d.pid(), Some(crate::daemon::proto::socket_path().display().to_string())),
        None => (None, None),
    }
}

/// Windows has no daemon: terminals live in the window.
#[cfg(not(unix))]
fn daemon_of_app() -> (Option<i32>, Option<String>) {
    (None, None)
}

pub fn bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1024. * 1024. * 1024. {
        format!("{:.1}G", n / (1024. * 1024. * 1024.))
    } else if n >= 1024. * 1024. {
        format!("{:.0}M", n / (1024. * 1024.))
    } else {
        format!("{:.0}K", n / 1024.)
    }
}

pub struct SysMon {
    pub snap: Rc<Snapshot>,
    _poll: Task<()>,
}

impl SysMon {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            let sampler = std::sync::Arc::new(std::sync::Mutex::new(Sampler::default()));
            loop {
                // Not while saving power: reading every process every two seconds is work too.
                if crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed) {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    if this.upgrade().is_none() {
                        break;
                    }
                    continue;
                }
                let s = sampler.clone();
                let snap = cx.background_spawn(async move { s.lock().unwrap().sample() }).await;
                if this
                    .update(cx, |this, cx| {
                        this.snap = Rc::new(snap);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        });
        Self { snap: Rc::default(), _poll: poll }
    }

    /// The title bar readout; the tooltip has the rest.
    pub fn render_widget(mon: &Entity<SysMon>, cx: &App) -> impl IntoElement {
        let s = mon.read(cx).snap.clone();
        let muted = cx.theme().muted_foreground;
        let mem_pct = if s.mem_total > 0 { 100. * s.mem_used as f32 / s.mem_total as f32 } else { 0. };
        let bar = |pct: f32, cx: &App| {
            let color = if pct >= 85. { gpui_kit::red() } else if pct >= 60. { hsla(0.11, 0.9, 0.55, 1.) } else { cx.theme().primary };
            div()
                .w(px(28.))
                .h(px(5.))
                .rounded_sm()
                .bg(muted.opacity(0.25))
                .child(div().h_full().rounded_sm().w(px(28. * pct.clamp(0., 100.) / 100.)).bg(color))
        };
        let tip = mon.clone();
        h_flex()
            .id("sysmon")
            .gap_1p5()
            .text_xs()
            .text_color(muted)
            .child("CPU")
            .child(bar(s.cpu, cx))
            .child(div().w(px(30.)).whitespace_nowrap().child(format!("{:.0}%", s.cpu)))
            .child("RAM")
            .child(bar(mem_pct, cx))
            .child(div().w(px(30.)).whitespace_nowrap().child(format!("{mem_pct:.0}%")))
            .child(div().pl_1().child(format!(
                "ideae {} · tasks {}",
                bytes(s.own_mem),
                bytes(s.app_mem.saturating_sub(s.own_mem))
            )))
            .tooltip(move |_, cx| {
                let mon = tip.clone();
                cx.new(|cx| SysTooltip::new(mon, cx)).into()
            })
    }
}

/// Follows the samples while it is shown.
struct SysTooltip {
    mon: Entity<SysMon>,
    _observe: Subscription,
}

impl SysTooltip {
    fn new(mon: Entity<SysMon>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&mon, |_, _, cx| cx.notify());
        Self { mon, _observe: observe }
    }
}

const TREE_ROWS: usize = 60;

impl Render for SysTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.mon.read(cx).snap.clone();
        let muted = cx.theme().muted_foreground;
        let mem_pct = if s.mem_total > 0 { 100. * s.mem_used as f32 / s.mem_total as f32 } else { 0. };
        let row = |left: AnyElement, mem: u64, cpu: f32| {
            h_flex()
                .gap_3()
                .child(div().flex_1().min_w_0().truncate().child(left))
                .child(div().w(px(56.)).text_right().child(bytes(mem)))
                .child(div().w(px(44.)).text_right().child(format!("{cpu:.1}%")))
        };
        let shown = s.procs.len().min(TREE_ROWS);
        v_flex()
            .w(px(460.))
            .p_3()
            .gap_2()
            .text_xs()
            .bg(cx.theme().popover)
            // Drawn outside the window's root, so nothing above sets the text color.
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .shadow_lg()
            .child(
                v_flex()
                    .child(format!("CPU {:.0}% of {} cores", s.cpu, std::thread::available_parallelism().map_or(1, |n| n.get())))
                    .child(format!("RAM {} of {} ({mem_pct:.0}%)", bytes(s.mem_used), bytes(s.mem_total)))
                    .child(format!("ideae with everything it started: {} · {:.1}% CPU", bytes(s.app_mem), s.app_cpu))
                    .child(format!("ideae itself (window, daemon, its language servers, browser): {}", bytes(s.own_mem))),
            )
            .child(div().text_color(muted).child("BY TYPE"))
            .child(v_flex().children(s.by_kind.iter().map(|&(kind, n, cpu, mem)| {
                let label = if kind == Kind::Agent { format!("{} ({n})", kind.label()) } else { kind.label().to_string() };
                row(div().child(label).into_any_element(), mem, cpu)
            })))
            .when(!s.by_task.is_empty(), |el| {
                el.child(div().text_color(muted).child("BY TASK"))
                    .child(v_flex().children(s.by_task.iter().map(|(task, cpu, mem)| row(div().child(task.clone()).into_any_element(), *mem, *cpu))))
            })
            .child(div().text_color(muted).child("PROCESSES"))
            .child(
                v_flex().font_family(cx.theme().mono_font_family.clone()).children(s.procs.iter().take(TREE_ROWS).map(|p| {
                    let name = h_flex()
                        .gap_2()
                        .pl(px(10. * p.depth as f32))
                        .child(p.name.clone())
                        .child(div().text_color(muted).child(p.pid.to_string()))
                        .when(p.kind == Kind::Daemon, |s| s.child(div().text_color(muted).child("[daemon]")))
                        .when_some(p.task.clone(), |s, task| {
                            let role = if p.kind == Kind::Agent { "agent" } else { "terminal" };
                            s.child(div().text_color(muted).child(format!("[{task} · {role}]")))
                        })
                        .into_any_element();
                    row(name, p.mem, p.cpu)
                })),
            )
            .when(s.procs.len() > shown, |el| {
                el.child(div().text_color(muted).child(format!("… {} more", s.procs.len() - shown)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, Sampler};

    /// Stands in for what runs in a terminal: started again by the test below.
    #[test]
    #[ignore]
    fn sleeper() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    fn sees_the_system_this_process_and_a_task_it_started() {
        let me = std::process::id() as i32;
        let exe = std::env::current_exe().unwrap();
        let sleeper = ["--exact", "sysmon::tests::sleeper", "--ignored"];
        // On Unix under a shell, whose own environment macOS keeps back.
        #[cfg(unix)]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(format!("'{}' {}; true", exe.display(), sleeper.join(" ")));
            command
        };
        #[cfg(not(unix))]
        let mut command = {
            let mut command = crate::procs::command(&exe);
            command.args(sleeper);
            command
        };
        command.env(crate::agent::TASK_ENV, "/somewhere/a-task").env(crate::agent::ROLE_ENV, "agent");
        #[cfg(unix)]
        // SAFETY: setsid in the child before exec, as a terminal's shell has it.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut shell = command.spawn().unwrap();
        let mut sampler = Sampler::default();
        sampler.sample();
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_millis(300) {
            std::hint::black_box((0..10_000u64).sum::<u64>());
        }
        let mut snap = sampler.sample();
        // The shell takes a moment to start what runs under it, whose
        // environment tells the task where the shell's own is kept back.
        let told = |snap: &super::Snapshot| snap.procs.iter().any(|p| p.pid == shell.id() as i32 && p.task.is_some());
        while !told(&snap) && started.elapsed() < std::time::Duration::from_secs(5) {
            std::hint::black_box((0..1_000_000u64).sum::<u64>());
            snap = sampler.sample();
        }
        for p in snap.procs.iter().filter(|p| p.top == shell.id() as i32) {
            assert!(crate::procs::alive(p.pid as u32));
            #[cfg(unix)]
            // SAFETY: kill only sends a signal, to what this test started.
            unsafe {
                libc::kill(p.pid, libc::SIGKILL)
            };
        }
        let _ = shell.kill();
        let _ = shell.wait();

        assert!(snap.mem_total > 0 && snap.mem_used > 0 && snap.mem_used <= snap.mem_total, "RAM {} of {}", snap.mem_used, snap.mem_total);
        assert!((0. ..=100.).contains(&snap.cpu), "CPU {}", snap.cpu);
        let own = snap.procs.iter().find(|p| p.pid == me).expect("this process");
        assert!(own.kind == Kind::Editor && own.mem > 1024 * 1024, "{} bytes", own.mem);
        assert!(own.cpu > 0., "a busy process uses CPU");
        let agent = snap.procs.iter().find(|p| p.pid == shell.id() as i32).expect("the shell it started");
        assert_eq!((agent.task.as_deref(), agent.kind == Kind::Agent), (Some("a-task"), true));
        assert_eq!(snap.by_task.first().map(|t| t.0.as_str()), Some("a-task"));
    }
}
