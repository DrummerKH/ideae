//! What edit prediction downloads: llama.cpp's server and the models,
//! each pinned to a release or revision and checked by its SHA-256.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};

use super::prompt::Format;

pub const LLAMA_RELEASE: &str = "b12477";

/// A file to download: where from, how big, its digest.
pub struct Pinned {
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Model {
    Sweep15,
    Sweep05,
    Zeta21,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::Sweep15, Model::Sweep05, Model::Zeta21];

    pub fn id(self) -> &'static str {
        match self {
            Model::Sweep15 => "sweep-1.5b",
            Model::Sweep05 => "sweep-0.5b",
            Model::Zeta21 => "zeta-2.1",
        }
    }

    pub fn by_id(id: &str) -> Option<Model> {
        Model::ALL.into_iter().find(|m| m.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Model::Sweep15 => "Sweep 1.5B",
            Model::Sweep05 => "Sweep 0.5B",
            Model::Zeta21 => "Zeta 2.1",
        }
    }

    pub fn format(self) -> Format {
        match self {
            Model::Zeta21 => Format::Zeta,
            _ => Format::Sweep,
        }
    }

    /// MiB of video memory it needs on a graphics card, with its cache.
    pub fn vram_mib(self) -> u64 {
        match self {
            Model::Sweep15 => 2500,
            Model::Sweep05 => 1200,
            Model::Zeta21 => 7000,
        }
    }

    pub fn file(self) -> &'static str {
        match self {
            Model::Sweep15 => "sweep-next-edit-1.5b.q8_0.v2.gguf",
            Model::Sweep05 => "sweep-next-edit-0.5b.q8_0.gguf",
            Model::Zeta21 => "zeta-2.1.Q4_K_M.gguf",
        }
    }

    pub fn pinned(self) -> Pinned {
        match self {
            Model::Sweep15 => Pinned {
                url: "https://huggingface.co/sweepai/sweep-next-edit-1.5B/resolve/409016591c6c1a94f545f22328a85a3516118f34/sweep-next-edit-1.5b.q8_0.v2.gguf",
                size: 1_537_269_856,
                sha256: "1321ea5e5d7529e60f9770c6a0b3a965f89542d16cf4ae51bab267f6a88150da",
            },
            Model::Sweep05 => Pinned {
                url: "https://huggingface.co/sweepai/sweep-next-edit-0.5B/resolve/8175624004cec8bf0b550fba35d6ad92126e48ac/sweep-next-edit-0.5b.q8_0.gguf",
                size: 531_065_312,
                sha256: "2d2f5ca856765a086d4ee3d0dc4d30358e9d562e36480e0c754240e9cf45ed64",
            },
            Model::Zeta21 => Pinned {
                url: "https://huggingface.co/mradermacher/zeta-2.1-GGUF/resolve/8c2528af755e2a640232eb25e7991cd812e04b07/zeta-2.1.Q4_K_M.gguf",
                size: 5_071_410_080,
                sha256: "f031e71d3a467f23457964f7168b9158189a91231efc28cc8b91e6ea473dd71b",
            },
        }
    }
}

/// llama.cpp's build for this system: the Vulkan one where there is one,
/// which falls back to the CPU by itself (its backends are loaded at run
/// time), Metal on a Mac.
pub fn runtime() -> Option<Pinned> {
    macro_rules! asset {
        ($name:literal, $size:literal, $sha:literal) => {
            Some(Pinned { url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b12477/", $name), size: $size, sha256: $sha })
        };
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => asset!("llama-b12477-bin-ubuntu-vulkan-x64.tar.gz", 36_431_246, "b6f9e4dbdebaa1d196111fc01b19f9f2447ba991004908f65c5f1431cfa0426d"),
        ("linux", "aarch64") => asset!("llama-b12477-bin-ubuntu-vulkan-arm64.tar.gz", 29_667_833, "fbc39620bbe3b528bca50895215d5372ff675ef663c9e276ffa65bf693427989"),
        ("windows", "x86_64") => asset!("llama-b12477-bin-win-vulkan-x64.zip", 38_304_567, "846c47a25198dea5721056867cd047013a2d84b60ab844daa65f74076ac4e22f"),
        ("windows", "aarch64") => asset!("llama-b12477-bin-win-vulkan-arm64.zip", 26_228_952, "6421d6ab8e9e222827dc09d3804416ee484dd967a03fd584d5d519ff825af812"),
        ("macos", "aarch64") => asset!("llama-b12477-bin-macos-arm64.tar.gz", 17_019_634, "534fb4bff92282b25b065d7622c6383f8852a3b36259de376583d093cbfd902d"),
        ("macos", "x86_64") => asset!("llama-b12477-bin-macos-x64.tar.gz", 12_774_878, "28284f47e95d92a8a42a6b242a5404c3c1abb7d5a7898e85c0edfccce6e35a2b"),
        _ => None,
    }
}

/// Where the runtime and the models are kept: by the language servers,
/// per app (ideae, ideae-dev).
pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("IDEAE_PREDICT_DIR") {
        return PathBuf::from(dir);
    }
    crate::state::data_dir().join("predict")
}

const SERVER: &str = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };

/// The server, when downloaded (or given by `IDEAE_PREDICT_SERVER`).
pub fn server() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("IDEAE_PREDICT_SERVER") {
        return Some(PathBuf::from(path));
    }
    let path = root().join(format!("llama-{LLAMA_RELEASE}")).join(SERVER);
    path.is_file().then_some(path)
}

/// The model's file, when downloaded and checked (or given by
/// `IDEAE_PREDICT_MODEL`, for tests).
pub fn model(model: Model) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("IDEAE_PREDICT_MODEL") {
        return Some(PathBuf::from(path));
    }
    let path = root().join("models").join(model.file());
    path.is_file().then_some(path)
}

/// Says how far a download is, and whether to go on.
pub trait Progress {
    fn progress(&mut self, done: u64, of: u64);
    fn stopped(&mut self) -> bool;
}

/// Downloads into `<to>.part`, going on from what an earlier try left,
/// checks the digest and only then gives the file its name.
pub fn download(pinned: &Pinned, to: &Path, progress: &mut dyn Progress) -> Result<()> {
    let part = to.with_extension("part");
    std::fs::create_dir_all(to.parent().context("no folder")?)?;
    if std::fs::metadata(&part).is_ok_and(|m| m.len() > pinned.size) {
        std::fs::remove_file(&part)?;
    }
    for attempt in 0..2 {
        if std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) < pinned.size {
            fetch(pinned, &part, progress)?;
        }
        if sha256(&part)? == pinned.sha256 {
            std::fs::rename(&part, to)?;
            return Ok(());
        }
        // A broken piece from an earlier try: once more from the start.
        std::fs::remove_file(&part)?;
        if attempt == 1 {
            break;
        }
    }
    bail!("{}: the download does not match its SHA-256", pinned.url.rsplit('/').next().unwrap_or_default())
}

fn fetch(pinned: &Pinned, part: &Path, progress: &mut dyn Progress) -> Result<()> {
    let mut child = super::server::tied(
        crate::procs::command("curl")
            .args(["-fsSL", "--retry", "3", "--connect-timeout", "20", "-C", "-", "-o"])
            .arg(part)
            .arg(pinned.url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped()),
    )
    .context("starting curl")?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if progress.stopped() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("stopped");
        }
        progress.progress(std::fs::metadata(part).map(|m| m.len()).unwrap_or(0), pinned.size);
        std::thread::sleep(Duration::from_millis(250));
    };
    super::server::untied(&child);
    if !status.success() {
        let mut said = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut said);
        }
        bail!("downloading {}: {}", pinned.url.rsplit('/').next().unwrap_or_default(), said.lines().last().unwrap_or("curl failed").trim());
    }
    progress.progress(pinned.size, pinned.size);
    Ok(())
}

pub fn sha256(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads and unpacks the runtime; only the server and its libraries
/// are kept.
pub fn install_runtime(progress: &mut dyn Progress) -> Result<PathBuf> {
    let pinned = runtime().ok_or_else(|| anyhow!("no llama.cpp build for {} {}", std::env::consts::OS, std::env::consts::ARCH))?;
    let root = root();
    let name = pinned.url.rsplit('/').next().unwrap_or("llama");
    let archive = root.join(name);
    download(&pinned, &archive, progress)?;
    let staging = root.join(format!(".llama-{LLAMA_RELEASE}.partial"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    if name.ends_with(".zip") {
        // Windows' own tar.exe unpacks zip archives.
        let out = crate::procs::command("tar").arg("-xf").arg(&archive).arg("-C").arg(&staging).output()?;
        if !out.status.success() {
            bail!("unpacking {name}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&archive)?));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if file == SERVER || file.starts_with("lib") || file == "LICENSE" {
                entry.unpack_in(&staging)?;
            }
        }
    }
    let unpacked = std::iter::once(staging.clone())
        .chain(std::fs::read_dir(&staging)?.filter_map(|e| e.ok()).map(|e| e.path()))
        .find(|dir| dir.join(SERVER).is_file())
        .ok_or_else(|| anyhow!("{name} holds no {SERVER}"))?;
    let target = root.join(format!("llama-{LLAMA_RELEASE}"));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&unpacked, &target)?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&archive);
    Ok(target.join(SERVER))
}

pub fn install_model(model: Model, progress: &mut dyn Progress) -> Result<PathBuf> {
    let to = root().join("models").join(model.file());
    download(&model.pinned(), &to, progress)?;
    Ok(to)
}
