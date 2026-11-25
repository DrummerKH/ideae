//! Tickets in text: links to them and their keys, read without asking
//! anything of the network. A link is known by an integration that is set
//! up, or by the address of the well-known trackers (Jira Cloud, Linear,
//! GitHub and GitLab issues); a bare key by the integration that owns its
//! prefix, and only by one: none owning it, it only looks like a ticket.

use super::{Integration, Kind, Ticket, Tracker};

/// Names of standards and the like that look like a ticket's key: a title
/// "SHA-256 for uploads" asks for no Jira issue.
const NOT_TICKETS: [&str; 22] = [
    "AES", "BASE", "COVID", "CP", "ECMA", "ES", "GPT", "HTTP", "IEC", "IEEE", "ISO", "MD", "PEP", "PSR", "RFC", "RSA", "SHA", "SSL", "TLS", "UCS",
    "UTF", "WCAG",
];

/// Whether `word` is a key as Jira, Linear and YouTrack write them: `DEV-123`.
pub fn is_key(word: &str) -> bool {
    let Some((project, number)) = word.split_once('-') else { return false };
    // Jira's project keys have two characters at least.
    project.len() >= 2
        && !NOT_TICKETS.contains(&project)
        && project.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && project.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

/// The key a title starts with.
pub fn key_at_start(title: &str) -> Option<&str> {
    let first = title.split_whitespace().next()?;
    is_key(first).then_some(first)
}

/// Every key in a text (a branch, a commit's subject): `DEV-12_fix` has `DEV-12`.
pub fn keys_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        // `DEV-12_fix`, `feature-DEV-12`: the key may be one part of a word.
        let parts: Vec<&str> = word.split('_').collect();
        for part in parts {
            let pieces: Vec<&str> = part.split('-').collect();
            for at in 0..pieces.len().saturating_sub(1) {
                let candidate = format!("{}-{}", pieces[at], pieces[at + 1]);
                if is_key(&candidate) && !found.contains(&candidate) {
                    found.push(candidate);
                }
            }
        }
    }
    found
}

/// The key after `/browse/` or `selectedIssue=` in a Jira link, as Jira writes it.
fn jira_key_of(url: &str) -> Option<String> {
    let (_, after) = url.split_once("/browse/").or_else(|| url.split_once("selectedIssue="))?;
    let key: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>().to_ascii_uppercase();
    is_key(&key).then_some(key)
}

/// `…/issue/ENG-123/fix-the-login` of a Linear link: the key and the words.
fn linear_key_of(url: &str) -> Option<(String, String)> {
    let (_, after) = url.split_once("/issue/")?;
    let mut parts = after.split(['/', '?', '#']);
    let key = parts.next()?.to_ascii_uppercase();
    is_key(&key).then(|| (key, parts.next().unwrap_or_default().replace('-', " ").trim().to_string()))
}

/// `owner/repo#12` of a GitHub or GitLab issue's link (`/issues/12`, `/-/issues/12`).
fn issue_of(url: &str) -> Option<(String, u64)> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (before, after) = rest.split_once("/issues/")?;
    let number: u64 = after.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let path = before.split_once('/')?.1.trim_end_matches("/-");
    (path.contains('/')).then(|| (path.to_string(), number))
}

pub struct Jira {
    pub base: String,
}

impl Tracker for Jira {
    fn parse_url(&self, url: &str) -> Option<String> {
        url.starts_with(&self.base).then(|| jira_key_of(url)).flatten()
    }

    fn url_for(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.base)
    }
}

pub struct Linear {
    pub base: String,
}

impl Tracker for Linear {
    fn parse_url(&self, url: &str) -> Option<String> {
        url.starts_with(&self.base).then(|| linear_key_of(url).map(|(key, _)| key)).flatten()
    }

    /// Linear finds an issue by its key under the workspace's address.
    fn url_for(&self, key: &str) -> String {
        format!("{}/issue/{key}", self.base)
    }
}

pub struct YouTrack {
    pub base: String,
}

impl Tracker for YouTrack {
    fn parse_url(&self, url: &str) -> Option<String> {
        let (_, after) = url.strip_prefix(&self.base)?.split_once("/issue/")?;
        let key: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>().to_ascii_uppercase();
        is_key(&key).then_some(key)
    }

    fn url_for(&self, key: &str) -> String {
        format!("{}/issue/{key}", self.base)
    }
}

/// GitHub's or GitLab's issues: keys `owner/repo#12`.
pub struct Issues {
    pub kind: Kind,
    pub base: String,
}

impl Tracker for Issues {
    fn parse_url(&self, url: &str) -> Option<String> {
        url.starts_with(&self.base).then(|| issue_of(url).map(|(path, n)| format!("{path}#{n}"))).flatten()
    }

    fn url_for(&self, key: &str) -> String {
        let (path, number) = key.rsplit_once('#').unwrap_or((key, ""));
        match self.kind {
            Kind::GitLab => format!("{}/{path}/-/issues/{number}", self.base),
            _ => format!("{}/{path}/issues/{number}", self.base),
        }
    }
}

/// What a title or prompt says of a ticket.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub ticket: Ticket,
    /// The link as it was pasted, to take out of the title.
    pub pasted: Option<String>,
    /// The words of the title a Linear link carries.
    pub words: String,
    /// `#12` at the start: an issue of the project's own repository.
    pub own_issue: Option<u64>,
}

/// The integration that owns a key's prefix: the one of the most specific
/// layer when two do (a project's over its space's, over Settings').
pub fn owner<'a>(key: &str, integrations: &'a [Integration]) -> Option<&'a Integration> {
    let prefix = key.split_once('-').map(|(p, _)| p)?;
    integrations.iter().filter(|i| i.kind.keyed() && i.owns(prefix)).max_by_key(|i| i.from)
}

/// A link to a ticket: by the integrations set up first (the longest
/// address that fits), then by the well-known addresses.
pub fn ticket_of_url(url: &str, integrations: &[Integration]) -> Option<Found> {
    let fits = integrations.iter().filter(|i| url.starts_with(&i.base)).max_by_key(|i| i.base.len());
    if let Some(i) = fits
        && let Some(key) = i.tracker().and_then(|t| t.parse_url(url))
    {
        let words = if i.kind == Kind::Linear { linear_key_of(url).map(|(_, w)| w).unwrap_or_default() } else { String::new() };
        return Some(Found { ticket: Ticket { key, kind: Some(i.kind), integration: Some(i.id.clone()), url: Some(url.to_string()) }, pasted: Some(url.to_string()), words, own_issue: None });
    }
    let known = |key: String, kind: Kind, words: String| {
        // A key some integration owns is its, though the link came from elsewhere.
        let owned = owner(&key, integrations).filter(|i| i.kind == kind).map(|i| i.id.clone());
        Some(Found { ticket: Ticket { key, kind: Some(kind), integration: owned, url: Some(url.to_string()) }, pasted: Some(url.to_string()), words, own_issue: None })
    };
    if url.contains("linear.app/")
        && let Some((key, words)) = linear_key_of(url)
    {
        return known(key, Kind::Linear, words);
    }
    if let Some(key) = jira_key_of(url) {
        return known(key, Kind::Jira, String::new());
    }
    if let Some((path, number)) = issue_of(url) {
        let kind = if url.contains("/-/issues/") || super::host_of(url).starts_with("gitlab") { Kind::GitLab } else { Kind::GitHub };
        return Some(Found {
            ticket: Ticket { key: format!("{path}#{number}"), kind: Some(kind), integration: None, url: Some(url.to_string()) },
            pasted: Some(url.to_string()),
            words: String::new(),
            own_issue: None,
        });
    }
    None
}

/// The ticket a new task's title names: a link in it, or a key or `#12`
/// it starts with.
pub fn ticket_in(title: &str, integrations: &[Integration]) -> Option<Found> {
    if let Some(found) = title.split_whitespace().filter(|w| w.starts_with("http")).find_map(|url| ticket_of_url(url, integrations)) {
        return Some(found);
    }
    let first = title.split_whitespace().next()?;
    if let Some(n) = first.strip_prefix('#').and_then(|n| n.parse().ok()) {
        return Some(Found { ticket: Ticket { key: format!("#{n}"), kind: None, integration: None, url: None }, pasted: None, words: String::new(), own_issue: Some(n) });
    }
    let key = key_at_start(title)?;
    let i = owner(key, integrations);
    Some(Found {
        ticket: Ticket {
            key: key.to_string(),
            kind: i.map(|i| i.kind),
            integration: i.map(|i| i.id.clone()),
            url: i.and_then(|i| i.tracker()).map(|t| t.url_for(key)),
        },
        pasted: None,
        words: String::new(),
        own_issue: None,
    })
}

/// Every link in a text: what a prompt carries to keep with the task.
pub fn links_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(at) = word.find("http://").or_else(|| word.find("https://")) else { continue };
        let url = word[at..].trim_end_matches(|c: char| matches!(c, ')' | ']' | '>' | ',' | '.' | ';' | ':' | '"' | '\'' | '`'));
        if super::host_of(url).contains('.') && !found.iter().any(|f| f == url) {
            found.push(url.to_string());
        }
    }
    found
}

/// The address of the tracker a link is of, for an integration made from
/// it: Jira's before `/browse/` (a server may live on a path) or its host,
/// Linear's workspace, YouTrack's before `/issue/`.
pub fn base_of_link(kind: Kind, url: &str) -> Option<String> {
    let cut = |marker: &str| url.split_once(marker).map(|(before, _)| before.to_string());
    let host_only = || {
        let (scheme, rest) = url.split_once("://")?;
        Some(format!("{scheme}://{}", rest.split('/').next()?))
    };
    let base = match kind {
        Kind::Jira => cut("/browse/").or_else(|| cut("/jira/")).or_else(host_only),
        Kind::YouTrack => cut("/issue/"),
        Kind::Linear => cut("/issue/"),
        _ => None,
    }?;
    super::normal_base(&base)
}

/// An agent's prose with the keys an integration owns made links to their
/// tickets: `[DEV-1](https://…/browse/DEV-1)`. Not in inline code, not in
/// a link already, nothing for a key no integration owns. None: no change.
pub fn link_keys(text: &str, integrations: &[Integration]) -> Option<String> {
    if !integrations.iter().any(|i| i.kind.keyed()) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    // Inline code between backticks stays as it is: every other part is prose.
    for (n, part) in text.split('`').enumerate() {
        if n > 0 {
            out.push('`');
        }
        if n % 2 == 1 {
            out.push_str(part);
            continue;
        }
        let mut last = 0;
        let mut word_start: Option<usize> = None;
        let chars: Vec<(usize, char)> = part.char_indices().collect();
        for (k, &(at, c)) in chars.iter().enumerate() {
            let in_word = c.is_ascii_alphanumeric() || c == '-' || c == '_';
            if in_word && word_start.is_none() {
                word_start = Some(at);
            }
            let ends = !in_word || k + 1 == chars.len();
            if let (true, Some(start)) = (ends, word_start) {
                let end = if in_word { at + c.len_utf8() } else { at };
                word_start = None;
                let word = part.get(start..end).unwrap_or_default().trim_end_matches('-');
                let before = part.get(..start).and_then(|b| b.chars().last());
                let after = part.get(start + word.len()..).and_then(|a| a.chars().next());
                // Not in an address (a word of `https://…?jql=key=DEV-5`), not in a link's text (`[see DEV-5](…)`).
                let head = part.get(..start).unwrap_or_default();
                let token_start = head.rfind(char::is_whitespace).map_or(0, |at| at + 1);
                let token_end = part.get(start..).and_then(|rest| rest.find(char::is_whitespace)).map_or(part.len(), |at| start + at);
                let token = part.get(token_start..token_end).unwrap_or_default();
                let in_address = token.contains("://") || token.starts_with("www.");
                let in_text = head.matches('[').count() > head.matches(']').count();
                let in_link = in_address || in_text || matches!(before, Some('[' | '/' | '#' | '(')) || matches!(after, Some(']'));
                if !in_link
                    && is_key(word)
                    && let Some(url) = owner(word, integrations).and_then(|i| i.tracker()).map(|t| t.url_for(word))
                {
                    out.push_str(part.get(last..start).unwrap_or_default());
                    out.push_str(&format!("[{word}]({url})"));
                    last = start + word.len();
                    changed = true;
                }
            }
        }
        out.push_str(part.get(last..).unwrap_or_default());
    }
    changed.then_some(out)
}

/// A ticket as a task keeps it.
pub fn link_of(ticket: &Ticket) -> crate::state::TaskLink {
    crate::state::TaskLink {
        what: "ticket".into(),
        key: ticket.key.clone(),
        tracker: ticket.kind.map(|k| k.word().to_string()).unwrap_or_default(),
        integration: ticket.integration.clone().unwrap_or_default(),
        url: ticket.url.clone().unwrap_or_default(),
        ..Default::default()
    }
}

/// What a new task keeps of its title and prompt: the ticket its title
/// names first, then every address in either, a ticket's as a ticket.
pub fn links_for_task(title: &str, prompt: &str, integrations: &[Integration]) -> Vec<crate::state::TaskLink> {
    let mut links: Vec<crate::state::TaskLink> = Vec::new();
    let add = |links: &mut Vec<crate::state::TaskLink>, link: crate::state::TaskLink| {
        let same = |l: &crate::state::TaskLink| (link.is_ticket() && l.is_ticket() && l.key == link.key) || (!link.url.is_empty() && l.url == link.url);
        if !links.iter().any(same) {
            links.push(link);
        }
    };
    if let Some(found) = ticket_in(title, integrations) {
        add(&mut links, link_of(&found.ticket));
    }
    for url in links_in(&format!("{title}\n{prompt}")) {
        match ticket_of_url(&url, integrations) {
            Some(found) => add(&mut links, link_of(&found.ticket)),
            None => add(&mut links, crate::state::TaskLink { what: "url".into(), url, ..Default::default() }),
        }
    }
    links
}

/// A task of before links: the addresses its prompt has, and the keys of
/// its branch an integration owns (one no integration owns is no link).
pub fn links_of_old_task(prompt: &str, branch: &str, integrations: &[Integration]) -> Vec<crate::state::TaskLink> {
    let mut links = links_for_task("", prompt, integrations);
    for key in keys_in(branch) {
        if let Some(i) = owner(&key, integrations)
            && !links.iter().any(|l| l.is_ticket() && l.key == key)
        {
            let url = i.tracker().map(|t| t.url_for(&key));
            links.push(link_of(&Ticket { key, kind: Some(i.kind), integration: Some(i.id.clone()), url }));
        }
    }
    links
}

/// The mark a task's row shows of its tickets: the first one's key, and
/// how many more (`DEV-1 +1`).
pub fn mark(links: &[crate::state::TaskLink]) -> Option<String> {
    let mut tickets = links.iter().filter(|l| l.is_ticket());
    let first = tickets.next()?;
    let more = tickets.count();
    Some(if more == 0 { first.key.clone() } else { format!("{} +{more}", first.key) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::Layer;

    fn jira(id: &str, base: &str, keys: &[&str], from: Layer) -> Integration {
        Integration { id: id.into(), kind: Kind::Jira, name: id.into(), base: base.into(), keys: keys.iter().map(|k| k.to_string()).collect(), from, account: String::new(), connected: false, agent_read: false }
    }

    #[test]
    fn keys_and_what_only_looks_like_one() {
        assert_eq!(key_at_start("DEV-1 Fix login"), Some("DEV-1"));
        assert_eq!(key_at_start("AB2-77"), Some("AB2-77"));
        for title in ["SHA-256 for uploads", "UTF-8 names", "ISO-8601 dates", "GPT-4 prompt", "A-1 one letter", "dev-5 lower", "DEV-x"] {
            assert_eq!(key_at_start(title), None, "{title}");
        }
        assert_eq!(keys_in("DEV-26368_php85FloatFix"), ["DEV-26368"]);
        assert_eq!(keys_in("feature/OPS-7-and-DEV-9 fix"), ["OPS-7", "DEV-9"]);
        assert!(keys_in("utf-8 sha-256").is_empty());
    }

    #[test]
    fn links_of_the_well_known_trackers() {
        let link = "https://example.atlassian.net/browse/DEV-26674";
        let found = ticket_of_url(link, &[]).unwrap();
        assert_eq!((found.ticket.key.as_str(), found.ticket.kind, found.ticket.integration.clone()), ("DEV-26674", Some(Kind::Jira), None));
        let board = "https://example.atlassian.net/jira/software/projects/DEV/boards/1?selectedIssue=DEV-7";
        assert_eq!(ticket_of_url(board, &[]).map(|f| f.ticket.key), Some("DEV-7".into()));
        assert_eq!(ticket_of_url("https://example.atlassian.net/browse/dev-5", &[]).map(|f| f.ticket.key), Some("DEV-5".into()));
        assert_eq!(ticket_of_url("https://example.com/browse/notakey", &[]), None);
        let linear = "https://linear.app/acme/issue/ENG-123/fix-the-login-form";
        let found = ticket_of_url(linear, &[]).unwrap();
        assert_eq!((found.ticket.key.as_str(), found.words.as_str()), ("ENG-123", "fix the login form"));
        let gl = ticket_of_url("https://gitlab.example.com/group/sub/app/-/issues/45", &[]).unwrap();
        assert_eq!((gl.ticket.key.as_str(), gl.ticket.kind), ("group/sub/app#45", Some(Kind::GitLab)));
        let gh = ticket_of_url("https://github.com/o/r/issues/9#issuecomment-1", &[]).unwrap();
        assert_eq!((gh.ticket.key.as_str(), gh.ticket.kind), ("o/r#9", Some(Kind::GitHub)));
    }

    #[test]
    fn a_key_is_the_integration_s_that_owns_its_prefix() {
        let all = [
            jira("acme", "https://acme.example.com", &["DEV"], Layer::Global),
            jira("globex", "https://corp.example.com/jira", &["DEV", "OPS"], Layer::Space),
        ];
        let found = ticket_in("DEV-5 fix it", &all).unwrap();
        assert_eq!(found.ticket.integration.as_deref(), Some("globex"), "the space's over Settings'");
        assert_eq!(found.ticket.url.as_deref(), Some("https://corp.example.com/jira/browse/DEV-5"));
        let unowned = ticket_in("ABC-123 x", &all).unwrap();
        assert_eq!((unowned.ticket.integration, unowned.ticket.url), (None, None), "only looks like a ticket");
        // A link of a Jira on a path, set up: the longest address fits.
        let found = ticket_in("see https://corp.example.com/jira/browse/OPS-3", &all).unwrap();
        assert_eq!((found.ticket.key.as_str(), found.ticket.integration.as_deref()), ("OPS-3", Some("globex")));
        assert_eq!(ticket_in("#42 the bug", &all).map(|f| f.own_issue), Some(Some(42)));
        assert_eq!(ticket_in("Fix the login", &all), None);
    }

    #[test]
    fn links_are_made_by_their_tracker() {
        assert_eq!(Jira { base: "https://example.atlassian.net".into() }.url_for("DEV-1"), "https://example.atlassian.net/browse/DEV-1");
        let gl = Issues { kind: Kind::GitLab, base: "https://gitlab.example.com".into() };
        assert_eq!(gl.url_for("g/s/p#4"), "https://gitlab.example.com/g/s/p/-/issues/4");
        assert_eq!(gl.parse_url("https://gitlab.example.com/g/s/p/-/issues/4").as_deref(), Some("g/s/p#4"));
        assert_eq!(YouTrack { base: "https://yt.example.com".into() }.parse_url("https://yt.example.com/issue/PRJ-9/title").as_deref(), Some("PRJ-9"));
    }

    #[test]
    fn a_task_keeps_its_tickets_and_addresses() {
        let all = [jira("acme", "https://acme.example.com", &["DEV"], Layer::Global)];
        let links = links_for_task("DEV-5 fix", "See https://sentry.example.com/issues/1/ and https://acme.example.com/browse/DEV-6", &all);
        assert_eq!(links.iter().map(|l| (l.what.as_str(), l.key.as_str())).collect::<Vec<_>>(), [("ticket", "DEV-5"), ("url", ""), ("ticket", "DEV-6")]);
        assert_eq!(links[0].url, "https://acme.example.com/browse/DEV-5");
        assert_eq!(mark(&links).as_deref(), Some("DEV-5 +1"));
        let old = links_of_old_task("Jira issue DEV-9 (https://acme.example.com/browse/DEV-9): read it", "DEV-9_fixIt", &all);
        assert_eq!(old.len(), 1, "the branch's key is the prompt's ticket");
        assert!(links_of_old_task("", "ABC-1_x", &all).is_empty(), "a key no integration owns is no link");
        assert_eq!(links_of_old_task("", "DEV-3_x", &all)[0].url, "https://acme.example.com/browse/DEV-3");
    }

    #[test]
    fn the_tracker_a_link_is_of() {
        assert_eq!(base_of_link(Kind::Jira, "https://example.atlassian.net/browse/DEV-1").as_deref(), Some("https://example.atlassian.net"));
        assert_eq!(base_of_link(Kind::Jira, "https://corp.example.com/jira/browse/DEV-1").as_deref(), Some("https://corp.example.com/jira"));
        assert_eq!(base_of_link(Kind::Jira, "https://example.atlassian.net/jira/software/projects/DEV/boards/1?selectedIssue=DEV-7").as_deref(), Some("https://example.atlassian.net"));
        assert_eq!(base_of_link(Kind::Linear, "https://linear.app/acme/issue/ENG-1/x").as_deref(), Some("https://linear.app/acme"));
    }

    #[test]
    fn keys_in_an_agent_s_words_become_links() {
        let all = [jira("acme", "https://acme.example.com", &["DEV"], Layer::Global)];
        let said = "Fixed DEV-12, see `DEV-13` and [DEV-14](x); ABC-1 and https://acme.example.com/browse/DEV-15 stay.";
        assert_eq!(
            link_keys(said, &all).as_deref(),
            Some("Fixed [DEV-12](https://acme.example.com/browse/DEV-12), see `DEV-13` and [DEV-14](x); ABC-1 and https://acme.example.com/browse/DEV-15 stay.")
        );
        assert_eq!(link_keys("Готово: DEV-7.", &all).as_deref(), Some("Готово: [DEV-7](https://acme.example.com/browse/DEV-7)."));
        assert_eq!(link_keys("nothing here", &all), None);
        assert_eq!(link_keys("see https://acme.example.com/issues/?jql=key=DEV-5 now", &all), None, "in an address");
        assert_eq!(link_keys("[see DEV-5 here](https://x.example/y)", &all), None, "in a link's text");
        assert_eq!(link_keys("[a](u) DEV-5", &all).as_deref(), Some("[a](u) [DEV-5](https://acme.example.com/browse/DEV-5)"));
        assert_eq!(link_keys("DEV-1", &[]), None);
    }

    #[test]
    fn links_in_a_prompt() {
        let text = "Read (https://example.atlassian.net/browse/DEV-1), and https://sentry.example.com/x?y=1. Then https://example.atlassian.net/browse/DEV-1 again";
        assert_eq!(links_in(text), ["https://example.atlassian.net/browse/DEV-1", "https://sentry.example.com/x?y=1"]);
        assert!(links_in("http://localhost:3000 is not kept").is_empty());
    }
}
