//! What a new task's browser starts with of the project's last: the logins
//! of its stands, never another site's. A Jira or a mail signed in to in a
//! task's tab would otherwise reach every next task's Chrome, the one its
//! agent drives.
//!
//! A stand is told by where it is, not by its name (`.local` is a company's
//! Jira as often as a dev stand, `lvh.me` is a public name of 127.0.0.1):
//! a host Chrome saw served from this machine (written to the profile's
//! `stand-hosts` by the task's tabs); one that resolves to this machine as
//! the next task starts; `localhost` and its subdomains; one the settings
//! name (`stand_hosts`).

use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    net::{IpAddr, ToSocketAddrs as _},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::Value;

/// The hosts a profile's Chrome saw served from this machine, a line each.
pub const STAND_HOSTS: &str = "stand-hosts";

/// The cookie stores of a profile: the old place and the one since Chrome 96.
const COOKIES: [&str; 2] = ["Default/Cookies", "Default/Network/Cookies"];
const LOCAL_STORAGE: &str = "Default/Local Storage/leveldb";

/// `.App.test`, `[::1]` → `app.test`, `::1`.
fn bare(host: &str) -> String {
    host.trim_start_matches('.').trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase()
}

/// The host and port an address is served at.
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, if scheme == "https" || scheme == "wss" { 443 } else { 80 }),
    };
    Some((bare(host), port))
}

/// Whether an address is of this machine: its loopback, one of its own, or
/// in the network of a bridge of its containers or VMs (docker0, br-…,
/// podman, libvirt's).
pub fn is_here(ip: IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    interfaces().iter().any(|(name, addr, mask)| *addr == ip || (bridge(name) && same_network(ip, *addr, *mask)))
}

fn bridge(name: &str) -> bool {
    ["docker", "br-", "podman", "cni", "virbr", "lxdbr", "vmnet", "vboxnet", "bridge"].iter().any(|p| name.starts_with(p))
}

fn same_network(ip: IpAddr, addr: IpAddr, mask: IpAddr) -> bool {
    match (ip, addr, mask) {
        (IpAddr::V4(ip), IpAddr::V4(addr), IpAddr::V4(mask)) => u32::from(ip) & u32::from(mask) == u32::from(addr) & u32::from(mask),
        (IpAddr::V6(ip), IpAddr::V6(addr), IpAddr::V6(mask)) => u128::from(ip) & u128::from(mask) == u128::from(addr) & u128::from(mask),
        _ => false,
    }
}

/// This machine's interfaces: name, address, netmask.
#[cfg(unix)]
fn interfaces() -> Vec<(String, IpAddr, IpAddr)> {
    fn ip_of(sa: *const libc::sockaddr) -> Option<IpAddr> {
        if sa.is_null() {
            return None;
        }
        // SAFETY: getifaddrs gives sockaddrs of the family they say.
        unsafe {
            match i32::from((*sa).sa_family) {
                libc::AF_INET => Some(IpAddr::from(u32::from_be((*(sa as *const libc::sockaddr_in)).sin_addr.s_addr).to_be_bytes())),
                libc::AF_INET6 => Some(IpAddr::from((*(sa as *const libc::sockaddr_in6)).sin6_addr.s6_addr)),
                _ => None,
            }
        }
    }
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: the list is read, then freed once.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut at = list;
        while !at.is_null() {
            let ifa = &*at;
            if let (Some(addr), Some(mask)) = (ip_of(ifa.ifa_addr), ip_of(ifa.ifa_netmask)) {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                found.push((name, addr, mask));
            }
            at = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

#[cfg(not(unix))]
fn interfaces() -> Vec<(String, IpAddr, IpAddr)> {
    Vec::new()
}

/// The hosts a tab's page saw served from this machine, kept in its
/// profile as they come; written by a page's reader thread.
pub struct Seen {
    file: PathBuf,
    known: HashSet<String>,
}

impl Seen {
    pub fn load(profile: &Path) -> Self {
        let file = profile.join(STAND_HOSTS);
        let known = read_hosts(&file);
        Self { file, known }
    }

    /// From a response: a host whose address is this machine's, at the
    /// address's own port. Through a proxy on this machine every site comes
    /// from its port, not the site's: none of those is taken.
    pub fn note(&mut self, msg: &Value) {
        if msg["method"] != "Network.responseReceived" {
            return;
        }
        let response = &msg["params"]["response"];
        let Some((host, port)) = response["url"].as_str().and_then(host_port) else { return };
        // An address, localhost: known for what they are without a note.
        if self.known.contains(&host) || host.parse::<IpAddr>().is_ok() || host == "localhost" || host.ends_with(".localhost") {
            return;
        }
        let Some(ip) = response["remoteIPAddress"].as_str().and_then(|ip| bare(ip).parse::<IpAddr>().ok()) else { return };
        if response["remotePort"].as_u64() != Some(u64::from(port)) || !is_here(ip) {
            return;
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) {
            let _ = writeln!(file, "{host}");
        }
        self.known.insert(host);
    }
}

fn read_hosts(file: &Path) -> HashSet<String> {
    std::fs::read_to_string(file).unwrap_or_default().lines().map(bare).filter(|h| !h.is_empty()).collect()
}

/// Whether `pattern` (`app.test`, `*.app.test`) names `host`. A domain of
/// one label (`*.com`) names nothing: it would be every site's.
fn named(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => domain.contains('.') && (host == domain || host.ends_with(&format!(".{domain}"))),
        None => host == pattern,
    }
}

/// What tells a stand from another site for one seeding.
struct Stands {
    seen: HashSet<String>,
    named: Vec<String>,
    resolved: HashMap<String, bool>,
}

impl Stands {
    /// A cookie's domain (`.app.test` serves its subdomains too) or a
    /// storage's host: a stand's, or a parent domain of one.
    fn holds(&self, host: &str) -> bool {
        let host = bare(host);
        if host.is_empty() {
            return false;
        }
        if host == "localhost" || host.ends_with(".localhost") {
            return true;
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            return is_here(ip);
        }
        // A parent domain's cookie serves the stand too: `.app.test`, never `.test`.
        let parent = host.contains('.');
        let under = |stand: &String| *stand == host || (parent && stand.ends_with(&format!(".{host}")));
        self.seen.iter().any(under) || self.named.iter().any(|p| named(p, &host) || (parent && p.strip_prefix("*.").is_some_and(|d| d.contains('.') && d.ends_with(&format!(".{host}"))))) || self.resolved.get(&host).copied().unwrap_or(false)
    }
}

/// Resolves the hosts on threads of their own, as many as answer within
/// `wait`: one that does not, a company's host off its VPN, is no stand.
fn resolve_here(hosts: impl IntoIterator<Item = String>, wait: Duration) -> HashMap<String, bool> {
    let (tx, rx) = mpsc::channel();
    let mut asked = 0;
    for host in hosts {
        if host.parse::<IpAddr>().is_ok() || host.is_empty() {
            continue;
        }
        asked += 1;
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("seed-resolve".into()).spawn(move || {
            let here = (host.as_str(), 80).to_socket_addrs().map(|mut addrs| addrs.next().is_some_and(|a| is_here(a.ip()))).unwrap_or(false);
            let _ = tx.send((host, here));
        });
    }
    drop(tx);
    let mut resolved = HashMap::new();
    let until = Instant::now() + wait;
    while resolved.len() < asked {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((host, here)) => {
                resolved.insert(host, here);
            }
            Err(_) => break,
        }
    }
    resolved
}

/// The hosts of a profile's cookies and local storage.
fn hosts_in(profile: &Path) -> HashSet<String> {
    let mut hosts = HashSet::new();
    for store in COOKIES {
        let path = profile.join(store);
        if !path.is_file() {
            continue;
        }
        if let Ok(db) = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            if let Ok(mut rows) = db.prepare("SELECT DISTINCT host_key FROM cookies") {
                hosts.extend(rows.query_map([], |r| r.get::<_, String>(0)).into_iter().flatten().flatten().map(|h| bare(&h)));
            }
        }
    }
    hosts
}

/// Carries the stands' logins of the profile `from` into the new one `to`:
/// the key their values are encrypted with, their cookies, their local
/// storage, and the hosts seen there. Nothing of another site; a store that
/// cannot be read through is left out whole, never copied as it is.
pub fn carry(from: &Path, to: &Path, named: &[String]) {
    let seen = read_hosts(&from.join(STAND_HOSTS));
    let mut stands = Stands { seen: seen.clone(), named: named.iter().map(|n| bare(n)).collect(), resolved: HashMap::new() };
    let undecided: Vec<String> = hosts_in(from).into_iter().filter(|h| !stands.holds(h)).collect();
    stands.resolved = resolve_here(undecided, Duration::from_millis(1500));

    let _ = copy(&from.join("Local State"), &to.join("Local State"));
    if !seen.is_empty() {
        let _ = std::fs::write(to.join(STAND_HOSTS), seen.iter().map(|h| format!("{h}\n")).collect::<String>());
    }
    for store in COOKIES {
        if from.join(store).is_file() && carry_cookies(&from.join(store), &to.join(store), &stands).is_err() {
            let _ = std::fs::remove_file(to.join(store));
            let _ = std::fs::remove_file(to.join(format!("{store}-journal")));
        }
    }
    if from.join(LOCAL_STORAGE).is_dir() && carry_storage(&from.join(LOCAL_STORAGE), &to.join(LOCAL_STORAGE), &stands).is_err() {
        let _ = std::fs::remove_dir_all(to.join(LOCAL_STORAGE));
    }
}

fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to).map(|_| ())
}

/// A copy of the store, its journal with it (a write half done is rolled
/// back as it opens), less every other site's rows; vacuumed, so what was
/// deleted is not left in its free pages.
fn carry_cookies(from: &Path, to: &Path, stands: &Stands) -> anyhow::Result<()> {
    copy(from, to)?;
    let journal = |p: &Path| PathBuf::from(format!("{}-journal", p.display()));
    if journal(from).is_file() {
        copy(&journal(from), &journal(to))?;
    }
    let db = rusqlite::Connection::open(to)?;
    let hosts: Vec<String> = db.prepare("SELECT DISTINCT host_key FROM cookies")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for host in hosts.iter().filter(|h| !stands.holds(h)) {
        db.execute("DELETE FROM cookies WHERE host_key = ?1", [host])?;
    }
    db.execute_batch("VACUUM")?;
    Ok(())
}

/// The origin a key of Chrome's local storage is of: `_<origin>\0…` its
/// values, `META:<origin>` and `METAACCESS:<origin>` what it keeps of them.
fn storage_origin(key: &[u8]) -> Option<&[u8]> {
    if let Some(rest) = key.strip_prefix(b"_") {
        return rest.split(|b| *b == 0).next();
    }
    key.strip_prefix(b"META:").or_else(|| key.strip_prefix(b"METAACCESS:"))
}

/// `https://app.test:8443/`, `http://localhost:3000^0https://top.example` → its host.
fn origin_host(origin: &[u8]) -> Option<String> {
    let origin = std::str::from_utf8(origin).ok()?;
    let rest = origin.split_once("://")?.1;
    let end = rest.find(['/', '^']).unwrap_or(rest.len());
    host_port(&format!("http://{}", rest.get(..end)?)).map(|(h, _)| h)
}

/// A new store with the stands' origins of the one at `from`, read from a
/// copy: the store of a Chrome still running is not opened.
fn carry_storage(from: &Path, to: &Path, stands: &Stands) -> anyhow::Result<()> {
    let read = to.with_file_name("leveldb.seed");
    let _ = std::fs::remove_dir_all(&read);
    crate::browser::copy_tree(from, &read)?;
    let _ = std::fs::remove_file(read.join("LOCK"));
    let kept = (|| -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut db = rusty_leveldb::DB::open(&read, rusty_leveldb::Options { create_if_missing: false, ..Default::default() }).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut iter = db.new_iter().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut kept = Vec::new();
        use rusty_leveldb::LdbIterator as _;
        while let Some((key, value)) = iter.next() {
            let keep = key == b"VERSION" || storage_origin(&key).and_then(origin_host).is_some_and(|h| stands.holds(&h));
            if keep {
                kept.push((key, value));
            }
        }
        Ok(kept)
    })();
    let _ = std::fs::remove_dir_all(&read);
    let kept = kept?;
    if kept.iter().all(|(k, _)| k == b"VERSION") {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    let mut db = rusty_leveldb::DB::open(to, rusty_leveldb::Options::default()).map_err(|e| anyhow::anyhow!("{e}"))?;
    for (key, value) in &kept {
        db.put(key, value).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    db.flush().map_err(|e| anyhow::anyhow!("{e}"))?;
    db.close().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// The hosts a project names as its stands, besides what is found.
pub fn named_stands(root: &Path) -> Vec<String> {
    crate::layers::of_root(root).values.stand_hosts.split([' ', ',', '\n']).map(str::trim).filter(|h| !h.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stand_is_where_it_is_served_from() {
        let stands = Stands {
            seen: ["api.app.test".to_string(), "jira.corp.local".to_string()].into(),
            named: vec!["*.dev.example".into(), "one.example".into(), "*.com".into()],
            resolved: [("lvh.me".to_string(), true), ("jira.company.local".to_string(), false)].into(),
        };
        for host in ["localhost", "a.localhost", "127.0.0.1", "::1", "[::1]", "api.app.test", ".app.test", "app.test", "jira.corp.local", "x.dev.example", ".dev.example", "one.example", "lvh.me"] {
            assert!(stands.holds(host), "{host} is a stand's");
        }
        for host in ["example.com", ".example.com", "jira.company.local", "two.example", "test", "", "8.8.8.8", "notlocalhost"] {
            assert!(!stands.holds(host), "{host} is no stand's");
        }
    }

    #[test]
    fn a_response_from_here_is_noted_once_and_not_through_a_proxy() {
        let dir = std::env::temp_dir().join(format!("ideae-seen-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut seen = Seen::load(&dir);
        let response = |url: &str, ip: &str, port: u16| json!({ "method": "Network.responseReceived", "params": { "response": { "url": url, "remoteIPAddress": ip, "remotePort": port } } });
        seen.note(&response("http://app.local/", "127.0.0.1", 80));
        seen.note(&response("http://app.local/x.js", "127.0.0.1", 80));
        seen.note(&response("https://api.app.test:8443/v1", "[::1]", 8443));
        // A local proxy: the site's port is not the address's.
        seen.note(&response("https://jira.corp.example/", "127.0.0.1", 3128));
        seen.note(&response("https://example.com/", "93.184.215.14", 443));
        seen.note(&response("http://127.0.0.1:3000/", "127.0.0.1", 3000));
        seen.note(&response("http://web.localhost/", "127.0.0.1", 80));
        seen.note(&json!({ "method": "Network.requestWillBeSent", "params": {} }));
        let mut written: Vec<String> = std::fs::read_to_string(dir.join(STAND_HOSTS)).unwrap().lines().map(str::to_string).collect();
        written.sort();
        assert_eq!(written, ["api.app.test", "app.local"]);
        assert_eq!(Seen::load(&dir).known.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_origin_of_a_stored_value() {
        let host = |key: &[u8]| storage_origin(key).and_then(origin_host);
        assert_eq!(host(b"_http://localhost:3000\x00\x01token"), Some("localhost".into()));
        assert_eq!(host(b"_https://app.test^0https://top.example\x00\x01k"), Some("app.test".into()));
        assert_eq!(host(b"META:https://example.com"), Some("example.com".into()));
        assert_eq!(host(b"METAACCESS:http://[::1]:8080"), Some("::1".into()));
        assert_eq!(host(b"VERSION"), None);
    }

    #[test]
    fn a_bridge_s_network_is_this_machine_s() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(same_network(ip("172.18.0.5"), ip("172.18.0.1"), ip("255.255.0.0")));
        assert!(!same_network(ip("172.19.0.5"), ip("172.18.0.1"), ip("255.255.0.0")));
        assert!(bridge("docker0") && bridge("br-3f2a") && !bridge("tun0") && !bridge("wlan0"));
        assert!(is_here(ip("127.0.0.1")) && is_here(ip("::1")) && !is_here(ip("8.8.8.8")));
    }
}
