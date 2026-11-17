//! Language servers ideae installs itself when the user asks, each in its own
//! directory under `~/.local/share/ideae/lsp/` (`%LOCALAPPDATA%\ideae\lsp` on
//! Windows), shared by the release and dev builds. A server on PATH always
//! wins over these. Some come only with their language (sourcekit-lsp with
//! Swift, `dart` with Dart) or need it to install (ruby-lsp, elixir-ls,
//! metals): those are used from PATH and have no recipe here.

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

/// Unpacks a zip: `unzip` on Linux; macOS's and Windows' own tar reads zips.
const UNZIP: &str = if cfg!(target_os = "linux") { "unzip" } else { "tar" };

pub fn recipe(program: &str) -> Option<Recipe> {
    let (summary, needs): (&str, &[&str]) = match program {
        "gopls" => ("Builds the latest gopls with `go install`.", &["go"]),
        "rust-analyzer" if cfg!(windows) => ("Downloads the latest rust-analyzer release from GitHub, about 16 MB.", &["curl", "tar"]),
        "rust-analyzer" => ("Downloads the latest rust-analyzer release from GitHub, about 16 MB.", &["curl", "gzip"]),
        "typescript-language-server" => {
            ("Installs typescript-language-server and typescript with npm, about 30 MB.", &["npm"])
        }
        "intelephense" => ("Installs intelephense with npm, about 20 MB.", &["npm"]),
        "kotlin-lsp" => (
            "Downloads the latest JetBrains Kotlin LSP release: about 370 MB, 1.2 GB unpacked, Java included. The download is checked against its published SHA-256.",
            &["curl", "tar"],
        ),
        "basedpyright-langserver" => ("Installs basedpyright with npm, about 45 MB.", &["npm"]),
        // No Linux build for ARM.
        "clangd" if cfg!(target_os = "linux") && std::env::consts::ARCH != "x86_64" => return None,
        "clangd" => ("Downloads the latest clangd release from GitHub: about 120 MB, 230 MB unpacked (30 MB and 60 MB on Windows).", &["curl", UNZIP]),
        "jdtls" => (
            "Downloads the latest Eclipse JDT Language Server, about 50 MB. It runs on the Java on PATH, 21 or newer. The download is checked against its published SHA-256.",
            &["curl", "tar", "java"],
        ),
        "csharp-ls" => ("Installs csharp-ls with `dotnet tool install`, about 100 MB.", &["dotnet"]),
        "lua-language-server" => ("Downloads the latest LuaLS release from GitHub: about 3 MB, 22 MB unpacked.", &["curl", "tar"]),
        "bash-language-server" => ("Installs bash-language-server with npm, about 25 MB.", &["npm"]),
        // GNU tar leaves .xz to xz.
        "zls" if cfg!(target_os = "linux") => ("Downloads the latest zls release from GitHub, about 4 MB, 19 MB unpacked. It works with the Zig release of its version.", &["curl", "tar", "xz"]),
        "zls" => ("Downloads the latest zls release from GitHub, about 4 MB, 19 MB unpacked. It works with the Zig release of its version.", &["curl", "tar"]),
        "vscode-json-language-server" | "vscode-css-language-server" | "vscode-html-language-server" => (
            "Installs vscode-langservers-extracted with npm, about 95 MB: the JSON, CSS and HTML servers together.",
            &["npm"],
        ),
        "yaml-language-server" => ("Installs yaml-language-server with npm, about 20 MB.", &["npm"]),
        "taplo" if cfg!(windows) => ("Downloads the latest Taplo release from GitHub: about 5 MB, 13 MB unpacked.", &["curl", "tar"]),
        "taplo" => ("Downloads the latest Taplo release from GitHub: about 5 MB, 13 MB unpacked.", &["curl", "gzip"]),
        "marksman" if cfg!(windows) && std::env::consts::ARCH != "x86_64" => return None,
        "marksman" => ("Downloads the latest Marksman release from GitHub, about 25 MB.", &["curl"]),
        "docker-langserver" => ("Installs dockerfile-language-server-nodejs with npm, about 4 MB.", &["npm"]),
        "terraform-ls" => (
            "Downloads the latest terraform-ls release from HashiCorp: about 15 MB, 45 MB unpacked. The download is checked against its published SHA-256.",
            &["curl", UNZIP],
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

/// The directory a server is installed in, named after it or after the
/// package that brings it along with others.
pub fn home(program: &str) -> &str {
    match program {
        "vscode-json-language-server" | "vscode-css-language-server" | "vscode-html-language-server" => {
            "vscode-langservers-extracted"
        }
        _ => program,
    }
}

/// What npm installs for a server, when npm does.
fn npm_packages(program: &str) -> Option<&'static [&'static str]> {
    Some(match program {
        // TypeScript 7 is native and has no tsserver.js for the server to run.
        "typescript-language-server" => &["typescript-language-server", "typescript@6"],
        "intelephense" => &["intelephense"],
        "basedpyright-langserver" => &["basedpyright"],
        "bash-language-server" => &["bash-language-server"],
        "vscode-json-language-server" | "vscode-css-language-server" | "vscode-html-language-server" => {
            &["vscode-langservers-extracted"]
        }
        "yaml-language-server" => &["yaml-language-server"],
        "docker-langserver" => &["dockerfile-language-server-nodejs"],
        _ => return None,
    })
}

const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

fn executable(program: &str) -> Option<String> {
    Some(match program {
        _ if npm_packages(program).is_some() => format!("node_modules/.bin/{program}{}", if cfg!(windows) { ".cmd" } else { "" }),
        "kotlin-lsp" => format!("bin/intellij-server{EXE}"),
        "jdtls" => format!("bin/ideae-jdtls{}", if cfg!(windows) { ".cmd" } else { "" }),
        "gopls" | "rust-analyzer" | "clangd" | "csharp-ls" | "lua-language-server" | "zls" | "taplo" | "marksman" | "terraform-ls" => {
            format!("bin/{program}{EXE}")
        }
        _ => return None,
    })
}

/// The server's executable, when ideae has installed it.
pub fn installed(program: &str) -> Option<PathBuf> {
    let path = root().join(home(program)).join(executable(program)?);
    #[cfg(unix)]
    let runnable = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let runnable = path.is_file();
    runnable.then_some(path)
}

/// `initializationOptions` for the server: its own, and for a copy ideae
/// installed what that needs: typescript-language-server runs the
/// TypeScript installed beside it when the project has none.
pub fn init_options(program: &str, command: &Path) -> serde_json::Value {
    let mut options = crate::lsp::init_options(program);
    let ours = root().join(home(program));
    if program == "typescript-language-server" && command.starts_with(&ours) {
        let tsserver = ours.join("node_modules/typescript/lib/tsserver.js");
        options["tsserver"] = serde_json::json!({ "fallbackPath": tsserver });
    }
    options
}

static INSTALLING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn is_installing(program: &str) -> bool {
    INSTALLING.lock().unwrap().as_ref().is_some_and(|s| s.contains(home(program)))
}

/// Installs `program`, replacing an earlier copy only once the new one is
/// complete. Blocks for as long as the download or build takes.
pub fn install(program: &str) -> Result<PathBuf> {
    let home = home(program);
    if !INSTALLING.lock().unwrap().get_or_insert_with(HashSet::new).insert(home.to_string()) {
        bail!("{home} is already being installed");
    }
    let result = install_inner(program);
    INSTALLING.lock().unwrap().as_mut().unwrap().remove(home);
    result
}

fn install_inner(program: &str) -> Result<PathBuf> {
    let recipe = recipe(program).ok_or_else(|| anyhow!("ideae cannot install {program}"))?;
    if let Some(tool) = missing_tool(&recipe) {
        bail!("{tool} is not on PATH");
    }
    let root = root();
    let home = home(program);
    let staging = root.join(format!(".{home}.partial"));
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
    let target = root.join(home);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&built, &target).with_context(|| format!("moving into {}", target.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    installed(program).ok_or_else(|| anyhow!("{program} installed, but {} is missing", executable(program).unwrap_or_default()))
}

/// Fills `staging` and returns the directory to become the server's own:
/// `staging` itself, or the folder an archive unpacked into.
fn build(program: &str, staging: &Path) -> Result<PathBuf> {
    if let Some(packages) = npm_packages(program) {
        npm(staging, packages)?;
        return Ok(staging.to_path_buf());
    }
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let bin = staging.join("bin");
    std::fs::create_dir_all(&bin)?;
    match program {
        "gopls" => {
            run(crate::procs::command("go").args(["install", "golang.org/x/tools/gopls@latest"]).env("GOBIN", &bin))?;
        }
        "rust-analyzer" => {
            let arch = match arch {
                "x86_64" | "aarch64" => arch,
                other => bail!("no rust-analyzer build for {other}"),
            };
            let latest = "https://github.com/rust-lang/rust-analyzer/releases/latest/download";
            #[cfg(windows)]
            {
                let zip = bin.join("rust-analyzer.zip");
                download(&format!("{latest}/rust-analyzer-{arch}-pc-windows-msvc.zip"), &zip)?;
                unzip(&zip, &bin)?;
            }
            #[cfg(unix)]
            {
                let system = if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-gnu" };
                gunzip_from(&format!("{latest}/rust-analyzer-{arch}-{system}.gz"), &bin.join("rust-analyzer"))?;
            }
        }
        "kotlin-lsp" => {
            let suffix = match arch {
                "x86_64" => "",
                "aarch64" => "-aarch64",
                other => bail!("no Kotlin LSP build for {other}"),
            };
            let release = fetch("https://api.github.com/repos/Kotlin/kotlin-lsp/releases/latest")?;
            let release: serde_json::Value = serde_json::from_slice(&release).context("reading the latest release")?;
            let tag = release["tag_name"].as_str().ok_or_else(|| anyhow!("no tag in the latest release"))?;
            let version = kotlin_version(tag).ok_or_else(|| anyhow!("unexpected release tag {tag}"))?;
            // A tarball for Linux, a zip for macOS (.sit) and Windows; tar
            // (bsdtar there) unpacks those zips too.
            let name = if cfg!(windows) {
                format!("kotlin-server-{version}{suffix}.win.zip")
            } else if cfg!(target_os = "macos") {
                format!("kotlin-server-{version}{suffix}.sit")
            } else {
                format!("kotlin-server-{version}{suffix}.tar.gz")
            };
            let url = format!("https://download.jetbrains.com/language-server/kotlin-server/{version}/{name}");
            let archive = staging.join(&name);
            download(&url, &archive)?;
            check_sha256(&archive, &fetch(&format!("{url}.sha256"))?)?;
            let into = staging.join("unpacked");
            std::fs::create_dir_all(&into)?;
            run(crate::procs::command("tar").arg("-xf").arg(&archive).arg("-C").arg(&into))?;
            std::fs::remove_file(&archive)?;
            // In a folder of its own (Linux, macOS) or right at the top (Windows).
            return holding(&into, &executable(program).unwrap_or_default());
        }
        "clangd" => {
            let tag = latest_tag("clangd/clangd")?;
            let system = match os {
                "linux" => "linux",
                "macos" => "mac",
                "windows" => "windows",
                other => bail!("no clangd build for {other}"),
            };
            let zip = staging.join("clangd.zip");
            download(&format!("https://github.com/clangd/clangd/releases/download/{tag}/clangd-{system}-{tag}.zip"), &zip)?;
            let into = staging.join("unpacked");
            unzip(&zip, &into)?;
            // clangd_<version>/bin/clangd, its headers in lib/ beside.
            return holding(&into, &executable(program).unwrap_or_default());
        }
        "jdtls" => {
            let snapshots = "https://download.eclipse.org/jdtls/snapshots";
            let name = String::from_utf8(fetch(&format!("{snapshots}/latest.txt"))?)?.trim().to_string();
            if !name.starts_with("jdt-language-server-") || name.contains('/') {
                bail!("unexpected latest jdtls {name}");
            }
            let archive = staging.join(&name);
            download(&format!("{snapshots}/{name}"), &archive)?;
            check_sha256(&archive, &fetch(&format!("{snapshots}/{name}.sha256"))?)?;
            run(crate::procs::command("tar").arg("-xf").arg(&archive).arg("-C").arg(staging))?;
            std::fs::remove_file(&archive)?;
            write_jdtls_launcher(staging)?;
        }
        "csharp-ls" => {
            run(crate::procs::command("dotnet").args(["tool", "install", "csharp-ls", "--tool-path"]).arg(&bin))?;
        }
        "lua-language-server" => {
            let tag = latest_tag("LuaLS/lua-language-server")?;
            let arch = match arch {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                other => bail!("no LuaLS build for {other}"),
            };
            let name = match os {
                "linux" => format!("lua-language-server-{tag}-linux-{arch}.tar.gz"),
                "macos" => format!("lua-language-server-{tag}-darwin-{arch}.tar.gz"),
                "windows" if arch == "x64" => format!("lua-language-server-{tag}-win32-x64.zip"),
                other => bail!("no LuaLS build for {other} on {arch}"),
            };
            let archive = staging.join(&name);
            download(&format!("https://github.com/LuaLS/lua-language-server/releases/download/{tag}/{name}"), &archive)?;
            // bin/ and the Lua it runs, side by side at the top.
            run(crate::procs::command("tar").arg("-xf").arg(&archive).arg("-C").arg(staging))?;
            std::fs::remove_file(&archive)?;
        }
        "zls" => {
            let name = match (arch, os) {
                ("x86_64" | "aarch64", "linux" | "macos") => format!("zls-{arch}-{os}.tar.xz"),
                ("x86_64" | "aarch64", "windows") => format!("zls-{arch}-windows.zip"),
                _ => bail!("no zls build for {os} on {arch}"),
            };
            let archive = staging.join(&name);
            download(&format!("https://github.com/zigtools/zls/releases/latest/download/{name}"), &archive)?;
            run(crate::procs::command("tar").arg("-xf").arg(&archive).arg("-C").arg(&bin))?;
            std::fs::remove_file(&archive)?;
        }
        "taplo" => {
            let system = match os {
                "macos" => "darwin",
                other => other,
            };
            let latest = "https://github.com/tamasfe/taplo/releases/latest/download";
            if cfg!(windows) {
                let zip = staging.join("taplo.zip");
                download(&format!("{latest}/taplo-windows-{arch}.zip"), &zip)?;
                unzip(&zip, &bin)?;
            } else {
                gunzip_from(&format!("{latest}/taplo-{system}-{arch}.gz"), &bin.join("taplo"))?;
            }
        }
        "marksman" => {
            let name = match (os, arch) {
                ("linux", "x86_64") => "marksman-linux-x64",
                ("linux", "aarch64") => "marksman-linux-arm64",
                ("macos", _) => "marksman-macos",
                ("windows", "x86_64") => "marksman.exe",
                _ => bail!("no Marksman build for {os} on {arch}"),
            };
            let to = bin.join(format!("marksman{EXE}"));
            download(&format!("https://github.com/artempyanykh/marksman/releases/latest/download/{name}"), &to)?;
            make_executable(&to)?;
        }
        "terraform-ls" => {
            let check = fetch("https://checkpoint-api.hashicorp.com/v1/check/terraform-ls")?;
            let check: serde_json::Value = serde_json::from_slice(&check).context("reading the latest terraform-ls")?;
            let version = check["current_version"].as_str().unwrap_or_default();
            if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                bail!("unexpected terraform-ls version {version}");
            }
            let arch = match arch {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => bail!("no terraform-ls build for {other}"),
            };
            let system = match os {
                "macos" => "darwin",
                other => other,
            };
            let base = format!("https://releases.hashicorp.com/terraform-ls/{version}");
            let name = format!("terraform-ls_{version}_{system}_{arch}.zip");
            let zip = staging.join(&name);
            download(&format!("{base}/{name}"), &zip)?;
            let sums = fetch(&format!("{base}/terraform-ls_{version}_SHA256SUMS"))?;
            let sums = String::from_utf8_lossy(&sums);
            let expected = sums.lines().find(|l| l.ends_with(&format!(" {name}")) || l.ends_with(&format!("*{name}"))).unwrap_or_default();
            check_sha256(&zip, expected.as_bytes())?;
            unzip(&zip, &bin)?;
        }
        _ => bail!("ideae cannot install {program}"),
    }
    Ok(staging.to_path_buf())
}

/// The folder under `dir` holding `executable`: `dir` itself or one level down.
fn holding(dir: &Path, executable: &str) -> Result<PathBuf> {
    std::iter::once(dir.to_path_buf())
        .chain(std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path()))
        .find(|p| p.is_dir() && p.join(executable).is_file())
        .ok_or_else(|| anyhow!("the download holds no {executable}"))
}

/// jdtls comes with a Python launcher (and a .bat that waits for a key
/// after it). What it runs is one `java` command: written here, with the
/// launcher jar of this release.
fn write_jdtls_launcher(dir: &Path) -> Result<()> {
    let jar = std::fs::read_dir(dir.join("plugins"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar"))
        .ok_or_else(|| anyhow!("jdtls: no Equinox launcher in plugins"))?;
    let config = match std::env::consts::OS {
        "macos" => "config_mac",
        "windows" => "config_win",
        _ => "config_linux",
    };
    let java_args = "-Declipse.application=org.eclipse.jdt.ls.core.id1 -Dosgi.bundles.defaultStartLevel=4 \
        -Declipse.product=org.eclipse.jdt.ls.core.product -Dosgi.checkConfiguration=true -Dosgi.sharedConfiguration.area.readOnly=true \
        -Dosgi.configuration.cascaded=true -Djdk.xml.maxGeneralEntitySizeLimit=0 -Djdk.xml.totalEntitySizeLimit=0 -Xms256m \
        --add-modules=ALL-SYSTEM --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.lang=ALL-UNNAMED";
    let launcher = dir.join(executable("jdtls").unwrap_or_default());
    if cfg!(windows) {
        let script = format!(
            "@echo off\r\nset \"here=%~dp0..\"\r\njava -Dosgi.sharedConfiguration.area=\"%here%\\{config}\" {java_args} -jar \"%here%\\plugins\\{jar}\" %*\r\n"
        );
        std::fs::write(&launcher, script)?;
    } else {
        let script = format!(
            "#!/bin/sh\nhere=\"$(cd \"$(dirname \"$0\")/..\" && pwd)\"\nexec java -Dosgi.sharedConfiguration.area=\"$here/{config}\" {java_args} -jar \"$here/plugins/{jar}\" \"$@\"\n"
        );
        std::fs::write(&launcher, script)?;
        make_executable(&launcher)?;
    }
    Ok(())
}

/// The tag of a GitHub repository's latest release, checked before it
/// goes into a URL.
fn latest_tag(repo: &str) -> Result<String> {
    let release = fetch(&format!("https://api.github.com/repos/{repo}/releases/latest"))?;
    let release: serde_json::Value = serde_json::from_slice(&release).context("reading the latest release")?;
    let tag = release["tag_name"].as_str().unwrap_or_default();
    if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        bail!("unexpected release tag {tag}");
    }
    Ok(tag.to_string())
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

/// A single gzipped executable, to `to`.
fn gunzip_from(url: &str, to: &Path) -> Result<()> {
    let mut gz = to.as_os_str().to_os_string();
    gz.push(".gz");
    let gz = PathBuf::from(gz);
    download(url, &gz)?;
    run(crate::procs::command("gzip").arg("-d").arg(&gz))?;
    make_executable(to)
}

fn unzip(zip: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into)?;
    if cfg!(target_os = "linux") {
        run(crate::procs::command("unzip").args(["-q", "-o"]).arg(zip).arg("-d").arg(into))?;
    } else {
        run(crate::procs::command("tar").arg("-xf").arg(zip).arg("-C").arg(into))?;
    }
    std::fs::remove_file(zip)?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = path;
    Ok(())
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let out = crate::procs::command("curl").args(["-fsSL", "--retry", "2", url]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("{url}: {}", last_line(&out.stderr));
    }
    Ok(out.stdout)
}

/// Fails unless the file's SHA-256 is the one `published` begins with.
fn check_sha256(path: &Path, published: &[u8]) -> Result<()> {
    let expected = String::from_utf8_lossy(published).split_whitespace().next().unwrap_or_default().to_lowercase();
    let actual = sha256(path)?;
    if expected.len() != 64 || expected != actual {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        bail!("{name}: SHA-256 {actual} does not match the published {expected}");
    }
    Ok(())
}

/// Here rather than sha256sum, which macOS and Windows lack.
fn sha256(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
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

    /// Those that come with their language, or need it to be installed.
    const FROM_PATH: &[&str] = &["ruby-lsp", "sourcekit-lsp", "dart", "elixir-ls", "metals"];

    #[test]
    fn every_server_has_a_recipe_and_an_executable() {
        for program in crate::lsp::SERVERS.iter().map(|s| s.program) {
            if FROM_PATH.contains(&program) {
                assert!(recipe(program).is_none() && executable(program).is_none(), "{program}");
            } else {
                assert!(executable(program).is_some(), "{program}");
            }
            if recipe(program).is_some() {
                assert!(executable(program).is_some(), "{program}");
            }
        }
    }

    #[test]
    fn the_extracted_servers_share_one_install() {
        assert_eq!(home("vscode-css-language-server"), home("vscode-json-language-server"));
        assert_eq!(home("taplo"), "taplo");
    }

    /// Installs into a scratch IDEAE_LSP_DIR, removed after; needs network.
    /// Into the IDEAE_LSP_DIR given, kept, to try the servers after.
    fn installs(program: &str) {
        let given = std::env::var_os("IDEAE_LSP_DIR").map(PathBuf::from);
        let dir = given.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("ideae-lsp-install-{}-{program}", std::process::id())));
        // SAFETY: the ignored install tests run one at a time (--test-threads=1).
        unsafe { std::env::set_var("IDEAE_LSP_DIR", &dir) };
        let path = install(program).unwrap();
        assert_eq!(installed(program), Some(path));
        assert!(!dir.join(format!(".{}.partial", home(program))).exists());
        if given.is_none() {
            let _ = std::fs::remove_dir_all(&dir);
            // SAFETY: as above.
            unsafe { std::env::remove_var("IDEAE_LSP_DIR") };
        }
    }

    #[test]
    #[ignore]
    fn installs_servers_with_npm() {
        for program in ["basedpyright-langserver", "bash-language-server", "vscode-json-language-server", "yaml-language-server", "docker-langserver"] {
            installs(program);
        }
    }

    #[test]
    #[ignore]
    fn installs_csharp_ls_with_dotnet() {
        installs("csharp-ls");
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

    #[test]
    #[ignore]
    fn installs_clangd_from_github() {
        installs("clangd");
    }

    #[test]
    #[ignore]
    fn installs_jdtls_from_eclipse() {
        installs("jdtls");
    }

    #[test]
    #[ignore]
    fn installs_terraform_ls_from_hashicorp() {
        installs("terraform-ls");
    }

    #[test]
    #[ignore]
    fn installs_small_downloads_from_github() {
        for program in ["lua-language-server", "zls", "taplo", "marksman"] {
            installs(program);
        }
    }
}
