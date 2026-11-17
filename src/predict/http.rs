//! The little of HTTP that llama-server on 127.0.0.1 needs: a status, and
//! a streamed completion that ends when its connection is dropped.

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, bail};

fn connect(port: u16, timeout: Duration) -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// The status line's code of `GET path`.
pub fn status(port: u16, path: &str) -> Result<u16> {
    let mut stream = connect(port, Duration::from_secs(2))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    code(&line)
}

fn code(status_line: &str) -> Result<u16> {
    status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).with_context(|| format!("not HTTP: {status_line:?}"))
}

/// Ends a request from another thread by closing its connection: the
/// server stops generating once it notices.
#[derive(Default)]
pub struct Cancel {
    stream: OnceLock<TcpStream>,
    cancelled: AtomicBool,
}

impl Cancel {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(stream) = self.stream.get() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Cancels when dropped: held by the future that waits for the answer.
pub struct CancelOnDrop(pub Arc<Cancel>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct Answer {
    pub content: String,
    /// It ended at the token limit, not at a stop: cut short.
    pub cut: bool,
    pub prompt_n: u64,
    pub cached_n: u64,
}

/// POSTs `body` to `/completion` with `"stream": true` and gathers the
/// pieces. `keep_going` is asked between pieces.
pub fn complete(port: u16, body: &[u8], cancel: &Cancel, keep_going: impl Fn() -> bool) -> Result<Answer> {
    let mut stream = connect(port, Duration::from_secs(30))?;
    let _ = cancel.stream.set(stream.try_clone()?);
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    write!(
        stream,
        "POST /completion HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let code = code(&line)?;
    let mut chunked = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("the server closed the connection");
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    let mut events = Events::default();
    let mut answer = Answer { content: String::new(), cut: false, prompt_n: 0, cached_n: 0 };
    let mut raw = Vec::new();
    loop {
        if cancel.is_cancelled() || !keep_going() {
            bail!("cancelled");
        }
        let piece = if chunked {
            line.clear();
            reader.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16).context("chunk size")?;
            if size == 0 {
                break;
            }
            let mut piece = vec![0; size + 2];
            reader.read_exact(&mut piece)?;
            piece.truncate(size);
            piece
        } else {
            let mut piece = vec![0; 4096];
            let n = reader.read(&mut piece)?;
            if n == 0 {
                break;
            }
            piece.truncate(n);
            piece
        };
        if code != 200 {
            raw.extend_from_slice(&piece);
            continue;
        }
        if events.feed(&piece, &mut answer) {
            return Ok(answer);
        }
    }
    if code != 200 {
        bail!("server answered {code}: {}", String::from_utf8_lossy(&raw).chars().take(300).collect::<String>());
    }
    bail!("the answer ended before its last piece")
}

/// Server-sent events, `data: {json}` each, split across reads anyhow.
#[derive(Default)]
struct Events {
    pending: Vec<u8>,
}

impl Events {
    /// Adds what came; true once the last event is in.
    fn feed(&mut self, bytes: &[u8], answer: &mut Answer) -> bool {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.pending.drain(..end + 2).collect();
            let text = String::from_utf8_lossy(&event);
            let Some(data) = text.trim().strip_prefix("data:") else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) else { continue };
            if let Some(piece) = value["content"].as_str() {
                answer.content += piece;
            }
            if value["stop"].as_bool() == Some(true) {
                answer.cut = value["stop_type"].as_str() == Some("limit");
                answer.prompt_n = value["timings"]["prompt_n"].as_u64().unwrap_or_default();
                answer.cached_n = value["tokens_cached"].as_u64().unwrap_or_default();
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_anywhere_add_up() {
        let stream = "data: {\"content\":\"fn \",\"stop\":false}\n\ndata: {\"content\":\"main\",\"stop\":false}\n\ndata: {\"content\":\"\",\"stop\":true,\"stop_type\":\"word\",\"tokens_cached\":12,\"timings\":{\"prompt_n\":3}}\n\n";
        for cut in 1..stream.len() {
            let mut events = Events::default();
            let mut answer = Answer { content: String::new(), cut: true, prompt_n: 0, cached_n: 0 };
            let done = events.feed(&stream.as_bytes()[..cut], &mut answer) | events.feed(&stream.as_bytes()[cut..], &mut answer);
            assert!(done);
            assert_eq!((answer.content.as_str(), answer.cut, answer.prompt_n, answer.cached_n), ("fn main", false, 3, 12));
        }
    }

    #[test]
    fn status_lines() {
        assert_eq!(code("HTTP/1.1 503 Service Unavailable\r\n").unwrap(), 503);
        assert!(code("garbage").is_err());
    }
}
