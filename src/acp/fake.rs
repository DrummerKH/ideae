//! A stand-in ACP agent for the tests: this test binary, run again with
//! [`ENV`] set, speaks ACP on its standard streams. What it does goes by the
//! prompt: "edit" asks to change `f.txt` and writes it through the client,
//! "read" reads it through the client, "slow" works until cancelled;
//! anything else is answered with a few words.

use std::io::{BufRead as _, Write as _};

use serde_json::{Value, json};

pub const ENV: &str = "IDEAE_FAKE_ACP";

/// The command that starts it.
pub fn command() -> String {
    let exe = std::env::current_exe().unwrap();
    format!("{} --exact acp::fake::tests::agent --ignored --nocapture --test-threads=1", exe.display())
}

struct Io {
    lines: std::io::Lines<std::io::StdinLock<'static>>,
    next: u64,
}

impl Io {
    fn send(&self, msg: Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    fn update(&self, session: &str, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } }));
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn next(&mut self) -> Option<Value> {
        loop {
            let line = self.lines.next()?.ok()?;
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                return Some(v);
            }
        }
    }

    /// Asks the client and waits for its answer, taking what comes meanwhile.
    fn ask(&mut self, method: &str, params: Value, meanwhile: &mut Vec<Value>) -> Value {
        self.next += 1;
        let id = 1000 + self.next;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let Some(msg) = self.next() else { std::process::exit(0) };
            if msg.get("id").and_then(Value::as_u64) == Some(id) && msg.get("method").is_none() {
                return msg;
            }
            meanwhile.push(msg);
        }
    }
}

fn options(model: &str) -> Value {
    json!([
        { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
          "options": [{ "value": "small", "name": "Small" }, { "value": "big", "name": "Big" }] },
        { "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "ask",
          "options": [{ "value": "ask", "name": "Ask" }, { "value": "auto", "name": "Auto" }] },
    ])
}

pub fn serve() {
    // The test harness wrote "test … " with no line end: ours starts a line.
    println!();
    let _ = std::fs::write(std::env::current_dir().unwrap().join("agent.pid"), std::process::id().to_string());
    let mut io = Io { lines: std::io::stdin().lock().lines(), next: 0 };
    let mut model = "small".to_string();
    let mut waiting: Vec<Value> = Vec::new();
    loop {
        let msg = match waiting.pop() {
            Some(msg) => msg,
            None => match io.next() {
                Some(msg) => msg,
                None => return,
            },
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let session = params.get("sessionId").and_then(Value::as_str).unwrap_or("fake-1").to_string();
        match msg.get("method").and_then(Value::as_str).unwrap_or_default() {
            "initialize" => io.reply(&id, json!({
                "protocolVersion": 1,
                "agentCapabilities": { "loadSession": true },
                "authMethods": [],
                "agentInfo": { "name": "fake", "version": "1" },
            })),
            "session/new" => {
                io.reply(&id, json!({ "sessionId": "fake-1", "configOptions": options(&model) }));
                io.update("fake-1", json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
                    { "name": "review", "description": "Review the changes of the task" },
                    { "name": "init", "description": "Write an AGENTS.md for the project" },
                    { "name": "compact", "description": "Sum up the conversation to free its context" }] }));
            }
            "session/load" if session == "gone" => {
                io.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32002, "message": "Resource not found" } }));
            }
            "session/load" => {
                io.update(&session, json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "from before" } }));
                io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "replayed" } }));
                io.update(&session, json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
                    { "name": "review", "description": "Review the changes of the task" },
                    { "name": "init", "description": "Write an AGENTS.md for the project" },
                    { "name": "compact", "description": "Sum up the conversation to free its context" }] }));
                io.reply(&id, json!({ "configOptions": options(&model) }));
            }
            "session/set_config_option" => {
                if params.get("configId").and_then(Value::as_str) == Some("model") {
                    model = params.get("value").and_then(Value::as_str).unwrap_or_default().to_string();
                }
                io.reply(&id, json!({ "configOptions": options(&model) }));
            }
            "session/prompt" => {
                let text = params.pointer("/prompt/0/text").and_then(Value::as_str).unwrap_or_default().to_string();
                let cwd = std::env::current_dir().unwrap();
                let file = cwd.join("f.txt");
                if text.contains("edit") {
                    let tool = json!({ "toolCallId": "t1", "title": "Edit f.txt", "kind": "edit", "status": "pending",
                        "locations": [{ "path": file }] });
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit f.txt", "kind": "edit", "status": "pending" }));
                    let answer = io.ask("session/request_permission", json!({ "sessionId": session, "toolCall": tool,
                        "options": [{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }, { "optionId": "no", "name": "Reject", "kind": "reject_once" }] }), &mut waiting);
                    let allowed = answer.pointer("/result/outcome/optionId").and_then(Value::as_str) == Some("yes");
                    if allowed {
                        io.ask("fs/write_text_file", json!({ "sessionId": session, "path": file, "content": "edited\n" }), &mut waiting);
                        io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                            "content": [{ "type": "diff", "path": file, "oldText": "one\n", "newText": "edited\n" }] }));
                        for part in ["Edi", "ted", "."] {
                            io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": part } }));
                        }
                    } else {
                        io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed" }));
                        io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Not edited." } }));
                    }
                } else if text.contains("read") {
                    let read = io.ask("fs/read_text_file", json!({ "sessionId": session, "path": file }), &mut waiting);
                    let content = read.pointer("/result/content").and_then(Value::as_str).unwrap_or("?").trim().to_string();
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("It says {content}.") } }));
                } else if text.contains("long") {
                    // A long answer in small pieces, as a model streams it.
                    for n in 0..3000 {
                        let piece = match n % 40 {
                            0 => format!("\n\n## Part {}\n\n", n / 40),
                            13 => "\n```rust\nfn main() { println!(\"hi\"); }\n```\n".to_string(),
                            _ => format!("word{n} **bold** `code` "),
                        };
                        io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": piece } }));
                    }
                } else if text.contains("look") {
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "t3", "title": "Read f.txt", "kind": "read", "status": "pending", "locations": [{ "path": file, "line": 1 }] }));
                    io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t3", "status": "completed" }));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Looked." } }));
                } else if text.contains("plan") {
                    io.update(&session, json!({ "sessionUpdate": "plan", "entries": [
                        { "content": "Read the code", "status": "completed", "priority": "high" },
                        { "content": "Change it", "status": "in_progress", "priority": "high" },
                        { "content": "Run the tests", "status": "pending", "priority": "medium" }] }));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Planned." } }));
                } else if text.contains("run") {
                    let made = io.ask("terminal/create", json!({ "sessionId": session, "command": "echo", "args": ["hello", "there"] }), &mut waiting);
                    let id = made.pointer("/result/terminalId").and_then(Value::as_str).unwrap_or_default().to_string();
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "echo hello there", "kind": "execute", "status": "in_progress",
                        "content": [{ "type": "terminal", "terminalId": id }] }));
                    io.ask("terminal/wait_for_exit", json!({ "sessionId": session, "terminalId": id }), &mut waiting);
                    let out = io.ask("terminal/output", json!({ "sessionId": session, "terminalId": id }), &mut waiting);
                    let printed = out.pointer("/result/output").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                    io.ask("terminal/release", json!({ "sessionId": session, "terminalId": id }), &mut waiting);
                    io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed" }));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("It printed {printed}.") } }));
                } else if text.contains("serve") {
                    // A server left running in the background: the command
                    // itself goes on too, until ended.
                    io.ask("terminal/create", json!({ "sessionId": session, "command": "sh", "args": ["-c", "sleep 300 & echo $! > bg.pid; sleep 300"] }), &mut waiting);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Serving." } }));
                } else if text.contains("script") {
                    // A command of many lines, as an agent writes a file with a heredoc.
                    let title = "mkdir -p /tmp/x && cat > /tmp/x/a.mjs <<'EOF'\nimport { spawn } from 'node:child_process';\nconst child = spawn('node', []);\nEOF\ncd /tmp/x && node a.mjs";
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "s1", "title": title, "kind": "execute", "status": "completed" }));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Ran the script. ".repeat(40) } }));
                } else if text.contains("clean up") {
                    // An agent tidying up that takes its own process for a stray one.
                    let me = std::process::id();
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "k1", "title": format!("kill {me}"), "kind": "execute", "status": "in_progress" }));
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    // SAFETY: signals this process itself, as `kill` would.
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(me as i32, libc::SIGTERM)
                    };
                    // Windows has no signal to send itself: it ends as one killed would.
                    #[cfg(not(unix))]
                    std::process::exit(143);
                    #[cfg(unix)]
                    std::thread::sleep(std::time::Duration::from_secs(5));
                } else if text.contains("crash") {
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Start" } }));
                    eprintln!("fake: out of memory");
                    std::process::exit(3);
                } else if text.contains("tools") {
                    // Many tool calls in one turn, as a search over a project.
                    for n in 0..300 {
                        let id = format!("m{n}");
                        io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": format!("Read src/file{n}.rs"), "kind": "read", "status": "in_progress",
                            "locations": [{ "path": cwd.join(format!("src/file{n}.rs")) }] }));
                        io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
                            "content": [{ "type": "content", "content": { "type": "text", "text": "fn x() {}\n".repeat(40) } }] }));
                    }
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Read them." } }));
                } else if text.contains("noisy") {
                    // A command that prints a lot for a while, shown live.
                    let made = io.ask("terminal/create", json!({ "sessionId": session, "command": "sh", "args": ["-c", "for i in $(seq 1 40); do seq 1 20000; sleep 0.02; done"], "outputByteLimit": 100000 }), &mut waiting);
                    let id = made.pointer("/result/terminalId").and_then(Value::as_str).unwrap_or_default().to_string();
                    io.update(&session, json!({ "sessionUpdate": "tool_call", "toolCallId": "n1", "title": "a noisy command", "kind": "execute", "status": "in_progress",
                        "content": [{ "type": "terminal", "terminalId": id }] }));
                    io.ask("terminal/wait_for_exit", json!({ "sessionId": session, "terminalId": id }), &mut waiting);
                    io.ask("terminal/release", json!({ "sessionId": session, "terminalId": id }), &mut waiting);
                    io.update(&session, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "n1", "status": "completed" }));
                    io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Noisy done." } }));
                } else if text.contains("slow") {
                    // Until cancelled.
                    loop {
                        let Some(next) = io.next() else { return };
                        if next.get("method").and_then(Value::as_str) == Some("session/cancel") {
                            break;
                        }
                        waiting.push(next);
                    }
                    io.reply(&id, json!({ "stopReason": "cancelled" }));
                    continue;
                } else {
                    io.update(&session, json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "Thinking." } }));
                    for part in ["Hello ", "there", "."] {
                        io.update(&session, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": part } }));
                    }
                    io.update(&session, json!({ "sessionUpdate": "usage_update", "used": 1200, "size": 200000, "cost": { "amount": 0.01, "currency": "USD" } }));
                }
                io.reply(&id, json!({ "stopReason": "end_turn" }));
            }
            _ if msg.get("method").is_some() && !id.is_null() => {
                io.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "unknown" } }));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    /// The agent itself, when run by a host; nothing otherwise.
    #[test]
    #[ignore]
    fn agent() {
        if std::env::var(super::ENV).is_ok() {
            super::serve();
        }
    }
}
