//! Minimal blocking rust-analyzer client over LSP stdio.
//!
//! Speaks just enough JSON-RPC for one flow: `initialize`,
//! `workspace/symbol`, `textDocument/prepareCallHierarchy`,
//! `callHierarchy/incomingCalls`. Server-to-client requests get a best
//! effort `null` reply so the server never stalls waiting on us.

#![forbid(unsafe_code)]

use deepfunc_core::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: u32,
    pub location: Location,
    #[serde(rename = "containerName")]
    pub container_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
}

impl SymbolInfo {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn container(&self) -> Option<&str> {
        self.container_name.as_deref()
    }
    pub fn uri(&self) -> &str {
        &self.location.uri
    }
    pub fn line0(&self) -> u32 {
        self.location.range.start.line
    }
    pub fn char0(&self) -> u32 {
        self.location.range.start.character
    }
}

impl HierarchyItem {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn uri(&self) -> &str {
        &self.uri
    }
    /// 0-based definition span for body extraction.
    pub fn def_span0(&self) -> (u32, u32) {
        (self.range.start.line, self.range.end.line)
    }
    /// 0-based identifier position for follow-up hierarchy requests.
    pub fn sel_pos0(&self) -> (u32, u32) {
        (
            self.selection_range.start.line,
            self.selection_range.start.character,
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct IncomingCall {
    pub from: HierarchyItem,
    #[serde(rename = "fromRanges")]
    pub from_ranges: Vec<Range>,
}

impl IncomingCall {
    pub fn from(&self) -> &HierarchyItem {
        &self.from
    }
    /// 1-based line of the first call site in the caller.
    pub fn call_line1(&self) -> u32 {
        self.from_ranges
            .first()
            .map(|range| range.start.line + 1)
            .unwrap_or(1)
    }
}

#[derive(Debug, Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    id: i64,
    method: String,
    params: Value,
}

enum ClientEvent {
    Response { id: i64, result: Value },
}

fn write_message(stdin: &Arc<Mutex<ChildStdin>>, body: &str) -> Result<(), Error> {
    let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    match stdin.lock() {
        Ok(mut guard) => match guard.write_all(framed.as_bytes()) {
            Ok(()) => match guard.flush() {
                Ok(()) => Ok(()),
                Err(error) => Err(Error::RequestFailed {
                    request: "stdio-write".to_owned(),
                    timeout_secs: 0,
                    detail: error.to_string(),
                }),
            },
            Err(error) => Err(Error::RequestFailed {
                request: "stdio-write".to_owned(),
                timeout_secs: 0,
                detail: error.to_string(),
            }),
        },
        Err(error) => Err(Error::RequestFailed {
            request: "stdio-write".to_owned(),
            timeout_secs: 0,
            detail: error.to_string(),
        }),
    }
}

fn read_message(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    match content_length {
        Some(length) => {
            let mut buffer = vec![0u8; length];
            reader.read_exact(&mut buffer)?;
            Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
        }
        None => Ok(Some(String::new())),
    }
}

pub struct RaClient {
    stdin: Arc<Mutex<ChildStdin>>,
    events: mpsc::Receiver<ClientEvent>,
    next_id: i64,
    child: Option<Child>,
    timeout_secs: u64,
}

impl RaClient {
    /// Spawn `ra_bin` and run `initialize` against `root`.
    pub fn spawn(ra_bin: &str, root: &Path, timeout_secs: u64) -> Result<Self, Error> {
        let root_uri = format!("file://{}", root.display());
        let mut child = match Command::new(ra_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return Err(Error::RaNotFound {
                    searched_path: format!("{ra_bin} ({error})"),
                })
            }
        };
        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take();
        let stdin: Arc<Mutex<ChildStdin>> = match child_stdin {
            Some(stdin) => Arc::new(Mutex::new(stdin)),
            None => {
                return Err(Error::RequestFailed {
                    request: "spawn".to_owned(),
                    timeout_secs,
                    detail: "rust-analyzer started without a stdin pipe".to_owned(),
                })
            }
        };
        let (events_tx, events_rx) = mpsc::channel::<ClientEvent>();
        if let Some(stdout) = child_stdout {
            let reply_stdin = Arc::clone(&stdin);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(text)) = read_message(&mut reader) {
                    if text.is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    // Server-to-client request: best-effort null reply.
                    if value.get("method").is_some() {
                        if let Some(id) = value.get("id") {
                            let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
                            if let Ok(body) = serde_json::to_string(&reply) {
                                let _ignored = write_message(&reply_stdin, &body);
                            }
                        }
                        continue;
                    }
                    if let Some(id) = value.get("id").and_then(Value::as_i64) {
                        let result = value.get("result").cloned().unwrap_or(Value::Null);
                        if value.get("error").is_some() {
                            let _ignored =
                                events_tx.send(ClientEvent::Response { id, result: value });
                        } else if events_tx
                            .send(ClientEvent::Response { id, result })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        }
        let mut client = Self {
            stdin,
            events: events_rx,
            next_id: 1,
            child: Some(child),
            timeout_secs,
        };
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
        });
        client.request("initialize", params)?;
        client.notify("initialized", json!({}));
        Ok(client)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let message = RpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_owned(),
            params,
        };
        let body = match serde_json::to_string(&message) {
            Ok(body) => body,
            Err(error) => {
                return Err(Error::RequestFailed {
                    request: method.to_owned(),
                    timeout_secs: self.timeout_secs,
                    detail: "failed to encode request: ".to_owned() + &error.to_string(),
                })
            }
        };
        write_message(&self.stdin, &body)?;
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::RequestFailed {
                    request: method.to_owned(),
                    timeout_secs: self.timeout_secs,
                    detail: "timed out waiting for a response".to_owned(),
                });
            }
            match self.events.recv_timeout(remaining) {
                Ok(ClientEvent::Response { id: got, result }) => {
                    if got == id {
                        if result.get("error").is_some() {
                            return Err(Error::RequestFailed {
                                request: method.to_owned(),
                                timeout_secs: self.timeout_secs,
                                detail: result.to_string(),
                            });
                        }
                        return Ok(result);
                    }
                }
                Err(_) => {
                    return Err(Error::RequestFailed {
                        request: method.to_owned(),
                        timeout_secs: self.timeout_secs,
                        detail: "timed out waiting for a response".to_owned(),
                    })
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Ok(text) = serde_json::to_string(&body) {
            let _ignored = write_message(&self.stdin, &text);
        }
    }

    /// Open a file so later hierarchy requests resolve against fresh text.
    pub fn did_open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "rust", "version": 1, "text": text,
            }}),
        );
    }

    /// Find function/method symbols matching `query` (rust-analyzer
    /// `workspace/symbol`). Returns only kind Function (12) / Method (6).
    /// Polls while empty: rust-analyzer loads the workspace asynchronously
    /// after `initialize`, so the first queries can race indexing.
    pub fn workspace_symbols(&mut self, query: &str) -> Result<Vec<SymbolInfo>, Error> {
        let start = Instant::now();
        let budget = Duration::from_secs(30);
        loop {
            let result = self.request("workspace/symbol", json!({"query": query}))?;
            let symbols: Vec<SymbolInfo> = match serde_json::from_value(result) {
                Ok(symbols) => symbols,
                Err(error) => {
                    return Err(Error::RequestFailed {
                        request: "workspace/symbol".to_owned(),
                        timeout_secs: self.timeout_secs,
                        detail: "failed to decode symbols: ".to_owned() + &error.to_string(),
                    })
                }
            };
            let functions: Vec<SymbolInfo> = symbols
                .into_iter()
                .filter(|symbol| symbol.kind == 6 || symbol.kind == 12)
                .collect();
            if !functions.is_empty() || start.elapsed() >= budget {
                return Ok(functions);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Resolve a document position to hierarchy items.
    pub fn prepare_hierarchy(
        &mut self,
        uri: &str,
        line0: u32,
        character: u32,
    ) -> Result<Vec<HierarchyItem>, Error> {
        let result = self.request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument": {"uri": uri},
                   "position": {"line": line0, "character": character}}),
        )?;
        match serde_json::from_value(result) {
            Ok(items) => Ok(items),
            Err(error) => Err(Error::RequestFailed {
                request: "textDocument/prepareCallHierarchy".to_owned(),
                timeout_secs: self.timeout_secs,
                detail: "failed to decode hierarchy: ".to_owned() + &error.to_string(),
            }),
        }
    }

    /// Direct callers of one hierarchy item.
    pub fn incoming_calls(&mut self, item: &HierarchyItem) -> Result<Vec<IncomingCall>, Error> {
        let item_value = match serde_json::to_value(item) {
            Ok(value) => value,
            Err(error) => {
                return Err(Error::RequestFailed {
                    request: "callHierarchy/incomingCalls".to_owned(),
                    timeout_secs: self.timeout_secs,
                    detail: "failed to encode item: ".to_owned() + &error.to_string(),
                })
            }
        };
        let result = self.request("callHierarchy/incomingCalls", json!({"item": item_value}))?;
        match serde_json::from_value(result) {
            Ok(calls) => Ok(calls),
            Err(error) => Err(Error::RequestFailed {
                request: "callHierarchy/incomingCalls".to_owned(),
                timeout_secs: self.timeout_secs,
                detail: "failed to decode callers: ".to_owned() + &error.to_string(),
            }),
        }
    }

    /// Best-effort LSP shutdown handshake.
    pub fn shutdown(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null});
        if let Ok(text) = serde_json::to_string(&body) {
            let _ignored = write_message(&self.stdin, &text);
        }
        self.notify("exit", Value::Null);
        if let Some(mut child) = self.child.take() {
            let _ignored = child.wait();
        }
    }
}

impl Drop for RaClient {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.shutdown();
        }
    }
}

/// Convert a `file://` URI to a filesystem path. Handles the common
/// percent-escapes; returns `None` for non-file URIs.
pub fn uri_to_path(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("file://")?;
    let mut out = String::with_capacity(stripped.len());
    let bytes = stripped.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high * 16 + low) as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
