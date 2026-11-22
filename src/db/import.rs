//! Connections read from DataGrip (and JetBrains IDEs with its plugin): its
//! data sources and SSH configurations. Never its passwords, which are in the
//! system's keyring under its own names.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use super::config::{Connection, Driver, SshHop};

/// A connection found, and whether it is there already.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub connection: Connection,
    pub known: bool,
}

/// Host, port and database of a JDBC URL this app has a driver for.
pub fn parse_jdbc(url: &str) -> Option<(Driver, String, u16, Option<String>)> {
    let rest = url.trim().strip_prefix("jdbc:")?;
    let (scheme, rest) = rest.split_once("://")?;
    let driver = match scheme {
        "mysql" | "mariadb" => Driver::MySql,
        "postgresql" => Driver::Postgres,
        "redis" => Driver::Redis,
        _ => return None,
    };
    let rest = rest.split(['?', ';']).next()?;
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    // Several hosts are a cluster's failover: not one address to connect to.
    if authority.contains(',') {
        return None;
    }
    let authority = authority.rsplit('@').next()?;
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        (host.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().ok()),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((driver, host, port.unwrap_or(driver.default_port()), (!database.is_empty()).then(|| database.to_string())))
}

fn home_in(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("$USER_HOME$") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(path),
    }
}

/// `sshConfigs.xml`: its configurations by id.
fn ssh_configs(xml: &str, home: &Path) -> HashMap<String, SshHop> {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return HashMap::new() };
    doc.descendants()
        .filter(|n| n.has_tag_name("sshConfig"))
        .filter_map(|n| {
            let id = n.attribute("id")?;
            let host = n.attribute("host")?;
            let hop = SshHop {
                host: host.to_string(),
                port: n.attribute("port").and_then(|p| p.parse().ok()).unwrap_or(22),
                user: n.attribute("username").unwrap_or_default().to_string(),
                key: n.attribute("keyPath").filter(|k| !k.is_empty()).map(|k| home_in(k, home)),
            };
            Some((id.to_string(), hop))
        })
        .collect()
}

#[derive(Default)]
struct Source {
    name: Option<String>,
    url: Option<String>,
    user: Option<String>,
    ssh: Option<String>,
}

/// Data sources in a file, into what is known by their uuid: one file has
/// the URL, another the user and the tunnel.
fn read_sources(xml: &str, into: &mut HashMap<String, Source>) {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return };
    for node in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
        let Some(uuid) = node.attribute("uuid") else { continue };
        let source = into.entry(uuid.to_string()).or_default();
        if let Some(name) = node.attribute("name") {
            source.name.get_or_insert_with(|| name.to_string());
        }
        let text = |tag: &str| node.children().find(|c| c.has_tag_name(tag)).and_then(|c| c.text()).map(str::trim).filter(|t| !t.is_empty()).map(String::from);
        if let Some(url) = text("jdbc-url") {
            source.url.get_or_insert(url);
        }
        if let Some(user) = text("user-name") {
            source.user.get_or_insert(user);
        }
        if let Some(ssh) = node.children().find(|c| c.has_tag_name("ssh-properties")) {
            let field = |tag: &str| ssh.children().find(|c| c.has_tag_name(tag)).and_then(|c| c.text()).map(str::trim);
            if field("enabled") == Some("true")
                && let Some(id) = field("ssh-config-id")
            {
                source.ssh.get_or_insert_with(|| id.to_string());
            }
        }
    }
}

fn connections(sources: HashMap<String, Source>, hops: &HashMap<String, SshHop>) -> Vec<Connection> {
    let mut out: Vec<Connection> = sources
        .into_iter()
        .filter_map(|(uuid, source)| {
            let (driver, host, port, database) = parse_jdbc(source.url.as_deref()?)?;
            Some(Connection {
                id: format!("datagrip-{uuid}"),
                name: source.name.unwrap_or_else(|| host.clone()),
                driver,
                host,
                port,
                user: source.user.unwrap_or_default(),
                database,
                ssh: source.ssh.and_then(|id| hops.get(&id).cloned()),
                bastion: None,
                tls: Default::default(),
                color: None,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn files_under(dir: &Path, wanted: &dyn Fn(&str) -> bool, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && depth > 0 {
            files_under(&path, wanted, depth - 1, out);
        } else if kind.is_file() && path.file_name().and_then(|n| n.to_str()).is_some_and(wanted) {
            out.push(path);
        }
    }
}

/// Where JetBrains keeps its settings on this system.
fn config_root(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/JetBrains")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map_or_else(|| home.join("AppData/Roaming"), PathBuf::from).join("JetBrains")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from).join("JetBrains")
    }
}

/// What DataGrip knows. Reads files: not for the UI thread.
pub fn find(existing: &[Connection]) -> Vec<Found> {
    let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from) else { return Vec::new() };
    let root = config_root(&home);
    let mut ssh_files = Vec::new();
    files_under(&root, &|name| name == "sshConfigs.xml", 2, &mut ssh_files);
    let mut hops = HashMap::new();
    for file in ssh_files {
        if let Ok(xml) = std::fs::read_to_string(file) {
            hops.extend(ssh_configs(&xml, &home));
        }
    }
    let is_sources = |name: &str| matches!(name, "dataSources.xml" | "dataSources.local.xml" | "data_sources_history.xml");
    let mut files = Vec::new();
    files_under(&home.join("DataGripProjects"), &is_sources, 4, &mut files);
    files_under(&root, &is_sources, 3, &mut files);
    // The projects' own files come first: what they say is newer than a history's.
    let mut sources = HashMap::new();
    for file in files {
        if let Ok(xml) = std::fs::read_to_string(file) {
            read_sources(&xml, &mut sources);
        }
    }
    connections(sources, &hops)
        .into_iter()
        .map(|connection| {
            let known = existing.iter().any(|c| c.id == connection.id);
            Found { connection, known }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdbc_urls_give_their_server() {
        assert_eq!(parse_jdbc("jdbc:mysql://db-internal:3307/app?useSSL=false"), Some((Driver::MySql, "db-internal".into(), 3307, Some("app".into()))));
        assert_eq!(parse_jdbc("jdbc:postgresql://pg/shop"), Some((Driver::Postgres, "pg".into(), 5432, Some("shop".into()))));
        assert_eq!(parse_jdbc("jdbc:mariadb://[fd00::1]:3306"), Some((Driver::MySql, "fd00::1".into(), 3306, None)));
        assert_eq!(parse_jdbc("jdbc:oracle:thin:@x:1521:y"), None);
        assert_eq!(parse_jdbc("jdbc:mysql://a:1,b:2/app"), None);
    }

    #[test]
    fn a_source_comes_together_from_its_files_with_its_tunnel() {
        let hops = ssh_configs(
            r#"<application><component name="SshConfigs"><configs>
                <sshConfig host="bastion.example" id="s1" keyPath="$USER_HOME$/.ssh/id_ed25519" port="2222" username="me"/>
            </configs></component></application>"#,
            Path::new("/home/someone"),
        );
        let mut sources = HashMap::new();
        read_sources(r#"<project><component><data-source name="Stage" uuid="u1"><jdbc-url>jdbc:mysql://db-internal:3306/app</jdbc-url></data-source></component></project>"#, &mut sources);
        read_sources(r#"<project><component><data-source name="Stage" uuid="u1"><user-name>reader</user-name><ssh-properties><enabled>true</enabled><ssh-config-id>s1</ssh-config-id></ssh-properties></data-source></component></project>"#, &mut sources);
        let found = connections(sources, &hops);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].name.as_str(), found[0].user.as_str(), found[0].id.as_str()), ("Stage", "reader", "datagrip-u1"));
        let hop = found[0].ssh.as_ref().unwrap();
        assert_eq!((hop.host.as_str(), hop.port), ("bastion.example", 2222));
        assert_eq!(hop.key.as_deref(), Some(Path::new("/home/someone/.ssh/id_ed25519")));
    }
}
