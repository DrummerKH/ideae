//! Language servers ideae installs itself when the user asks, each in its own
//! directory under `~/.local/share/ideae/lsp/` (`%LOCALAPPDATA%\ideae\lsp` on
//! Windows), shared by the release and dev builds. A server on PATH always
//! wins over these.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use anyhow::{Context as _, Result, anyhow, bail};

/// What the confirmation tells the user, and what must be on PATH first.
pub struct Recipe {
    pub summary: &'static str,
    pub needs: &'static [&'static str],
}

pub fn recipe(program: &str) -> Option<Recipe> {
    let (summary, needs): (&str, &[&str]) = match program {
        "gopls" => ("Builds the latest gopls with `go install`.", &["go"]),
        "rust-analyzer" if cfg!(windows) => ("Downloads the latest rust-analyzer release from GitHub, about 16 MB.", &["curl", "tar"]),
        "rust-analyzer" => ("Downloads the latest rust-analyzer release from GitHub, about 16 MB.", &["curl", "gzip"]),
        "typescript-language-server" => {
            ("Installs typescript-language-server and typescript with npm, about 30 MB.", &["npm"])
        }
        "intelephense" => ("Installs intelephense with npm, about 20 MB.", &["npm"]),
        "kotlin-lsp" if cfg!(any(windows, target_os = "macos")) => return None,
        "kotlin-lsp" => (
            "Downloads the latest JetBrains Kotlin LSP release: about 370 MB, 1.2 GB unpacked, Java included. The download is checked against its published SHA-256.",
            &["curl", "tar", "sha256sum"],
        ),
        _ => return None,
    };
    Some(Recipe { summary, needs })
}

/// The first tool the recipe needs that is not on PATH.
pub fn missing_tool(recipe: &Recipe) -> Option<&'static str> {
    recipe.needs.iter().copied().find(|tool| !crate::lsp::on_path(tool))
}

pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("IDEAE_LSP_DIR") {
        return PathBuf::from(dir);
    }
    crate::state::data_home().join("ideae").join("lsp")
}

fn executable(program: &str) -> Option<&'static str> {
    Some(match program {
        "gopls" if cfg!(windows) => "bin/gopls.exe",
        "rust-analyzer" if cfg!(windows) => "bin/rust-analyzer.exe",
        "typescript-language-server" if cfg!(windows) => "node_modules/.bin/typescript-language-server.cmd",
        "intelephense" if cfg!(windows) => "node_modules/.bin/intelephense.cmd",
        "gopls" => "bin/gopls",
        "rust-analyzer" => "bin/rust-analyzer",
        "typescript-language-server" => "node_modules/.bin/typescript-language-server",
        "intelephense" => "node_modules/.bin/intelephense",
        "kotlin-lsp" => "bin/intellij-server",
        _ => return None,
    })
}

/// The server's executable, when ideae has installed it.
pub fn installed(program: &str) -> Option<PathBuf> {
    let path = root().join(program).join(executable(program)?);
    #[cfg(unix)]
    let runnable = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let runnable = path.is_file();
    runnable.then_some(path)
}

/// `initializationOptions` an installed copy needs: typescript-language-server
/// runs the TypeScript installed beside it when the project has none.
pub fn init_options(program: &str, command: &Path) -> serde_json::Value {
    let ours = root().join(program);
    if program != "typescript-language-server" || !command.starts_with(&ours) {
        return serde_json::Value::Null;
    }
    let tsserver = ours.join("node_modules/typescript/lib/tsserver.js");
    serde_json::json!({ "tsserver": { "fallbackPath": tsserver } })
}

static INSTALLING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn is_installing(program: &str) -> bool {
    INSTALLING.lock().unwrap().as_ref().is_some_and(|s| s.contains(program))
}

/// Installs `program`, replacing an earlier copy only once the new one is
/// complete. Blocks for as long as the download or build takes.
pub fn install(program: &str) -> Result<PathBuf> {
    if !INSTALLING.lock().unwrap().get_or_insert_with(HashSet::new).insert(program.to_string()) {
        bail!("{program} is already being installed");
    }
    let result = install_inner(program);
    INSTALLING.lock().unwrap().as_mut().unwrap().remove(program);
    result
}

fn install_inner(program: &str) -> Result<PathBuf> {
    let recipe = recipe(program).ok_or_else(|| anyhow!("ideae cannot install {program}"))?;
    if let Some(tool) = missing_tool(&recipe) {
        bail!("{tool} is not on PATH");
    }
    let root = root();
    let staging = root.join(format!(".{program}.partial"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let built = build(program, &staging);
    let built = match built {
        Ok(dir) => dir,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let target = root.join(program);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&built, &target).with_context(|| format!("moving into {}", target.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    installed(program).ok_or_else(|| anyhow!("{program} installed, but {} is missing", executable(program).unwrap_or_default()))
}

/// Fills `staging` and returns the directory to become the server's own:
/// `staging` itself, or the folder an archive unpacked into.
fn build(program: &str, staging: &Path) -> Result<PathBuf> {
    match program {
        "gopls" => {
            run(crate::procs::command("go").args(["install", "golang.org/x/tools/gopls@latest"]).env("GOBIN", staging.join("bin")))?;
        }
        // TypeScript 7 is native and has no tsserver.js for the server to run.
        "typescript-language-server" => npm(staging, &["typescript-language-server", "typescript@6"])?,
        "intelephense" => npm(staging, &["intelephense"])?,
        "rust-analyzer" => {
            let arch = match std::env::consts::ARCH {
                "x86_64" | "aarch64" => std::env::consts::ARCH,
                other => bail!("no rust-analyzer build for {other}"),
            };
            let bin = staging.join("bin");
            std::fs::create_dir_all(&bin)?;
            let latest = "https://github.com/rust-lang/rust-analyzer/releases/latest/download";
            #[cfg(windows)]
            {
                // Windows' own tar.exe unpacks zip archives.
                let zip = bin.join("rust-analyzer.zip");
                download(&format!("{latest}/rust-analyzer-{arch}-pc-windows-msvc.zip"), &zip)?;
                run(crate::procs::command("tar").arg("-xf").arg(&zip).arg("-C").arg(&bin))?;
                std::fs::remove_file(&zip)?;
            }
            #[cfg(unix)]
            {
                let gz = bin.join("rust-analyzer.gz");
                let system = if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-gnu" };
                download(&format!("{latest}/rust-analyzer-{arch}-{system}.gz"), &gz)?;
                run(crate::procs::command("gzip").arg("-d").arg(&gz))?;
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(bin.join("rust-analyzer"), std::fs::Permissions::from_mode(0o755))?;
            }
        }
        "kotlin-lsp" => {
            let suffix = match std::env::consts::ARCH {
                "x86_64" => "",
                "aarch64" => "-aarch64",
                other => bail!("no Kotlin LSP build for {other}"),
            };
            let release = fetch("https://api.github.com/repos/Kotlin/kotlin-lsp/releases/latest")?;
            let release: serde_json::Value = serde_json::from_slice(&release).context("reading the latest release")?;
            let tag = release["tag_name"].as_str().ok_or_else(|| anyhow!("no tag in the latest release"))?;
            let version = kotlin_version(tag).ok_or_else(|| anyhow!("unexpected release tag {tag}"))?;
            let name = format!("kotlin-server-{version}{suffix}.tar.gz");
            let url = format!("https://download.jetbrains.com/language-server/kotlin-server/{version}/{name}");
            let archive = staging.join(&name);
            download(&url, &archive)?;
            let published = fetch(&format!("{url}.sha256"))?;
            let expected = String::from_utf8_lossy(&published).split_whitespace().next().unwrap_or_default().to_lowercase();
            let actual = sha256(&archive)?;
            if expected.len() != 64 || expected != actual {
                bail!("{name}: SHA-256 {actual} does not match the published {expected}");
            }
            run(crate::procs::command("tar").arg("xzf").arg(&archive).arg("-C").arg(staging))?;
            std::fs::remove_file(&archive)?;
            let unpacked = std::fs::read_dir(staging)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.is_dir() && p.join("bin/intellij-server").is_file())
                .ok_or_else(|| anyhow!("{name} holds no bin/intellij-server"))?;
            return Ok(unpacked);
        }
        _ => bail!("ideae cannot install {program}"),
    }
    Ok(staging.to_path_buf())
}

/// `kotlin-lsp/v271.2085.0` → `271.2085.0`.
fn kotlin_version(tag: &str) -> Option<&str> {
    let version = tag.rsplit('/').next()?.trim_start_matches('v');
    (!version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.')).then_some(version)
}

fn npm(prefix: &Path, packages: &[&str]) -> Result<()> {
    run(crate::procs::command("npm")
        .args(["install", "--no-fund", "--no-audit", "--no-save", "--prefix"])
        .arg(prefix)
        .args(packages))
}

fn download(url: &str, to: &Path) -> Result<()> {
    run(crate::procs::command("curl").args(["-fsSL", "--retry", "2", "-o"]).arg(to).arg(url))
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let out = crate::procs::command("curl").args(["-fsSL", "--retry", "2", url]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("{url}: {}", last_line(&out.stderr));
    }
    Ok(out.stdout)
}

fn sha256(path: &Path) -> Result<String> {
    let out = crate::procs::command("sha256sum").arg(path).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("sha256sum: {}", last_line(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_lowercase())
}

/// Runs a step, failing with the last line it printed.
fn run(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let out = command.stdin(Stdio::null()).output().with_context(|| format!("starting {program}"))?;
    if !out.status.success() {
        let said = if out.stderr.trim_ascii().is_empty() { &out.stdout } else { &out.stderr };
        bail!("{program}: {}", last_line(said));
    }
    Ok(())
}

fn last_line(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kotlin_release_tags() {
        assert_eq!(kotlin_version("kotlin-lsp/v271.2085.0"), Some("271.2085.0"));
        assert_eq!(kotlin_version("v0.253.10629"), Some("0.253.10629"));
        assert_eq!(kotlin_version("kotlin-lsp/v1.0; rm"), None);
    }

    #[test]
    fn every_server_has_a_recipe_and_an_executable() {
        for program in ["gopls", "rust-analyzer", "typescript-language-server", "intelephense", "kotlin-lsp"] {
            if cfg!(windows) && program == "kotlin-lsp" {
                continue;
            }
            assert!(recipe(program).is_some() && executable(program).is_some(), "{program}");
        }
    }

    /// Installs into a scratch IDEAE_LSP_DIR; needs network.
    fn installs(program: &str) {
        let dir = std::env::temp_dir().join(format!("ideae-lsp-install-{}-{program}", std::process::id()));
        // SAFETY: the ignored install tests run one at a time (--test-threads=1).
        unsafe { std::env::set_var("IDEAE_LSP_DIR", &dir) };
        let path = install(program).unwrap();
        assert_eq!(installed(program), Some(path));
        assert!(!dir.join(format!(".{program}.partial")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Needs npm: `cargo test lsp_install -- --ignored --test-threads=1`.
    #[test]
    #[ignore]
    fn installs_intelephense_with_npm() {
        installs("intelephense");
    }

    /// Downloads about 370 MB.
    #[test]
    #[ignore]
    fn installs_kotlin_lsp_from_jetbrains() {
        installs("kotlin-lsp");
    }
}
