//! Which agents are installed: looked for on PATH (the login shell's, as
//! the app took it) and where their installers put them, for the first
//! start and Settings → Agents → Find installed agents.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use super::{Kind, Profile};

/// An agent the app knows how to look for.
#[derive(Debug, PartialEq, Eq)]
pub struct Known {
    pub kind: Kind,
    pub program: &'static str,
    pub name: &'static str,
    /// Where to read how to install it.
    pub url: &'static str,
}

pub const KNOWN: [Known; 4] = [
    Known { kind: Kind::Claude, program: "claude", name: "Claude Code", url: "https://github.com/anthropics/claude-code" },
    Known { kind: Kind::Codex, program: "codex", name: "Codex", url: "https://github.com/openai/codex" },
    Known { kind: Kind::OpenCode, program: "opencode", name: "OpenCode", url: "https://opencode.ai" },
    // No adapter of its own: its status is guessed from its terminal.
    Known { kind: Kind::Generic, program: "gemini", name: "Gemini CLI", url: "https://github.com/google-gemini/gemini-cli" },
];

/// An agent found installed.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub known: &'static Known,
    pub path: PathBuf,
    /// Found on PATH: started by its name.
    pub on_path: bool,
}

impl Found {
    /// What a profile runs: its name when PATH has it, else where it is.
    pub fn command(&self) -> String {
        if self.on_path {
            return self.known.program.to_string();
        }
        let path = self.path.to_string_lossy().into_owned();
        if path.contains(char::is_whitespace) { format!("\"{path}\"") } else { path }
    }

    /// Where it is, for reading: the home folder as `~`.
    pub fn shown(&self) -> String {
        let command = self.command();
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
        match command.strip_prefix(&home) {
            Some(rest) if !home.is_empty() && !self.on_path => format!("~{rest}"),
            _ => command,
        }
    }

    /// A new profile for it, its id not one of `taken`'s.
    pub fn profile(&self, taken: &[Profile]) -> Profile {
        Profile { id: super::new_profile_id(self.known.name, taken), name: self.known.name.into(), kind: self.known.kind, command: self.command(), env: Default::default() }
    }
}

/// The program a profile starts, as a bare name: `claude`, `codex`.
fn program_of(profile: &Profile) -> String {
    let first = profile.command.split_whitespace().next().unwrap_or(profile.kind.program()).trim_matches('"');
    let name = Path::new(first).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.trim_end_matches(".exe").trim_end_matches(".cmd").trim_end_matches(".bat").to_string()
}

/// Where installers put agents, besides PATH: npm's global folders,
/// Claude Code's and OpenCode's own installers, Homebrew, Scoop and the like.
pub fn usual_places(home: Option<&Path>, var: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        let under = |name: &str, rest: &str| var(name).filter(|v| !v.is_empty()).map(|v| Path::new(&v).join(rest));
        places.extend(under("APPDATA", "npm"));
        places.extend(under("LOCALAPPDATA", "pnpm"));
        places.extend(under("LOCALAPPDATA", r"Microsoft\WinGet\Links"));
        places.extend(under("LOCALAPPDATA", r"Volta\bin"));
        if let Some(home) = home {
            places.extend([r".local\bin", r".bun\bin", r".opencode\bin", r"scoop\shims"].map(|rest| home.join(rest)));
        }
        return places;
    }
    if let Some(prefix) = var("NPM_CONFIG_PREFIX").filter(|v| !v.is_empty()) {
        places.push(Path::new(&prefix).join("bin"));
    }
    if let Some(home) = home {
        places.extend([".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".opencode/bin", ".volta/bin", ".yarn/bin", "bin"].map(|rest| home.join(rest)));
        places.push(home.join(if cfg!(target_os = "macos") { "Library/pnpm" } else { ".local/share/pnpm" }));
        // Each Node version of nvm has its own global folder.
        if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            places.extend(versions.flatten().map(|v| v.path().join("bin")));
        }
    }
    places.extend(["/usr/local/bin", "/opt/homebrew/bin", "/home/linuxbrew/.linuxbrew/bin"].map(PathBuf::from));
    places
}

/// `program` in `dir`, when it is there to run.
fn executable(dir: &Path, program: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        return ["exe", "cmd", "bat"].iter().map(|ext| dir.join(format!("{program}.{ext}"))).find(|p| p.is_file());
    }
    let candidate = dir.join(program);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&candidate).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).then_some(candidate)
    }
    #[cfg(not(unix))]
    candidate.is_file().then_some(candidate)
}

/// The known agents found in PATH's folders, else in `places`.
pub fn find_in(path: Option<&OsStr>, places: &[PathBuf]) -> Vec<Found> {
    let on_path: Vec<PathBuf> = path.map(|p| std::env::split_paths(p).filter(|d| !d.as_os_str().is_empty()).collect()).unwrap_or_default();
    KNOWN
        .iter()
        .filter_map(|known| {
            let found = |dirs: &[PathBuf]| dirs.iter().find_map(|dir| executable(dir, known.program));
            match found(&on_path) {
                Some(path) => Some(Found { known, path, on_path: true }),
                None => found(places).map(|path| Found { known, path, on_path: false }),
            }
        })
        .collect()
}

/// The agents installed here. Off the UI thread: it reads folders.
pub fn find_installed() -> Vec<Found> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()).map(PathBuf::from);
    find_in(std::env::var_os("PATH").as_deref(), &usual_places(home.as_deref(), |name| std::env::var(name).ok()))
}

/// The agents found that no profile runs yet.
pub fn not_added(found: &[Found], profiles: &[Profile]) -> Vec<Found> {
    found
        .iter()
        .filter(|f| !profiles.iter().any(|p| (p.kind == f.known.kind && p.kind != Kind::Generic) || program_of(p) == f.known.program))
        .cloned()
        .collect()
}

/// What the first start says of agents.
#[derive(Debug, PartialEq)]
pub enum Welcome {
    /// What is installed has its profile: today's Claude Code alone.
    Nothing,
    /// Agents found that no profile runs: add them?
    Offer(Vec<Found>),
    /// No agent at all: tasks open with a shell; where to get one.
    NoneFound,
}

pub fn welcome(found: &[Found], profiles: &[Profile]) -> Welcome {
    let offers = not_added(found, profiles);
    if !offers.is_empty() {
        return Welcome::Offer(offers);
    }
    // A program of the user's own may be one; it was not looked for.
    let own = profiles.iter().any(|p| !KNOWN.iter().any(|k| k.program == program_of(p)));
    if found.is_empty() && !own { Welcome::NoneFound } else { Welcome::Nothing }
}

/// Whether the default profile runs one of the agents found.
pub fn default_found(found: &[Found], profiles: &[Profile], default: &str) -> bool {
    let Some(profile) = profiles.iter().find(|p| p.id == default) else { return false };
    let program = program_of(profile);
    found.iter().any(|f| f.known.program == program) || !KNOWN.iter().any(|k| k.program == program)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of stand-in programs: `name` made executable, `-name` a
    /// file that is not.
    fn folder(root: &Path, dir: &str, files: &[&str]) -> PathBuf {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            let (name, run) = match file.strip_prefix('-') {
                Some(name) => (name, false),
                None => (*file, true),
            };
            let name = if cfg!(windows) && run { format!("{name}.cmd") } else { name.to_string() };
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(if run { 0o755 } else { 0o644 })).unwrap();
            }
        }
        dir
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ideae-find-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn on_path_first_then_the_usual_places() {
        let root = scratch("path");
        let a = folder(&root, "a", &["codex", "-claude"]);
        let b = folder(&root, "b", &["claude", "codex"]);
        let home_bin = folder(&root, "home/.opencode/bin", &["opencode"]);
        let path = std::env::join_paths([a.clone(), b.clone()]).unwrap();
        let found = find_in(Some(&path), &[home_bin.clone(), b.clone()]);
        let names: Vec<(&str, bool)> = found.iter().map(|f| (f.known.program, f.on_path)).collect();
        assert_eq!(names, [("claude", true), ("codex", true), ("opencode", false)]);
        // The first folder of PATH that has it wins; one that is not executable does not count.
        assert_eq!(found[0].path.parent(), Some(b.as_path()));
        assert_eq!(found[1].path.parent(), Some(a.as_path()));
        assert_eq!(found[0].command(), "claude");
        assert_eq!(found[2].command(), found[2].path.to_string_lossy());
        let home = root.join("home");
        assert!(usual_places(Some(&home), |_| None).contains(&home_bin) || cfg!(windows));
        assert!(find_in(None, &[]).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_the_first_start_says() {
        let root = scratch("welcome");
        let bin = folder(&root, "bin", &["claude", "codex", "gemini"]);
        let path = bin.as_os_str().to_os_string();
        let only_claude = find_in(Some(&path), &[]).into_iter().filter(|f| f.known.kind == Kind::Claude).collect::<Vec<_>>();
        let all = find_in(Some(&path), &[]);
        let claude = vec![Profile::claude_code()];
        // Claude Code installed and the default: nothing to ask.
        assert_eq!(welcome(&only_claude, &claude), Welcome::Nothing);
        assert!(default_found(&only_claude, &claude, "claude"));
        // Others too: they are offered, Claude Code not again.
        let Welcome::Offer(offers) = welcome(&all, &claude) else { panic!("an offer") };
        assert_eq!(offers.iter().map(|f| f.known.name).collect::<Vec<_>>(), ["Codex", "Gemini CLI"]);
        let gemini = offers[1].profile(&claude);
        assert_eq!((gemini.id.as_str(), gemini.kind, gemini.command.as_str(), gemini.letter().as_str()), ("gemini-cli", Kind::Generic, "gemini", "G"));
        // A profile of a kind, whatever its command, counts as added.
        let mut with_codex = claude.clone();
        with_codex.push(Profile { id: "x".into(), name: "Work".into(), kind: Kind::Codex, command: "/opt/codex --full-auto".into(), ..Default::default() });
        assert_eq!(not_added(&all, &with_codex).len(), 1);
        // Nothing installed: where to get one.
        assert_eq!(welcome(&[], &claude), Welcome::NoneFound);
        assert!(!default_found(&[], &claude, "claude"));
        // Unless a program of the user's own is set.
        let own = vec![Profile { id: "aider".into(), name: "aider".into(), kind: Kind::Generic, command: "aider --yes".into(), ..Default::default() }];
        assert_eq!(welcome(&[], &own), Welcome::Nothing);
        assert!(default_found(&[], &own, "aider"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
