//! Keys the app keeps for agents (an API key in a profile's environment):
//! not in `state.json`, which gets copied into backups, dotfiles and bug
//! reports, but in a file of their own next to it, readable by its owner
//! only, each value sealed with a key made from this machine's id. A copy
//! of the file elsewhere is of no use; on this machine it is no vault (any
//! program of the user could do what the app does), which no store is
//! against an agent that is handed the key anyway.
//!
//! No keyring: asking it may pop up its password dialog.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::{Deserialize, Serialize};

const INFO: &[u8] = b"ideae secrets v1";

#[derive(Serialize, Deserialize, Default)]
struct File {
    v: u32,
    /// Random, made once: with the machine's id, what the key is made of.
    salt: String,
    /// Scope ("profile/VARIABLE") to nonce and sealed value, in base64.
    items: BTreeMap<String, String>,
}

struct Store {
    salt: Vec<u8>,
    values: BTreeMap<String, String>,
    /// Sealed values this run cannot open (another HOME or user): kept as
    /// they are, written back, not lost.
    unread: BTreeMap<String, String>,
}

fn path() -> std::path::PathBuf {
    crate::state::state_dir().join("secrets.json")
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// What only this machine and this user have: the system's machine id.
fn machine() -> Vec<u8> {
    let mut id = Vec::new();
    #[cfg(target_os = "linux")]
    for at in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read(at) {
            id = text.trim_ascii().to_vec();
            break;
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = crate::procs::command("ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().find(|l| l.contains("IOPlatformUUID")) {
            id = line.rsplit('"').nth(1).unwrap_or_default().as_bytes().to_vec();
        }
    }
    #[cfg(windows)]
    if let Ok(out) = crate::procs::command("reg").args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(guid) = text.lines().find(|l| l.contains("MachineGuid")).and_then(|l| l.split_whitespace().last()) {
            id = guid.as_bytes().to_vec();
        }
    }
    if id.is_empty() {
        id = b"ideae".to_vec();
    }
    #[cfg(unix)]
    id.extend_from_slice(&unsafe { libc::getuid() }.to_le_bytes());
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        id.extend_from_slice(home.to_string_lossy().as_bytes());
    }
    id
}

fn key(salt: &[u8]) -> LessSafeKey {
    struct Len;
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            32
        }
    }
    let mut bytes = [0u8; 32];
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
        .extract(&machine())
        .expand(&[INFO], Len)
        .and_then(|okm| okm.fill(&mut bytes))
        .expect("32 bytes of HKDF-SHA256");
    LessSafeKey::new(UnboundKey::new(&aead::CHACHA20_POLY1305, &bytes).expect("a 32-byte key"))
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(load()))
}

fn load() -> Store {
    let file: File = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let salt = b64().decode(&file.salt).ok().filter(|s| s.len() >= 16).unwrap_or_else(|| {
        let mut salt = vec![0u8; 32];
        let _ = SystemRandom::new().fill(&mut salt);
        salt
    });
    let key = key(&salt);
    let open = |scope: &str, sealed: &str| {
        let mut bytes = b64().decode(sealed).ok()?;
        if bytes.len() < 12 {
            return None;
        }
        let mut body = bytes.split_off(12);
        let nonce = Nonce::try_assume_unique_for_key(&bytes).ok()?;
        let plain = key.open_in_place(nonce, Aad::from(scope.as_bytes()), &mut body).ok()?;
        String::from_utf8(plain.to_vec()).ok()
    };
    let (mut values, mut unread) = (BTreeMap::new(), BTreeMap::new());
    for (scope, sealed) in file.items {
        match open(&scope, &sealed) {
            Some(value) => {
                values.insert(scope, value);
            }
            None => {
                unread.insert(scope, sealed);
            }
        }
    }
    Store { salt, values, unread }
}

/// Reads the file off the thread that will ask for a value first.
pub fn preload() {
    std::thread::spawn(|| {
        let _ = store();
    });
}

pub fn get(scope: &str) -> Option<String> {
    store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).values.get(scope).cloned()
}

pub fn set(scope: &str, value: &str) {
    let mut store = store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.values.get(scope).map(String::as_str) == Some(value) {
        return;
    }
    store.values.insert(scope.to_string(), value.to_string());
    store.unread.remove(scope);
    drop(store);
    write_later();
}

pub fn remove(scope: &str) {
    let mut store = store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.values.remove(scope).is_none() & store.unread.remove(scope).is_none() {
        return;
    }
    drop(store);
    write_later();
}

/// The scopes kept, for dropping those of profiles that are gone.
pub fn scopes() -> Vec<String> {
    store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).values.keys().cloned().collect()
}

static WRITING: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Written off the UI thread; [`flush`] waits for it.
fn write_later() {
    let handle = std::thread::spawn(|| {
        static ONE: Mutex<()> = Mutex::new(());
        let _one = ONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = write() {
            eprintln!("saving the agents' keys: {e}");
        }
    });
    WRITING.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(handle);
}

/// On disk when it returns: the app quits next.
pub fn flush() {
    let pending: Vec<_> = std::mem::take(&mut *WRITING.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    for handle in pending {
        let _ = handle.join();
    }
}

fn write() -> std::io::Result<()> {
    let (salt, values, unread) = {
        let store = store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (store.salt.clone(), store.values.clone(), store.unread.clone())
    };
    let key = key(&salt);
    let random = SystemRandom::new();
    let mut items = BTreeMap::new();
    for (scope, value) in values {
        let mut nonce = [0u8; 12];
        random.fill(&mut nonce).map_err(|_| std::io::Error::other("no randomness"))?;
        let mut body = value.into_bytes();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(scope.as_bytes()), &mut body)
            .map_err(|_| std::io::Error::other("sealing failed"))?;
        items.insert(scope, b64().encode([nonce.as_slice(), &body].concat()));
    }
    for (scope, sealed) in unread {
        items.entry(scope).or_insert(sealed);
    }
    let file = File { v: 1, salt: b64().encode(&salt), items };
    let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        use std::io::Write as _;
        options.open(&tmp)?.write_all(&json)?;
    }
    std::fs::rename(&tmp, &path)
}

/// A variable whose value is a secret by its name: an API key, a token.
pub fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"].iter().any(|word| name.contains(word))
}

/// A value the shell makes when the agent starts: `$(op read …)`.
pub use ideae_core::env::command_value;

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    #[test]
    fn a_value_comes_back_sealed_and_only_here() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: set before the store is read, under the lock the tests share.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        let salt = vec![7u8; 32];
        {
            let mut store = super::store().lock().unwrap();
            store.salt = salt.clone();
            store.values.insert("p/ANTHROPIC_API_KEY".into(), "sk-test-123".into());
        }
        super::write().unwrap();
        let text = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!text.contains("sk-test-123"), "{text}");
        let again = super::load();
        assert_eq!(again.values.get("p/ANTHROPIC_API_KEY").map(String::as_str), Some("sk-test-123"));
        let mut file: super::File = serde_json::from_str(&text).unwrap();
        file.salt = super::b64().encode([1u8; 32]);
        std::fs::write(dir.join("secrets.json"), serde_json::to_vec(&file).unwrap()).unwrap();
        let unopened = super::load();
        assert!(unopened.values.is_empty(), "a wrong key opens nothing");
        // Not lost either: written back as it was, for the run that can open it.
        let sealed = file.items.get("p/ANTHROPIC_API_KEY").cloned();
        assert_eq!(unopened.unread.get("p/ANTHROPIC_API_KEY"), sealed.as_ref());
        *super::store().lock().unwrap() = unopened;
        super::set("p/OTHER_KEY", "x");
        super::flush();
        let kept: super::File = serde_json::from_slice(&std::fs::read(dir.join("secrets.json")).unwrap()).unwrap();
        assert_eq!(kept.items.get("p/ANTHROPIC_API_KEY"), sealed.as_ref(), "kept sealed as it was");
        assert!(kept.items.contains_key("p/OTHER_KEY"));
        assert!(super::secret_name("ANTHROPIC_API_KEY") && super::secret_name("GH_TOKEN") && !super::secret_name("CLAUDE_CONFIG_DIR"));
        assert_eq!(super::command_value("$(op read op://x/y)"), Some("op read op://x/y"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A profile's key moves out of the state, shows masked, is kept when
    /// the mask is left, replaced when typed anew, and forgotten when its
    /// line goes; a value made by a command stays a command.
    #[test]
    fn a_profile_keeps_its_key_sealed() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ideae-secrets-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: under the lock the tests share.
        unsafe { std::env::set_var("IDEAE_STATE", dir.join("state.json")) };
        let mut p = crate::agents::Profile { id: "cc".into(), command: "claude".into(), ..Default::default() };
        p.env = [("ANTHROPIC_API_KEY".into(), "sk-test-1".into()), ("CLAUDE_CONFIG_DIR".into(), "/c".into()), ("GH_TOKEN".into(), "$(pass show gh)".into())].into();
        assert!(p.seal_secrets());
        let saved = serde_json::to_string(&p).unwrap();
        assert!(!saved.contains("sk-test-1") && saved.contains("ANTHROPIC_API_KEY"), "{saved}");
        let vars = p.env_vars();
        assert!(vars.contains(&("ANTHROPIC_API_KEY".into(), "sk-test-1".into())));
        assert!(vars.contains(&("GH_TOKEN".into(), "$(pass show gh)".into())), "a command stays one");
        let text = p.env_text();
        assert!(text.contains(&format!("ANTHROPIC_API_KEY={}", crate::agents::MASK)) && !text.contains("sk-test-1"), "{text}");

        p.set_env_text(&text);
        assert!(p.env_vars().contains(&("ANTHROPIC_API_KEY".into(), "sk-test-1".into())), "the mask left keeps it");
        p.set_env_text(&text.replace(crate::agents::MASK, "sk-test-2"));
        assert!(p.env_vars().contains(&("ANTHROPIC_API_KEY".into(), "sk-test-2".into())), "a new value replaces it");
        p.set_env_text("CLAUDE_CONFIG_DIR=/c");
        assert!(p.secrets.is_empty() && super::get("cc/ANTHROPIC_API_KEY").is_none(), "taken out, forgotten");
        let launch = crate::agents::Launch::new(crate::agents::Profile { env: [("GH_TOKEN".into(), "$(echo t)".into())].into(), ..p }, crate::agents::Start::New(None), None);
        // In a shell the command makes the value itself; Windows has no such line, and the app runs it.
        if cfg!(unix) {
            assert!(launch.command().starts_with("GH_TOKEN=\"$(echo t)\" "), "{}", launch.command());
        }
        assert!(!launch.env(std::path::Path::new("/w")).iter().any(|(k, _)| k == "GH_TOKEN"));
        assert_eq!(crate::agents::run_made(vec![("GH_TOKEN".into(), "$(echo t)".into())]), vec![("GH_TOKEN".to_string(), "t".to_string())]);
        super::flush();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
