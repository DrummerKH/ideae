//! Integrations' tokens, yours, kept by host and account (two Jiras of two
//! companies never share one; two spaces of one Jira do). In the system's
//! keyring, asked only when a token is first used or given: the keyring may
//! show a dialog of its own, which nobody expects at the start. Where there
//! is no keyring (Linux without a desktop session), sealed for this machine
//! in the app's own store, and you are told that this is weaker.
//!
//! Names differ for ideae and ideae-dev (`APP_ID`): a debug build never
//! reads the release's tokens.

use std::collections::HashMap;
use std::sync::RwLock;

use gpui_kit::{App, Task};

/// Tokens read once this run, by their id: the background jobs that use
/// them ask no keyring and no window.
static READ: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// When a token was last given or taken away: a token refused before is
/// asked about again after.
static CHANGED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Whether a token was given or taken away since `at`.
pub fn changed_since(at: std::time::Instant) -> bool {
    CHANGED.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|changed| changed > at)
}

fn scope(id: &str) -> String {
    format!("token:{id}")
}

/// One read already, or none yet: no keyring asked.
pub fn known(id: &str) -> Option<String> {
    READ.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|all| all.get(id).cloned()).or_else(|| crate::secrets::get(&scope(id)))
}

fn changed() {
    *CHANGED.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
}

fn remember(id: &str, token: Option<&str>) {
    let mut all = READ.write().unwrap_or_else(|e| e.into_inner());
    let all = all.get_or_insert_with(HashMap::new);
    match token {
        Some(token) => all.insert(id.to_string(), token.to_string()),
        None => all.remove(id),
    };
}

/// Debug builds only: a token for every integration (tests, the headless driver).
fn stand_in() -> Option<String> {
    if cfg!(debug_assertions) { std::env::var("IDEAE_TOKEN").ok() } else { None }
}

#[cfg(target_os = "linux")]
mod service {
    use anyhow::Result;
    use oo7::Keyring;

    fn attributes(id: &str) -> [(&'static str, String); 3] {
        [("application", crate::state::APP_ID.to_string()), ("kind", "integration-token".to_string()), ("account", id.to_string())]
    }

    pub async fn read(id: &str) -> Result<Option<String>> {
        let keyring = Keyring::new().await?;
        keyring.unlock().await?;
        let Some(item) = keyring.search_items(&attributes(id)).await?.into_iter().next() else { return Ok(None) };
        item.unlock().await?;
        Ok(Some(String::from_utf8_lossy(item.secret().await?.as_bytes()).into_owned()))
    }

    pub async fn write(id: &str, token: &str) -> Result<()> {
        let keyring = Keyring::new().await?;
        keyring.unlock().await?;
        keyring.create_item(&format!("{}: {id}", crate::state::APP_ID), &attributes(id), token, true).await?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<()> {
        let keyring = Keyring::new().await?;
        keyring.unlock().await?;
        for item in keyring.search_items(&attributes(id)).await? {
            item.delete().await?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn url(id: &str) -> String {
    format!("{}://token/{id}", crate::state::APP_ID)
}

/// The token of `id`: kept already, else from the keyring.
pub fn read(id: &str, cx: &App) -> Task<Option<String>> {
    if let Some(token) = stand_in().or_else(|| known(id)) {
        return Task::ready(Some(token));
    }
    let id = id.to_string();
    #[cfg(target_os = "linux")]
    let found = cx.background_executor().spawn({
        let id = id.clone();
        async move { service::read(&id).await.ok().flatten() }
    });
    #[cfg(not(target_os = "linux"))]
    let found = {
        let read = cx.read_credentials(&url(&id));
        cx.background_executor().spawn(async move { read.await.ok().flatten().map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned()) })
    };
    cx.background_executor().spawn(async move {
        let token = found.await;
        remember(&id, token.as_deref());
        token
    })
}

/// Keeps a token: in the keyring, else sealed here. What it says when it
/// had to be the latter.
pub fn write(id: &str, token: &str, cx: &App) -> Task<Option<String>> {
    remember(id, Some(token));
    changed();
    if stand_in().is_some() {
        return Task::ready(None);
    }
    let (id, token) = (id.to_string(), token.to_string());
    #[cfg(target_os = "linux")]
    let kept = cx.background_executor().spawn({
        let (id, token) = (id.clone(), token.clone());
        async move { service::write(&id, &token).await }
    });
    #[cfg(not(target_os = "linux"))]
    let kept = cx.write_credentials(&url(&id), "token", token.as_bytes());
    cx.background_executor().spawn(async move {
        match kept.await {
            Ok(()) => {
                crate::secrets::remove(&scope(&id));
                None
            }
            Err(e) => {
                crate::secrets::set(&scope(&id), &token);
                Some(format!("No keyring answered ({e}): the token is sealed in the app's own store instead, readable by anything that runs as you."))
            }
        }
    })
}

pub fn delete(id: &str, cx: &App) -> Task<()> {
    remember(id, None);
    changed();
    crate::secrets::remove(&scope(id));
    if stand_in().is_some() {
        return Task::ready(());
    }
    #[cfg(target_os = "linux")]
    {
        let id = id.to_string();
        cx.background_executor().spawn(async move {
            let _ = service::delete(&id).await;
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let gone = cx.delete_credentials(&url(id));
        cx.background_executor().spawn(async move {
            let _ = gone.await;
        })
    }
}

/// Whether a scope of the sealed store is a token's: kept when Settings
/// clean up the scopes of agents gone.
pub fn is_token_scope(scope: &str) -> bool {
    scope.starts_with("token:")
}
