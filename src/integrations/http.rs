//! Asking a tracker's API over HTTPS with a token of yours: blocking, off
//! the UI thread. The token goes in a header, never on a command line (a
//! process list shows those). Answers are bounded; each failure says what
//! to do about it.

use std::time::Duration;

use serde_json::Value;

use super::IntError;

/// The most of an answer read: a ticket, not an attachment.
const MOST: u64 = 1 << 20;

/// How a request proves who sends it.
#[derive(Clone, Debug, PartialEq)]
pub enum Auth {
    /// `Authorization: Bearer <token>`: Jira Server/Data Center, YouTrack.
    Bearer(String),
    /// `Authorization: Basic <user:token>`: Jira Cloud with an email.
    Basic { user: String, token: String },
    /// `Authorization: <token>` as it is: Linear's personal API key.
    Plain(String),
}

impl Auth {
    fn header(&self) -> String {
        use base64::Engine as _;
        match self {
            Auth::Bearer(token) => format!("Bearer {token}"),
            Auth::Basic { user, token } => format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))),
            Auth::Plain(token) => token.clone(),
        }
    }
}

fn agent() -> ureq::Agent {
    // A Jira on a company's own server is signed by the company's own
    // authority: the system's trust store knows it, webpki's roots do not.
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .tls_config(tls)
        .user_agent(format!("ideae/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// What an answer's status means: the body when it is one, else why not.
fn answer(mut response: ureq::http::Response<ureq::Body>, settings_hint: &str) -> Result<Value, IntError> {
    let status = response.status().as_u16();
    let text = response.body_mut().with_config().limit(MOST).read_to_string().unwrap_or_default();
    match status {
        200..=299 => serde_json::from_str(&text).map_err(|e| IntError::Parse(e.to_string())),
        401 => Err(IntError::NotSignedIn { hint: format!("the token was refused: give a new one in {settings_hint}") }),
        403 => Err(IntError::Forbidden),
        404 => Err(IntError::NotFound),
        429 => Err(IntError::RateLimited),
        _ => Err(IntError::Network(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>()))),
    }
}

const SETTINGS: &str = "Settings → Integrations";

pub fn get_json(url: &str, auth: &Auth) -> Result<Value, IntError> {
    let response = agent().get(url).header("Authorization", auth.header()).header("Accept", "application/json").call().map_err(|e| IntError::Network(e.to_string()))?;
    answer(response, SETTINGS)
}

pub fn post_json(url: &str, auth: &Auth, body: &Value) -> Result<Value, IntError> {
    let response = agent()
        .post(url)
        .header("Authorization", auth.header())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| IntError::Network(e.to_string()))?;
    answer(response, SETTINGS)
}

#[cfg(test)]
pub mod fake {
    //! A stand-in tracker on loopback: answers by path from what it was
    //! given, and keeps the requests it got (their Authorization header).

    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    pub struct Server {
        pub base: String,
        pub seen: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    /// `routes`: a path's start, its status and its body.
    pub fn start(routes: Vec<(&'static str, u16, String)>) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or_default().to_string();
                let (mut auth, mut length) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap_or_default();
                    match name.to_ascii_lowercase().as_str() {
                        "authorization" => auth = value.trim().to_string(),
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                log.lock().unwrap().push((path.clone(), auth, String::from_utf8_lossy(&body).into_owned()));
                let (status, answer) = routes.iter().find(|(start, _, _)| path.starts_with(start)).map(|(_, s, b)| (*s, b.clone())).unwrap_or((404, "{}".into()));
                let mut stream = stream;
                let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            }
        });
        Server { base, seen }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_say_what_to_do() {
        let server = fake::start(vec![("/ok", 200, r#"{"a":1}"#.into()), ("/gone", 404, "{}".into()), ("/no", 401, "{}".into()), ("/slow", 429, "{}".into())]);
        let auth = Auth::Basic { user: "me@example.com".into(), token: "t0k".into() };
        assert_eq!(get_json(&format!("{}/ok", server.base), &auth).unwrap()["a"], 1);
        assert_eq!(get_json(&format!("{}/gone", server.base), &auth), Err(IntError::NotFound));
        assert!(matches!(get_json(&format!("{}/no", server.base), &auth), Err(IntError::NotSignedIn { .. })));
        assert_eq!(get_json(&format!("{}/slow", server.base), &auth), Err(IntError::RateLimited));
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen[0].1, "Basic bWVAZXhhbXBsZS5jb206dDBr", "the token in a header, as basic auth");
        assert_eq!(Auth::Bearer("x".into()).header(), "Bearer x");
    }
}
