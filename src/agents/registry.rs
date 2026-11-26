//! The Agent Client Protocol's registry of agents (agentclientprotocol.com):
//! what Settings → Agents → Browse ACP agents lists. An entry says how the
//! agent is distributed: an npm package run by `npx`, a Python one run by
//! `uvx`, or an archive per platform. One installed already is run as it
//! is; an archive is downloaded, checked against its sha256 and unpacked
//! into the app's data folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::Kind;

pub const URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub distribution: Distribution,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Distribution {
    pub npx: Option<Package>,
    pub uvx: Option<Package>,
    pub binary: BTreeMap<String, Binary>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Package {
    pub package: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Binary {
    pub archive: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub sha256: Option<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Registry {
    agents: Vec<Entry>,
}

/// How an agent of the registry would run here.
#[derive(Clone, Debug, PartialEq)]
pub enum How {
    /// Its program is installed: this command.
    Installed(String),
    /// `npx` or `uvx` fetches and runs it.
    Package(String),
    /// Its archive for this platform is downloaded first.
    Download,
    /// Not here: why.
    Not(String),
}

/// The registry, fetched; the copy kept from last time when that fails.
/// Not for the UI thread.
pub fn fetch() -> Result<Vec<Entry>, String> {
    let kept = crate::state::data_dir().join("acp-registry.json");
    let fetched = crate::procs::command("curl").args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--max-time", "20", "--max-filesize", "20000000", URL]).output();
    let bytes = match fetched {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            let _ = crate::state::write_atomic(&kept, &out.stdout);
            out.stdout
        }
        _ => std::fs::read(&kept).map_err(|_| "the ACP registry could not be fetched (is curl installed, and the network up?)".to_string())?,
    };
    let registry: Registry = serde_json::from_slice(&bytes).map_err(|e| format!("the ACP registry: {e}"))?;
    let mut agents = registry.agents;
    agents.sort_by_key(|a| a.name.to_lowercase());
    Ok(agents)
}

/// This platform as the registry names it.
pub fn platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    format!("{os}-{arch}")
}

/// A word of the registry's as the shell takes it whole and as it is:
/// nothing in it expanded (`$`, a backquote).
fn quote(word: &str) -> String {
    let plain = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    match (plain, cfg!(windows)) {
        (true, _) => word.to_string(),
        (false, true) => format!("\"{}\"", word.replace('"', "")),
        (false, false) => format!("'{}'", word.replace('\'', "'\\''")),
    }
}

fn line(program: &str, args: &[String]) -> String {
    std::iter::once(quote(program)).chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ")
}

/// The program an archive's command names: `./dist-package/cursor-agent`
/// is `cursor-agent`.
fn program_name(cmd: &str) -> String {
    let name = Path::new(cmd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.trim_end_matches(".exe").to_string()
}

/// How `entry` runs here, `on_path` telling which programs are installed.
pub fn how(entry: &Entry, on_path: &dyn Fn(&str) -> bool) -> How {
    let binary = entry.distribution.binary.get(&platform());
    // Its own program, installed: the archive's command, or the package's bin.
    let installed = binary.map(|b| (program_name(&b.cmd), b.args.clone())).or_else(|| {
        let package = entry.distribution.npx.as_ref()?;
        Some((known_program(&entry.id)?.to_string(), package.args.clone()))
    });
    if let Some((program, args)) = installed.filter(|(p, _)| !p.is_empty() && on_path(p)) {
        return How::Installed(line(&program, &args));
    }
    if let Some(npx) = &entry.distribution.npx {
        if on_path("npx") {
            return How::Package(line("npx", &[vec!["-y".to_string(), npx.package.clone()], npx.args.clone()].concat()));
        }
    }
    if let Some(uvx) = &entry.distribution.uvx {
        if on_path("uvx") {
            return How::Package(line("uvx", &[vec![uvx.package.clone()], uvx.args.clone()].concat()));
        }
    }
    match binary {
        Some(b) if unpackable(&b.archive) => How::Download,
        Some(_) => How::Not("its archive is of a kind the app does not unpack".into()),
        None if entry.distribution.npx.is_some() => How::Not("it needs Node.js (npx)".into()),
        None if entry.distribution.uvx.is_some() => How::Not("it needs uv (uvx)".into()),
        None => How::Not(format!("no build for {}", platform())),
    }
}

/// The program an npm package installs, for agents known by it.
fn known_program(id: &str) -> Option<&'static str> {
    Some(match id {
        "gemini" => "gemini",
        "kimi-code" => "kimi",
        "qwen-code" => "qwen",
        "github-copilot-cli" => "copilot",
        "codex-acp" => "codex-acp",
        "claude-acp" => "claude-agent-acp",
        _ => return None,
    })
}

/// What kind of agent a profile for it is.
pub fn kind_of(entry: &Entry) -> Kind {
    match entry.id.as_str() {
        "claude-acp" => Kind::Claude,
        "codex-acp" => Kind::Codex,
        "kimi-code" => Kind::Kimi,
        id if id.starts_with("opencode") => Kind::OpenCode,
        _ => Kind::Generic,
    }
}

/// Flags the app adds for an agent: Claude Code's adapter never on a
/// claude.ai plan (see docs/agents.md).
pub fn adjust(entry: &Entry, command: String) -> String {
    if entry.id == "claude-acp" && !command.contains("--hide-claude-auth") {
        return format!("{command} --hide-claude-auth");
    }
    command
}

fn unpackable(archive: &str) -> bool {
    let name = archive_name(archive);
    name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".zip") || name.ends_with(".exe") || !name.contains('.')
}

fn archive_name(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or_default().to_lowercase()
}

/// Where an agent's archive is unpacked.
pub fn install_dir(entry: &Entry) -> PathBuf {
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect::<String>();
    crate::state::data_dir().join("acp-agents").join(format!("{}-{}", safe(&entry.id), safe(&entry.version)))
}

/// Downloads the archive for this platform, checks it, unpacks it, and
/// returns the command that runs the agent. Not for the UI thread.
pub fn install(entry: &Entry) -> Result<String, String> {
    let binary = entry.distribution.binary.get(&platform()).ok_or_else(|| format!("no build for {}", platform()))?;
    // What runs is in its own folder: no path out of it, and nothing run
    // that the registry gave no checksum for.
    let cmd = binary.cmd.trim_start_matches("./");
    if Path::new(cmd).is_absolute() || cmd.contains(':') || cmd.split(['/', '\\']).any(|p| p == "..") || cmd.is_empty() {
        return Err(format!("the registry's command {} is not in the agent's folder", binary.cmd));
    }
    if binary.sha256.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err("the registry gives no checksum for this download: not installed".into());
    }
    let dir = install_dir(entry);
    let program = dir.join(cmd);
    if !program.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let download = dir.with_extension("download");
        let got = crate::procs::command("curl")
            .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--max-time", "900", "--max-filesize", "1000000000", "-o"])
            .arg(&download)
            .arg(&binary.archive)
            .status()
            .map_err(|e| format!("curl: {e}"))?;
        if !got.success() {
            return Err(format!("could not download {}", binary.archive));
        }
        let result = check_and_unpack(binary, &download, &dir, &program);
        let _ = std::fs::remove_file(&download);
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755));
    }
    Ok(line(&program.display().to_string(), &binary.args))
}

fn check_and_unpack(binary: &Binary, download: &Path, dir: &Path, program: &Path) -> Result<(), String> {
    if let Some(want) = &binary.sha256 {
        use sha2::Digest as _;
        let bytes = std::fs::read(download).map_err(|e| e.to_string())?;
        let got = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
        if !got.eq_ignore_ascii_case(want.trim()) {
            return Err("the download does not match the registry's checksum".into());
        }
    }
    let name = archive_name(&binary.archive);
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let file = std::fs::File::open(download).map_err(|e| e.to_string())?;
        tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(dir).map_err(|e| format!("unpacking: {e}"))?;
    } else if name.ends_with(".zip") {
        let unzip = if cfg!(target_os = "linux") {
            crate::procs::command("unzip").arg("-q").arg(download).arg("-d").arg(dir).status()
        } else {
            crate::procs::command("tar").arg("-xf").arg(download).arg("-C").arg(dir).status()
        };
        if !unzip.map_err(|e| format!("unzip: {e}"))?.success() {
            return Err("could not unpack the zip archive (is unzip installed?)".into());
        }
    } else {
        // The program itself.
        if let Some(parent) = program.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(download, program).map_err(|e| e.to_string())?;
    }
    if !program.is_file() {
        return Err(format!("{} is not in the archive", binary.cmd));
    }
    // A link in the archive does not lead out of the folder either.
    let (real, home) = (program.canonicalize().map_err(|e| e.to_string())?, dir.canonicalize().map_err(|e| e.to_string())?);
    if !real.starts_with(&home) {
        return Err(format!("{} leads out of the agent's folder", binary.cmd));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(json: &str) -> Entry {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn an_agent_runs_as_installed_else_by_its_package_else_downloaded() {
        let gemini = entry(r#"{"id":"gemini","name":"Gemini CLI","version":"0.68.0","distribution":{"npx":{"package":"@google/gemini-cli@0.68.0","args":["--acp"]}}}"#);
        assert_eq!(how(&gemini, &|p| p == "gemini" || p == "npx"), How::Installed("gemini --acp".into()));
        assert_eq!(how(&gemini, &|p| p == "npx"), How::Package("npx -y @google/gemini-cli@0.68.0 --acp".into()));
        assert!(matches!(how(&gemini, &|_| false), How::Not(_)));
        let plat = platform();
        let goose = entry(&format!(r#"{{"id":"goose","name":"goose","version":"1.0","distribution":{{"binary":{{"{plat}":{{"archive":"https://x/goose.tar.gz","cmd":"./goose","args":["acp"]}}}}}}}}"#));
        assert_eq!(how(&goose, &|p| p == "goose"), How::Installed("goose acp".into()));
        assert_eq!(how(&goose, &|_| false), How::Download);
        let claude = entry(r#"{"id":"claude-acp","name":"Claude Agent","version":"1","distribution":{"npx":{"package":"@agentclientprotocol/claude-agent-acp@0.108.0"}}}"#);
        let How::Package(command) = how(&claude, &|p| p == "npx") else { panic!() };
        assert_eq!(adjust(&claude, command), "npx -y @agentclientprotocol/claude-agent-acp@0.108.0 --hide-claude-auth");
        assert_eq!(kind_of(&claude), Kind::Claude);
    }

    /// The registry as published parses, every agent in it.
    #[test]
    fn the_registry_format_is_read() {
        let sample = r#"{"version":"1.0.0","agents":[{"id":"amp-acp","name":"Amp","version":"0.13.0","description":"d","distribution":{"binary":{"linux-x86_64":{"archive":"https://x/a.tar.gz","cmd":"./amp-acp","sha256":"ab"}}}},{"id":"fast-agent","name":"fast-agent","version":"1","distribution":{"uvx":{"package":"fast-agent-acp==1","args":["acp"],"env":{"A":"b"}}}}],"extensions":[]}"#;
        let registry: Registry = serde_json::from_str(sample).unwrap();
        assert_eq!(registry.agents.len(), 2);
        assert_eq!(registry.agents[1].distribution.uvx.as_ref().unwrap().args, ["acp"]);
    }
}
