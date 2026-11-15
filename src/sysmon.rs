//! System load and what the app and everything it started cost: a small
//! readout in the title bar, the process tree in its tooltip.
//!
//! CPU is a share of the whole machine (all cores = 100%), for processes as
//! for the system, so the numbers add up. Memory is PSS where readable:
//! shared pages are split between the processes that map them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::{collections::HashMap, rc::Rc, time::{Duration, Instant}};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

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

#[cfg(target_os = "linux")]
struct Raw {
    ppid: i32,
    sid: i32,
    name: String,
    ticks: u64,
}

#[derive(Default)]
struct Sampler {
    cpu: Option<(u64, u64)>,
    ticks: HashMap<i32, u64>,
    at: Option<Instant>,
}

#[cfg(target_os = "linux")]
fn clk_tck() -> f32 {
    // SAFETY: sysconf only reads a constant.
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f32
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: as above.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(1) as u64
}

/// Only Linux is read for now; elsewhere the readout stays hidden.
pub const SUPPORTED: bool = cfg!(target_os = "linux");

#[cfg(not(target_os = "linux"))]
impl Sampler {
    fn sample(&mut self) -> Snapshot {
        Snapshot::default()
    }
}

#[cfg(target_os = "linux")]
impl Sampler {
    fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let elapsed = self.at.map_or(0., |at| now.duration_since(at).as_secs_f32());
        self.at = Some(now);

        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let cores = stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count().max(1) as f32;
        let mut snap = Snapshot::default();
        if let Some(line) = stat.lines().next() {
            let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            if let Some((pt, pi)) = self.cpu {
                let (dt, di) = (total.saturating_sub(pt), idle.saturating_sub(pi));
                if dt > 0 {
                    snap.cpu = 100. * (1. - di as f32 / dt as f32);
                }
            }
            self.cpu = Some((total, idle));
        }
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let field = |name: &str| {
            meminfo
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        snap.mem_total = field("MemTotal:");
        snap.mem_used = snap.mem_total.saturating_sub(field("MemAvailable:"));

        // Every process, to find the app's descendants.
        let mut all: HashMap<i32, Raw> = HashMap::new();
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
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for (&pid, raw) in &all {
            children.entry(raw.ppid).or_default().push(pid);
        }
        for list in children.values_mut() {
            list.sort();
        }

        let me = std::process::id() as i32;
        let (tck, pages) = (clk_tck(), page_size());
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
                .filter(|(pid, _)| session_env(**pid).2.as_deref() == Some(socket.as_str()))
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
                let (task, agent, _) = session_env(pid);
                (task, agent)
            } else {
                (task, false)
            };
            let kind = if agent_tab { Kind::Agent } else { kind };
            let prev = self.ticks.get(&pid).copied();
            ticks.insert(pid, raw.ticks);
            let cpu = match prev {
                Some(prev) if elapsed > 0. => 100. * raw.ticks.saturating_sub(prev) as f32 / (elapsed * tck * cores),
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
                mem: memory(pid, pages),
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

#[cfg(target_os = "linux")]
/// PSS, or RSS when smaps_rollup cannot be read.
fn memory(pid: i32, page: u64) -> u64 {
    if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        if let Some(kb) = text.lines().find(|l| l.starts_with("Pss:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()) {
            return kb * 1024;
        }
    }
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * page)
}

#[cfg(target_os = "linux")]
/// The task a terminal's shell was started for, and whether it is the
/// agent's tab, from its environment.
fn session_env(pid: i32) -> (Option<String>, bool, Option<String>) {
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { return (None, false, None) };
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
    (task, agent, sock)
}

#[cfg(target_os = "linux")]
/// The daemon's pid and socket, when terminals run there.
fn daemon_of_app() -> (Option<i32>, Option<String>) {
    match crate::daemon::client::get() {
        Some(d) => (d.pid(), Some(crate::daemon::proto::socket_path().display().to_string())),
        None => (None, None),
    }
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
            .child(div().w(px(26.)).child(format!("{:.0}%", s.cpu)))
            .child("RAM")
            .child(bar(mem_pct, cx))
            .child(div().w(px(26.)).child(format!("{mem_pct:.0}%")))
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
